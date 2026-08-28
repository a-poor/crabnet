use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use rusqlite::{
    Connection, Transaction, TransactionBehavior, functions::FunctionFlags, params,
    params_from_iter,
};

use crate::{
    CypherValue, NodeValue, Parameters, PathValue, QueryResult, QuerySummary, RelationshipValue,
    Result,
    planner::{
        self, EntityKind, OutputShape, Plan, ReadPlan, ResolvedExpr, ResolvedMutation,
        ResolvedMutationPlan, SlotId, SqlQuery, UnionKind,
    },
};

mod traversal;

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
        register_functions(&connection)?;
        Ok(Self { connection })
    }

    /// Executes a Cypher query without parameters.
    pub fn execute(&mut self, cypher: &str) -> Result<QueryResult> {
        self.execute_with(cypher, &Parameters::new())
    }

    /// Executes a Cypher query with named `$parameter` values.
    pub fn execute_with(&mut self, cypher: &str, parameters: &Parameters) -> Result<QueryResult> {
        match planner::compile(cypher)? {
            Plan::Read(plan) => self.execute_read(plan, parameters),
            Plan::Mutate(plan) => {
                // Resolving every property and RETURN expression here keeps a
                // missing/invalid parameter from starting a partial write.
                let plan = (*plan).resolve(parameters)?;
                self.execute_mutation(plan)
            }
        }
    }

    fn execute_read(&mut self, plan: ReadPlan, parameters: &Parameters) -> Result<QueryResult> {
        // Path identities are emitted by the primary SQL fragments and
        // hydrated with follow-up reads. Keep every UNION branch and that
        // shared hydration pass on one deferred read snapshot.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let mut trail_budget = traversal::TrailBudget::default();
        let result = execute_read_plan(&transaction, plan, parameters, &mut trail_budget)?;
        transaction.commit()?;
        Ok(result)
    }

    fn execute_mutation(&mut self, plan: ResolvedMutationPlan) -> Result<QueryResult> {
        // IMMEDIATE obtains the SQLite write reservation up front. MATCH and
        // its mutations still share one transaction, but do not risk a deferred
        // read-to-write upgrade failing halfway through the statement.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut input_rows = materialize_input(&transaction, &plan)?;
        let mut rows = Vec::with_capacity(input_rows.len());
        let mut summary = QuerySummary::default();

        // Mutation-major execution mirrors a Cypher update pipeline: each
        // operation observes the complete input bag before the next operation
        // starts. DELETE in particular must gather and deduplicate its targets
        // across every matched row before removing anything.
        for mutation in &plan.mutations {
            match mutation {
                ResolvedMutation::CreateNode {
                    slot,
                    label,
                    properties,
                } => {
                    for input_row in &mut input_rows {
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
                }
                ResolvedMutation::CreateRelationship {
                    slot,
                    relationship_type,
                    from,
                    to,
                    properties,
                } => {
                    for input_row in &mut input_rows {
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
                ResolvedMutation::SetProperty { slot, key, value } => {
                    summary.properties_set +=
                        update_properties(&transaction, &mut input_rows, *slot, |properties| {
                            if value.is_null() {
                                properties.remove(key);
                            } else {
                                properties.insert(key.clone(), value.clone());
                            }
                        })?;
                }
                ResolvedMutation::ReplaceProperties { slot, properties } => {
                    let replacement = properties
                        .as_object()
                        .expect("resolved replacement properties are validated maps");
                    summary.properties_set +=
                        update_properties(&transaction, &mut input_rows, *slot, |current| {
                            *current = replacement.clone()
                        })?;
                }
                ResolvedMutation::MergeProperties { slot, properties } => {
                    let patch = properties
                        .as_object()
                        .expect("resolved merged properties are validated maps");
                    summary.properties_set +=
                        update_properties(&transaction, &mut input_rows, *slot, |current| {
                            for (key, value) in patch {
                                if value.is_null() {
                                    current.remove(key);
                                } else {
                                    current.insert(key.clone(), value.clone());
                                }
                            }
                        })?;
                }
                ResolvedMutation::RemoveProperty { slot, key } => {
                    summary.properties_set +=
                        update_properties(&transaction, &mut input_rows, *slot, |properties| {
                            properties.remove(key);
                        })?;
                }
                ResolvedMutation::Delete { targets, detach } => {
                    delete_entities(&transaction, &input_rows, targets, *detach, &mut summary)?;
                }
            }
        }

        if let Some(projection) = &plan.projection {
            for input_row in &input_rows {
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
enum PendingValue {
    Materialized(CypherValue),
    Path(PathIdentity),
    PathNodes(Vec<String>),
    PathRelationships(Vec<String>),
}

type PendingRow = Vec<PendingValue>;

fn execute_read_plan(
    connection: &Connection,
    plan: ReadPlan,
    parameters: &Parameters,
    trail_budget: &mut traversal::TrailBudget,
) -> Result<QueryResult> {
    for branch in &plan.branches {
        branch.preflight(parameters)?;
    }
    let first = plan
        .branches
        .first()
        .expect("read plans always contain at least one branch");
    let columns = first.columns.clone();
    let mut branches = plan.branches.iter();
    let mut rows = execute_sql_query(
        connection,
        branches
            .next()
            .expect("read plans always contain at least one branch"),
        parameters,
        trail_budget,
    )?;
    for (kind, branch) in plan.unions.iter().zip(branches) {
        rows.extend(execute_sql_query(
            connection,
            branch,
            parameters,
            trail_budget,
        )?);
        if *kind == UnionKind::Distinct {
            rows = distinct_pending_rows(rows);
        }
    }
    let rows = hydrate_pending_rows(connection, rows)?;
    Ok(QueryResult {
        columns,
        rows,
        summary: QuerySummary::default(),
    })
}

fn execute_sql_query(
    connection: &Connection,
    plan: &SqlQuery,
    parameters: &Parameters,
    trail_budget: &mut traversal::TrailBudget,
) -> Result<Vec<PendingRow>> {
    let post_pagination = plan.resolve_post_pagination(parameters)?;
    let bindings = plan.resolve_bindings(parameters, |spec| {
        traversal::materialize(connection, spec, trail_budget)
    })?;
    debug_assert_eq!(plan.columns.len(), plan.output_shapes.len());

    // Completely drain and drop the SQLite cursor before a later Rust stage
    // issues either branch SQL or the shared path-hydration queries.
    let encoded_rows = {
        let mut statement = connection.prepare(&plan.sql)?;
        debug_assert!(statement.readonly());
        statement
            .query_map(params_from_iter(bindings.iter()), |row| {
                (0..plan.output_shapes.len())
                    .map(|index| row.get::<_, String>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut rows = encoded_rows
        .into_iter()
        .map(|values| {
            values
                .into_iter()
                .zip(plan.output_shapes.iter().copied())
                .map(|(value, shape)| decode_pending_output(&value, shape))
                .collect::<Result<PendingRow>>()
        })
        .collect::<Result<Vec<_>>>()?;
    if plan.distinct() {
        rows = distinct_pending_rows(rows);
        let (offset, limit) = post_pagination;
        rows = rows
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .collect();
    }
    Ok(rows)
}

fn delete_entities(
    transaction: &Transaction<'_>,
    rows: &[ExecutionRow],
    targets: &[(SlotId, EntityKind)],
    detach: bool,
    summary: &mut QuerySummary,
) -> Result<()> {
    let mut node_ids = BTreeSet::new();
    let mut relationship_ids = BTreeSet::new();

    for row in rows {
        for (slot, expected_kind) in targets {
            let (actual_kind, id) = row.entity_identity(*slot)?;
            if actual_kind != *expected_kind {
                return Err(crate::Error::Data(format!(
                    "logical slot {} changed entity kind during DELETE",
                    slot.0
                )));
            }
            match actual_kind {
                EntityKind::Node => {
                    node_ids.insert(id);
                }
                EntityKind::Relationship => {
                    relationship_ids.insert(id);
                }
            }
        }
    }

    // Relationship targets are removed first regardless of their textual
    // position, so `DELETE node, relationship` can satisfy the node foreign
    // keys just like `DELETE relationship, node`.
    for id in relationship_ids {
        summary.relationships_deleted +=
            transaction.execute("DELETE FROM edges WHERE id = ?1", params![id])?;
    }

    if detach {
        for id in &node_ids {
            summary.relationships_deleted += transaction.execute(
                "DELETE FROM edges WHERE from_node = ?1 OR to_node = ?1",
                params![id],
            )?;
        }
    }

    for id in node_ids {
        summary.nodes_deleted +=
            transaction.execute("DELETE FROM nodes WHERE id = ?1", params![id])?;
    }

    Ok(())
}

fn update_properties(
    transaction: &Transaction<'_>,
    rows: &mut [ExecutionRow],
    slot: SlotId,
    update: impl Fn(&mut serde_json::Map<String, serde_json::Value>),
) -> Result<usize> {
    let mut total_changed = 0;
    for target_index in 0..rows.len() {
        let (kind, id) = rows[target_index].entity_identity(slot)?;
        let select_sql = match kind {
            EntityKind::Node => "SELECT props FROM nodes WHERE id = ?1",
            EntityKind::Relationship => "SELECT props FROM edges WHERE id = ?1",
        };
        let encoded =
            transaction.query_row(select_sql, params![id], |record| record.get::<_, String>(0))?;
        let serde_json::Value::Object(mut properties) =
            serde_json::from_str(&encoded).map_err(|error| {
                crate::Error::Data(format!(
                    "stored entity properties are malformed JSON: {error}"
                ))
            })?
        else {
            return Err(crate::Error::Data(
                "stored entity properties are not an object".into(),
            ));
        };

        let before = properties.clone();
        update(&mut properties);
        let changed = property_change_count(&before, &properties);
        if changed > 0 {
            let update_sql = match kind {
                EntityKind::Node => "UPDATE nodes SET props = json(?1) WHERE id = ?2",
                EntityKind::Relationship => "UPDATE edges SET props = json(?1) WHERE id = ?2",
            };
            transaction.execute(
                update_sql,
                params![
                    serde_json::Value::Object(properties.clone()).to_string(),
                    id
                ],
            )?;
        }

        let properties = cypher_properties(serde_json::Value::Object(properties))?;
        let mut refreshed = false;
        for row in rows.iter_mut() {
            refreshed |= row.replace_properties_for_identity(kind, &id, &properties);
        }
        if !refreshed {
            return Err(crate::Error::Data(format!(
                "updated {kind:?} `{id}` is absent from the materialized input"
            )));
        }
        total_changed += changed;
    }

    Ok(total_changed)
}

fn property_change_count(
    before: &serde_json::Map<String, serde_json::Value>,
    after: &serde_json::Map<String, serde_json::Value>,
) -> usize {
    before
        .iter()
        .filter(|(key, value)| after.get(*key) != Some(*value))
        .count()
        + after
            .keys()
            .filter(|key| !before.contains_key(*key))
            .count()
}

fn distinct_pending_rows(rows: Vec<PendingRow>) -> Vec<PendingRow> {
    let mut distinct: Vec<PendingRow> = Vec::with_capacity(rows.len());
    for row in rows {
        if !distinct
            .iter()
            .any(|existing| pending_rows_equivalent(existing, &row))
        {
            distinct.push(row);
        }
    }
    distinct
}

fn pending_rows_equivalent(left: &[PendingValue], right: &[PendingValue]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| pending_values_equivalent(left, right))
}

fn pending_values_equivalent(left: &PendingValue, right: &PendingValue) -> bool {
    match (left, right) {
        (PendingValue::Materialized(left), PendingValue::Materialized(right)) => {
            cypher_values_equivalent(left, right)
        }
        (PendingValue::Path(left), PendingValue::Path(right)) => left == right,
        (PendingValue::PathNodes(left), PendingValue::PathNodes(right)) => left == right,
        (PendingValue::PathRelationships(left), PendingValue::PathRelationships(right)) => {
            left == right
        }
        (PendingValue::PathNodes(left), PendingValue::PathRelationships(right))
        | (PendingValue::PathRelationships(right), PendingValue::PathNodes(left)) => {
            left.is_empty() && right.is_empty()
        }
        (PendingValue::PathNodes(ids), PendingValue::Materialized(value))
        | (PendingValue::Materialized(value), PendingValue::PathNodes(ids)) => {
            entity_id_list_matches_value(EntityKind::Node, ids, value)
        }
        (PendingValue::PathRelationships(ids), PendingValue::Materialized(value))
        | (PendingValue::Materialized(value), PendingValue::PathRelationships(ids)) => {
            entity_id_list_matches_value(EntityKind::Relationship, ids, value)
        }
        _ => false,
    }
}

fn entity_id_list_matches_value(kind: EntityKind, ids: &[String], value: &CypherValue) -> bool {
    let CypherValue::List(values) = value else {
        return false;
    };
    ids.len() == values.len()
        && ids
            .iter()
            .zip(values)
            .all(|(id, value)| match (kind, value) {
                (EntityKind::Node, CypherValue::Node(node)) => id == &node.element_id,
                (EntityKind::Relationship, CypherValue::Relationship(relationship)) => {
                    id == &relationship.element_id
                }
                _ => false,
            })
}

fn cypher_rows_equivalent(left: &[CypherValue], right: &[CypherValue]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| cypher_values_equivalent(left, right))
}

fn cypher_values_equivalent(left: &CypherValue, right: &CypherValue) -> bool {
    match (left, right) {
        (CypherValue::Null, CypherValue::Null) => true,
        (CypherValue::Boolean(left), CypherValue::Boolean(right)) => left == right,
        (CypherValue::Integer(left), CypherValue::Integer(right)) => left == right,
        (CypherValue::Float(left), CypherValue::Float(right)) => left == right,
        (CypherValue::Integer(integer), CypherValue::Float(float))
        | (CypherValue::Float(float), CypherValue::Integer(integer)) => {
            float.is_finite() && float.fract() == 0.0 && (*float as i128) == i128::from(*integer)
        }
        (CypherValue::String(left), CypherValue::String(right)) => left == right,
        (CypherValue::List(left), CypherValue::List(right)) => cypher_rows_equivalent(left, right),
        (CypherValue::Map(left), CypherValue::Map(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| cypher_values_equivalent(left, right))
                })
        }
        (CypherValue::Node(left), CypherValue::Node(right)) => left.element_id == right.element_id,
        (CypherValue::Relationship(left), CypherValue::Relationship(right)) => {
            left.element_id == right.element_id
        }
        (CypherValue::Path(left), CypherValue::Path(right)) => {
            left.nodes.len() == right.nodes.len()
                && left.relationships.len() == right.relationships.len()
                && left
                    .nodes
                    .iter()
                    .zip(&right.nodes)
                    .all(|(left, right)| left.element_id == right.element_id)
                && left
                    .relationships
                    .iter()
                    .zip(&right.relationships)
                    .all(|(left, right)| left.element_id == right.element_id)
        }
        _ => false,
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

    fn entity_identity(&self, slot: SlotId) -> Result<(EntityKind, String)> {
        match self.entity(slot)? {
            EntityValue::Node(node) => Ok((EntityKind::Node, node.element_id.clone())),
            EntityValue::Relationship(relationship) => {
                Ok((EntityKind::Relationship, relationship.element_id.clone()))
            }
        }
    }

    fn replace_properties_for_identity(
        &mut self,
        kind: EntityKind,
        id: &str,
        properties: &std::collections::BTreeMap<String, CypherValue>,
    ) -> bool {
        let mut found = false;
        for entity in self.entities.iter_mut().flatten() {
            match entity {
                EntityValue::Node(node) if kind == EntityKind::Node && node.element_id == id => {
                    node.properties = properties.clone();
                    found = true;
                }
                EntityValue::Relationship(relationship)
                    if kind == EntityKind::Relationship && relationship.element_id == id =>
                {
                    relationship.properties = properties.clone();
                    found = true;
                }
                _ => {}
            }
        }
        found
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct PathIdentity {
    node_ids: Vec<String>,
    relationship_ids: Vec<String>,
}

#[derive(Default)]
struct PathHydrator {
    nodes: HashMap<String, NodeValue>,
    relationships: HashMap<String, RelationshipValue>,
}

impl PathHydrator {
    fn load(connection: &Connection, rows: &[PendingRow]) -> Result<Self> {
        let (mut node_ids, mut relationship_ids) = collect_path_ids(rows);
        if node_ids.is_empty() && relationship_ids.is_empty() {
            return Ok(Self::default());
        }
        let mut hydrator = Self::default();

        // Whole entities already decoded from the primary SQL fragments are
        // snapshots from the same read transaction and can seed the cache.
        for value in rows.iter().flatten() {
            match value {
                PendingValue::Materialized(CypherValue::Node(node))
                    if node_ids.remove(&node.element_id) =>
                {
                    hydrator
                        .nodes
                        .entry(node.element_id.clone())
                        .or_insert_with(|| node.clone());
                }
                PendingValue::Materialized(CypherValue::Relationship(relationship))
                    if relationship_ids.remove(&relationship.element_id) =>
                {
                    hydrator
                        .relationships
                        .entry(relationship.element_id.clone())
                        .or_insert_with(|| relationship.clone());
                }
                PendingValue::Materialized(_)
                | PendingValue::Path(_)
                | PendingValue::PathNodes(_)
                | PendingValue::PathRelationships(_) => {}
            }
        }

        hydrator.load_nodes(connection, &node_ids)?;
        hydrator.load_relationships(connection, &relationship_ids)?;
        Ok(hydrator)
    }

    fn load_nodes(&mut self, connection: &Connection, ids: &BTreeSet<String>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let encoded_ids = encode_id_set(ids);
        let encoded_nodes = {
            let mut statement = connection.prepare(
                "SELECT json_object(
                    'id', n.id, 'label', n.label, 'props', json(n.props)
                 )
                 FROM json_each(?1) AS wanted
                 JOIN nodes AS n ON n.id = wanted.value
                 WHERE wanted.type = 'text'",
            )?;
            statement
                .query_map(params![encoded_ids], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for encoded in encoded_nodes {
            let EntityValue::Node(node) = decode_entity(&encoded, EntityKind::Node)? else {
                unreachable!("node decoder always returns a node")
            };
            self.nodes.insert(node.element_id.clone(), node);
        }
        if let Some(id) = ids.iter().find(|id| !self.nodes.contains_key(id.as_str())) {
            return Err(crate::Error::Data(format!(
                "path references missing node `{id}`"
            )));
        }
        Ok(())
    }

    fn load_relationships(
        &mut self,
        connection: &Connection,
        ids: &BTreeSet<String>,
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let encoded_ids = encode_id_set(ids);
        let encoded_relationships = {
            let mut statement = connection.prepare(
                "SELECT json_object(
                    'id', e.id, 'type', e.edge_type, 'from', e.from_node,
                    'to', e.to_node, 'props', json(e.props)
                 )
                 FROM json_each(?1) AS wanted
                 JOIN edges AS e ON e.id = wanted.value
                 WHERE wanted.type = 'text'",
            )?;
            statement
                .query_map(params![encoded_ids], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for encoded in encoded_relationships {
            let EntityValue::Relationship(relationship) =
                decode_entity(&encoded, EntityKind::Relationship)?
            else {
                unreachable!("relationship decoder always returns a relationship")
            };
            self.relationships
                .insert(relationship.element_id.clone(), relationship);
        }
        if let Some(id) = ids
            .iter()
            .find(|id| !self.relationships.contains_key(id.as_str()))
        {
            return Err(crate::Error::Data(format!(
                "path references missing relationship `{id}`"
            )));
        }
        Ok(())
    }

    fn hydrate(&self, identity: PathIdentity) -> Result<PathValue> {
        let mut nodes = Vec::with_capacity(identity.node_ids.len());
        for id in identity.node_ids {
            nodes.push(self.node(&id)?);
        }
        let mut relationships = Vec::with_capacity(identity.relationship_ids.len());
        for id in identity.relationship_ids {
            relationships.push(self.relationship(&id)?);
        }
        if nodes.len() != relationships.len() + 1 {
            return Err(crate::Error::Data(
                "hydrated path violates the node/relationship cardinality invariant".into(),
            ));
        }
        for (index, relationship) in relationships.iter().enumerate() {
            let left = &nodes[index].element_id;
            let right = &nodes[index + 1].element_id;
            let connects_in_storage_order = relationship.start_node_element_id == *left
                && relationship.end_node_element_id == *right;
            let connects_against_storage_order = relationship.start_node_element_id == *right
                && relationship.end_node_element_id == *left;
            if !connects_in_storage_order && !connects_against_storage_order {
                return Err(crate::Error::Data(format!(
                    "path relationship `{}` does not connect adjacent nodes `{left}` and `{right}`",
                    relationship.element_id
                )));
            }
        }
        Ok(PathValue {
            nodes,
            relationships,
        })
    }

    fn node(&self, id: &str) -> Result<NodeValue> {
        self.nodes
            .get(id)
            .cloned()
            .ok_or_else(|| crate::Error::Data(format!("path references missing node `{id}`")))
    }

    fn relationship(&self, id: &str) -> Result<RelationshipValue> {
        self.relationships.get(id).cloned().ok_or_else(|| {
            crate::Error::Data(format!("path references missing relationship `{id}`"))
        })
    }

    fn hydrate_node_list(&self, ids: Vec<String>) -> Result<CypherValue> {
        ids.into_iter()
            .map(|id| self.node(&id).map(CypherValue::Node))
            .collect::<Result<Vec<_>>>()
            .map(CypherValue::List)
    }

    fn hydrate_relationship_list(&self, ids: Vec<String>) -> Result<CypherValue> {
        ids.into_iter()
            .map(|id| self.relationship(&id).map(CypherValue::Relationship))
            .collect::<Result<Vec<_>>>()
            .map(CypherValue::List)
    }
}

fn collect_path_ids(rows: &[PendingRow]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut nodes = BTreeSet::new();
    let mut relationships = BTreeSet::new();
    for value in rows.iter().flatten() {
        match value {
            PendingValue::Path(path) => {
                nodes.extend(path.node_ids.iter().cloned());
                relationships.extend(path.relationship_ids.iter().cloned());
            }
            PendingValue::PathNodes(ids) => nodes.extend(ids.iter().cloned()),
            PendingValue::PathRelationships(ids) => {
                relationships.extend(ids.iter().cloned());
            }
            PendingValue::Materialized(_) => {}
        }
    }
    (nodes, relationships)
}

fn encode_id_set(ids: &BTreeSet<String>) -> String {
    serde_json::Value::Array(ids.iter().cloned().map(serde_json::Value::String).collect())
        .to_string()
}

fn decode_path_identity(encoded: &str) -> Result<PathIdentity> {
    let value = serde_json::from_str::<serde_json::Value>(encoded).map_err(|error| {
        crate::Error::Data(format!("query returned a malformed path identity: {error}"))
    })?;
    let serde_json::Value::Object(mut path) = value else {
        return Err(crate::Error::Data(
            "query returned a path identity that is not an object".into(),
        ));
    };
    let Some(serde_json::Value::Array(segments)) = path.remove("segments") else {
        return Err(crate::Error::Data(
            "query returned a path identity without a segment array".into(),
        ));
    };
    if segments.is_empty() {
        return Err(crate::Error::Data(
            "query returned a path identity without any segments".into(),
        ));
    }

    let mut node_ids = Vec::new();
    let mut relationship_ids = Vec::new();
    for (index, segment) in segments.into_iter().enumerate() {
        let serde_json::Value::Object(mut segment) = segment else {
            return Err(crate::Error::Data(format!(
                "path segment {index} is not an object"
            )));
        };
        let nodes = take_string_array(&mut segment, "nodes", index)?;
        let relationships = take_string_array(&mut segment, "relationships", index)?;
        if nodes.is_empty() || nodes.len() != relationships.len() + 1 {
            return Err(crate::Error::Data(format!(
                "path segment {index} violates the node/relationship cardinality invariant"
            )));
        }
        if let Some(previous) = node_ids.last()
            && previous != &nodes[0]
        {
            return Err(crate::Error::Data(format!(
                "path segment {index} does not continue from the previous segment"
            )));
        }
        if node_ids.is_empty() {
            node_ids.extend(nodes);
        } else {
            node_ids.extend(nodes.into_iter().skip(1));
        }
        relationship_ids.extend(relationships);
    }

    if node_ids.len() != relationship_ids.len() + 1 {
        return Err(crate::Error::Data(
            "path identity violates the node/relationship cardinality invariant".into(),
        ));
    }
    Ok(PathIdentity {
        node_ids,
        relationship_ids,
    })
}

fn take_string_array(
    fields: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    segment: usize,
) -> Result<Vec<String>> {
    let Some(serde_json::Value::Array(values)) = fields.remove(key) else {
        return Err(crate::Error::Data(format!(
            "path segment {segment} is missing `{key}`"
        )));
    };
    values
        .into_iter()
        .map(|value| match value {
            serde_json::Value::String(value) => Ok(value),
            _ => Err(crate::Error::Data(format!(
                "path segment {segment} contains a non-string `{key}` entry"
            ))),
        })
        .collect()
}

fn hydrate_pending_rows(
    connection: &Connection,
    rows: Vec<PendingRow>,
) -> Result<Vec<Vec<CypherValue>>> {
    let hydrator = PathHydrator::load(connection, &rows)?;
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    PendingValue::Materialized(value) => Ok(value),
                    PendingValue::Path(identity) => {
                        hydrator.hydrate(identity).map(CypherValue::Path)
                    }
                    PendingValue::PathNodes(ids) => hydrator.hydrate_node_list(ids),
                    PendingValue::PathRelationships(ids) => hydrator.hydrate_relationship_list(ids),
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect()
}

fn decode_pending_output(encoded: &str, shape: OutputShape) -> Result<PendingValue> {
    match shape {
        OutputShape::Json => {
            let value = serde_json::from_str::<serde_json::Value>(encoded).map_err(|error| {
                crate::Error::Data(format!(
                    "query returned malformed JSON `{encoded}`: {error}"
                ))
            })?;
            CypherValue::try_from(value).map(PendingValue::Materialized)
        }
        OutputShape::Entity(_) if encoded == "null" => {
            Ok(PendingValue::Materialized(CypherValue::Null))
        }
        OutputShape::Entity(kind) => decode_entity(encoded, kind)
            .map(|value| PendingValue::Materialized(value.to_cypher_value())),
        OutputShape::PathIdentity => decode_path_identity(encoded).map(PendingValue::Path),
        OutputShape::PathNodes => {
            decode_path_identity(encoded).map(|identity| PendingValue::PathNodes(identity.node_ids))
        }
        OutputShape::PathRelationships => decode_path_identity(encoded)
            .map(|identity| PendingValue::PathRelationships(identity.relationship_ids)),
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

fn register_functions(connection: &Connection) -> Result<()> {
    connection.create_scalar_function(
        "crabnet_semantic_key",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let encoded = context.get::<String>(0)?;
            let value = serde_json::from_str(&encoded)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
            Ok(semantic_json_key(&value))
        },
    )?;
    Ok(())
}

/// Produces a stable, recursively typed key for the JSON-backed part of the
/// Cypher value domain. Equal integers and floats deliberately share a key;
/// booleans remain distinct from numbers, and collection values recurse using
/// the same rules. SQLite uses the resulting blob for GROUP BY and DISTINCT.
fn semantic_json_key(value: &serde_json::Value) -> Vec<u8> {
    fn append_len(output: &mut Vec<u8>, len: usize) {
        output.extend_from_slice(&(len as u64).to_be_bytes());
    }

    fn append_value(output: &mut Vec<u8>, value: &serde_json::Value) {
        match value {
            serde_json::Value::Null => output.push(b'n'),
            serde_json::Value::Bool(value) => {
                output.push(b'b');
                output.push(u8::from(*value));
            }
            serde_json::Value::Number(number) => {
                output.push(b'd');
                if let Some(integer) = number.as_i64() {
                    output.push(b'i');
                    output.extend_from_slice(&integer.to_be_bytes());
                } else if let Some(unsigned) = number.as_u64() {
                    output.push(b'u');
                    output.extend_from_slice(&unsigned.to_be_bytes());
                } else {
                    let float = number
                        .as_f64()
                        .expect("serde_json numbers always have a numeric representation");
                    let integral = (float.is_finite() && float.fract() == 0.0)
                        .then_some(float as i128)
                        .and_then(|integer| i64::try_from(integer).ok());
                    if let Some(integer) = integral {
                        output.push(b'i');
                        output.extend_from_slice(&integer.to_be_bytes());
                    } else {
                        output.push(b'f');
                        // Cypher numeric equality treats the two signed zero
                        // representations as the same floating-point value.
                        let normalized = if float == 0.0 { 0.0 } else { float };
                        output.extend_from_slice(&normalized.to_bits().to_be_bytes());
                    }
                }
            }
            serde_json::Value::String(value) => {
                output.push(b's');
                append_len(output, value.len());
                output.extend_from_slice(value.as_bytes());
            }
            serde_json::Value::Array(values) => {
                output.push(b'l');
                append_len(output, values.len());
                for value in values {
                    let nested = semantic_json_key(value);
                    append_len(output, nested.len());
                    output.extend_from_slice(&nested);
                }
            }
            serde_json::Value::Object(values) => {
                output.push(b'm');
                append_len(output, values.len());
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_unstable_by_key(|(key, _)| *key);
                for (key, value) in entries {
                    append_len(output, key.len());
                    output.extend_from_slice(key.as_bytes());
                    let nested = semantic_json_key(value);
                    append_len(output, nested.len());
                    output.extend_from_slice(&nested);
                }
            }
        }
    }

    let mut output = Vec::new();
    append_value(&mut output, value);
    output
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
    fn distinct_pagination_parameters_preflight_before_trail_materialization() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:Preflight)-[:LINK]->(:Preflight)")
            .unwrap();
        let Plan::Read(plan) = planner::compile(
            "MATCH (:Preflight)-[:LINK*]->(end) RETURN DISTINCT end LIMIT $missing",
        )
        .unwrap() else {
            panic!("expected a read plan");
        };
        let branch = plan
            .branches
            .first()
            .expect("read plans always contain a branch");
        let mut budget = traversal::TrailBudget::new(0);

        let error = execute_sql_query(
            &database.connection,
            branch,
            &Parameters::new(),
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(error, crate::Error::MissingParameter(name) if name == "missing"));
    }

    #[test]
    fn all_union_parameters_preflight_before_branch_materialization() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:Preflight)-[:LINK]->(:Preflight)")
            .unwrap();
        let Plan::Read(plan) = planner::compile(
            "MATCH (:Preflight)-[:LINK*]->(end) RETURN end AS value UNION ALL MATCH (node) RETURN $missing AS value",
        )
        .unwrap()
        else {
            panic!("expected a read plan");
        };
        let mut budget = traversal::TrailBudget::new(0);

        let error = execute_read_plan(&database.connection, plan, &Parameters::new(), &mut budget)
            .unwrap_err();

        assert!(matches!(error, crate::Error::MissingParameter(name) if name == "missing"));
    }

    #[test]
    fn trail_resource_limit_propagates_through_the_executor() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:BudgetStart)-[:LINK]->(:BudgetEnd)")
            .unwrap();
        let Plan::Read(plan) =
            planner::compile("MATCH (:BudgetStart)-[:LINK*]->(end) RETURN end").unwrap()
        else {
            panic!("expected a read plan");
        };
        let mut budget = traversal::TrailBudget::new(0);

        let error = execute_read_plan(&database.connection, plan, &Parameters::new(), &mut budget)
            .unwrap_err();

        assert!(matches!(
            error,
            crate::Error::ResourceLimit(message)
                if message.contains("trail expansion") && message.contains("0-state budget")
        ));
    }

    #[test]
    fn distinct_numeric_equivalence_does_not_round_large_integers_through_f64() {
        assert!(cypher_values_equivalent(
            &CypherValue::Integer(1),
            &CypherValue::Float(1.0)
        ));
        assert!(!cypher_values_equivalent(
            &CypherValue::Integer(9_007_199_254_740_993),
            &CypherValue::Float(9_007_199_254_740_992.0)
        ));
    }

    #[test]
    fn pending_distinct_uses_typed_values_and_ordered_path_identity() {
        let first = PathIdentity {
            node_ids: vec!["a".into(), "b".into()],
            relationship_ids: vec!["edge".into()],
        };
        let same = first.clone();
        let reversed = PathIdentity {
            node_ids: vec!["b".into(), "a".into()],
            relationship_ids: vec!["edge".into()],
        };
        let different_edge = PathIdentity {
            node_ids: vec!["a".into(), "b".into()],
            relationship_ids: vec!["other".into()],
        };

        assert!(pending_values_equivalent(
            &PendingValue::Materialized(CypherValue::Integer(1)),
            &PendingValue::Materialized(CypherValue::Float(1.0)),
        ));
        assert!(pending_values_equivalent(
            &PendingValue::Path(first.clone()),
            &PendingValue::Path(same),
        ));
        assert!(!pending_values_equivalent(
            &PendingValue::Path(first.clone()),
            &PendingValue::Path(reversed),
        ));
        assert!(!pending_values_equivalent(
            &PendingValue::Path(first.clone()),
            &PendingValue::Path(different_edge),
        ));
        assert!(!pending_values_equivalent(
            &PendingValue::Path(first),
            &PendingValue::Materialized(CypherValue::String("path".into())),
        ));
    }

    #[test]
    fn pending_path_collections_use_entity_identity_and_bridge_typed_lists() {
        let node = |id: &str| {
            CypherValue::Node(NodeValue {
                element_id: id.into(),
                labels: Vec::new(),
                properties: Default::default(),
            })
        };
        let relationship = |id: &str| {
            CypherValue::Relationship(RelationshipValue {
                element_id: id.into(),
                relationship_type: "LINK".into(),
                start_node_element_id: "a".into(),
                end_node_element_id: "b".into(),
                properties: Default::default(),
            })
        };

        let nodes = PendingValue::PathNodes(vec!["a".into(), "b".into()]);
        let materialized_nodes =
            PendingValue::Materialized(CypherValue::List(vec![node("a"), node("b")]));
        assert!(pending_values_equivalent(&nodes, &materialized_nodes));
        assert!(pending_values_equivalent(&materialized_nodes, &nodes));

        let relationships = PendingValue::PathRelationships(vec!["edge".into()]);
        let materialized_relationships =
            PendingValue::Materialized(CypherValue::List(vec![relationship("edge")]));
        assert!(pending_values_equivalent(
            &relationships,
            &materialized_relationships,
        ));
        assert!(!pending_values_equivalent(&nodes, &relationships));

        let empty_relationships = PendingValue::PathRelationships(Vec::new());
        let materialized_empty = PendingValue::Materialized(CypherValue::List(Vec::new()));
        assert!(pending_values_equivalent(
            &empty_relationships,
            &materialized_empty,
        ));
        assert!(pending_values_equivalent(
            &materialized_empty,
            &empty_relationships,
        ));
    }

    #[test]
    fn pending_path_id_collection_is_unique_across_rows_and_columns() {
        let rows = vec![
            vec![
                PendingValue::Path(PathIdentity {
                    node_ids: vec!["a".into(), "b".into()],
                    relationship_ids: vec!["ab".into()],
                }),
                PendingValue::Path(PathIdentity {
                    node_ids: vec!["b".into(), "c".into()],
                    relationship_ids: vec!["bc".into()],
                }),
            ],
            vec![
                PendingValue::Path(PathIdentity {
                    node_ids: vec!["a".into(), "b".into()],
                    relationship_ids: vec!["ab".into()],
                }),
                PendingValue::PathNodes(vec!["node-only".into(), "b".into()]),
                PendingValue::PathRelationships(vec!["edge-only".into(), "bc".into()]),
            ],
        ];

        let (nodes, relationships) = collect_path_ids(&rows);
        assert_eq!(
            nodes,
            BTreeSet::from([
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "node-only".to_owned()
            ])
        );
        assert_eq!(
            relationships,
            BTreeSet::from(["ab".to_owned(), "bc".to_owned(), "edge-only".to_owned()])
        );
    }

    #[test]
    fn directly_materialized_entities_seed_path_hydration() {
        let node = NodeValue {
            element_id: "seeded".into(),
            labels: vec!["Seed".into()],
            properties: Default::default(),
        };
        let rows = vec![vec![
            PendingValue::Materialized(CypherValue::Node(node.clone())),
            PendingValue::Path(PathIdentity {
                node_ids: vec![node.element_id.clone()],
                relationship_ids: Vec::new(),
            }),
        ]];

        // This connection intentionally has no graph schema. Loading can only
        // succeed if the directly materialized node satisfies the path cache.
        let connection = Connection::open_in_memory().unwrap();
        let hydrator = PathHydrator::load(&connection, &rows).unwrap();
        let path = hydrator
            .hydrate(PathIdentity {
                node_ids: vec![node.element_id.clone()],
                relationship_ids: Vec::new(),
            })
            .unwrap();
        assert_eq!(path.nodes, [node]);
        assert!(path.relationships.is_empty());
    }

    #[test]
    fn empty_and_seeded_relationship_lists_need_no_storage_query() {
        let relationship = RelationshipValue {
            element_id: "seeded-edge".into(),
            relationship_type: "LINK".into(),
            start_node_element_id: "a".into(),
            end_node_element_id: "b".into(),
            properties: Default::default(),
        };
        let rows = vec![
            vec![PendingValue::PathRelationships(Vec::new())],
            vec![
                PendingValue::Materialized(CypherValue::Relationship(relationship.clone())),
                PendingValue::PathRelationships(vec![relationship.element_id.clone()]),
            ],
        ];

        // No graph schema exists: both the empty zero-hop list and the seeded
        // relationship list must avoid a storage lookup entirely.
        let connection = Connection::open_in_memory().unwrap();
        let hydrator = PathHydrator::load(&connection, &rows).unwrap();
        assert_eq!(
            hydrator.hydrate_relationship_list(Vec::new()).unwrap(),
            CypherValue::List(Vec::new())
        );
        assert_eq!(
            hydrator
                .hydrate_relationship_list(vec![relationship.element_id.clone()])
                .unwrap(),
            CypherValue::List(vec![CypherValue::Relationship(relationship)])
        );
    }

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
