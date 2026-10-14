use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};

use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, params_from_iter,
};

use crate::{
    CypherValue, NodeValue, OpenMode, OpenOptions, Parameters, PathValue, QueryOptions,
    QueryResult, QuerySummary, RelationshipValue, Result,
    planner::{
        self, EntityKind, OutputShape, Plan, PreparedFragmentArgument, PreparedPhysicalOperator,
        PreparedReadPlan, PreparedSqlFragment, PreparedValueOperator, ReadPlan, ResolvedMutation,
        ResolvedMutationPlan, ResolvedPropertyTarget, ResolvedRowSource, ResolvedSegment,
        ResolvedStoredValue, RowLayout, SlotId, UnionKind,
    },
};

mod backup;
mod comparison;
pub(crate) mod compiled;
pub(crate) mod constraints;
pub(crate) mod cursor;
mod functions;
mod nested;
pub(crate) mod open;
mod resources;
#[cfg(test)]
mod storage_failures;
mod text;
pub(crate) mod transaction;
mod traversal;

use functions::register as register_functions;
use resources::ExecutionContext;

const APPLICATION_ID: i64 = 0x4352_4142; // "CRAB"
const SCHEMA_VERSION: i64 = 5;

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

// Version 5 retains unchanged property-index entries. Keep the historical
// schema above intact so all upgrade paths use this same atomic replacement.
const PROPERTY_INDEX_UPDATE_V5: &str = "
DROP TRIGGER node_properties_update;
CREATE TRIGGER node_properties_update AFTER UPDATE OF id, props ON nodes BEGIN
    DELETE FROM node_properties
        WHERE node_id = OLD.id
          AND (OLD.id IS NOT NEW.id OR key NOT IN (
              SELECT key FROM json_each(NEW.props) WHERE type <> 'null'
          ));
    INSERT INTO node_properties (node_id, key, value)
        SELECT NEW.id, key, value FROM json_each(NEW.props) WHERE type <> 'null'
        ON CONFLICT (node_id, key) DO UPDATE SET value = excluded.value
        WHERE node_properties.value IS NOT excluded.value
           OR typeof(node_properties.value) <> typeof(excluded.value);
END;
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

    /// Opens or creates a graph at a filesystem path with default [`OpenOptions`].
    /// Use [`Self::open_with_options`] to require an existing graph, open
    /// read-only, or configure lock waiting.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, &OpenOptions::default())
    }

    fn from_connection(mut connection: Connection) -> Result<Self> {
        connection.busy_timeout(OpenOptions::default().busy_timeout)?;
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
    /// A read-only database returns [`crate::Error::ReadOnly`]; contention for
    /// the writer returns [`crate::Error::Busy`] according to its opening options.
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
        self.ensure_writable()?;
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
            Plan::Read(plan) => self.execute_read(&plan, parameters, &context),
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
        plan: &ReadPlan,
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
        self.ensure_writable()?;
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
    plan: &ReadPlan,
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
        rows.push(decode_pending_row(row, &plan.output_shapes, context)?);
    }
    Ok(rows)
}

fn decode_pending_row(
    row: &rusqlite::Row<'_>,
    shapes: &[OutputShape],
    context: &ExecutionContext,
) -> Result<PendingRow> {
    decode_pending_columns(row, shapes.iter().copied().enumerate(), context)
}

fn decode_pending_columns(
    row: &rusqlite::Row<'_>,
    columns: impl IntoIterator<Item = (usize, OutputShape)>,
    context: &ExecutionContext,
) -> Result<PendingRow> {
    columns
        .into_iter()
        .map(|(index, shape)| {
            context.check()?;
            decode_pending_output(
                row.get_ref(index)?
                    .as_str()
                    .map_err(rusqlite::Error::from)?,
                shape,
            )
        })
        .collect()
}

fn finish_pending_row(row: PendingRow) -> Result<Vec<CypherValue>> {
    row.into_iter()
        .map(|value| match value {
            PendingValue::Materialized(value) => Ok(value),
            PendingValue::Path(_)
            | PendingValue::PathNodes(_)
            | PendingValue::PathRelationships(_) => Err(crate::Error::Data(
                "physical read plan omitted required batch hydration".into(),
            )),
        })
        .collect()
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
            let row = finish_pending_row(row)?;
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
    computed_targets: Vec<Vec<CypherValue>>,
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

    // Validate every computed target before deleting anything. Only decoded
    // graph values convey identity; property maps cannot impersonate entities.
    for row in computed_targets {
        for value in row {
            context.check()?;
            match value {
                CypherValue::Null => {}
                CypherValue::Node(node) => {
                    node_ids.insert(node.element_id);
                }
                CypherValue::Relationship(relationship) => {
                    relationship_ids.insert(relationship.element_id);
                }
                _ => {
                    return Err(crate::Error::Type(
                        "DELETE target must evaluate to a node, relationship, or null".into(),
                    ));
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
    targets: &PropertyTargets,
    update: impl Fn(usize, &mut serde_json::Map<String, serde_json::Value>),
    context: &ExecutionContext,
) -> Result<usize> {
    let mut total_changed = 0;
    for target_index in 0..rows.len() {
        context.check()?;
        let Some((kind, id)) = targets.identity(target_index, &rows[target_index])? else {
            continue;
        };
        let select_sql = match kind {
            EntityKind::Node => "SELECT props FROM nodes WHERE id = ?1",
            EntityKind::Relationship => "SELECT props FROM edges WHERE id = ?1",
        };
        let encoded = transaction
            .prepare_cached(select_sql)?
            .query_row(params![id], |record| record.get::<_, String>(0))?;
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
            transaction.prepare_cached(update_sql)?.execute(params![
                serde_json::Value::Object(properties.clone()).to_string(),
                id
            ])?;
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
    entity_lists: Vec<Option<Vec<Option<EntityValue>>>>,
    nested_values: Vec<Option<nested::NestedValue>>,
}

impl ExecutionRow {
    fn empty(layout: &RowLayout) -> Self {
        Self {
            entities: vec![None; layout.slots.len()],
            scalars: vec![None; layout.scalar_slots],
            entity_lists: vec![None; layout.scalar_slots],
            nested_values: vec![None; layout.scalar_slots],
        }
    }

    fn all_entities_mut(&mut self) -> impl Iterator<Item = &mut EntityValue> {
        self.entities
            .iter_mut()
            .flatten()
            .chain(self.entity_lists.iter_mut().flatten().flatten().flatten())
            .chain(
                self.nested_values
                    .iter_mut()
                    .flatten()
                    .flat_map(nested::NestedValue::entities_mut),
            )
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
                let id = transaction
                    .prepare_cached(
                        "INSERT INTO nodes (id, labels, props)
                     VALUES (lower(hex(randomblob(16))), json(?1), json(?2))
                     RETURNING id",
                    )?
                    .query_row(params![encoded_labels, properties.to_string()], |row| {
                        row.get::<_, String>(0)
                    })?;
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
                let id = transaction
                    .prepare_cached(
                        "INSERT INTO edges (id, edge_type, from_node, to_node, props)
                     VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, json(?4))
                     RETURNING id",
                    )?
                    .query_row(
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
        ResolvedMutation::SetProperty { target, key, value } => {
            let targets = property_targets(transaction, layout, rows, target, "SET", context)?;
            let values = row_values(transaction, layout, rows, value, false, context)?;
            summary.properties_set += update_properties(
                transaction,
                rows,
                &targets,
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
                &PropertyTargets::Bound(*slot),
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
                &PropertyTargets::Bound(*slot),
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
        ResolvedMutation::RemoveProperty { target, key } => {
            let targets = property_targets(transaction, layout, rows, target, "REMOVE", context)?;
            summary.properties_set += update_properties(
                transaction,
                rows,
                &targets,
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
        ResolvedMutation::Delete {
            targets,
            computed_targets,
            detach,
        } => {
            let values = match computed_targets {
                Some(plan) => {
                    let result = read_materialized_rows(
                        transaction,
                        layout,
                        rows,
                        plan.clone(),
                        false,
                        context,
                    )?;
                    if result.rows.len() != rows.len() {
                        return Err(crate::Error::Data(
                            "computed DELETE targets did not yield one result per row".into(),
                        ));
                    }
                    result.rows
                }
                None => Vec::new(),
            };
            delete_entities(
                transaction,
                rows,
                targets,
                values,
                *detach,
                summary,
                context,
            )?;
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

/// Direct bindings need no read; computed selections retain one identity per
/// input row, including duplicates and nulls. They are frozen before an item
/// writes, just like its stored values.
enum PropertyTargets {
    Bound(SlotId),
    PerRow(Vec<Option<(EntityKind, String)>>),
}

impl PropertyTargets {
    fn identity(&self, index: usize, row: &ExecutionRow) -> Result<Option<(EntityKind, String)>> {
        match self {
            Self::Bound(slot) if row.binds(*slot) => row.entity_identity(*slot).map(Some),
            Self::Bound(_) => Ok(None),
            Self::PerRow(targets) => Ok(targets[index].clone()),
        }
    }
}

fn property_targets(
    transaction: &Transaction<'_>,
    layout: &RowLayout,
    rows: &[ExecutionRow],
    target: &ResolvedPropertyTarget,
    clause: &str,
    context: &ExecutionContext,
) -> Result<PropertyTargets> {
    let plan = match target {
        ResolvedPropertyTarget::Bound(slot) => return Ok(PropertyTargets::Bound(*slot)),
        ResolvedPropertyTarget::Computed(plan) => plan.clone(),
    };
    let result = read_materialized_rows(transaction, layout, rows, plan, false, context)?;
    if result.rows.len() != rows.len() {
        return Err(crate::Error::Data(
            "computed property targets did not yield one result per row".into(),
        ));
    }
    result
        .rows
        .into_iter()
        .map(|mut row| {
            context.check()?;
            match row.remove(0) {
                CypherValue::Null => Ok(None),
                CypherValue::Node(node) => Ok(Some((EntityKind::Node, node.element_id))),
                CypherValue::Relationship(edge) => {
                    Ok(Some((EntityKind::Relationship, edge.element_id)))
                }
                _ => Err(crate::Error::Type(format!(
                    "{clause} target must evaluate to a node, relationship, or null"
                ))),
            }
        })
        .collect::<Result<Vec<_>>>()
        .map(PropertyTargets::PerRow)
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
            let records = std::iter::once(index.to_string())
                .chain(row.entities.iter().map(|entity| {
                    entity
                        .as_ref()
                        .map_or_else(|| "null".to_owned(), encode_entity)
                }))
                .chain(row.scalars.iter().enumerate().map(|(slot, scalar)| {
                    if let Some(value) = &row.nested_values[slot] {
                        return value.encode().to_string();
                    }
                    match &row.entity_lists[slot] {
                        Some(entities) => format!(
                            "[{}]",
                            entities
                                .iter()
                                .map(|entity| entity
                                    .as_ref()
                                    .map_or_else(|| "null".to_owned(), encode_entity))
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
            if layout.nested_slots.contains(slot)
                && let Some(encoded) = destination.as_deref()
            {
                execution_row.nested_values[slot.0] = Some(nested::NestedValue::decode(encoded)?);
                *destination = None;
            }
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

fn decode_entity_list(encoded: &str, kind: EntityKind) -> Result<Vec<Option<EntityValue>>> {
    let values: Vec<serde_json::Value> = serde_json::from_str(encoded)
        .map_err(|error| crate::Error::Data(format!("malformed entity list: {error}")))?;
    values
        .into_iter()
        .map(|value| {
            if value.is_null() {
                Ok(None)
            } else {
                decode_entity(&value.to_string(), kind).map(Some)
            }
        })
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
    /// Admit a final row before cloning any repeated graph snapshots.
    fn finish_row(
        &self,
        row: PendingRow,
        result_bytes: Option<&mut usize>,
        context: &ExecutionContext,
    ) -> Result<Vec<CypherValue>> {
        context.check()?;
        if let Some(bytes) = result_bytes.filter(|_| context.counts_result_bytes()) {
            context.admit_result_bytes(8, bytes)?;
            for value in &row {
                context.admit_result_bytes(self.pending_bytes(value, context)?, bytes)?;
            }
        }
        row.into_iter()
            .map(|value| match value {
                PendingValue::Materialized(value) => Ok(value),
                PendingValue::Path(identity) => {
                    self.hydrate(identity, context).map(CypherValue::Path)
                }
                PendingValue::PathNodes(ids) => self.hydrate_node_list(ids, context),
                PendingValue::PathRelationships(ids) => {
                    self.hydrate_relationship_list(ids, context)
                }
            })
            .collect()
    }

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
        .map(|row| hydrator.finish_row(row, is_output.then_some(&mut result_bytes), context))
        .collect()
}

fn decode_pending_output(encoded: &str, shape: OutputShape) -> Result<PendingValue> {
    match shape {
        OutputShape::Nested => nested::NestedValue::decode(encoded)
            .map(|value| PendingValue::Materialized(value.value())),
        OutputShape::Json => {
            let value = serde_json::from_str::<serde_json::Value>(encoded).map_err(|error| {
                crate::Error::Data(format!(
                    "query returned malformed JSON `{encoded}`: {error}"
                ))
            })?;
            CypherValue::try_from(value).map(PendingValue::Materialized)
        }
        OutputShape::Entity(_) | OutputShape::EntityList(_) if encoded == "null" => {
            Ok(PendingValue::Materialized(CypherValue::Null))
        }
        OutputShape::Entity(kind) => decode_entity(encoded, kind)
            .map(|value| PendingValue::Materialized(value.to_cypher_value())),
        OutputShape::EntityList(kind) => {
            let values = decode_entity_list(encoded, kind)?
                .into_iter()
                .map(|entity| entity.map_or(CypherValue::Null, |entity| entity.to_cypher_value()))
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
    initialize_with_mode(connection, OpenMode::OpenOrCreate)
}

fn initialize_with_mode(connection: &mut Connection, mode: OpenMode) -> Result<()> {
    constraints::register(connection)?;
    // Foreign-key enforcement is connection-local and cannot be changed while
    // a transaction is active, so enable it before schema initialization.
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;

    // Inspect all metadata and validate under one read snapshot. Opening a
    // current graph needs no writer reservation, including on read-only handles.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let version = inspect_schema(&transaction, mode)?;
    if version == SCHEMA_VERSION {
        validate_schema(&transaction)?;
        transaction.commit()?;
        return Ok(());
    }
    transaction.commit()?;
    upgrade_schema(connection, mode)
}

/// Classify a file without modifying it. Zero denotes an uninitialized file.
fn inspect_schema(connection: &Connection, mode: OpenMode) -> Result<i64> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let schema_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if application_id == APPLICATION_ID {
        if matches!(schema_version, 1..=4) {
            if mode == OpenMode::ReadOnly {
                return Err(crate::Error::Data(format!(
                    "Crabnet schema version {schema_version} requires migration; open read/write first"
                )));
            }
            return Ok(schema_version);
        }
        if schema_version != SCHEMA_VERSION {
            return Err(crate::Error::Data(format!(
                "unsupported Crabnet schema version {schema_version}; expected {SCHEMA_VERSION}"
            )));
        }
        return Ok(schema_version);
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

    if mode != OpenMode::OpenOrCreate {
        return Err(crate::Error::Data(
            "expected an existing Crabnet graph; file is uninitialized".into(),
        ));
    }
    Ok(0)
}

/// Install labels, property indexes, uniqueness, and incremental maintenance.
/// Roll back schema changes and the version marker if validation/backfill fails.
fn upgrade_schema(connection: &mut Connection, mode: OpenMode) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another opener may have initialized/upgraded the file while we waited
    // for the writer. Never apply DDL using the earlier snapshot's version.
    let from = inspect_schema(&transaction, mode)?;
    if from == SCHEMA_VERSION {
        validate_schema(&transaction)?;
        transaction.commit()?;
        return Ok(());
    }
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
        3 | 4 => {}
        _ => unreachable!("only known schema versions can be upgraded"),
    }
    if from < 3 {
        transaction.execute_batch(PROPERTY_INDEX_SCHEMA)?;
    }
    if from < 4 {
        transaction.execute_batch(constraints::SCHEMA)?;
    }
    transaction.execute_batch(PROPERTY_INDEX_UPDATE_V5)?;
    transaction.execute_batch(constraints::UPDATE_V5)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    validate_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
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
mod tests;
