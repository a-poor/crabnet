//! Persistent node-property uniqueness, shared by standalone and transactional
//! schema operations. Triggers keep keys and graph changes in the same SQLite
//! statement, including when a label is added or removed.

use rusqlite::{Connection, TransactionBehavior, functions::FunctionFlags, params};

use crate::{Database, Result};

/// Uniqueness of one property among nodes carrying a particular label.
///
/// Missing and null properties are exempt. Non-null values use the recursive
/// typed equivalence of `DISTINCT`: equal integers and floats conflict, while
/// booleans and strings stay distinct from numbers. Lists and maps compare
/// recursively, including equal null members. Names are exact, case-sensitive
/// strings, supplied without Cypher quoting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniqueConstraint {
    /// The node label whose members must have distinct property values.
    pub label: String,
    /// The top-level property key to constrain.
    pub property: String,
}

pub(super) const SCHEMA: &str = r#"
CREATE TABLE node_unique_constraints (
    label TEXT NOT NULL,
    property TEXT NOT NULL,
    PRIMARY KEY (label, property)
) STRICT, WITHOUT ROWID;

CREATE TABLE node_unique_values (
    label TEXT NOT NULL,
    property TEXT NOT NULL,
    value BLOB NOT NULL,
    node_id TEXT NOT NULL,
    PRIMARY KEY (label, property, value),
    FOREIGN KEY (label, property) REFERENCES node_unique_constraints(label, property)
        ON DELETE CASCADE
) STRICT, WITHOUT ROWID;
CREATE INDEX node_unique_values_by_node ON node_unique_values(node_id);

-- Report the application key rather than leaking an internal index name.
-- RAISE also keeps an outer ON CONFLICT policy from suppressing a violation.
CREATE TRIGGER node_unique_values_conflict BEFORE INSERT ON node_unique_values
WHEN EXISTS (
    SELECT 1 FROM node_unique_values
    WHERE label = NEW.label AND property = NEW.property AND value = NEW.value
)
BEGIN
    SELECT RAISE(ABORT, 'crabnet-constraint-error: uniqueness of node label '
        || json_quote(NEW.label) || ' property ' || json_quote(NEW.property));
END;

CREATE TRIGGER node_unique_constraints_insert AFTER INSERT ON node_unique_constraints BEGIN
    INSERT INTO node_unique_values (label, property, value, node_id)
        SELECT NEW.label, NEW.property, crabnet_unique_key(nodes.props, NEW.property), nodes.id
        FROM node_labels JOIN nodes ON nodes.id = node_labels.node_id
        WHERE node_labels.label = NEW.label
          AND crabnet_unique_key(nodes.props, NEW.property) IS NOT NULL;
END;

-- Read NEW.labels directly: maintenance must not depend on which AFTER
-- trigger (labels, properties, or uniqueness) SQLite chooses to run first.
CREATE TRIGGER node_unique_insert AFTER INSERT ON nodes BEGIN
    INSERT INTO node_unique_values (label, property, value, node_id)
        SELECT c.label, c.property, crabnet_unique_key(NEW.props, c.property), NEW.id
        FROM json_each(NEW.labels) AS labels
        JOIN node_unique_constraints AS c ON c.label = labels.value
        WHERE crabnet_unique_key(NEW.props, c.property) IS NOT NULL;
END;
CREATE TRIGGER node_unique_update AFTER UPDATE OF id, labels, props ON nodes BEGIN
    DELETE FROM node_unique_values WHERE node_id = OLD.id;
    INSERT INTO node_unique_values (label, property, value, node_id)
        SELECT c.label, c.property, crabnet_unique_key(NEW.props, c.property), NEW.id
        FROM json_each(NEW.labels) AS labels
        JOIN node_unique_constraints AS c ON c.label = labels.value
        WHERE crabnet_unique_key(NEW.props, c.property) IS NOT NULL;
END;
CREATE TRIGGER node_unique_delete AFTER DELETE ON nodes BEGIN
    DELETE FROM node_unique_values WHERE node_id = OLD.id;
END;
"#;

pub(super) fn register(connection: &Connection) -> Result<()> {
    connection.create_scalar_function(
        "crabnet_unique_key",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            // Decode the original property object in Rust. Extracting through
            // SQLite JSON first would round some floating-point values.
            let properties = super::json_argument(context, 0)?;
            let property = context.get::<String>(1)?;
            Ok(properties
                .get(&property)
                .filter(|value| !value.is_null())
                .map(super::semantic_json_key))
        },
    )?;
    Ok(())
}

pub(super) fn create(connection: &Connection, label: &str, property: &str) -> Result<bool> {
    Ok(connection.execute(
        "INSERT INTO node_unique_constraints (label, property) VALUES (?1, ?2)
         ON CONFLICT (label, property) DO NOTHING",
        params![label, property],
    )? != 0)
}

pub(super) fn drop(connection: &Connection, label: &str, property: &str) -> Result<bool> {
    Ok(connection.execute(
        "DELETE FROM node_unique_constraints WHERE label = ?1 AND property = ?2",
        params![label, property],
    )? != 0)
}

pub(super) fn list(connection: &Connection) -> Result<Vec<UniqueConstraint>> {
    Ok(connection
        .prepare("SELECT label, property FROM node_unique_constraints ORDER BY label, property")?
        .query_map([], |row| {
            Ok(UniqueConstraint {
                label: row.get(0)?,
                property: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

impl Database {
    /// Installs uniqueness for a node label/property pair, checking existing data.
    ///
    /// Returns `true` if created, or `false` if already installed. Conflicting
    /// data returns [`crate::Error::Constraint`] and leaves the schema and graph
    /// unchanged. Checks apply immediately to each node mutation, including
    /// intermediate states within a query. See [`UniqueConstraint`] for equality
    /// and null semantics.
    ///
    /// ```
    /// use crabnet::Database;
    /// let mut database = Database::in_memory()?;
    /// database.create_unique_constraint("Person", "email")?;
    /// database.execute("MERGE (:Person {email: 'ada@example.org'})")?;
    /// assert!(database.execute("CREATE (:Person {email: 'ada@example.org'})").is_err());
    /// # Ok::<(), crabnet::Error>(())
    /// ```
    pub fn create_unique_constraint(&mut self, label: &str, property: &str) -> Result<bool> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let created = create(&transaction, label, property)?;
        transaction.commit()?;
        Ok(created)
    }

    /// Removes a node uniqueness constraint. Returns `false` if it was absent.
    /// Stored nodes and their properties are unchanged.
    pub fn drop_unique_constraint(&mut self, label: &str, property: &str) -> Result<bool> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = drop(&transaction, label, property)?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Lists installed constraints in label/property order.
    pub fn unique_constraints(&self) -> Result<Vec<UniqueConstraint>> {
        list(&self.connection)
    }
}
