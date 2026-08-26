use std::path::Path;

use rusqlite::{Connection, Transaction, TransactionBehavior, params, params_from_iter};

use crate::{
    CypherValue, NodeValue, Parameters, QueryResult, QuerySummary, RelationshipValue, Result,
    planner::{
        self, EntityKind, OutputShape, Plan, ResolvedExpr, ResolvedMutation, ResolvedMutationPlan,
        SlotId,
    },
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

CREATE TABLE edges (
    id TEXT PRIMARY KEY NOT NULL,
    edge_type TEXT NOT NULL CHECK (edge_type <> ''),
    from_node TEXT NOT NULL
        REFERENCES nodes(id) ON DELETE RESTRICT,
    to_node TEXT NOT NULL
        REFERENCES nodes(id) ON DELETE RESTRICT,
    props TEXT NOT NULL DEFAULT '{}'
        CHECK (json_valid(props) AND json_type(props) = 'object')
) STRICT;

CREATE INDEX nodes_by_label
    ON nodes(label);
CREATE INDEX edges_from_type_to
    ON edges(from_node, edge_type, to_node);
CREATE INDEX edges_to_type_from
    ON edges(to_node, edge_type, from_node);

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
                debug_assert_eq!(plan.columns.len(), plan.output_shapes.len());
                let rows = statement
                    .query_map(params_from_iter(bindings.iter()), |row| {
                        (0..plan.output_shapes.len())
                            .map(|index| row.get::<_, String>(index))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })?
                    .map(|row| {
                        row.map_err(crate::Error::from).and_then(|values| {
                            values
                                .into_iter()
                                .zip(plan.output_shapes.iter().copied())
                                .map(|(value, shape)| decode_output(&value, shape))
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
            Plan::Mutate(plan) => {
                // Resolving every property and RETURN expression here keeps a
                // missing/invalid parameter from starting a partial write.
                let plan = plan.resolve(parameters)?;
                self.execute_mutation(plan)
            }
        }
    }

    fn execute_mutation(&mut self, plan: ResolvedMutationPlan) -> Result<QueryResult> {
        // IMMEDIATE obtains the SQLite write reservation up front. MATCH and
        // CREATE still share one transaction, but do not risk a deferred
        // read-to-write upgrade failing halfway through the statement.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut input_rows = materialize_input(&transaction, &plan)?;
        let mut rows = Vec::with_capacity(input_rows.len());
        let mut summary = QuerySummary::default();

        for input_row in &mut input_rows {
            for mutation in &plan.mutations {
                match mutation {
                    ResolvedMutation::CreateNode {
                        slot,
                        label,
                        properties,
                    } => {
                        let properties_text = properties.to_string();
                        let id = transaction.query_row(
                            "INSERT INTO nodes (id, label, props)
                             VALUES (lower(hex(randomblob(16))), ?1, json(?2))
                             RETURNING id",
                            params![label.as_deref(), properties_text],
                            |row| row.get::<_, String>(0),
                        )?;
                        input_row.set(
                            &plan.slots,
                            *slot,
                            EntityValue::Node(NodeValue {
                                element_id: id,
                                labels: label.iter().cloned().collect(),
                                properties: cypher_properties(properties.clone())?,
                            }),
                        )?;
                        summary.nodes_created += 1;
                    }
                    ResolvedMutation::CreateRelationship {
                        slot,
                        relationship_type,
                        from,
                        to,
                        properties,
                    } => {
                        let from = input_row.node_id(*from)?.to_owned();
                        let to = input_row.node_id(*to)?.to_owned();
                        let properties_text = properties.to_string();
                        let id = transaction.query_row(
                            "INSERT INTO edges (id, edge_type, from_node, to_node, props)
                             VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, json(?4))
                             RETURNING id",
                            params![relationship_type, from, to, properties_text],
                            |row| row.get::<_, String>(0),
                        )?;
                        input_row.set(
                            &plan.slots,
                            *slot,
                            EntityValue::Relationship(RelationshipValue {
                                element_id: id,
                                relationship_type: relationship_type.clone(),
                                start_node_element_id: from,
                                end_node_element_id: to,
                                properties: cypher_properties(properties.clone())?,
                            }),
                        )?;
                        summary.relationships_created += 1;
                    }
                }
            }

            if let Some(projection) = &plan.projection {
                rows.push(
                    projection
                        .expressions
                        .iter()
                        .map(|expression| input_row.evaluate(expression))
                        .collect::<Result<Vec<_>>>()?,
                );
            }
        }

        let columns = plan
            .projection
            .as_ref()
            .map(|projection| projection.columns.clone())
            .unwrap_or_default();
        transaction.commit()?;
        Ok(QueryResult {
            columns,
            rows,
            summary,
        })
    }
}

#[derive(Clone, Debug)]
enum EntityValue {
    Node(NodeValue),
    Relationship(RelationshipValue),
}

impl EntityValue {
    fn kind(&self) -> EntityKind {
        match self {
            Self::Node(_) => EntityKind::Node,
            Self::Relationship(_) => EntityKind::Relationship,
        }
    }

    fn properties(&self) -> &std::collections::BTreeMap<String, CypherValue> {
        match self {
            Self::Node(node) => &node.properties,
            Self::Relationship(relationship) => &relationship.properties,
        }
    }

    fn to_cypher_value(&self) -> CypherValue {
        match self {
            Self::Node(node) => CypherValue::Node(node.clone()),
            Self::Relationship(relationship) => CypherValue::Relationship(relationship.clone()),
        }
    }
}

struct ExecutionRow {
    entities: Vec<Option<EntityValue>>,
}

impl ExecutionRow {
    fn empty(slot_count: usize) -> Self {
        Self {
            entities: vec![None; slot_count],
        }
    }

    fn set(&mut self, slots: &[EntityKind], slot: SlotId, entity: EntityValue) -> Result<()> {
        let expected = slots
            .get(slot.0)
            .ok_or_else(|| crate::Error::Data(format!("unknown logical slot {}", slot.0)))?;
        if *expected != entity.kind() {
            return Err(crate::Error::Data(format!(
                "logical slot {} received the wrong entity kind",
                slot.0
            )));
        }
        let destination = self
            .entities
            .get_mut(slot.0)
            .ok_or_else(|| crate::Error::Data(format!("unknown execution slot {}", slot.0)))?;
        if destination.replace(entity).is_some() {
            return Err(crate::Error::Data(format!(
                "logical slot {} was populated twice",
                slot.0
            )));
        }
        Ok(())
    }

    fn entity(&self, slot: SlotId) -> Result<&EntityValue> {
        self.entities
            .get(slot.0)
            .and_then(Option::as_ref)
            .ok_or_else(|| crate::Error::Data(format!("logical slot {} is empty", slot.0)))
    }

    fn node_id(&self, slot: SlotId) -> Result<&str> {
        match self.entity(slot)? {
            EntityValue::Node(node) => Ok(&node.element_id),
            EntityValue::Relationship(_) => Err(crate::Error::Data(format!(
                "logical slot {} is not a node",
                slot.0
            ))),
        }
    }

    fn evaluate(&self, expression: &ResolvedExpr) -> Result<CypherValue> {
        match expression {
            ResolvedExpr::Value(value) => CypherValue::try_from(value),
            ResolvedExpr::Entity(slot) => Ok(self.entity(*slot)?.to_cypher_value()),
            ResolvedExpr::Property(slot, key) => self
                .entity(*slot)?
                .properties()
                .get(key)
                .cloned()
                .map_or(Ok(CypherValue::Null), Ok),
            ResolvedExpr::ElementId(slot) => match self.entity(*slot)? {
                EntityValue::Node(node) => Ok(CypherValue::String(node.element_id.clone())),
                EntityValue::Relationship(relationship) => {
                    Ok(CypherValue::String(relationship.element_id.clone()))
                }
            },
            ResolvedExpr::Labels(slot) => match self.entity(*slot)? {
                EntityValue::Node(node) => Ok(CypherValue::List(
                    node.labels
                        .iter()
                        .cloned()
                        .map(CypherValue::String)
                        .collect(),
                )),
                EntityValue::Relationship(_) => Err(crate::Error::Data(format!(
                    "logical slot {} is not a node",
                    slot.0
                ))),
            },
            ResolvedExpr::RelationshipType(slot) => match self.entity(*slot)? {
                EntityValue::Relationship(relationship) => {
                    Ok(CypherValue::String(relationship.relationship_type.clone()))
                }
                EntityValue::Node(_) => Err(crate::Error::Data(format!(
                    "logical slot {} is not a relationship",
                    slot.0
                ))),
            },
        }
    }
}

fn materialize_input(
    transaction: &Transaction<'_>,
    plan: &ResolvedMutationPlan,
) -> Result<Vec<ExecutionRow>> {
    let Some(input) = &plan.input else {
        return Ok(vec![ExecutionRow::empty(plan.slots.len())]);
    };

    let mut statement = transaction.prepare(&input.sql)?;
    debug_assert!(statement.readonly());
    let mut cursor = statement.query(params_from_iter(input.bindings.iter()))?;
    let mut input_rows = Vec::new();
    while let Some(row) = cursor.next()? {
        let mut execution_row = ExecutionRow::empty(plan.slots.len());
        for (column, (slot, kind)) in input.slots.iter().enumerate() {
            let encoded = row.get::<_, String>(column)?;
            let entity = decode_entity(&encoded, *kind)?;
            execution_row.set(&plan.slots, *slot, entity)?;
        }
        input_rows.push(execution_row);
    }
    Ok(input_rows)
}

fn decode_entity(encoded: &str, kind: EntityKind) -> Result<EntityValue> {
    let value = serde_json::from_str::<serde_json::Value>(encoded).map_err(|error| {
        crate::Error::Data(format!("matched entity is malformed JSON: {error}"))
    })?;
    let serde_json::Value::Object(mut fields) = value else {
        return Err(crate::Error::Data(
            "matched entity record is not an object".into(),
        ));
    };
    let id = take_string(&mut fields, "id")?;
    let properties = fields
        .remove("props")
        .ok_or_else(|| crate::Error::Data("matched entity is missing `props`".into()))?;
    if !properties.is_object() {
        return Err(crate::Error::Data(
            "matched entity properties are not an object".into(),
        ));
    }

    match kind {
        EntityKind::Node => {
            let labels = match fields.remove("label") {
                Some(serde_json::Value::String(label)) => vec![label],
                Some(serde_json::Value::Null) => Vec::new(),
                _ => {
                    return Err(crate::Error::Data(
                        "matched node has an invalid label".into(),
                    ));
                }
            };
            Ok(EntityValue::Node(NodeValue {
                element_id: id,
                labels,
                properties: cypher_properties(properties)?,
            }))
        }
        EntityKind::Relationship => Ok(EntityValue::Relationship(RelationshipValue {
            element_id: id,
            relationship_type: take_string(&mut fields, "type")?,
            start_node_element_id: take_string(&mut fields, "from")?,
            end_node_element_id: take_string(&mut fields, "to")?,
            properties: cypher_properties(properties)?,
        })),
    }
}

fn decode_output(encoded: &str, shape: OutputShape) -> Result<CypherValue> {
    match shape {
        OutputShape::Json => {
            let value = serde_json::from_str::<serde_json::Value>(encoded).map_err(|error| {
                crate::Error::Data(format!(
                    "query returned malformed JSON `{encoded}`: {error}"
                ))
            })?;
            CypherValue::try_from(value)
        }
        OutputShape::Entity(_) if encoded == "null" => Ok(CypherValue::Null),
        OutputShape::Entity(kind) => {
            decode_entity(encoded, kind).map(|value| value.to_cypher_value())
        }
    }
}

fn cypher_properties(
    properties: serde_json::Value,
) -> Result<std::collections::BTreeMap<String, CypherValue>> {
    match CypherValue::try_from(properties)? {
        CypherValue::Map(properties) => Ok(properties),
        _ => Err(crate::Error::Data(
            "entity properties are not a JSON object".into(),
        )),
    }
}

fn take_string(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String> {
    match fields.remove(key) {
        Some(serde_json::Value::String(value)) => Ok(value),
        _ => Err(crate::Error::Data(format!(
            "matched entity is missing string field `{key}`"
        ))),
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
    connection.prepare("SELECT id, edge_type, from_node, to_node, props FROM edges LIMIT 0")?;
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
                "INSERT INTO edges (id, edge_type, from_node, to_node)
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
                "INSERT INTO edges (id, edge_type, from_node, to_node)
                 VALUES ('bad', 'ANY_TYPE', 'one', 'missing')",
                [],
            )
            .unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));

        let error = database
            .connection
            .execute(
                "INSERT INTO edges (id, edge_type, from_node, to_node)
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
        let future_version = SCHEMA_VERSION + 1;
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID}; \
                 PRAGMA user_version = {future_version};"
            ))
            .unwrap();

        let error = initialize(&mut connection).unwrap_err();
        assert!(matches!(error, crate::Error::Data(_)));
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, future_version);
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

    #[test]
    fn a_late_matched_row_failure_rolls_back_every_rows_writes() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:Source {number: 1}), (:Source {number: 2})")
            .unwrap();
        database
            .connection
            .execute_batch(
                "CREATE TRIGGER reject_second_edge
                 BEFORE INSERT ON edges
                 WHEN (SELECT count(*) FROM edges) = 1
                 BEGIN
                   SELECT RAISE(ABORT, 'rejected second edge for rollback test');
                 END;",
            )
            .unwrap();

        assert!(
            database
                .execute(
                    "MATCH (source:Source) \
                     CREATE (source)-[:GENERATED]->(:Sink)"
                )
                .is_err()
        );

        let edges: i64 = database
            .connection
            .query_row("SELECT count(*) FROM edges", [], |row| row.get(0))
            .unwrap();
        let sinks: i64 = database
            .connection
            .query_row(
                "SELECT count(*) FROM nodes WHERE label = 'Sink'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let sources: i64 = database
            .connection
            .query_row(
                "SELECT count(*) FROM nodes WHERE label = 'Source'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edges, 0);
        assert_eq!(sinks, 0);
        assert_eq!(sources, 2);
    }
}
