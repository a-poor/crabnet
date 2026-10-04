use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};

use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior,
    functions::{Aggregate, Context, FunctionFlags},
    params, params_from_iter,
    types::ValueRef,
};

use crate::{
    CypherValue, NodeValue, Parameters, PathValue, QueryOptions, QueryResult, QuerySummary,
    RelationshipValue, Result,
    planner::{
        self, EntityKind, OutputShape, Plan, PreparedFragmentArgument, PreparedPhysicalOperator,
        PreparedReadPlan, PreparedSqlFragment, PreparedValueOperator, ReadPlan, ResolvedMutation,
        ResolvedMutationPlan, ResolvedRowSource, ResolvedSegment, ResolvedStoredValue, RowLayout,
        SlotId, UnionKind,
    },
};

mod comparison;
mod resources;
use resources::ExecutionContext;
pub(crate) mod constraints;
pub(crate) mod transaction;
mod traversal;

const APPLICATION_ID: i64 = 0x4352_4142; // "CRAB"
const SCHEMA_VERSION: i64 = 4;

const INITIAL_SCHEMA: &str = "

CREATE TABLE nodes (
    id TEXT PRIMARY KEY NOT NULL,
    labels TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(labels) AND json_type(labels) = 'array'),
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

CREATE TABLE node_labels (
    label TEXT NOT NULL,
    node_id TEXT NOT NULL,
    PRIMARY KEY (label, node_id)
) STRICT;
CREATE TRIGGER nodes_labels_insert AFTER INSERT ON nodes BEGIN
    INSERT INTO node_labels (label, node_id)
        SELECT value, NEW.id FROM json_each(NEW.labels);
END;
CREATE TRIGGER nodes_labels_update AFTER UPDATE OF labels ON nodes BEGIN
    DELETE FROM node_labels WHERE node_id = NEW.id;
    INSERT INTO node_labels (label, node_id)
        SELECT value, NEW.id FROM json_each(NEW.labels);
END;
CREATE TRIGGER nodes_labels_delete AFTER DELETE ON nodes BEGIN
    DELETE FROM node_labels WHERE node_id = OLD.id;
END;
CREATE INDEX edges_from_type_to
    ON edges(from_node, edge_type, to_node);
CREATE INDEX edges_to_type_from
    ON edges(to_node, edge_type, from_node);

PRAGMA application_id = 1129464130;
";

/// Upgrades a version 1 file: the single `label` column becomes the
/// `labels` JSON list backed by the `node_labels` index table.
const MIGRATE_V1_TO_V2: &str = "
ALTER TABLE nodes ADD COLUMN labels TEXT NOT NULL DEFAULT '[]';
UPDATE nodes SET labels = CASE WHEN label IS NULL THEN '[]' ELSE json_array(label) END;
DROP INDEX nodes_by_label;
ALTER TABLE nodes DROP COLUMN label;
CREATE TABLE node_labels (
    label TEXT NOT NULL,
    node_id TEXT NOT NULL,
    PRIMARY KEY (label, node_id)
) STRICT;
CREATE TRIGGER nodes_labels_insert AFTER INSERT ON nodes BEGIN
    INSERT INTO node_labels (label, node_id)
        SELECT value, NEW.id FROM json_each(NEW.labels);
END;
CREATE TRIGGER nodes_labels_update AFTER UPDATE OF labels ON nodes BEGIN
    DELETE FROM node_labels WHERE node_id = NEW.id;
    INSERT INTO node_labels (label, node_id)
        SELECT value, NEW.id FROM json_each(NEW.labels);
END;
CREATE TRIGGER nodes_labels_delete AFTER DELETE ON nodes BEGIN
    DELETE FROM node_labels WHERE node_id = OLD.id;
END;
INSERT INTO node_labels (label, node_id)
    SELECT labels.value, nodes.id FROM nodes, json_each(nodes.labels) AS labels;
";

const PROPERTY_INDEX_SCHEMA: &str = "
-- Every top-level non-null node property, as json_extract returns it, so an
-- equality on a property can find its nodes without scanning their label.
-- Triggers keep it in step with every write to nodes.props.
CREATE TABLE node_properties (
    node_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value ANY NOT NULL,
    PRIMARY KEY (node_id, key)
) STRICT, WITHOUT ROWID;

CREATE INDEX node_properties_by_value
    ON node_properties(key, value);

CREATE TRIGGER node_properties_insert AFTER INSERT ON nodes BEGIN
    INSERT INTO node_properties (node_id, key, value)
        SELECT NEW.id, key, value FROM json_each(NEW.props) WHERE type <> 'null';
END;

CREATE TRIGGER node_properties_update AFTER UPDATE OF id, props ON nodes BEGIN
    DELETE FROM node_properties WHERE node_id = OLD.id;
    INSERT INTO node_properties (node_id, key, value)
        SELECT NEW.id, key, value FROM json_each(NEW.props) WHERE type <> 'null';
END;

CREATE TRIGGER node_properties_delete AFTER DELETE ON nodes BEGIN
    DELETE FROM node_properties WHERE node_id = OLD.id;
END;

INSERT INTO node_properties (node_id, key, value)
    SELECT nodes.id, property.key, property.value
    FROM nodes, json_each(nodes.props) AS property WHERE property.type <> 'null';
";

/// An embedded graph database connection.
///
/// Each `Database` owns one SQLite connection. Standalone queries are planned
/// completely, and all named parameters are resolved, before a write
/// transaction begins. Use [`Database::transaction`] to group queries into one
/// atomic unit.
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

    /// Begins a transaction that can execute multiple Cypher queries.
    ///
    /// The transaction reserves the SQLite writer immediately and exclusively
    /// borrows this database until it is consumed or dropped. Queries see the
    /// transaction's earlier writes. Any query error rolls back the entire
    /// transaction and prevents further execution or commit. Dropping an
    /// uncommitted transaction also rolls it back.
    ///
    /// ```
    /// use crabnet::Database;
    ///
    /// let mut database = Database::in_memory()?;
    /// let mut transaction = database.transaction()?;
    /// transaction.execute("CREATE (:Person {name: 'Ada'})")?;
    /// let people = transaction.execute("MATCH (p:Person) RETURN p")?;
    /// assert_eq!(people.rows.len(), 1);
    /// transaction.execute("MATCH (p:Person) SET p.verified = true")?;
    /// transaction.commit()?;
    /// # Ok::<(), crabnet::Error>(())
    /// ```
    pub fn transaction(&mut self) -> Result<crate::Transaction<'_>> {
        transaction::Transaction::begin(&mut self.connection)
    }

    /// Executes a Cypher query without parameters.
    pub fn execute(&mut self, cypher: &str) -> Result<QueryResult> {
        self.execute_with(cypher, &Parameters::new())
    }

    /// Executes a Cypher query with named `$parameter` values.
    pub fn execute_with(&mut self, cypher: &str, parameters: &Parameters) -> Result<QueryResult> {
        self.execute_with_options(cypher, parameters, &QueryOptions::default())
    }

    /// Executes a parameterized query with per-query resource controls.
    ///
    /// Limits cover all branches and write stages. Failures roll back the
    /// statement; no partial result is returned. Existing execution methods
    /// use [`QueryOptions::default`].
    pub fn execute_with_options(
        &mut self,
        cypher: &str,
        parameters: &Parameters,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        let context = ExecutionContext::new(options);
        context.check()?;
        match planner::compile(cypher)? {
            Plan::Read(plan) => self.execute_read(plan, parameters, &context),
            Plan::Mutate(plan) => {
                // Resolving every property and RETURN expression here keeps a
                // missing/invalid parameter from starting a partial write.
                let plan = (*plan).resolve(parameters)?;
                self.execute_mutation(plan, &context)
            }
        }
    }

    fn execute_read(
        &mut self,
        plan: ReadPlan,
        parameters: &Parameters,
        context: &ExecutionContext,
    ) -> Result<QueryResult> {
        context.check()?;
        // Path identities are emitted by the primary SQL fragments and
        // hydrated with follow-up reads. Keep every UNION branch and that
        // shared hydration pass on one deferred read snapshot.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let result = context.run(&transaction, || {
            execute_read_plan(&transaction, plan, parameters, context)
        })?;
        transaction.commit()?;
        Ok(result)
    }

    fn execute_mutation(
        &mut self,
        plan: ResolvedMutationPlan,
        context: &ExecutionContext,
    ) -> Result<QueryResult> {
        context.check()?;
        // IMMEDIATE obtains the SQLite write reservation up front. MATCH and
        // its mutations still share one transaction, but do not risk a deferred
        // read-to-write upgrade failing halfway through the statement.
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = context.run(&transaction, || {
            execute_mutation_plan(&transaction, plan, context)
        })?;
        transaction.commit()?;
        Ok(result)
    }
}

fn execute_mutation_plan(
    transaction: &Transaction<'_>,
    plan: ResolvedMutationPlan,
    context: &ExecutionContext,
) -> Result<QueryResult> {
    let ResolvedMutationPlan {
        input,
        layout,
        mutations,
        segments,
        result,
    } = plan;
    let mut input_rows = match &input {
        Some(input) => read_row_source(transaction, input, &layout, context)?,
        None => vec![ExecutionRow::empty(&layout)],
    };
    let mut summary = QuerySummary::default();

    // Mutation-major execution mirrors a Cypher update pipeline: each
    // operation observes the complete input bag before the next operation
    // starts. DELETE in particular must gather and deduplicate its targets
    // across every matched row before removing anything. A computed value
    // is evaluated for every row over the rows as they stand when its
    // operation starts.
    for mutation in &mutations {
        apply_mutation(
            transaction,
            &layout,
            &mut input_rows,
            mutation,
            &mut summary,
            context,
        )?;
    }
    // Each later segment reads on from the rows so far (its WITH, UNWIND,
    // and MATCH stages see the graph as the mutations left it) and then
    // applies its own mutations to the rows it produced.
    for ResolvedSegment { source, mutations } in &segments {
        input_rows = with_rows_table(
            transaction,
            &layout,
            &input_rows,
            |transaction| read_row_source(transaction, source, &layout, context),
            context,
        )?;
        for mutation in mutations {
            apply_mutation(
                transaction,
                &layout,
                &mut input_rows,
                mutation,
                &mut summary,
                context,
            )?;
        }
    }

    let (columns, rows) = match result {
        Some(result) => {
            let result =
                read_materialized_rows(transaction, &layout, &input_rows, result, true, context)?;
            (result.columns, result.rows)
        }
        None => (Vec::new(), Vec::new()),
    };
    Ok(QueryResult {
        columns,
        rows,
        summary,
    })
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
    context: &ExecutionContext,
) -> Result<QueryResult> {
    // Preparation walks the complete operator tree before execution. A bad
    // parameter in a later UNION branch therefore cannot spend traversal work
    // or issue a SQLite read in an earlier branch.
    let plan = plan.prepare(parameters)?;
    execute_prepared_read_plan(connection, plan, true, context)
}

fn execute_prepared_read_plan(
    connection: &Connection,
    plan: PreparedReadPlan,
    is_output: bool,
    context: &ExecutionContext,
) -> Result<QueryResult> {
    let columns = plan.columns;
    let rows = match plan.root {
        PreparedPhysicalOperator::BatchHydrate { input } => {
            let pending = execute_physical_operator(connection, *input, context)?;
            hydrate_pending_rows(connection, pending, is_output, context)?
        }
        operator => finish_materialized_rows(
            execute_physical_operator(connection, operator, context)?,
            is_output,
            context,
        )?,
    };
    Ok(QueryResult {
        columns,
        rows,
        summary: QuerySummary::default(),
    })
}

fn execute_physical_operator(
    connection: &Connection,
    operator: PreparedPhysicalOperator,
    context: &ExecutionContext,
) -> Result<Vec<PendingRow>> {
    context.check()?;
    match operator {
        PreparedPhysicalOperator::Sqlite(fragment) => {
            execute_sql_fragment(connection, fragment, context)
        }
        PreparedPhysicalOperator::SemanticDistinct { input } => distinct_pending_rows(
            execute_physical_operator(connection, *input, context)?,
            context,
        ),
        PreparedPhysicalOperator::Paginate {
            input,
            offset,
            limit,
        } => Ok(execute_physical_operator(connection, *input, context)?
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .collect()),
        PreparedPhysicalOperator::Union { left, right, kind } => {
            let mut rows = execute_physical_operator(connection, *left, context)?;
            rows.extend(execute_physical_operator(connection, *right, context)?);
            if kind == UnionKind::Distinct {
                rows = distinct_pending_rows(rows, context)?;
            }
            Ok(rows)
        }
        PreparedPhysicalOperator::BatchHydrate { .. } => Err(crate::Error::Data(
            "batch hydration must be the root physical operator".into(),
        )),
    }
}

fn execute_sql_fragment(
    connection: &Connection,
    plan: PreparedSqlFragment,
    context: &ExecutionContext,
) -> Result<Vec<PendingRow>> {
    let mut runtime_values = HashMap::with_capacity(plan.inputs.len());
    for input in plan.inputs {
        let (output, value) = match input {
            PreparedValueOperator::TrailExpand { output, spec } => (
                output,
                traversal::materialize_trails(connection, &spec, &mut context.trails.borrow_mut())?,
            ),
            PreparedValueOperator::ShortestExpand { output, mode, spec } => (
                output,
                traversal::materialize_shortest(
                    connection,
                    &spec,
                    mode,
                    &mut context.trails.borrow_mut(),
                )?,
            ),
        };
        if runtime_values.insert(output, value).is_some() {
            return Err(crate::Error::Data(format!(
                "runtime slot %r{} was materialized more than once",
                output.0
            )));
        }
    }

    let bindings = plan
        .arguments
        .into_iter()
        .map(|argument| match argument {
            PreparedFragmentArgument::Value(value) => Ok(value),
            PreparedFragmentArgument::Runtime(slot) => {
                runtime_values.remove(&slot).ok_or_else(|| {
                    crate::Error::Data(format!(
                        "SQLite fragment is missing runtime slot %r{}",
                        slot.0
                    ))
                })
            }
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(slot) = runtime_values.keys().next() {
        return Err(crate::Error::Data(format!(
            "runtime slot %r{} was not consumed",
            slot.0
        )));
    }
    debug_assert_eq!(plan.columns.len(), plan.output_shapes.len());

    // Completely drain and drop the SQLite cursor before a later Rust operator
    // issues another fragment, traversal query, or hydration query.
    let mut statement = connection.prepare_cached(&plan.sql)?;
    debug_assert!(statement.readonly());
    let mut cursor = statement.query(params_from_iter(bindings.iter()))?;
    let mut rows = Vec::new();
    while let Some(row) = cursor.next()? {
        context.record_sql_row(row, plan.output_shapes.len())?;
        let values = plan
            .output_shapes
            .iter()
            .enumerate()
            .map(|(index, shape)| {
                context.check()?;
                decode_pending_output(
                    row.get_ref(index)?
                        .as_str()
                        .map_err(rusqlite::Error::from)?,
                    *shape,
                )
            })
            .collect::<Result<PendingRow>>()?;
        rows.push(values);
    }
    Ok(rows)
}

fn finish_materialized_rows(
    rows: Vec<PendingRow>,
    is_output: bool,
    context: &ExecutionContext,
) -> Result<Vec<Vec<CypherValue>>> {
    if is_output {
        context.result_rows(rows.len())?;
    }
    let mut result_bytes = 0;
    rows.into_iter()
        .map(|row| {
            context.check()?;
            let row = row
                .into_iter()
                .map(|value| match value {
                    PendingValue::Materialized(value) => Ok(value),
                    PendingValue::Path(_)
                    | PendingValue::PathNodes(_)
                    | PendingValue::PathRelationships(_) => Err(crate::Error::Data(
                        "physical read plan omitted required batch hydration".into(),
                    )),
                })
                .collect::<Result<Vec<_>>>()?;
            if is_output {
                context.result_row(&row, &mut result_bytes)?;
            }
            Ok(row)
        })
        .collect()
}

fn delete_entities(
    transaction: &Transaction<'_>,
    rows: &[ExecutionRow],
    targets: &[(SlotId, EntityKind)],
    detach: bool,
    summary: &mut QuerySummary,
    context: &ExecutionContext,
) -> Result<()> {
    let mut node_ids = BTreeSet::new();
    let mut relationship_ids = BTreeSet::new();

    for row in rows {
        context.check()?;
        for (slot, expected_kind) in targets {
            if !row.binds(*slot) {
                continue;
            }
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
        context.check()?;
        summary.relationships_deleted +=
            transaction.execute("DELETE FROM edges WHERE id = ?1", params![id])?;
    }

    if detach {
        for id in &node_ids {
            context.check()?;
            summary.relationships_deleted += transaction.execute(
                "DELETE FROM edges WHERE from_node = ?1 OR to_node = ?1",
                params![id],
            )?;
        }
    }

    for id in node_ids {
        context.check()?;
        summary.nodes_deleted += transaction
            .execute("DELETE FROM nodes WHERE id = ?1", params![id])
            .map_err(|error| match &error {
                // The edge foreign keys restrict deleting an endpoint. The
                // index triggers on `nodes` make SQLite report the failure
                // with the trigger's extended code, so the message decides.
                rusqlite::Error::SqliteFailure(failure, Some(message))
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation
                        && message == "FOREIGN KEY constraint failed" =>
                {
                    crate::Error::Constraint(
                        "cannot delete a node that still has relationships; \
                         delete them first or use DETACH DELETE"
                            .into(),
                    )
                }
                _ => error.into(),
            })?;
    }

    Ok(())
}

/// Adds (`add`) or removes the labels on the node in `slot` of every row
/// that binds it, returning the number of node/label pairs that changed.
fn update_labels(
    transaction: &Transaction<'_>,
    rows: &mut [ExecutionRow],
    slot: SlotId,
    labels: &[String],
    add: bool,
    context: &ExecutionContext,
) -> Result<usize> {
    let mut changed = 0;
    for target_index in 0..rows.len() {
        context.check()?;
        if !rows[target_index].binds(slot) {
            continue;
        }
        let id = rows[target_index].node_id(slot)?.to_owned();
        let encoded = transaction.query_row(
            "SELECT labels FROM nodes WHERE id = ?1",
            params![id],
            |record| record.get::<_, String>(0),
        )?;
        let mut current = serde_json::from_str::<Vec<String>>(&encoded).map_err(|error| {
            crate::Error::Data(format!("stored node labels are malformed JSON: {error}"))
        })?;
        let before = current.len();
        if add {
            for label in labels {
                context.check()?;
                if !current.contains(label) {
                    current.push(label.clone());
                }
            }
        } else {
            current.retain(|label| !labels.contains(label));
        }
        if current.len() != before {
            changed += before.abs_diff(current.len());
            transaction.execute(
                "UPDATE nodes SET labels = json(?1) WHERE id = ?2",
                params![serde_json::Value::from(current.clone()).to_string(), id],
            )?;
        }
        for row in rows.iter_mut() {
            context.check()?;
            for entity in row.all_entities_mut() {
                if let EntityValue::Node(node) = entity
                    && node.element_id == id
                {
                    node.labels = current.clone();
                }
            }
        }
    }
    Ok(changed)
}

fn update_properties(
    transaction: &Transaction<'_>,
    rows: &mut [ExecutionRow],
    slot: SlotId,
    update: impl Fn(usize, &mut serde_json::Map<String, serde_json::Value>),
    context: &ExecutionContext,
) -> Result<usize> {
    let mut total_changed = 0;
    for target_index in 0..rows.len() {
        context.check()?;
        if !rows[target_index].binds(slot) {
            continue;
        }
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
        update(target_index, &mut properties);
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
            context.check()?;
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

fn distinct_pending_rows(
    rows: Vec<PendingRow>,
    context: &ExecutionContext,
) -> Result<Vec<PendingRow>> {
    let mut distinct: Vec<PendingRow> = Vec::with_capacity(rows.len());
    'rows: for row in rows {
        context.check()?;
        for existing in &distinct {
            context.check()?;
            if pending_rows_equivalent(existing, &row) {
                continue 'rows;
            }
        }
        distinct.push(row);
    }
    Ok(distinct)
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

    fn to_cypher_value(&self) -> CypherValue {
        match self {
            Self::Node(node) => CypherValue::Node(node.clone()),
            Self::Relationship(relationship) => CypherValue::Relationship(relationship.clone()),
        }
    }
}

/// One row of a mutation pipeline: the entity bound to each entity slot and
/// the JSON text of each bound scalar slot (as the SQL compiler encodes
/// result columns), `None` where the row does not bind the slot.
#[derive(Clone)]
struct ExecutionRow {
    entities: Vec<Option<EntityValue>>,
    scalars: Vec<Option<String>>,
    // Typed collection slots live here during writes, so identity-based
    // property/label refreshes also reach entities retained inside a list.
    entity_lists: Vec<Option<Vec<EntityValue>>>,
}

impl ExecutionRow {
    fn empty(layout: &RowLayout) -> Self {
        Self {
            entities: vec![None; layout.slots.len()],
            scalars: vec![None; layout.scalar_slots],
            entity_lists: vec![None; layout.scalar_slots],
        }
    }

    fn all_entities_mut(&mut self) -> impl Iterator<Item = &mut EntityValue> {
        self.entities
            .iter_mut()
            .flatten()
            .chain(self.entity_lists.iter_mut().flatten().flatten())
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

    /// Whether this row binds `slot`. `OPTIONAL MATCH` leaves slots unbound,
    /// and updating clauses skip those rows.
    fn binds(&self, slot: SlotId) -> bool {
        self.entities.get(slot.0).is_some_and(Option::is_some)
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
        for entity in self.all_entities_mut() {
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
}

/// Applies one mutation to every row, in place. Nested mutation lists
/// (`MERGE` templates and actions) call this on row subsets.
fn apply_mutation(
    transaction: &Transaction<'_>,
    layout: &RowLayout,
    rows: &mut Vec<ExecutionRow>,
    mutation: &ResolvedMutation,
    summary: &mut QuerySummary,
    context: &ExecutionContext,
) -> Result<()> {
    let slots = layout.slots.as_slice();
    context.check()?;
    match mutation {
        ResolvedMutation::CreateNode {
            slot,
            labels,
            properties,
        } => {
            let values = row_values(transaction, layout, rows, properties, true, context)?;
            let encoded_labels = serde_json::Value::from(labels.clone()).to_string();
            for (index, input_row) in rows.iter_mut().enumerate() {
                context.check()?;
                let properties = &stored_properties(values.get(index));
                let id = transaction.query_row(
                    "INSERT INTO nodes (id, labels, props)
                     VALUES (lower(hex(randomblob(16))), json(?1), json(?2))
                     RETURNING id",
                    params![encoded_labels, properties.to_string()],
                    |row| row.get::<_, String>(0),
                )?;
                input_row.set(
                    slots,
                    *slot,
                    EntityValue::Node(NodeValue {
                        element_id: id,
                        labels: labels.clone(),
                        properties: cypher_properties(properties.clone())?,
                    }),
                )?;
                summary.nodes_created += 1;
                summary.labels_added += labels.len();
            }
        }
        ResolvedMutation::CreateRelationship {
            slot,
            relationship_type,
            from,
            to,
            properties,
        } => {
            let values = row_values(transaction, layout, rows, properties, true, context)?;
            for (index, input_row) in rows.iter_mut().enumerate() {
                context.check()?;
                let properties = &stored_properties(values.get(index));
                let from = input_row.node_id(*from)?.to_owned();
                let to = input_row.node_id(*to)?.to_owned();
                let id = transaction.query_row(
                    "INSERT INTO edges (id, edge_type, from_node, to_node, props)
                     VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, json(?4))
                     RETURNING id",
                    params![relationship_type, from, to, properties.to_string()],
                    |row| row.get::<_, String>(0),
                )?;
                input_row.set(
                    slots,
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
            let values = row_values(transaction, layout, rows, value, false, context)?;
            summary.properties_set += update_properties(
                transaction,
                rows,
                *slot,
                |index, properties| {
                    let value = values.get(index);
                    if value.is_null() {
                        properties.remove(key);
                    } else {
                        properties.insert(key.clone(), value.clone());
                    }
                },
                context,
            )?;
        }
        ResolvedMutation::ReplaceProperties { slot, properties } => {
            let values = row_values(transaction, layout, rows, properties, true, context)?;
            summary.properties_set += update_properties(
                transaction,
                rows,
                *slot,
                |index, current| {
                    let serde_json::Value::Object(replacement) =
                        stored_properties(values.get(index))
                    else {
                        unreachable!("stored properties are validated maps")
                    };
                    *current = replacement;
                },
                context,
            )?;
        }
        ResolvedMutation::MergeProperties { slot, properties } => {
            let values = row_values(transaction, layout, rows, properties, true, context)?;
            summary.properties_set += update_properties(
                transaction,
                rows,
                *slot,
                |index, current| {
                    let patch = values
                        .get(index)
                        .as_object()
                        .expect("stored properties are validated maps");
                    for (key, value) in patch {
                        if value.is_null() {
                            current.remove(key);
                        } else {
                            current.insert(key.clone(), value.clone());
                        }
                    }
                },
                context,
            )?;
        }
        ResolvedMutation::RemoveProperty { slot, key } => {
            summary.properties_set += update_properties(
                transaction,
                rows,
                *slot,
                |_, properties| {
                    properties.remove(key);
                },
                context,
            )?;
        }
        ResolvedMutation::SetLabels { slot, labels } => {
            summary.labels_added += update_labels(transaction, rows, *slot, labels, true, context)?;
        }
        ResolvedMutation::RemoveLabels { slot, labels } => {
            summary.labels_removed +=
                update_labels(transaction, rows, *slot, labels, false, context)?;
        }
        ResolvedMutation::Delete { targets, detach } => {
            delete_entities(transaction, rows, targets, *detach, summary, context)?;
        }
        ResolvedMutation::Merge {
            matcher,
            fresh,
            creates,
            on_create,
            on_match,
        } => merge_rows(
            transaction,
            layout,
            rows,
            matcher,
            fresh,
            creates,
            on_create,
            on_match,
            summary,
            context,
        )?,
    }
    Ok(())
}

/// A property map as it is stored: a property whose value is null does not
/// exist, so `CREATE ({k: null})` and `SET n = {k: null}` leave `k` absent.
fn stored_properties(properties: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(entries) = properties else {
        unreachable!("stored properties are validated maps")
    };
    serde_json::Value::Object(
        entries
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

/// Executes `MERGE` over the rows. Rows are handled in order: each is matched
/// against the graph as it stands, and a row without a match creates the
/// pattern once before the next row is matched, so later rows can match what
/// earlier rows created. Matching rows are expanded once per match.
///
/// Rows that match before the merge changes anything are found with one read
/// over all of them. The rest are settled one at a time: each is matched
/// alone, because it may match what an earlier row created, and creates the
/// pattern when it still has no match. That keeps the matcher work linear in
/// the row count; re-reading every pending row after each create made a
/// merge that creates once per row cubic. A row is only re-matched when the
/// merge has written something since it was last matched.
#[allow(clippy::too_many_arguments)]
fn merge_rows(
    transaction: &Transaction<'_>,
    layout: &RowLayout,
    rows: &mut Vec<ExecutionRow>,
    matcher: &PreparedReadPlan,
    fresh: &[(SlotId, EntityKind)],
    creates: &[ResolvedMutation],
    on_create: &[ResolvedMutation],
    on_match: &[ResolvedMutation],
    summary: &mut QuerySummary,
    context: &ExecutionContext,
) -> Result<()> {
    let mut merge = MergeRun {
        context,
        transaction,
        layout,
        matcher,
        fresh,
        on_match,
        summary,
        finished: Vec::new(),
        changed: false,
    };
    let pending = rows.drain(..).enumerate().collect::<Vec<_>>();
    let unmatched = merge.match_batch(pending)?;
    for row in unmatched {
        context.check()?;
        let (origin, row) = if merge.changed {
            let Some(row) = merge.match_batch(vec![row])?.pop() else {
                continue;
            };
            row
        } else {
            row
        };
        let mut created = vec![row];
        for mutation in creates.iter().chain(on_create) {
            context.check()?;
            apply_mutation(
                transaction,
                layout,
                &mut created,
                mutation,
                merge.summary,
                context,
            )?;
        }
        merge.changed = true;
        merge
            .finished
            .extend(created.into_iter().map(|row| (origin, row)));
    }
    let mut finished = merge.finished;
    finished.sort_by_key(|(origin, _)| *origin);
    rows.extend(finished.into_iter().map(|(_, row)| row));
    Ok(())
}

/// The state one `MERGE` shares between its matcher reads.
struct MergeRun<'a, 'b> {
    context: &'a ExecutionContext,
    transaction: &'a Transaction<'b>,
    layout: &'a RowLayout,
    matcher: &'a PreparedReadPlan,
    fresh: &'a [(SlotId, EntityKind)],
    on_match: &'a [ResolvedMutation],
    summary: &'a mut QuerySummary,
    /// Settled rows tagged with their input position.
    finished: Vec<(usize, ExecutionRow)>,
    /// Whether the merge has created or updated anything yet, so a row
    /// matched before that must be matched again.
    changed: bool,
}

impl MergeRun<'_, '_> {
    /// Matches the pattern from each of `pending` with one read, expands the
    /// matching rows per match, applies the `ON MATCH` actions to them, and
    /// returns the rows that matched nothing, in their input order.
    fn match_batch(
        &mut self,
        pending: Vec<(usize, ExecutionRow)>,
    ) -> Result<Vec<(usize, ExecutionRow)>> {
        let context = self.context;
        context.check()?;
        let subset = pending
            .iter()
            .map(|(_, row)| row.clone())
            .collect::<Vec<_>>();
        let result = read_materialized_rows(
            self.transaction,
            self.layout,
            &subset,
            self.matcher.clone(),
            false,
            context,
        )?;
        let mut matches: Vec<Vec<Vec<CypherValue>>> = vec![Vec::new(); subset.len()];
        for mut row in result.rows {
            context.check()?;
            let index = match row.remove(0) {
                CypherValue::Integer(index) => usize::try_from(index).ok(),
                _ => None,
            }
            .filter(|index| *index < matches.len())
            .ok_or_else(|| crate::Error::Data("MERGE matcher returned a bad row index".into()))?;
            matches[index].push(row);
        }

        let mut matched: Vec<(usize, ExecutionRow)> = Vec::new();
        let mut unmatched = Vec::new();
        for ((origin, row), found) in pending.into_iter().zip(matches) {
            context.check()?;
            if found.is_empty() {
                unmatched.push((origin, row));
                continue;
            }
            for values in found {
                context.check()?;
                let mut expanded = row.clone();
                for ((slot, kind), value) in self.fresh.iter().zip(values) {
                    context.check()?;
                    expanded.set(&self.layout.slots, *slot, entity_from_value(value, *kind)?)?;
                }
                matched.push((origin, expanded));
            }
        }
        if !matched.is_empty() {
            self.changed |= !self.on_match.is_empty();
            let (origins, mut matched_rows): (Vec<_>, Vec<_>) = matched.into_iter().unzip();
            for mutation in self.on_match {
                context.check()?;
                apply_mutation(
                    self.transaction,
                    self.layout,
                    &mut matched_rows,
                    mutation,
                    self.summary,
                    context,
                )?;
            }
            self.finished.extend(origins.into_iter().zip(matched_rows));
        }
        Ok(unmatched)
    }
}

fn entity_from_value(value: CypherValue, kind: EntityKind) -> Result<EntityValue> {
    match (value, kind) {
        (CypherValue::Node(node), EntityKind::Node) => Ok(EntityValue::Node(node)),
        (CypherValue::Relationship(relationship), EntityKind::Relationship) => {
            Ok(EntityValue::Relationship(relationship))
        }
        _ => Err(crate::Error::Data(
            "MERGE matcher returned a value of the wrong kind".into(),
        )),
    }
}

/// One stored value per input row: a static value shared by every row, or
/// the per-row results of a computed expression.
enum RowValues {
    Static(serde_json::Value),
    PerRow(Vec<serde_json::Value>),
}

impl RowValues {
    fn get(&self, index: usize) -> &serde_json::Value {
        match self {
            Self::Static(value) => value,
            Self::PerRow(values) => &values[index],
        }
    }
}

/// Evaluates a computed stored value over the current rows. `map` says the
/// value must be a property map (CREATE, `SET =`, `SET +=`) rather than a
/// single property value.
fn row_values(
    transaction: &Transaction<'_>,
    layout: &RowLayout,
    rows: &[ExecutionRow],
    value: &ResolvedStoredValue,
    map: bool,
    context: &ExecutionContext,
) -> Result<RowValues> {
    let plan = match value {
        ResolvedStoredValue::Static(value) => return Ok(RowValues::Static(value.clone())),
        ResolvedStoredValue::Computed(plan) => plan.clone(),
    };
    let result = read_materialized_rows(transaction, layout, rows, plan, false, context)?;
    if result.rows.len() != rows.len() {
        return Err(crate::Error::Data(
            "a computed stored value did not yield one value per row".into(),
        ));
    }
    result
        .rows
        .into_iter()
        .map(|mut row| {
            let value = storable_json(row.swap_remove(0))?;
            if map {
                crate::planner::require_object(&value, "computed properties")?;
                crate::planner::validate_storable_properties(&value)?;
            } else {
                crate::planner::validate_storable_value(&value)?;
            }
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()
        .map(RowValues::PerRow)
}

/// A computed value as stored JSON. Graph entities and paths are not
/// property values.
fn storable_json(value: CypherValue) -> Result<serde_json::Value> {
    match &value {
        CypherValue::Node(_) | CypherValue::Relationship(_) | CypherValue::Path(_) => {
            Err(crate::Error::Semantic(
                "graph entities and paths cannot be stored as property values".into(),
            ))
        }
        CypherValue::List(items)
            if items.iter().any(|item| {
                matches!(
                    item,
                    CypherValue::Node(_) | CypherValue::Relationship(_) | CypherValue::Path(_)
                )
            }) =>
        {
            Err(crate::Error::Semantic(
                "graph entities and paths cannot be stored as property values".into(),
            ))
        }
        _ => Ok(json_from_cypher(&value)),
    }
}

/// Runs a prepared read over the rows: the post-mutation `RETURN`, a
/// computed stored value, or a `MERGE` matcher.
fn read_materialized_rows(
    transaction: &Transaction<'_>,
    layout: &RowLayout,
    rows: &[ExecutionRow],
    result: PreparedReadPlan,
    is_output: bool,
    context: &ExecutionContext,
) -> Result<QueryResult> {
    with_rows_table(
        transaction,
        layout,
        rows,
        |transaction| execute_prepared_read_plan(transaction, result, is_output, context),
        context,
    )
}

/// Writes the rows to the temporary rows table, one encoded entity record
/// per entity slot and one JSON text per scalar slot, runs `body` over it
/// inside the same transaction, and empties the table again.
///
/// The table outlives the call while its columns fit the next layout:
/// creating or dropping it changes the schema, which invalidates every
/// prepared statement, so a `MERGE` that reads once per row would otherwise
/// re-prepare its matcher each time.
fn with_rows_table<T>(
    transaction: &Transaction<'_>,
    layout: &RowLayout,
    rows: &[ExecutionRow],
    body: impl FnOnce(&Transaction<'_>) -> Result<T>,
    context: &ExecutionContext,
) -> Result<T> {
    let table = crate::planner::ROWS_TABLE;
    let columns = std::iter::once("k TEXT".to_owned())
        .chain((0..layout.slots.len()).map(|index| format!("e{index} TEXT")))
        .chain((0..layout.scalar_slots).map(|index| format!("s{index} TEXT")))
        .collect::<Vec<_>>()
        .join(", ");
    let existing = transaction
        .query_row(
            "SELECT sql FROM temp.sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if existing.is_some_and(|sql| sql.ends_with(&format!("({columns})"))) {
        transaction.execute(&format!("DELETE FROM temp.{table}"), [])?;
    } else {
        transaction.execute_batch(&format!(
            "DROP TABLE IF EXISTS temp.{table}; CREATE TEMP TABLE {table} ({columns});"
        ))?;
    }
    {
        let placeholders = (1..=layout.slots.len() + layout.scalar_slots + 1)
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut insert =
            transaction.prepare_cached(&format!("INSERT INTO {table} VALUES ({placeholders})"))?;
        for (index, row) in rows.iter().enumerate() {
            context.check()?;
            // Unbound slots hold JSON `null` text, exactly as an OPTIONAL
            // MATCH leaves them in a read pipeline's intermediate rows.
            let records =
                std::iter::once(index.to_string())
                    .chain(row.entities.iter().map(|entity| {
                        entity
                            .as_ref()
                            .map_or_else(|| "null".to_owned(), encode_entity)
                    }))
                    .chain(row.scalars.iter().enumerate().map(|(slot, scalar)| {
                        match &row.entity_lists[slot] {
                            Some(entities) => format!(
                                "[{}]",
                                entities
                                    .iter()
                                    .map(encode_entity)
                                    .collect::<Vec<_>>()
                                    .join(",")
                            ),
                            None => scalar.clone().unwrap_or_else(|| "null".to_owned()),
                        }
                    }))
                    .collect::<Vec<_>>();
            insert.execute(params_from_iter(records.iter()))?;
        }
    }
    let result = body(transaction)?;
    transaction.execute(&format!("DELETE FROM temp.{table}"), [])?;
    Ok(result)
}

/// The JSON record the SQL compiler expects for an encoded entity: the
/// inverse of `decode_entity`, reflecting the entity's current in-memory
/// properties.
fn encode_entity(entity: &EntityValue) -> String {
    let record = match entity {
        EntityValue::Node(node) => serde_json::json!({
            "id": node.element_id,
            "labels": node.labels,
            "props": json_from_properties(&node.properties),
        }),
        EntityValue::Relationship(relationship) => serde_json::json!({
            "id": relationship.element_id,
            "type": relationship.relationship_type,
            "from": relationship.start_node_element_id,
            "to": relationship.end_node_element_id,
            "props": json_from_properties(&relationship.properties),
        }),
    };
    record.to_string()
}

fn json_from_properties(
    properties: &std::collections::BTreeMap<String, CypherValue>,
) -> serde_json::Value {
    serde_json::Value::Object(
        properties
            .iter()
            .map(|(key, value)| (key.clone(), json_from_cypher(value)))
            .collect(),
    )
}

/// Stored property values are JSON: scalars, lists, and maps. Entities and
/// paths never occur inside properties.
fn json_from_cypher(value: &CypherValue) -> serde_json::Value {
    match value {
        CypherValue::Null => serde_json::Value::Null,
        CypherValue::Boolean(value) => serde_json::Value::Bool(*value),
        CypherValue::Integer(value) => serde_json::Value::from(*value),
        CypherValue::Float(value) => serde_json::Number::from_f64(*value)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        CypherValue::String(value) => serde_json::Value::String(value.clone()),
        CypherValue::List(items) => {
            serde_json::Value::Array(items.iter().map(json_from_cypher).collect())
        }
        CypherValue::Map(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), json_from_cypher(value)))
                .collect(),
        ),
        CypherValue::Node(_) | CypherValue::Relationship(_) | CypherValue::Path(_) => {
            serde_json::Value::Null
        }
    }
}

/// Runs a row source and decodes its rows: the entity columns first, then
/// the scalar columns, each kept as the JSON text the compiler produced.
fn read_row_source(
    transaction: &Transaction<'_>,
    input: &ResolvedRowSource,
    layout: &RowLayout,
    context: &ExecutionContext,
) -> Result<Vec<ExecutionRow>> {
    let mut statement = transaction.prepare_cached(&input.sql)?;
    debug_assert!(statement.readonly());
    let mut cursor = statement.query(params_from_iter(input.bindings.iter()))?;
    let mut input_rows = Vec::new();
    while let Some(row) = cursor.next()? {
        context.record_sql_row(row, input.slots.len() + input.scalars.len())?;
        let mut execution_row = ExecutionRow::empty(layout);
        for (column, (slot, kind)) in input.slots.iter().enumerate() {
            let Some(encoded) = row.get::<_, Option<String>>(column)? else {
                continue;
            };
            if encoded == "null" {
                continue;
            }
            let entity = decode_entity(&encoded, *kind)?;
            execution_row.set(&layout.slots, *slot, entity)?;
        }
        for (offset, slot) in input.scalars.iter().enumerate() {
            let column = input.slots.len() + offset;
            let destination = execution_row
                .scalars
                .get_mut(slot.0)
                .ok_or_else(|| crate::Error::Data(format!("unknown scalar slot {}", slot.0)))?;
            *destination = row.get::<_, Option<String>>(column)?;
            if let Some((_, kind)) = layout
                .entity_lists
                .iter()
                .find(|(candidate, _)| candidate == slot)
                && let Some(encoded) = destination.as_deref().filter(|value| *value != "null")
            {
                execution_row.entity_lists[slot.0] = Some(decode_entity_list(encoded, *kind)?);
                *destination = None;
            }
        }
        input_rows.push(execution_row);
    }
    Ok(input_rows)
}

fn decode_entity_list(encoded: &str, kind: EntityKind) -> Result<Vec<EntityValue>> {
    let values: Vec<serde_json::Value> = serde_json::from_str(encoded)
        .map_err(|error| crate::Error::Data(format!("malformed entity list: {error}")))?;
    values
        .into_iter()
        .map(|value| decode_entity(&value.to_string(), kind))
        .collect()
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
            let labels = match fields.remove("labels") {
                Some(serde_json::Value::Array(labels)) => labels
                    .into_iter()
                    .map(|label| match label {
                        serde_json::Value::String(label) => Ok(label),
                        _ => Err(crate::Error::Data(
                            "matched node has an invalid label".into(),
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?,
                Some(serde_json::Value::Null) | None => Vec::new(),
                _ => {
                    return Err(crate::Error::Data(
                        "matched node has an invalid label list".into(),
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
    /// Account for every repeated snapshot before cloning hydrated entities.
    fn pending_bytes(&self, value: &PendingValue, context: &ExecutionContext) -> Result<usize> {
        let node_bytes = |id: &String| -> Result<usize> {
            let node = self.nodes.get(id).ok_or_else(|| {
                crate::Error::Data(format!("path references missing node `{id}`"))
            })?;
            Ok(8_usize.saturating_add(context.node_bytes(node)?))
        };
        let relationship_bytes = |id: &String| -> Result<usize> {
            let edge = self.relationships.get(id).ok_or_else(|| {
                crate::Error::Data(format!("path references missing relationship `{id}`"))
            })?;
            Ok(8_usize.saturating_add(context.relationship_bytes(edge)?))
        };
        match value {
            PendingValue::Materialized(value) => context.value_bytes(value),
            PendingValue::Path(path) => {
                let mut bytes = 24_usize; // path value and its two containers
                for id in &path.node_ids {
                    bytes = bytes.saturating_add(node_bytes(id)?);
                }
                for id in &path.relationship_ids {
                    bytes = bytes.saturating_add(relationship_bytes(id)?);
                }
                Ok(bytes)
            }
            PendingValue::PathNodes(ids) => ids.iter().try_fold(8_usize, |bytes, id| {
                Ok(bytes.saturating_add(node_bytes(id)?))
            }),
            PendingValue::PathRelationships(ids) => ids.iter().try_fold(8_usize, |bytes, id| {
                Ok(bytes.saturating_add(relationship_bytes(id)?))
            }),
        }
    }

    fn load(
        connection: &Connection,
        rows: &[PendingRow],
        context: &ExecutionContext,
    ) -> Result<Self> {
        let (mut node_ids, mut relationship_ids) = collect_path_ids(rows, context)?;
        if node_ids.is_empty() && relationship_ids.is_empty() {
            return Ok(Self::default());
        }
        let mut hydrator = Self::default();

        // Whole entities already decoded from the primary SQL fragments are
        // snapshots from the same read transaction and can seed the cache.
        for value in rows.iter().flatten() {
            context.check()?;
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

        hydrator.load_nodes(connection, &node_ids, context)?;
        hydrator.load_relationships(connection, &relationship_ids, context)?;
        Ok(hydrator)
    }

    fn load_nodes(
        &mut self,
        connection: &Connection,
        ids: &BTreeSet<String>,
        context: &ExecutionContext,
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let encoded_ids = encode_id_set(ids);
        let mut statement = connection.prepare(
            "SELECT json_object(
                    'id', n.id, 'labels', json(n.labels), 'props', json(n.props)
                 )
                 FROM json_each(?1) AS wanted
                 JOIN nodes AS n ON n.id = wanted.value
                 WHERE wanted.type = 'text'",
        )?;
        let mut cursor = statement.query(params![encoded_ids])?;
        while let Some(row) = cursor.next()? {
            context.record_sql_row(row, 1)?;
            let encoded = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            let EntityValue::Node(node) = decode_entity(encoded, EntityKind::Node)? else {
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
        context: &ExecutionContext,
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let encoded_ids = encode_id_set(ids);
        let mut statement = connection.prepare(
            "SELECT json_object(
                    'id', e.id, 'type', e.edge_type, 'from', e.from_node,
                    'to', e.to_node, 'props', json(e.props)
                 )
                 FROM json_each(?1) AS wanted
                 JOIN edges AS e ON e.id = wanted.value
                 WHERE wanted.type = 'text'",
        )?;
        let mut cursor = statement.query(params![encoded_ids])?;
        while let Some(row) = cursor.next()? {
            context.record_sql_row(row, 1)?;
            let encoded = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            let EntityValue::Relationship(relationship) =
                decode_entity(encoded, EntityKind::Relationship)?
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

    fn hydrate(&self, identity: PathIdentity, context: &ExecutionContext) -> Result<PathValue> {
        let mut nodes = Vec::with_capacity(identity.node_ids.len());
        for id in identity.node_ids {
            nodes.push(self.node(&id, context)?);
        }
        let mut relationships = Vec::with_capacity(identity.relationship_ids.len());
        for id in identity.relationship_ids {
            relationships.push(self.relationship(&id, context)?);
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

    fn node(&self, id: &str, context: &ExecutionContext) -> Result<NodeValue> {
        context.check()?;
        self.nodes
            .get(id)
            .cloned()
            .ok_or_else(|| crate::Error::Data(format!("path references missing node `{id}`")))
    }

    fn relationship(&self, id: &str, context: &ExecutionContext) -> Result<RelationshipValue> {
        context.check()?;
        self.relationships.get(id).cloned().ok_or_else(|| {
            crate::Error::Data(format!("path references missing relationship `{id}`"))
        })
    }

    fn hydrate_node_list(
        &self,
        ids: Vec<String>,
        context: &ExecutionContext,
    ) -> Result<CypherValue> {
        ids.into_iter()
            .map(|id| self.node(&id, context).map(CypherValue::Node))
            .collect::<Result<Vec<_>>>()
            .map(CypherValue::List)
    }

    fn hydrate_relationship_list(
        &self,
        ids: Vec<String>,
        context: &ExecutionContext,
    ) -> Result<CypherValue> {
        ids.into_iter()
            .map(|id| {
                self.relationship(&id, context)
                    .map(CypherValue::Relationship)
            })
            .collect::<Result<Vec<_>>>()
            .map(CypherValue::List)
    }
}

fn collect_path_ids(
    rows: &[PendingRow],
    context: &ExecutionContext,
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut nodes = BTreeSet::new();
    let mut relationships = BTreeSet::new();
    for value in rows.iter().flatten() {
        context.check()?;
        let (node_ids, relationship_ids): (&[String], &[String]) = match value {
            PendingValue::Path(path) => (&path.node_ids, &path.relationship_ids),
            PendingValue::PathNodes(ids) => (ids, &[]),
            PendingValue::PathRelationships(ids) => (&[], ids),
            PendingValue::Materialized(_) => continue,
        };
        for id in node_ids {
            context.check()?;
            nodes.insert(id.clone());
        }
        for id in relationship_ids {
            context.check()?;
            relationships.insert(id.clone());
        }
    }
    Ok((nodes, relationships))
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
    is_output: bool,
    context: &ExecutionContext,
) -> Result<Vec<Vec<CypherValue>>> {
    if is_output {
        context.result_rows(rows.len())?;
    }
    let mut result_bytes = 0;
    let hydrator = PathHydrator::load(connection, &rows, context)?;
    rows.into_iter()
        .map(|row| {
            context.check()?;
            if is_output && context.counts_result_bytes() {
                context.admit_result_bytes(8, &mut result_bytes)?;
                for value in &row {
                    context.admit_result_bytes(
                        hydrator.pending_bytes(value, context)?,
                        &mut result_bytes,
                    )?;
                }
            }
            let row = row
                .into_iter()
                .map(|value| match value {
                    PendingValue::Materialized(value) => Ok(value),
                    PendingValue::Path(identity) => {
                        hydrator.hydrate(identity, context).map(CypherValue::Path)
                    }
                    PendingValue::PathNodes(ids) => hydrator.hydrate_node_list(ids, context),
                    PendingValue::PathRelationships(ids) => {
                        hydrator.hydrate_relationship_list(ids, context)
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(row)
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
        OutputShape::EntityList(kind) => {
            let values = decode_entity_list(encoded, kind)?
                .into_iter()
                .map(|entity| entity.to_cypher_value())
                .collect();
            Ok(PendingValue::Materialized(CypherValue::List(values)))
        }
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
    constraints::register(connection)?;
    // Foreign-key enforcement is connection-local and cannot be changed while
    // a transaction is active, so enable it before schema initialization.
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;

    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let schema_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if application_id == APPLICATION_ID {
        if matches!(schema_version, 1..=3) {
            return upgrade_schema(connection, schema_version);
        }
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

    upgrade_schema(connection, 0)
}

/// Install labels, property indexes, and uniqueness support, rolling back schema
/// changes and the version marker if any validation or backfill fails.
fn upgrade_schema(connection: &mut Connection, from: i64) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match from {
        0 => transaction.execute_batch(INITIAL_SCHEMA)?,
        1 => transaction.execute_batch(MIGRATE_V1_TO_V2)?,
        2 => {
            // An experimental branch also called its single-label layout v2.
            // It is not main's v2 format and must not be silently accepted.
            transaction.prepare("SELECT labels FROM nodes LIMIT 0").map_err(|_| {
                crate::Error::Data("unsupported experimental version 2 schema; expected main's multi-label layout".into())
            })?;
        }
        3 => {}
        _ => unreachable!("only known schema versions can be upgraded"),
    }
    if from < 3 {
        transaction.execute_batch(PROPERTY_INDEX_SCHEMA)?;
    }
    transaction.execute_batch(constraints::SCHEMA)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    validate_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn register_functions(connection: &Connection) -> Result<()> {
    connection.create_aggregate_function(
        "crabnet_collect_distinct",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        CollectDistinct,
    )?;
    connection.create_scalar_function(
        "crabnet_entity_list_key",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let value = json_argument(context, 0)?;
            let ids = value
                .as_array()
                .ok_or_else(|| {
                    rusqlite::Error::UserFunctionError(Box::new(crate::Error::Data(
                        "entity list is not an array".into(),
                    )))
                })?
                .iter()
                .map(|entity| {
                    entity
                        .get("id")
                        .filter(|id| id.is_string())
                        .cloned()
                        .ok_or_else(|| {
                            rusqlite::Error::UserFunctionError(Box::new(crate::Error::Data(
                                "collected entity is missing its identity".into(),
                            )))
                        })
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(semantic_json_key(&serde_json::Value::Array(ids)))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_path_key",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let encoded = context.get::<String>(0)?;
            let kind = context.get::<String>(1)?;
            let identity = decode_path_identity(&encoded)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
            // Ignore segment boundaries and, for graph lists, the identities
            // belonging only to the other half of the underlying path.
            let value = match kind.as_str() {
                "nodes" => serde_json::json!(identity.node_ids),
                "relationships" => serde_json::json!(identity.relationship_ids),
                _ => serde_json::json!([identity.node_ids, identity.relationship_ids]),
            };
            Ok(semantic_json_key(&value))
        },
    )?;
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
    // Cypher equality, comparison, and sort order for values SQLite cannot
    // compare natively. Each takes JSON text; generated SQL routes lists and
    // maps here and keeps primitives in plain SQL.
    connection.create_scalar_function(
        "crabnet_equals",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let left = json_argument(context, 0)?;
            let right = json_argument(context, 1)?;
            Ok(comparison::equals(&left, &right))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_compare",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let left = json_argument(context, 0)?;
            let right = json_argument(context, 1)?;
            Ok(comparison::compare(&left, &right).map(|ordering| ordering as i64))
        },
    )?;
    // The sort key takes a JSON type name and the SQL value, so primitives
    // need no JSON round trip; lists and maps arrive as JSON text.
    connection.create_scalar_function(
        "crabnet_order_key",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let value_type = context.get::<Option<String>>(0)?;
            let value = match (value_type.as_deref(), context.get_raw(1)) {
                (_, ValueRef::Null) | (None | Some("null"), _) => serde_json::Value::Null,
                (Some("true"), _) => serde_json::Value::Bool(true),
                (Some("false"), _) => serde_json::Value::Bool(false),
                (Some("integer"), ValueRef::Integer(integer)) => integer.into(),
                (Some("real"), ValueRef::Real(real)) => real.into(),
                (Some("real"), ValueRef::Integer(integer)) => (integer as f64).into(),
                (Some("text"), ValueRef::Text(text)) => {
                    serde_json::Value::String(String::from_utf8_lossy(text).into_owned())
                }
                _ => json_argument(context, 1)?,
            };
            Ok(comparison::order_key(&value))
        },
    )?;
    for (name, keep) in [
        ("crabnet_min", Ordering::Less),
        ("crabnet_max", Ordering::Greater),
    ] {
        connection.create_aggregate_function(
            name,
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            Extremum(keep),
        )?;
    }
    // SQLite renders REAL values into JSON text with fifteen significant
    // digits, which silently perturbs any float that needs more. Every scalar
    // leaves SQL as JSON text, so the encoder routes reals through this
    // shortest round-trip formatter instead of `json()`.
    connection.create_scalar_function(
        "crabnet_json_real",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| Ok(context.get::<Option<f64>>(0)?.map(json_real_text)),
    )?;
    // The bundled SQLite is built without its math library, so the numeric
    // functions come from Rust. Non-finite results read as null.
    type MathFunction = fn(f64) -> f64;
    let math: [(&str, MathFunction); 12] = [
        ("crabnet_ceil", f64::ceil),
        ("crabnet_floor", f64::floor),
        ("crabnet_sqrt", f64::sqrt),
        ("crabnet_exp", f64::exp),
        ("crabnet_ln", f64::ln),
        ("crabnet_log10", f64::log10),
        ("crabnet_sin", f64::sin),
        ("crabnet_cos", f64::cos),
        ("crabnet_tan", f64::tan),
        ("crabnet_asin", f64::asin),
        ("crabnet_acos", f64::acos),
        ("crabnet_atan", f64::atan),
    ];
    for (name, function) in math {
        connection.create_scalar_function(
            name,
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            move |context| {
                Ok(context
                    .get::<Option<f64>>(0)?
                    .map(function)
                    .filter(|value| value.is_finite()))
            },
        )?;
    }
    // Text conversions and string helpers that SQLite lacks. Each takes text
    // (the generated SQL gates the argument type) and returns NULL when the
    // text does not convert.
    connection.create_scalar_function(
        "crabnet_to_integer",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            Ok(context
                .get::<Option<String>>(0)?
                .and_then(|text| text_to_integer(&text)))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_to_float",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            Ok(context
                .get::<Option<String>>(0)?
                .and_then(|text| text_to_float(&text)))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_reverse",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            Ok(context
                .get::<Option<String>>(0)?
                .map(|text| text.chars().rev().collect::<String>()))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_split",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let text = context.get::<Option<String>>(0)?;
            let delimiter = context.get::<Option<String>>(1)?;
            Ok(match (text, delimiter) {
                (Some(text), Some(delimiter)) => Some(split_text(&text, &delimiter)),
                _ => None,
            })
        },
    )?;
    // Generated SQL calls this from a CASE branch to raise a runtime argument
    // error (for example a zero `range()` step) instead of producing a value.
    connection.create_scalar_function(
        "crabnet_raise",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| -> rusqlite::Result<Option<i64>> {
            let message = context.get::<String>(0)?;
            Err(rusqlite::Error::UserFunctionError(
                format!("{}{message}", crate::error::ARGUMENT_ERROR_TAG).into(),
            ))
        },
    )?;
    // Generated SQL raises a runtime type error, such as `NOT` of a number,
    // the same way.
    connection.create_scalar_function(
        "crabnet_type_error",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| -> rusqlite::Result<Option<i64>> {
            let message = context.get::<String>(0)?;
            Err(rusqlite::Error::UserFunctionError(
                format!("{}{message}", crate::error::TYPE_ERROR_TAG).into(),
            ))
        },
    )?;
    Ok(())
}

fn json_argument(context: &Context<'_>, index: usize) -> rusqlite::Result<serde_json::Value> {
    let encoded = context.get::<String>(index)?;
    serde_json::from_str(&encoded)
        .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
}

/// `min()` and `max()` under Cypher orderability: the argument is a value's
/// JSON text, nulls are skipped, and the result is the JSON text of the
/// least or greatest value, or NULL when there was none.
struct Extremum(Ordering);

/// Keeps the first representative of each typed value in input order.
struct CollectDistinct;

#[derive(Default)]
struct CollectedValues {
    keys: HashSet<Vec<u8>>,
    values: Vec<serde_json::Value>,
}

impl Aggregate<CollectedValues, String> for CollectDistinct {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<CollectedValues> {
        Ok(CollectedValues::default())
    }

    fn step(&self, context: &mut Context<'_>, state: &mut CollectedValues) -> rusqlite::Result<()> {
        let value = json_argument(context, 0)?;
        if value.is_null() {
            return Ok(());
        }
        let key = match context.get_raw(1) {
            ValueRef::Blob(key) | ValueRef::Text(key) => key.to_vec(),
            _ => {
                return Err(rusqlite::Error::UserFunctionError(Box::new(
                    crate::Error::Data("collected value has no identity key".into()),
                )));
            }
        };
        if state.keys.insert(key) {
            state.values.push(value);
        }
        Ok(())
    }

    fn finalize(
        &self,
        _: &mut Context<'_>,
        state: Option<CollectedValues>,
    ) -> rusqlite::Result<String> {
        Ok(serde_json::Value::Array(state.unwrap_or_default().values).to_string())
    }
}

impl Aggregate<Option<(Vec<u8>, String)>, Option<String>> for Extremum {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<Option<(Vec<u8>, String)>> {
        Ok(None)
    }

    fn step(
        &self,
        context: &mut Context<'_>,
        best: &mut Option<(Vec<u8>, String)>,
    ) -> rusqlite::Result<()> {
        let Some(encoded) = context.get::<Option<String>>(0)? else {
            return Ok(());
        };
        let value: serde_json::Value = serde_json::from_str(&encoded)
            .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
        if value.is_null() {
            return Ok(());
        }
        let key = comparison::order_key(&value);
        if best
            .as_ref()
            .is_none_or(|(best, _)| key.cmp(best) == self.0)
        {
            *best = Some((key, encoded));
        }
        Ok(())
    }

    fn finalize(
        &self,
        _: &mut Context<'_>,
        best: Option<Option<(Vec<u8>, String)>>,
    ) -> rusqlite::Result<Option<String>> {
        Ok(best.flatten().map(|(_, encoded)| encoded))
    }
}

/// `toInteger()` of a string: an integer literal, or a finite float literal
/// truncated toward zero; anything else is null.
fn text_to_integer(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(integer) = text.parse::<i64>() {
        return Some(integer);
    }
    let float = text.parse::<f64>().ok().filter(|float| float.is_finite())?;
    let truncated = float.trunc();
    (truncated >= i64::MIN as f64 && truncated < i64::MAX as f64).then_some(truncated as i64)
}

/// `toFloat()` of a string: a finite float literal, else null.
fn text_to_float(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|float| float.is_finite())
}

/// `split()`: the pieces between delimiters as a JSON array of strings. An
/// empty delimiter splits into characters.
fn split_text(text: &str, delimiter: &str) -> String {
    let pieces = if delimiter.is_empty() {
        text.chars().map(String::from).collect::<Vec<_>>()
    } else {
        text.split(delimiter).map(str::to_owned).collect()
    };
    serde_json::Value::Array(pieces.into_iter().map(serde_json::Value::String).collect())
        .to_string()
}

/// Formats a float as the shortest JSON number text that parses back to the
/// same `f64`. Non-finite values mirror SQLite's own `json()` spelling so the
/// transport stays parseable.
fn json_real_text(value: f64) -> String {
    match serde_json::Number::from_f64(value) {
        Some(number) => number.to_string(),
        None if value.is_nan() => "null".into(),
        None if value.is_sign_negative() => "-9.0e999".into(),
        None => "9.0e999".into(),
    }
}

/// Produces a stable, recursively typed key for the JSON-backed part of the
/// Cypher value domain. Equal integers and floats deliberately share a key;
/// booleans remain distinct from numbers, and collection values recurse using
/// the same rules. SQLite uses the resulting blob for GROUP BY and DISTINCT.
/// Uniqueness constraints persist these keys; changing this encoding requires
/// rebuilding their keys in a schema migration.
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
    connection.prepare("SELECT id, labels, props FROM nodes LIMIT 0")?;
    connection.prepare("SELECT label, node_id FROM node_labels LIMIT 0")?;
    connection.prepare("SELECT id, edge_type, from_node, to_node, props FROM edges LIMIT 0")?;
    connection.prepare("SELECT node_id, key, value FROM node_properties LIMIT 0")?;
    connection.prepare("SELECT label, property FROM node_unique_constraints LIMIT 0")?;
    connection.prepare("SELECT label, property, value, node_id FROM node_unique_values LIMIT 0")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_after_a_write_aborts_the_transaction_and_restores_indexes() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mut database = Database::in_memory().unwrap();
        database.create_unique_constraint("Item", "key").unwrap();
        database.execute("CREATE (:Item {key: 'saved'})").unwrap();
        let token = crate::CancellationToken::new();
        let from_trigger = token.clone();
        let wrote = Arc::new(AtomicBool::new(false));
        let trigger_wrote = wrote.clone();
        database
            .connection
            .create_scalar_function("cancel_query", 0, FunctionFlags::SQLITE_UTF8, move |_| {
                trigger_wrote.store(true, Ordering::Relaxed);
                from_trigger.cancel();
                Ok(0)
            })
            .unwrap();
        database.connection.execute_batch("CREATE TRIGGER cancel_insert AFTER INSERT ON nodes WHEN json_extract(NEW.props, '$.cancel') = 1 BEGIN SELECT cancel_query(); END;").unwrap();
        let mut transaction = database.transaction().unwrap();
        transaction
            .execute("MATCH (n:Item) SET n.key = 'changed'")
            .unwrap();
        let options = QueryOptions {
            cancellation: Some(token),
            ..QueryOptions::default()
        };
        let error = transaction
            .execute_with_options(
                "CREATE (:Item {key: 'new', cancel: true}), (:Item {key: 'later'})",
                &Parameters::new(),
                &options,
            )
            .unwrap_err();
        assert!(
            wrote.load(Ordering::Relaxed),
            "the cancellation must happen after a write"
        );
        assert!(matches!(error, crate::Error::Cancelled), "{error}");
        assert!(matches!(
            transaction.commit(),
            Err(crate::Error::TransactionAborted)
        ));
        assert_eq!(
            database
                .execute("MATCH (n:Item {key: 'saved'}) RETURN n.key")
                .unwrap()
                .rows,
            [vec![crate::Value::from("saved")]]
        );
        database
            .execute(
                "CREATE (:Item {key: 'changed'}), (:Item {key: 'new'}), (:Item {key: 'later'})",
            )
            .unwrap();
    }

    #[test]
    fn rust_distinct_checks_cancellation_without_running_sqlite() {
        let token = crate::CancellationToken::new();
        let context = ExecutionContext::new(&QueryOptions {
            cancellation: Some(token.clone()),
            ..QueryOptions::default()
        });
        token.cancel();
        let result = distinct_pending_rows(
            vec![vec![PendingValue::Materialized(CypherValue::Integer(1))]],
            &context,
        );
        assert!(matches!(result, Err(crate::Error::Cancelled)));
    }
    use rusqlite::OptionalExtension;
    use serde_json::json;

    #[test]
    fn generated_read_fragments_prepare_with_exact_dense_bindings() {
        #[derive(Default)]
        struct Inspected {
            fragments: usize,
            edge_sources: usize,
            node_domains: usize,
        }

        fn assert_dense_bindings(
            statement: &rusqlite::Statement<'_>,
            expected: usize,
            context: &str,
        ) {
            assert_eq!(
                statement.parameter_count(),
                expected,
                "{context}: SQLite parameter arity differs from the physical arguments"
            );
            for index in 1..=expected {
                let expected_name = format!("?{index}");
                assert_eq!(
                    statement.parameter_name(index),
                    Some(expected_name.as_str()),
                    "{context}: physical placeholders must be dense and numbered from ?1"
                );
            }
        }

        fn inspect_operator(
            connection: &Connection,
            operator: &PreparedPhysicalOperator,
            expected_columns: &[String],
            context: &str,
            inspected: &mut Inspected,
        ) {
            match operator {
                PreparedPhysicalOperator::Sqlite(fragment) => {
                    inspected.fragments += 1;
                    let statement = connection.prepare(&fragment.sql).unwrap_or_else(|error| {
                        panic!("{context}: generated SQLite fragment did not prepare: {error}")
                    });
                    assert!(statement.readonly(), "{context}: read fragment writes");
                    assert_dense_bindings(&statement, fragment.arguments.len(), context);
                    assert_eq!(
                        fragment.columns, expected_columns,
                        "{context}: fragment columns differ from the read plan"
                    );
                    assert_eq!(
                        statement.column_count(),
                        fragment.columns.len(),
                        "{context}: SQLite result width differs from the planned columns"
                    );
                    assert_eq!(
                        fragment.columns.len(),
                        fragment.output_shapes.len(),
                        "{context}: every result column needs one output shape"
                    );

                    for (index, input) in fragment.inputs.iter().enumerate() {
                        inspected.edge_sources += 1;
                        let edge_context = format!("{context}/edge-source-{index}");
                        let spec = match input {
                            PreparedValueOperator::TrailExpand { spec, .. }
                            | PreparedValueOperator::ShortestExpand { spec, .. } => spec,
                        };
                        let statement =
                            connection.prepare(&spec.edge_sql).unwrap_or_else(|error| {
                                panic!(
                                    "{edge_context}: generated edge SQL did not prepare: {error}"
                                )
                            });
                        assert!(statement.readonly(), "{edge_context}: edge source writes");
                        assert_dense_bindings(&statement, spec.edge_bindings.len(), &edge_context);
                        assert_eq!(
                            statement.column_count(),
                            3,
                            "{edge_context}: traversal edge sources must return id/from/to"
                        );
                        for (name, domain) in [
                            ("start-domain", spec.start_domain.as_ref()),
                            ("end-domain", spec.end_domain.as_ref()),
                        ] {
                            let Some(domain) = domain else { continue };
                            inspected.node_domains += 1;
                            let domain_context = format!("{edge_context}/{name}");
                            let statement =
                                connection.prepare(&domain.sql).unwrap_or_else(|error| {
                                    panic!(
                                        "{domain_context}: generated node-domain SQL did not \
                                         prepare: {error}"
                                    )
                                });
                            assert!(statement.readonly(), "{domain_context}: domain writes");
                            assert_dense_bindings(
                                &statement,
                                domain.bindings.len(),
                                &domain_context,
                            );
                            assert_eq!(
                                statement.column_count(),
                                1,
                                "{domain_context}: node domains must return only ids"
                            );
                        }
                    }
                }
                PreparedPhysicalOperator::SemanticDistinct { input }
                | PreparedPhysicalOperator::Paginate { input, .. }
                | PreparedPhysicalOperator::BatchHydrate { input } => {
                    inspect_operator(connection, input, expected_columns, context, inspected)
                }
                PreparedPhysicalOperator::Union { left, right, .. } => {
                    inspect_operator(
                        connection,
                        left,
                        expected_columns,
                        &format!("{context}/union-left"),
                        inspected,
                    );
                    inspect_operator(
                        connection,
                        right,
                        expected_columns,
                        &format!("{context}/union-right"),
                        inspected,
                    );
                }
            }
        }

        let database = Database::in_memory().unwrap();
        let cases = vec![
            (
                "literal-and-parameter-projections",
                "RETURN 1 AS literal, $value AS parameter, [$value] AS nested",
                Parameters::from([("value".to_owned(), json!({"items": [true, 2]}))]),
                1,
                0,
            ),
            (
                "typed-predicates",
                r#"MATCH (node:Thing {kind: $kind})
                   WHERE node.score >= $floor AND node.name CONTAINS $piece
                   RETURN node.value AS value ORDER BY value"#,
                Parameters::from([
                    ("kind".to_owned(), json!("sample")),
                    ("floor".to_owned(), json!(2)),
                    ("piece".to_owned(), json!("am")),
                ]),
                1,
                0,
            ),
            (
                "with-aggregate-and-ordering",
                r#"WITH $seed AS seed WHERE seed >= $floor
                   WITH seed + $step AS adjusted
                   RETURN count(adjusted) AS total ORDER BY $order"#,
                Parameters::from([
                    ("seed".to_owned(), json!(4)),
                    ("floor".to_owned(), json!(1)),
                    ("step".to_owned(), json!(3)),
                    ("order".to_owned(), json!(0)),
                ]),
                1,
                0,
            ),
            (
                "semantic-distinct-pagination",
                r#"MATCH (node:Thing {kind: $kind})
                   RETURN DISTINCT node.value AS value ORDER BY value
                   SKIP $skip LIMIT $limit"#,
                Parameters::from([
                    ("kind".to_owned(), json!("sample")),
                    ("skip".to_owned(), json!(1)),
                    ("limit".to_owned(), json!(2)),
                ]),
                1,
                0,
            ),
            (
                "with-distinct-typed-keys-and-modifiers",
                r#"UNWIND $values AS value
                   WITH DISTINCT value ORDER BY value + $step SKIP $skip LIMIT $limit
                   WHERE value > $floor
                   RETURN value"#,
                Parameters::from([
                    ("values".to_owned(), json!([1, 1.0, 2])),
                    ("step".to_owned(), json!(1)),
                    ("skip".to_owned(), json!(0)),
                    ("limit".to_owned(), json!(2)),
                    ("floor".to_owned(), json!(0)),
                ]),
                1,
                0,
            ),
            (
                "with-distinct-graph-list-keys",
                r#"MATCH p = (a)-[:STEP*0..]->(b)
                   WITH DISTINCT nodes(p) AS ns, relationships(p) AS rs
                   RETURN ns, rs"#,
                Parameters::new(),
                1,
                1,
            ),
            (
                "collected-entities-and-distinct-scalars",
                r#"MATCH (node:Thing {kind: $kind})
                   WITH node ORDER BY node.value
                   WITH collect(DISTINCT node) AS entities,
                        collect(DISTINCT node.value + $step) AS values
                   WITH DISTINCT entities, values WHERE size(entities) > $floor
                   UNWIND entities AS entity
                   RETURN entity, entities, values"#,
                Parameters::from([
                    ("kind".to_owned(), json!("sample")),
                    ("step".to_owned(), json!(1)),
                    ("floor".to_owned(), json!(0)),
                ]),
                1,
                0,
            ),
            (
                "collected-entities-seed-runtime-traversal",
                r#"MATCH (node:Start {key: $key})
                   WITH collect(node) AS entities UNWIND entities AS entity
                   MATCH (entity)-[:STEP*1..]->(target)
                   RETURN collect(DISTINCT target) AS targets"#,
                Parameters::from([("key".to_owned(), json!(1))]),
                1,
                1,
            ),
            (
                "path-lists-unwind-into-entities",
                r#"MATCH p = (start:Start {key: $key})-[:STEP*0..2]->()-[:STEP]->()
                   WITH DISTINCT nodes(p) AS ns, relationships(p) AS rs LIMIT $limit
                   UNWIND ns AS node UNWIND rs AS edge
                   OPTIONAL MATCH (node)-[:EXTRA]->(other)
                   RETURN node, edge, other"#,
                Parameters::from([("key".to_owned(), json!(1)), ("limit".to_owned(), json!(2))]),
                1,
                0,
            ),
            (
                "runtime-path-unwind-seeds-another-runtime-traversal",
                r#"MATCH p = (start:Start {key: $key})-[rels:STEP*]->()
                   UNWIND nodes(p) AS node UNWIND rels AS edge
                   MATCH (node)-[:MORE* {enabled: $enabled}]->(target)
                   RETURN p, node, edge, target"#,
                Parameters::from([
                    ("key".to_owned(), json!(1)),
                    ("enabled".to_owned(), json!(true)),
                ]),
                1,
                2,
            ),
            (
                "aggregate-expressions-group-before-scalar-composition",
                r#"UNWIND $values AS value
                   WITH value % 2 AS parity, value
                   WITH DISTINCT parity, count(*) + $step AS tally,
                        [x IN collect(value) WHERE x > $floor | x + $step] AS kept
                   ORDER BY tally DESC LIMIT $limit
                   WHERE tally > $floor
                   RETURN parity, kept, {count: tally, first: kept[0]} AS report"#,
                Parameters::from([
                    ("values".to_owned(), json!([1, 2, 2, 3])),
                    ("step".to_owned(), json!(1)),
                    ("floor".to_owned(), json!(0)),
                    ("limit".to_owned(), json!(2)),
                ]),
                1,
                0,
            ),
            (
                "three-branch-union",
                r#"RETURN $left AS value
                   UNION RETURN $middle AS value
                   UNION RETURN $right AS value"#,
                Parameters::from([
                    ("left".to_owned(), json!(1)),
                    ("middle".to_owned(), json!(1.0)),
                    ("right".to_owned(), json!([2])),
                ]),
                3,
                0,
            ),
            (
                "two-runtime-trail-inputs",
                r#"MATCH (start:Start)-[:FIRST* {enabled: $enabled}]->(middle:Middle)-
                         [:SECOND* {weight: $weight}]->(end:End)
                   WHERE end.flag = $flag RETURN end"#,
                Parameters::from([
                    ("enabled".to_owned(), json!(true)),
                    ("weight".to_owned(), json!(2)),
                    ("flag".to_owned(), json!("kept")),
                ]),
                1,
                2,
            ),
            (
                "shortest-runtime-input",
                r#"MATCH route = allShortestPaths(
                         (start)-[:ROAD*..4 {open: $open}]->(end))
                   RETURN length(route) AS hops"#,
                Parameters::from([("open".to_owned(), json!(true))]),
                1,
                1,
            ),
            (
                "same-clause-relationship-list-rematch",
                r#"MATCH (start:Start {name: $name})-[rels:LINK*1..2]->(end),
                         (start)-[rels:LINK*1..2]->(other)
                   RETURN end, other"#,
                Parameters::from([("name".to_owned(), json!("seed"))]),
                1,
                0,
            ),
            (
                "prebound-relationship-list-rematch-through-with",
                r#"MATCH (:Start {name: $name})-[rels:LINK*1..2]->() WITH rels
                   MATCH (x)-[rels:LINK* {enabled: $enabled}]->(y)
                   RETURN x, y"#,
                Parameters::from([
                    ("name".to_owned(), json!("seed")),
                    ("enabled".to_owned(), json!(true)),
                ]),
                1,
                1,
            ),
        ];

        let mut prepared_node_domains = 0;
        for (name, query, parameters, expected_fragments, expected_edge_sources) in cases {
            let Plan::Read(plan) = planner::compile(query)
                .unwrap_or_else(|error| panic!("{name}: query did not compile: {error}"))
            else {
                panic!("{name}: expected a read plan")
            };
            let plan = plan
                .prepare(&parameters)
                .unwrap_or_else(|error| panic!("{name}: physical plan did not prepare: {error}"));
            let mut inspected = Inspected::default();
            inspect_operator(
                &database.connection,
                &plan.root,
                &plan.columns,
                name,
                &mut inspected,
            );
            assert_eq!(
                inspected.fragments, expected_fragments,
                "{name}: unexpected number of SQLite fragments"
            );
            assert_eq!(
                inspected.edge_sources, expected_edge_sources,
                "{name}: unexpected number of runtime edge sources"
            );
            prepared_node_domains += inspected.node_domains;
        }
        assert!(
            prepared_node_domains > 0,
            "the corpus must prepare at least one constrained traversal endpoint domain"
        );
    }

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
        let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(0));

        let error =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &budget).unwrap_err();

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
        let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(0));

        let error =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &budget).unwrap_err();

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
        let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(0));

        let error =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &budget).unwrap_err();

        assert!(matches!(
            error,
            crate::Error::ResourceLimit(message)
                if message.contains("trail expansion") && message.contains("0-state budget")
        ));
    }

    #[test]
    fn union_branches_share_one_trail_budget() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:BudgetStart)-[:LINK]->(:BudgetEnd)")
            .unwrap();
        let branch = "MATCH (:BudgetStart)-[:LINK*]->(end) RETURN end AS value";
        let Plan::Read(plan) = planner::compile(&format!("{branch} UNION ALL {branch}")).unwrap()
        else {
            panic!("expected a read plan");
        };
        let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(1));

        let error =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &budget).unwrap_err();

        assert!(matches!(
            error,
            crate::Error::ResourceLimit(message)
                if message.contains("trail expansion") && message.contains("1-state budget")
        ));
    }

    #[test]
    fn union_branches_share_one_serialized_trail_payload_budget() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:PayloadStart)-[:LINK]->(:PayloadEnd)")
            .unwrap();
        let branch = "MATCH (:PayloadStart)-[:LINK*]->(end) RETURN end AS value";
        let Plan::Read(single_plan) = planner::compile(branch).unwrap() else {
            panic!("expected a read plan");
        };
        let measurement =
            ExecutionContext::with_trails(traversal::TrailBudget::with_limits(100, usize::MAX));
        execute_read_plan(
            &database.connection,
            single_plan,
            &Parameters::new(),
            &measurement,
        )
        .unwrap();
        let one_branch_payload = measurement.trails.borrow().payload_used();
        assert!(one_branch_payload > 0);

        let Plan::Read(plan) = planner::compile(&format!("{branch} UNION ALL {branch}")).unwrap()
        else {
            panic!("expected a read plan");
        };
        let budget = ExecutionContext::with_trails(traversal::TrailBudget::with_limits(
            100,
            one_branch_payload,
        ));

        let error =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &budget).unwrap_err();

        assert!(matches!(
            error,
            crate::Error::ResourceLimit(message)
                if message.contains("trail expansion")
                    && message.contains(&format!(
                        "{one_branch_payload}-byte serialized-payload budget"
                    ))
        ));
    }

    #[test]
    fn retained_memory_limit_propagates_through_the_executor() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:MemoryStart)-[:LINK]->(:MemoryEnd)")
            .unwrap();
        let Plan::Read(plan) =
            planner::compile("MATCH (:MemoryStart)-[:LINK*]->(end) RETURN end").unwrap()
        else {
            panic!("expected a read plan");
        };
        let budget =
            ExecutionContext::with_trails(traversal::TrailBudget::new(100).with_memory_limit(0));

        let error =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &budget).unwrap_err();

        assert!(matches!(
            error,
            crate::Error::ResourceLimit(message)
                if message.contains("trail expansion")
                    && message.contains("0-byte retained-memory budget")
        ));
    }

    #[test]
    fn retained_memory_is_released_between_producers_rather_than_accumulated() {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:MemoryStart)-[:LINK]->(:MemoryEnd)")
            .unwrap();
        let branch = "MATCH (:MemoryStart)-[:LINK*]->(end) RETURN end AS value";
        let Plan::Read(single_plan) = planner::compile(branch).unwrap() else {
            panic!("expected a read plan");
        };
        let measurement =
            ExecutionContext::with_trails(traversal::TrailBudget::with_limits(100, usize::MAX));
        execute_read_plan(
            &database.connection,
            single_plan,
            &Parameters::new(),
            &measurement,
        )
        .unwrap();
        let one_branch_peak = measurement.trails.borrow().memory_peak();
        assert!(one_branch_peak > 0);
        assert_eq!(measurement.trails.borrow().memory_used(), 0);

        // Unlike the cumulative state and payload counters, each producer
        // returns its inputs when it finishes, so two branches fit inside the
        // peak of one.
        let Plan::Read(plan) = planner::compile(&format!("{branch} UNION ALL {branch}")).unwrap()
        else {
            panic!("expected a read plan");
        };
        let exact = ExecutionContext::with_trails(
            traversal::TrailBudget::with_limits(100, usize::MAX).with_memory_limit(one_branch_peak),
        );
        let result =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &exact).unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(exact.trails.borrow().memory_peak(), one_branch_peak);
        assert_eq!(exact.trails.borrow().memory_used(), 0);

        let Plan::Read(plan) = planner::compile(branch).unwrap() else {
            panic!("expected a read plan");
        };
        let short = ExecutionContext::with_trails(
            traversal::TrailBudget::with_limits(100, usize::MAX)
                .with_memory_limit(one_branch_peak - 1),
        );
        let error =
            execute_read_plan(&database.connection, plan, &Parameters::new(), &short).unwrap_err();
        assert!(matches!(
            error,
            crate::Error::ResourceLimit(message)
                if message.contains(&format!(
                    "{}-byte retained-memory budget",
                    one_branch_peak - 1
                ))
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

        let (nodes, relationships) =
            collect_path_ids(&rows, &ExecutionContext::new(&QueryOptions::default())).unwrap();
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
        let context = ExecutionContext::new(&QueryOptions::default());
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
        let hydrator = PathHydrator::load(
            &connection,
            &rows,
            &ExecutionContext::new(&QueryOptions::default()),
        )
        .unwrap();
        let path = hydrator
            .hydrate(
                PathIdentity {
                    node_ids: vec![node.element_id.clone()],
                    relationship_ids: Vec::new(),
                },
                &context,
            )
            .unwrap();
        assert_eq!(path.nodes, [node]);
        assert!(path.relationships.is_empty());
    }

    #[test]
    fn empty_and_seeded_relationship_lists_need_no_storage_query() {
        let context = ExecutionContext::new(&QueryOptions::default());
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
        let hydrator = PathHydrator::load(
            &connection,
            &rows,
            &ExecutionContext::new(&QueryOptions::default()),
        )
        .unwrap();
        assert_eq!(
            hydrator
                .hydrate_relationship_list(Vec::new(), &context)
                .unwrap(),
            CypherValue::List(Vec::new())
        );
        assert_eq!(
            hydrator
                .hydrate_relationship_list(vec![relationship.element_id.clone()], &context)
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
                "INSERT INTO nodes (id, labels) VALUES ('one', '[\"Thing\"]'), ('two', '[\"Thing\"]')",
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
    fn version_one_files_migrate_to_the_label_list_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE nodes (
                     id TEXT PRIMARY KEY NOT NULL,
                     label TEXT,
                     props TEXT NOT NULL DEFAULT '{{}}'
                         CHECK (json_valid(props) AND json_type(props) = 'object')
                 ) STRICT;
                 CREATE TABLE edges (
                     id TEXT PRIMARY KEY NOT NULL,
                     edge_type TEXT NOT NULL CHECK (edge_type <> ''),
                     from_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
                     to_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
                     props TEXT NOT NULL DEFAULT '{{}}'
                         CHECK (json_valid(props) AND json_type(props) = 'object')
                 ) STRICT;
                 CREATE INDEX nodes_by_label ON nodes(label);
                 INSERT INTO nodes (id, label, props)
                     VALUES ('labeled', 'Thing', '{{\"n\": 1}}'), ('bare', NULL, '{{}}');
                 INSERT INTO edges (id, edge_type, from_node, to_node)
                     VALUES ('link', 'LINK', 'labeled', 'bare');
                 PRAGMA application_id = {APPLICATION_ID};
                 PRAGMA user_version = 1;"
            ))
            .unwrap();

        initialize(&mut connection).unwrap();

        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let labels: Vec<(String, String)> = connection
            .prepare("SELECT id, labels FROM nodes ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            labels,
            vec![
                ("bare".to_owned(), "[]".to_owned()),
                ("labeled".to_owned(), "[\"Thing\"]".to_owned()),
            ]
        );
        let indexed: Vec<(String, String)> = connection
            .prepare("SELECT label, node_id FROM node_labels")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(indexed, vec![("Thing".to_owned(), "labeled".to_owned())]);
        let edges: i64 = connection
            .query_row("SELECT count(*) FROM edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(edges, 1);

        // Opening again is a no-op on the migrated file, and the triggers
        // maintain the index table from here on.
        initialize(&mut connection).unwrap();
        connection
            .execute(
                "UPDATE nodes SET labels = '[\"Thing\", \"Other\"]' WHERE id = 'bare'",
                [],
            )
            .unwrap();
        let things: i64 = connection
            .query_row(
                "SELECT count(*) FROM node_labels WHERE label = 'Thing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(things, 2);
    }

    #[test]
    fn version_two_upgrade_backfills_properties_and_preserves_labels() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(INITIAL_SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"
            PRAGMA user_version = 2;
            INSERT INTO nodes (id, labels, props)
                VALUES ('one', '["Person", "Member"]', '{"id":7,"missing":null,"tags":[1,2]}');
        "#,
            )
            .unwrap();
        let mut database = Database::from_connection(connection).unwrap();
        let result = database
            .execute("MATCH (n:Person:Member {id: 7}) RETURN n.id")
            .unwrap();
        assert_eq!(result.rows, vec![vec![CypherValue::Integer(7)]]);
        let count: i64 = database
            .connection
            .query_row("SELECT count(*) FROM node_properties", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        initialize(&mut database.connection).unwrap();
        database
            .execute("MATCH (n:Member) SET n.id = 8 REMOVE n:Person")
            .unwrap();
        let result = database
            .execute("MATCH (n:Member {id: 8}) RETURN n.id")
            .unwrap();
        assert_eq!(result.rows, vec![vec![CypherValue::Integer(8)]]);
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn failed_property_index_upgrade_rolls_back_schema_and_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(INITIAL_SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"
            PRAGMA user_version = 2;
            PRAGMA ignore_check_constraints = ON;
            INSERT INTO nodes (id, props) VALUES ('bad', 'invalid json');
            PRAGMA ignore_check_constraints = OFF;
        "#,
            )
            .unwrap();
        assert!(initialize(&mut connection).is_err());
        assert!(connection.is_autocommit());
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let objects: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'node_properties%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(objects, 0);
    }

    #[test]
    fn version_three_upgrade_preserves_data_and_supports_unique_constraints() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(INITIAL_SCHEMA).unwrap();
        connection.execute_batch(PROPERTY_INDEX_SCHEMA).unwrap();
        connection
            .execute_batch(
                r#"PRAGMA user_version = 3;
               INSERT INTO nodes (id, labels, props) VALUES
                   ('a', '["Person", "Member"]', '{"email":"a"}'),
                   ('b', '["Person"]', '{"email":"a"}');
               INSERT INTO edges (id, edge_type, from_node, to_node)
                   VALUES ('edge', 'LINK', 'a', 'b');"#,
            )
            .unwrap();
        let mut database = Database::from_connection(connection).unwrap();
        assert!(database.unique_constraints().unwrap().is_empty());
        assert!(matches!(
            database.create_unique_constraint("Person", "email"),
            Err(crate::Error::Constraint(_))
        ));
        assert_eq!(
            database
                .execute("MATCH (a:Member)-[:LINK]->(b:Person) RETURN a.email, b.email")
                .unwrap()
                .rows,
            [vec![json!("a"), json!("a")]]
        );
        database
            .execute("MATCH (a:Member) SET a.email = 'changed'")
            .unwrap();
        database
            .create_unique_constraint("Person", "email")
            .unwrap();
        assert!(matches!(
            database.execute("CREATE (:Person {email: 'changed'})"),
            Err(crate::Error::Constraint(_))
        ));
        let version: i64 = database
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        initialize(&mut database.connection).unwrap();
        assert_eq!(database.unique_constraints().unwrap().len(), 1);
    }

    #[test]
    fn failed_uniqueness_upgrade_rolls_back_ddl_and_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(INITIAL_SCHEMA).unwrap();
        connection.execute_batch(PROPERTY_INDEX_SCHEMA).unwrap();
        // Make a later DDL statement fail after the new tables were created.
        connection
            .execute_batch(
                "PRAGMA user_version = 3;
             CREATE TRIGGER node_unique_insert AFTER INSERT ON nodes BEGIN SELECT 1; END;
             INSERT INTO nodes (id) VALUES ('saved');",
            )
            .unwrap();
        assert!(initialize(&mut connection).is_err());
        assert!(connection.is_autocommit());
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let objects: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_schema WHERE name LIKE 'node_unique%' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(objects, ["node_unique_insert"]);
        let nodes: i64 = connection
            .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(nodes, 1);
        connection
            .execute_batch("DROP TRIGGER node_unique_insert")
            .unwrap();
        initialize(&mut connection).unwrap();
        assert_eq!(constraints::list(&connection).unwrap().len(), 0);
    }

    #[test]
    fn entities_with_the_same_id_but_different_kinds_are_not_equal() {
        let mut database = Database::in_memory().unwrap();
        database
            .connection
            .execute_batch(
                "INSERT INTO nodes (id) VALUES ('same');
            INSERT INTO edges (id, edge_type, from_node, to_node)
                VALUES ('same', 'SELF', 'same', 'same');",
            )
            .unwrap();
        let result = database
            .execute("MATCH (n)-[r]->(n) RETURN n = r AS same, n <> r AS different")
            .unwrap();
        assert_eq!(
            result.rows,
            vec![vec![
                CypherValue::Boolean(false),
                CypherValue::Boolean(true)
            ]]
        );
    }

    #[test]
    fn indexed_property_lookups_probe_each_candidates_labels() {
        let database = Database::in_memory().unwrap();
        let Plan::Read(plan) =
            planner::compile("MATCH (n:Person:Member {id: 7}) RETURN n.id").unwrap()
        else {
            panic!("expected a read");
        };
        let PreparedPhysicalOperator::Sqlite(fragment) =
            plan.prepare(&Parameters::new()).unwrap().root
        else {
            panic!("expected a SQLite fragment");
        };
        let values = fragment
            .arguments
            .into_iter()
            .map(|argument| match argument {
                PreparedFragmentArgument::Value(value) => value,
                _ => panic!("unexpected runtime input"),
            })
            .collect::<Vec<_>>();
        let details = database
            .connection
            .prepare(&format!("EXPLAIN QUERY PLAN {}", fragment.sql))
            .unwrap()
            .query_map(params_from_iter(values.iter()), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(details.contains("node_properties_by_value"), "{details}");
        assert_eq!(
            details.matches("label=? AND node_id=?").count(),
            2,
            "{details}"
        );
        assert!(details.contains("label=? AND node_id=?"), "{details}");
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
                "SELECT count(*) FROM node_labels WHERE label = 'Sink'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let sources: i64 = database
            .connection
            .query_row(
                "SELECT count(*) FROM node_labels WHERE label = 'Source'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edges, 0);
        assert_eq!(sinks, 0);
        assert_eq!(sources, 2);
    }
}
