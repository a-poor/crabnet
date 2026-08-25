use std::path::Path;

use rusqlite::{Connection, params, params_from_iter};

use crate::{
    Parameters, QueryResult, QuerySummary, Result,
    planner::{self, Plan, ResolvedCreatePlan},
};

const APPLICATION_ID: i64 = 0x4352_4142; // "CRAB"
const SCHEMA_VERSION: i64 = 1;

const INITIAL_SCHEMA: &str = "
BEGIN IMMEDIATE;

CREATE TABLE nodes (
    id TEXT PRIMARY KEY NOT NULL,
    label TEXT,
    props TEXT NOT NULL DEFAULT '{}'
        CHECK (json_valid(props) AND json_type(props) = 'object')
) STRICT;

CREATE TABLE paths (
    id TEXT PRIMARY KEY NOT NULL,
    path_type TEXT NOT NULL CHECK (path_type <> ''),
    from_node TEXT NOT NULL
        REFERENCES nodes(id) ON DELETE RESTRICT,
    to_node TEXT NOT NULL
        REFERENCES nodes(id) ON DELETE RESTRICT,
    props TEXT NOT NULL DEFAULT '{}'
        CHECK (json_valid(props) AND json_type(props) = 'object')
) STRICT;

CREATE INDEX nodes_by_label
    ON nodes(label);
CREATE INDEX paths_from_type_to
    ON paths(from_node, path_type, to_node);
CREATE INDEX paths_to_type_from
    ON paths(to_node, path_type, from_node);

PRAGMA application_id = 1129464130;
PRAGMA user_version = 1;
COMMIT;
";

/// An embedded graph database connection.
///
/// Each `Database` owns one SQLite connection. Queries are planned completely,
/// and all named parameters are resolved, before a write transaction begins.
pub struct Database {
    connection: Connection,
}

impl Database {
    /// Opens a transient in-memory graph.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Opens or creates a graph at a SQLite database path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self> {
        initialize(&mut connection)?;
        Ok(Self { connection })
    }

    /// Executes a Cypher query without parameters.
    pub fn execute(&mut self, cypher: &str) -> Result<QueryResult> {
        self.execute_with(cypher, &Parameters::new())
    }

    /// Executes a Cypher query with named `$parameter` values.
    pub fn execute_with(&mut self, cypher: &str, parameters: &Parameters) -> Result<QueryResult> {
        match planner::compile(cypher)? {
            Plan::Read(plan) => {
                let bindings = plan.resolve_bindings(parameters)?;
                let mut statement = self.connection.prepare(&plan.sql)?;
                debug_assert!(statement.readonly());
                let column_count = plan.columns.len();
                let rows = statement
                    .query_map(params_from_iter(bindings.iter()), |row| {
                        (0..column_count)
                            .map(|index| row.get::<_, String>(index))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })?
                    .map(|row| {
                        row.map_err(crate::Error::from).and_then(|values| {
                            values
                                .into_iter()
                                .map(|value| {
                                    serde_json::from_str(&value).map_err(|error| {
                                        crate::Error::Data(format!(
                                            "query returned malformed JSON `{value}`: {error}"
                                        ))
                                    })
                                })
                                .collect::<Result<Vec<_>>>()
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(QueryResult {
                    columns: plan.columns,
                    rows,
                    summary: QuerySummary::default(),
                })
            }
            Plan::Create(plan) => {
                // Resolving every property and RETURN expression here keeps a
                // missing/invalid parameter from starting a partial write.
                let plan = plan.resolve(parameters)?;
                self.execute_create(plan)
            }
        }
    }

    fn execute_create(&mut self, plan: ResolvedCreatePlan) -> Result<QueryResult> {
        let transaction = self.connection.transaction()?;
        let mut node_ids = Vec::with_capacity(plan.nodes.len());
        for node in &plan.nodes {
            let properties = node.properties.to_string();
            let id = transaction.query_row(
                "INSERT INTO nodes (id, label, props)
                 VALUES (lower(hex(randomblob(16))), ?1, json(?2))
                 RETURNING id",
                params![node.label.as_deref(), properties],
                |row| row.get::<_, String>(0),
            )?;
            node_ids.push(id);
        }

        let mut relationship_ids = Vec::with_capacity(plan.relationships.len());
        for relationship in &plan.relationships {
            let from = node_ids.get(relationship.from).ok_or_else(|| {
                crate::Error::Data("relationship has an invalid start-node slot".into())
            })?;
            let to = node_ids.get(relationship.to).ok_or_else(|| {
                crate::Error::Data("relationship has an invalid end-node slot".into())
            })?;
            let properties = relationship.properties.to_string();
            let id = transaction.query_row(
                "INSERT INTO paths (id, path_type, from_node, to_node, props)
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, json(?4))
                 RETURNING id",
                params![relationship.relationship_type, from, to, properties],
                |row| row.get::<_, String>(0),
            )?;
            relationship_ids.push(id);
        }

        let rows = plan
            .result_row(&node_ids, &relationship_ids)?
            .into_iter()
            .collect();
        let result = QueryResult {
            columns: plan.columns(),
            rows,
            summary: QuerySummary {
                nodes_created: plan.nodes.len(),
                relationships_created: plan.relationships.len(),
            },
        };
        transaction.commit()?;
        Ok(result)
    }
}

fn initialize(connection: &mut Connection) -> Result<()> {
    // Foreign-key enforcement is connection-local and cannot be changed while
    // a transaction is active, so enable it before schema initialization.
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;

    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let schema_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if application_id == APPLICATION_ID {
        if schema_version != SCHEMA_VERSION {
            return Err(crate::Error::Data(format!(
                "unsupported Crabnet schema version {schema_version}; expected {SCHEMA_VERSION}"
            )));
        }
        validate_schema(connection)?;
        return Ok(());
    }
    if application_id != 0 || schema_version != 0 {
        return Err(crate::Error::Data(format!(
            "SQLite file belongs to another application or schema \
             (application_id={application_id}, user_version={schema_version})"
        )));
    }

    let user_tables: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if user_tables != 0 {
        return Err(crate::Error::Data(
            "refusing to initialize Crabnet in a non-empty SQLite database".into(),
        ));
    }

    if let Err(error) = connection.execute_batch(INITIAL_SCHEMA) {
        let _ = connection.execute_batch("ROLLBACK;");
        return Err(error.into());
    }
    validate_schema(connection)
}

fn validate_schema(connection: &Connection) -> Result<()> {
    connection.prepare("SELECT id, label, props FROM nodes LIMIT 0")?;
    connection.prepare("SELECT id, path_type, from_node, to_node, props FROM paths LIMIT 0")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    #[test]
    fn schema_enables_foreign_keys_and_checks_both_endpoints() {
        let database = Database::in_memory().unwrap();
        let enabled: i64 = database
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(enabled, 1);
        let application_id: i64 = database
            .connection
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        let schema_version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(application_id, APPLICATION_ID);
        assert_eq!(schema_version, SCHEMA_VERSION);

        database
            .connection
            .execute(
                "INSERT INTO nodes (id, label) VALUES ('one', 'Thing'), ('two', 'Thing')",
                [],
            )
            .unwrap();
        database
            .connection
            .execute(
                "INSERT INTO paths (id, path_type, from_node, to_node)
                 VALUES ('valid', 'ARBITRARY_TYPE', 'one', 'two')",
                [],
            )
            .unwrap();
        assert!(
            database
                .connection
                .execute("DELETE FROM nodes WHERE id = 'one'", [])
                .is_err()
        );
        let error = database
            .connection
            .execute(
                "INSERT INTO paths (id, path_type, from_node, to_node)
                 VALUES ('bad', 'ANY_TYPE', 'one', 'missing')",
                [],
            )
            .unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));

        let error = database
            .connection
            .execute(
                "INSERT INTO paths (id, path_type, from_node, to_node)
                 VALUES ('also-bad', 'ANY_TYPE', 'missing', 'one')",
                [],
            )
            .unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));
    }

    #[test]
    fn schema_requires_object_properties() {
        let database = Database::in_memory().unwrap();
        assert!(
            database
                .connection
                .execute(
                    "INSERT INTO nodes (id, props) VALUES ('bad-json', '[]')",
                    [],
                )
                .is_err()
        );
        let props: String = database
            .connection
            .query_row(
                "INSERT INTO nodes (id) VALUES ('default-props') RETURNING props",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
            .unwrap();
        assert_eq!(props, "{}");
    }

    #[test]
    fn initialization_does_not_downstamp_a_future_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID}; PRAGMA user_version = 2;"
            ))
            .unwrap();

        let error = initialize(&mut connection).unwrap_err();
        assert!(matches!(error, crate::Error::Data(_)));
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn initialization_is_idempotent_for_the_same_database() {
        let mut database = Database::in_memory().unwrap();
        database
            .connection
            .execute("INSERT INTO nodes (id) VALUES ('preserved')", [])
            .unwrap();

        initialize(&mut database.connection).unwrap();

        let count: i64 = database
            .connection
            .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn a_late_create_failure_rolls_back_earlier_nodes() {
        let mut database = Database::in_memory().unwrap();
        database
            .connection
            .execute_batch(
                "CREATE TRIGGER reject_flagged_node
                 BEFORE INSERT ON nodes
                 WHEN json_extract(NEW.props, '$.reject') = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'rejected for rollback test');
                 END;",
            )
            .unwrap();

        assert!(
            database
                .execute("CREATE (:Thing {ok: true}), (:Thing {reject: true})")
                .is_err()
        );
        let count: i64 = database
            .connection
            .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
