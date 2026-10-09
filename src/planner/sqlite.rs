use std::collections::{HashMap, HashSet};

use rusqlite::types::Value as SqlValue;
#[cfg(test)]
use serde_json::Value;

use crate::{Error, Parameters, Result};

use super::logical::{
    AggregateKind, BinaryOp, CaseAlternative, Direction, EntityAccess, EntityKind, Expr, Input,
    ListQuantifier, ListVariableId, MatchOperator, MatchPlan, PathCollectionKind, PathFunctionKind,
    PathId, PathSegment, Projection, ROW_INDEX_SLOT, ReadPipeline, ReadStage, RelationshipUse,
    ScalarFunction, ScalarSlotId, SlotId, SortDirection, TraversalId, UnaryOp, UnwindBinding,
    UnwindStage, ValueExpr, WithBinding, WithStage,
};
use super::physical::{
    FragmentArgument, NodeDomainSpec, OutputShape, PhysicalOperator, RuntimeSlot, ShortestMode,
    SqlFragment, SqlParameter, TrailExpansionSpec, ValueOperator,
};
#[cfg(test)]
use super::physical::{PreparedValueOperator, TrailDirection};

#[derive(Clone, Debug)]
pub(crate) struct CompiledRead {
    pub(crate) fragment: SqlFragment,
    distinct: bool,
    post_offset: Option<ValueExpr>,
    post_limit: Option<ValueExpr>,
}

impl std::ops::Deref for CompiledRead {
    type Target = SqlFragment;

    fn deref(&self) -> &Self::Target {
        &self.fragment
    }
}

impl CompiledRead {
    #[cfg(test)]
    pub(crate) fn distinct(&self) -> bool {
        self.distinct
    }

    pub(crate) fn has_path_identity_output(&self) -> bool {
        self.fragment.has_path_identity_output()
    }

    #[cfg(test)]
    pub(crate) fn render_physical(&self, batch_hydrate_paths: bool) -> String {
        let hydrate = batch_hydrate_paths && self.has_path_identity_output();
        let mut operator = self.clone().into_operator();
        if hydrate {
            operator = PhysicalOperator::BatchHydrate {
                input: Box::new(operator),
            };
        }
        operator.render()
    }

    #[cfg(test)]
    fn resolve_bindings(
        &self,
        parameters: &Parameters,
        mut materialize: impl FnMut(&PreparedValueOperator) -> Result<SqlValue>,
    ) -> Result<Vec<SqlValue>> {
        fn sqlite_fragment(
            operator: super::physical::PreparedPhysicalOperator,
        ) -> Result<super::physical::PreparedSqlFragment> {
            match operator {
                super::physical::PreparedPhysicalOperator::Sqlite(fragment) => Ok(fragment),
                super::physical::PreparedPhysicalOperator::SemanticDistinct { input }
                | super::physical::PreparedPhysicalOperator::Paginate { input, .. } => {
                    sqlite_fragment(*input)
                }
                super::physical::PreparedPhysicalOperator::Union { .. }
                | super::physical::PreparedPhysicalOperator::BatchHydrate { .. } => Err(
                    Error::Data("compiled read test helper expected one SQL fragment".into()),
                ),
            }
        }

        let fragment = sqlite_fragment(self.clone().into_operator().prepare(parameters)?)?;
        let mut runtime_values = HashMap::new();
        for input in fragment.inputs {
            let output = input.output();
            runtime_values.insert(output, materialize(&input)?);
        }
        fragment
            .arguments
            .into_iter()
            .map(|argument| match argument {
                super::physical::PreparedFragmentArgument::Value(value) => Ok(value),
                super::physical::PreparedFragmentArgument::Runtime(slot) => {
                    runtime_values.remove(&slot).ok_or_else(|| {
                        Error::Data(format!(
                            "compiled read test helper is missing runtime slot %r{}",
                            slot.0
                        ))
                    })
                }
            })
            .collect()
    }

    pub(crate) fn into_operator(self) -> PhysicalOperator {
        let mut operator = PhysicalOperator::Sqlite(self.fragment);
        if self.distinct {
            operator = PhysicalOperator::SemanticDistinct {
                input: Box::new(operator),
            };
            if self.post_offset.is_some() || self.post_limit.is_some() {
                operator = PhysicalOperator::Paginate {
                    input: Box::new(operator),
                    offset: self.post_offset,
                    limit: self.post_limit,
                };
            }
        }
        operator
    }
}

/// A read whose rows feed mutations: one encoded entity column per bound
/// entity slot followed by one JSON column per bound scalar slot.
pub(crate) struct RowSource {
    pub(crate) sql: String,
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    pub(crate) scalars: Vec<ScalarSlotId>,
    bindings: Vec<SqlParameter>,
}

pub(crate) struct ResolvedRowSource {
    pub(crate) sql: String,
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    pub(crate) scalars: Vec<ScalarSlotId>,
    pub(crate) bindings: Vec<SqlValue>,
}

impl RowSource {
    pub(crate) fn resolve(self, parameters: &Parameters) -> Result<ResolvedRowSource> {
        Ok(ResolvedRowSource {
            bindings: resolve_bindings(&self.bindings, parameters)?,
            sql: self.sql,
            slots: self.slots,
            scalars: self.scalars,
        })
    }
}

fn resolve_bindings(bindings: &[SqlParameter], parameters: &Parameters) -> Result<Vec<SqlValue>> {
    bindings
        .iter()
        .map(|binding| binding.resolve(parameters))
        .collect()
}

/// The temporary table a mutation pipeline fills with its materialized rows
/// before running a post-mutation `RETURN` over them.
pub(crate) const ROWS_TABLE: &str = "_crabnet_rows";

pub(crate) fn compile_read(input: &Input, projection: &Projection) -> Result<CompiledRead> {
    compile_read_with_probes(input, projection, false)
}

/// Unlike a read filter, MERGE must evaluate its property keys even when no
/// nodes match: they are needed to create the unmatched pattern. Only this
/// scope may probe fallible computed keys before scanning candidates.
pub(crate) fn compile_merge_read(input: &Input, projection: &Projection) -> Result<CompiledRead> {
    compile_read_with_probes(input, projection, true)
}

fn compile_read_with_probes(
    input: &Input,
    projection: &Projection,
    computed_probes: bool,
) -> Result<CompiledRead> {
    let mut compiler = Compiler::new();
    compiler.computed_probes = computed_probes;
    compiler.compile_input(input)?;

    let aggregating = projection.expressions.iter().any(Expr::contains_aggregate);
    if aggregating {
        compiler.compile_aggregation(
            &projection.expressions,
            &projection
                .order_by
                .iter()
                .map(|item| &item.expression)
                .collect::<Vec<_>>(),
        )?;
    }
    let outputs = projection
        .expressions
        .iter()
        .enumerate()
        .map(|(index, expression)| {
            compiler
                .output_expression(expression)
                .map(|(sql, shape)| (format!("{sql} AS c{index}"), shape))
        })
        .collect::<Result<Vec<_>>>()?;
    let (select, output_shapes): (Vec<_>, Vec<_>) = outputs.into_iter().unzip();
    let order_items = if !projection.order_by.is_empty() {
        let items = projection
            .order_by
            .iter()
            .map(|item| {
                let key = compiler.order_key(&item.expression)?;
                Ok(order_expression(&key, item.direction))
            })
            .collect::<Result<Vec<_>>>()?;
        Some(items.join(", "))
    } else if !aggregating && let Some(ordering) = &compiler.ordering {
        Some(ordering.clone())
    } else {
        None
    };
    // ORDER BY can introduce per-row sources such as rand(), too.
    let mut sql = compiler.select_sql(&select);
    if let Some(ordering) = order_items {
        sql.push_str(" ORDER BY ");
        sql.push_str(&ordering);
    }

    if !projection.distinct && (projection.limit.is_some() || projection.offset.is_some()) {
        sql.push_str(" LIMIT ");
        match &projection.limit {
            Some(limit) => sql.push_str(&compiler.push_binding(SqlParameter::Page(limit.clone()))),
            None => sql.push_str("-1"),
        }
    }
    if !projection.distinct
        && let Some(offset) = &projection.offset
    {
        sql.push_str(" OFFSET ");
        sql.push_str(&compiler.push_binding(SqlParameter::Page(offset.clone())));
    }

    sql = compiler.with_ctes(sql);

    debug_assert_eq!(projection.columns.len(), output_shapes.len());
    Ok(CompiledRead {
        fragment: SqlFragment {
            sql,
            columns: projection.columns.clone(),
            output_shapes,
            arguments: compiler.bindings,
            inputs: compiler.inputs,
        },
        distinct: projection.distinct,
        post_offset: projection
            .distinct
            .then(|| projection.offset.clone())
            .flatten(),
        post_limit: projection
            .distinct
            .then(|| projection.limit.clone())
            .flatten(),
    })
}

/// Compiles the read that feeds a mutation step: the input's stages, then
/// every entity and scalar binding they leave live, in the order the stages
/// produce rows.
pub(crate) fn compile_row_source(input: &Input) -> Result<RowSource> {
    let plans: Vec<&MatchPlan> = match input {
        Input::Unit => Vec::new(),
        Input::Match(plan) => vec![plan],
        Input::Pipeline(ReadPipeline { stages }) | Input::Rows { stages, .. } => stages
            .iter()
            .filter_map(|stage| match stage {
                ReadStage::Match(plan) | ReadStage::OptionalMatch(plan) => Some(plan),
                ReadStage::With(_) | ReadStage::Unwind(_) => None,
            })
            .collect(),
    };
    if plans.iter().any(|plan| plan.has_variable_length()) {
        return Err(Error::Unsupported(
            "writes fed by variable-length traversal".into(),
        ));
    }
    let mut compiler = Compiler::new();
    compiler.compile_input(input)?;
    if !compiler.inputs.is_empty() {
        return Err(Error::Unsupported(
            "writes fed by variable-length traversal".into(),
        ));
    }

    let mut slots = compiler
        .entities
        .iter()
        .map(|(slot, source)| (*slot, source.kind()))
        .collect::<Vec<_>>();
    slots.sort_by_key(|(slot, _)| *slot);
    let mut scalars = compiler
        .scalars
        .keys()
        .copied()
        .filter(|slot| *slot != ROW_INDEX_SLOT)
        .collect::<Vec<_>>();
    scalars.sort();

    let mut select = slots
        .iter()
        .map(|(slot, kind)| compiler.encoded_entity(*slot, *kind))
        .collect::<Result<Vec<_>>>()?;
    select.extend(scalars.iter().map(|slot| {
        compiler
            .scalars
            .get(slot)
            .cloned()
            .expect("collected scalar slot has a source")
    }));
    if select.is_empty() {
        // A source with no live bindings still yields one row per input row.
        select.push("1".to_owned());
    }
    let mut sql = compiler.select_sql(&select);
    if let Some(ordering) = &compiler.ordering {
        sql.push_str(" ORDER BY ");
        sql.push_str(ordering);
    }
    let sql = compiler.with_ctes(sql);

    let bindings = compiler
        .bindings
        .into_iter()
        .map(|argument| match argument {
            FragmentArgument::Static(parameter) => Ok(parameter),
            FragmentArgument::Runtime(_) => Err(Error::Unsupported(
                "writes fed by variable-length traversal".into(),
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RowSource {
        sql,
        slots,
        scalars,
        bindings,
    })
}

#[derive(Clone, Debug)]
enum EntitySource {
    Table { alias: String, kind: EntityKind },
    Encoded { sql: String, kind: EntityKind },
}

impl EntitySource {
    fn kind(&self) -> EntityKind {
        match self {
            Self::Table { kind, .. } | Self::Encoded { kind, .. } => *kind,
        }
    }
}

/// Alias of the `nodes` row that starts each SQLite recursive walk.
const WALK_SEED: &str = "seed";
const PROPERTY_INDEX: &str = "_crabnet_property";

/// Node slots scanned by `operators` that also carry an equality on one of
/// their properties, which the property index can answer.
fn property_indexed_nodes(
    operators: &[MatchOperator],
    computed: Option<&HashSet<SlotId>>,
) -> HashSet<SlotId> {
    let scanned = operators
        .iter()
        .filter_map(|operator| match operator {
            MatchOperator::ScanNode { slot } => Some(*slot),
            _ => None,
        })
        .collect::<HashSet<_>>();
    operators
        .iter()
        .flat_map(|operator| match operator {
            MatchOperator::PropertyEquals { slot, .. } => vec![*slot],
            MatchOperator::Filter(expression) => indexable_equalities(expression, computed)
                .into_iter()
                .map(|(slot, _, _)| slot)
                .collect(),
            _ => Vec::new(),
        })
        .filter(|slot| scanned.contains(slot))
        .collect()
}

/// The `entity.key = expected` conjuncts at the top of a filter whose other
/// side is cheap, deterministic, and independent of that entity, so it can be
/// compiled a second time as a property-index probe.
fn indexable_equalities<'a>(
    expression: &'a Expr,
    computed: Option<&HashSet<SlotId>>,
) -> Vec<(SlotId, &'a str, &'a Expr)> {
    match expression {
        Expr::Binary {
            left,
            operator: BinaryOp::And,
            right,
        } => {
            let mut equalities = indexable_equalities(left, computed);
            equalities.extend(indexable_equalities(right, computed));
            equalities
        }
        Expr::Binary {
            left,
            operator: BinaryOp::Equal,
            right,
        } => property_probe(left, right, computed)
            .map(|(slot, key)| (slot, key, right.as_ref()))
            .or_else(|| {
                property_probe(right, left, computed).map(|(slot, key)| (slot, key, left.as_ref()))
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn property_probe<'a>(
    property: &'a Expr,
    expected: &Expr,
    computed: Option<&HashSet<SlotId>>,
) -> Option<(SlotId, &'a str)> {
    let Expr::Property(slot, key) = property else {
        return None;
    };
    let eligible = match expected {
        Expr::Value(_) | Expr::Scalar(_) => true,
        Expr::Property(other, _) => other != slot,
        _ => computed.is_some_and(|bound| probe_operand(expected, bound, &mut 32)),
    };
    eligible.then_some((*slot, key.as_str()))
}

/// Probes duplicate their operand beside the exact comparison. Admit only
/// small scalar trees whose entity leaves were bound before the matcher.
/// In particular, do not duplicate random calls, traversal, aggregates, or
/// collection iteration. An already materialized scalar (even a projected
/// random value) is safe to read again.
fn probe_operand(expression: &Expr, bound: &HashSet<SlotId>, remaining: &mut usize) -> bool {
    let Some(rest) = remaining.checked_sub(1) else {
        return false;
    };
    *remaining = rest;
    match expression {
        Expr::Value(_) | Expr::Scalar(_) => true,
        Expr::Property(slot, _) | Expr::ElementId(slot) | Expr::RelationshipType(slot) => {
            bound.contains(slot)
        }
        Expr::MapProperty { .. }
        | Expr::Index { .. }
        | Expr::Unary { .. }
        | Expr::Binary { .. }
        | Expr::IsNull { .. }
        | Expr::Size(_)
        | Expr::List(_)
        | Expr::Map(_)
        | Expr::Case { .. }
        | Expr::Function {
            function:
                ScalarFunction::Coalesce
                | ScalarFunction::ToInteger
                | ScalarFunction::ToFloat
                | ScalarFunction::ToString
                | ScalarFunction::ToBoolean
                | ScalarFunction::ToLower
                | ScalarFunction::ToUpper
                | ScalarFunction::Trim
                | ScalarFunction::LTrim
                | ScalarFunction::RTrim
                | ScalarFunction::Abs
                | ScalarFunction::Sign
                | ScalarFunction::Ceil
                | ScalarFunction::Floor
                | ScalarFunction::Round,
            ..
        } => expression
            .children()
            .into_iter()
            .all(|child| probe_operand(child, bound, remaining)),
        _ => false,
    }
}

/// Keep label-only scans indexable. Once a property probe supplies candidate
/// IDs, test each candidate's labels directly instead of materializing every
/// node with the label.
fn node_label_predicate(id: &str, label: &str, property_indexed: bool) -> String {
    if property_indexed {
        format!("EXISTS (SELECT 1 FROM node_labels WHERE label = {label} AND node_id = {id})")
    } else {
        format!("{id} IN (SELECT node_id FROM node_labels WHERE label = {label})")
    }
}

struct VariableLengthSpec<'a> {
    traversal: TraversalId,
    left: SlotId,
    right: SlotId,
    direction: Direction,
    minimum: usize,
    maximum: usize,
    relationship_type: Option<&'a str>,
    properties: &'a [(String, ValueExpr)],
    /// Label and inline-property predicates on the start endpoint, already
    /// rendered against the recursive seed alias.
    seed_filters: Vec<String>,
}

struct HybridVariableLengthSpec<'a> {
    traversal: TraversalId,
    left: SlotId,
    right: SlotId,
    direction: Direction,
    minimum: usize,
    maximum: Option<usize>,
    kind: HybridExpansionKind,
    relationship_type: Option<&'a str>,
    properties: &'a [(String, ValueExpr)],
    start_domain: Option<NodeDomainSpec>,
    end_domain: Option<NodeDomainSpec>,
}

#[derive(Clone, Copy, Debug)]
enum HybridExpansionKind {
    Trails,
    Shortest(ShortestMode),
}

#[derive(Clone, Debug)]
struct TraversalSource {
    edge_ids: String,
    node_ids: String,
}

/// A compiled scalar expression: its SQL value and how to name its JSON type.
#[derive(Clone, Debug)]
struct Scalar {
    value: String,
    kind: ScalarType,
}

#[derive(Clone, Debug)]
enum ScalarType {
    Encoded,
    /// Type SQL that stands on its own, such as `json_type(...)` or a
    /// constant like `'integer'`.
    Sql(String),
    /// A three-valued SQL boolean whose type name follows from its value.
    Boolean,
    /// A value of one fixed JSON type when present and null otherwise, such
    /// as `toInteger()`'s integer-or-null.
    Nullable(&'static str),
}

impl Scalar {
    fn sql(value: impl Into<String>, value_type: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            kind: ScalarType::Sql(value_type.into()),
        }
    }

    fn boolean(value: String) -> Self {
        Self {
            value,
            kind: ScalarType::Boolean,
        }
    }

    fn nullable(value: String, type_name: &'static str) -> Self {
        Self {
            value,
            kind: ScalarType::Nullable(type_name),
        }
    }

    /// The type SQL, given SQL that yields this scalar's value. Derived
    /// kinds are computed from `value` rather than from the scalar's own
    /// value text, so a scope can compute the value once and type it by name.
    fn type_of(&self, value: &str) -> String {
        match &self.kind {
            ScalarType::Encoded => format!("json_type({value}, '$')"),
            ScalarType::Sql(sql) => sql.clone(),
            ScalarType::Boolean => boolean_type(value),
            ScalarType::Nullable(type_name) => {
                format!("(CASE WHEN ({value}) IS NULL THEN 'null' ELSE '{type_name}' END)")
            }
        }
    }

    fn into_parts(self) -> (String, String) {
        let value_type = self.type_of(&self.value);
        let value = self.raw_value(&self.value);
        (value, value_type)
    }

    fn raw_value(&self, value: &str) -> String {
        if matches!(self.kind, ScalarType::Encoded) {
            format!("json_extract({value}, '$')")
        } else {
            value.to_owned()
        }
    }
}

impl From<(String, String)> for Scalar {
    fn from((value, value_type): (String, String)) -> Self {
        Self::sql(value, value_type)
    }
}

struct Compiler {
    computed_probes: bool,
    entities: HashMap<SlotId, EntitySource>,
    scalars: HashMap<ScalarSlotId, String>,
    // Materialized grouping keys/aggregates or projections visible to
    // DISTINCT modifiers. These substitutions last for one projection.
    projected_scalars: Vec<(Expr, String)>,
    // Value and type SQL for each list-bound variable currently in scope.
    list_elements: HashMap<ListVariableId, (String, String)>,
    // Scalar-subquery scopes opened so far; each gets unique column names.
    scopes: usize,
    traversals: HashMap<TraversalId, TraversalSource>,
    paths: HashMap<PathId, String>,
    scanned: Vec<(SlotId, EntityKind)>,
    from: Vec<String>,
    // One `json_each(json_array(random()))` source per `rand()` call site,
    // joined after the row sources so each row draws a fresh value that the
    // value and type SQL then share by column name.
    random_sources: Vec<String>,
    // Sources captured for the `json_each` body being compiled (a list
    // comprehension or quantifier), innermost last.
    local_sources: Vec<Vec<String>>,
    // Inside an OPTIONAL MATCH predicate the joined sources are fixed, so
    // `rand()` falls back to an inline `random()` there.
    inline_random: bool,
    filters: Vec<String>,
    ordering: Option<String>,
    bindings: Vec<FragmentArgument>,
    inputs: Vec<ValueOperator>,
    ctes: Vec<(String, String)>,
    has_recursive_cte: bool,
}

impl Compiler {
    fn new() -> Self {
        Self {
            computed_probes: false,
            entities: HashMap::new(),
            scalars: HashMap::new(),
            projected_scalars: Vec::new(),
            list_elements: HashMap::new(),
            scopes: 0,
            traversals: HashMap::new(),
            paths: HashMap::new(),
            scanned: Vec::new(),
            from: Vec::new(),
            random_sources: Vec::new(),
            local_sources: Vec::new(),
            inline_random: false,
            filters: Vec::new(),
            inputs: Vec::new(),
            ordering: None,
            bindings: Vec::new(),
            ctes: Vec::new(),
            has_recursive_cte: false,
        }
    }

    /// Reads materialized mutation rows from `ROWS_TABLE`, whose column
    /// `e{slot}` holds each slot's encoded entity record (NULL when the slot
    /// is unbound in that row). Rows keep their input order unless the
    /// projection orders them itself.
    fn compile_rows(&mut self, slots: &[(SlotId, EntityKind)], scalars: &[ScalarSlotId]) {
        let relation = "mr";
        for (slot, kind) in slots {
            self.entities.insert(
                *slot,
                EntitySource::Encoded {
                    sql: format!("{relation}.e{}", slot.0),
                    kind: *kind,
                },
            );
        }
        for slot in scalars {
            self.scalars
                .insert(*slot, format!("{relation}.s{}", slot.0));
        }
        self.scalars.insert(ROW_INDEX_SLOT, format!("{relation}.k"));
        self.from.push(format!("{ROWS_TABLE} AS {relation}"));
        self.ordering = Some(format!("{relation}.rowid"));
    }

    /// Compiles a pattern predicate as a correlated `EXISTS` subquery: the
    /// pattern's fresh entities are scanned inside it while its bound
    /// entities refer to the enclosing query's sources.
    fn exists_subquery(
        &mut self,
        pattern: &MatchPlan,
        local: &[(SlotId, EntityKind)],
        bindings: &[(SlotId, EntityKind, Expr)],
    ) -> Result<String> {
        let sources = bindings
            .iter()
            .map(|(slot, kind, value)| {
                Ok((
                    *slot,
                    EntitySource::Encoded {
                        sql: self.output_expression(value)?.0,
                        kind: *kind,
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let previous = sources
            .into_iter()
            .map(|(slot, source)| (slot, self.entities.insert(slot, source)))
            .collect::<Vec<_>>();
        let saved_from = std::mem::take(&mut self.from);
        let saved_filters = std::mem::take(&mut self.filters);
        let saved_scanned = std::mem::take(&mut self.scanned);
        let saved_ordering = self.ordering.take();
        // A read over materialized rows registers every slot; the predicate's
        // own slots are never bound there and must be scanned afresh.
        for (slot, _) in local {
            self.entities.remove(slot);
        }
        let compiled = self.compile_match(pattern);
        let from = std::mem::replace(&mut self.from, saved_from);
        let filters = std::mem::replace(&mut self.filters, saved_filters);
        self.scanned = saved_scanned;
        self.ordering = saved_ordering;
        for (slot, _) in local {
            self.entities.remove(slot);
        }
        for (slot, source) in previous {
            match source {
                Some(source) => {
                    self.entities.insert(slot, source);
                }
                None => {
                    self.entities.remove(&slot);
                }
            }
        }
        compiled?;
        // If every node and relationship is already bound, the predicate
        // only checks endpoints/labels/properties and needs no table scan.
        let mut sql = "EXISTS (SELECT 1".to_owned();
        if !from.is_empty() {
            sql.push_str(" FROM ");
            sql.push_str(&from.join(", "));
        }
        if !filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&filters.join(" AND "));
        }
        sql.push(')');
        Ok(sql)
    }

    fn compile_input(&mut self, input: &Input) -> Result<()> {
        match input {
            Input::Unit => Ok(()),
            Input::Rows {
                slots,
                scalars,
                stages,
            } => {
                self.compile_rows(slots, scalars);
                self.compile_stages(stages)
            }
            Input::Match(plan) => self.compile_match(plan),
            Input::Pipeline(pipeline) => self.compile_stages(&pipeline.stages),
        }
    }

    fn compile_stages(&mut self, stages: &[ReadStage]) -> Result<()> {
        for stage in stages {
            match stage {
                ReadStage::Match(plan) => self.compile_match(plan)?,
                ReadStage::OptionalMatch(plan) => self.compile_optional_match(plan)?,
                ReadStage::With(stage) => self.compile_with(stage)?,
                ReadStage::Unwind(stage) => self.compile_unwind(stage)?,
            }
        }
        Ok(())
    }

    fn compile_match(&mut self, plan: &MatchPlan) -> Result<()> {
        let bound = self
            .computed_probes
            .then(|| self.entities.keys().copied().collect());
        let indexed = property_indexed_nodes(&plan.operators, bound.as_ref());
        // One probe per node is enough to drive its lookup; the remaining
        // equalities filter the probed nodes.
        let mut probed = HashSet::new();
        for operator in &plan.operators {
            match operator {
                MatchOperator::ScanNode { slot } => {
                    let alias = format!("n{}", slot.0);
                    self.insert_scan(*slot, EntityKind::Node, alias.clone())?;
                    self.from.push(format!("nodes AS {alias}"));
                }
                MatchOperator::ScanRelationship { slot } => {
                    let alias = format!("r{}", slot.0);
                    self.insert_scan(*slot, EntityKind::Relationship, alias.clone())?;
                    self.from.push(format!("edges AS {alias}"));
                }
                MatchOperator::HasLabel { slot, label } => {
                    let value = self.push_binding(SqlParameter::Text(label.clone()));
                    let test = if indexed.contains(slot) {
                        node_label_predicate(&self.entity_field(*slot, "id")?, &value, true)
                    } else {
                        self.label_test(*slot, &value)?
                    };
                    self.filters.push(test);
                }
                MatchOperator::HasRelationshipType {
                    slot,
                    relationship_type,
                } => {
                    let type_sql = self.entity_field(*slot, "type")?;
                    let value = self.push_binding(SqlParameter::Text(relationship_type.clone()));
                    self.filters.push(format!("{type_sql} = {value}"));
                }
                MatchOperator::PropertyEquals { slot, key, value } => {
                    if indexed.contains(slot) && probed.insert(*slot) {
                        let id = self.entity_field(*slot, "id")?;
                        let (expected, expected_type) = self.probe_value(value);
                        let prefilter =
                            self.property_index_prefilter(&id, key, (&expected, &expected_type));
                        self.filters.push(prefilter);
                    }
                    let (property, property_type) = self.entity_property_parts(*slot, key)?;
                    let comparison = self.typed_value_comparison(
                        property,
                        property_type,
                        value.clone(),
                        BinaryOp::Equal,
                    );
                    self.filters.push(comparison);
                }
                MatchOperator::ExpandRelationship {
                    relationship,
                    left,
                    right,
                    direction,
                } => {
                    let relationship_from = self.entity_field(*relationship, "from")?;
                    let relationship_to = self.entity_field(*relationship, "to")?;
                    let left = self.entity_field(*left, "id")?;
                    let right = self.entity_field(*right, "id")?;
                    let forward =
                        format!("({relationship_from} = {left} AND {relationship_to} = {right})");
                    let reverse =
                        format!("({relationship_from} = {right} AND {relationship_to} = {left})");
                    self.filters.push(match direction {
                        Direction::Right => forward,
                        Direction::Left => reverse,
                        Direction::Undirected => format!("({forward} OR {reverse})"),
                    });
                }
                MatchOperator::ExpandVariableLength {
                    traversal,
                    left,
                    right,
                    direction,
                    minimum,
                    maximum,
                    relationship_type,
                    properties,
                } => {
                    let seed_filters =
                        self.endpoint_constraint_filters(&plan.operators, *left, WALK_SEED);
                    self.compile_variable_length(VariableLengthSpec {
                        traversal: *traversal,
                        left: *left,
                        right: *right,
                        direction: *direction,
                        minimum: *minimum,
                        maximum: *maximum,
                        relationship_type: relationship_type.as_deref(),
                        properties,
                        seed_filters,
                    })?;
                }
                MatchOperator::ExpandHybridVariableLength {
                    traversal,
                    left,
                    right,
                    direction,
                    minimum,
                    maximum,
                    relationship_type,
                    properties,
                } => {
                    self.compile_hybrid_variable_length(HybridVariableLengthSpec {
                        traversal: *traversal,
                        left: *left,
                        right: *right,
                        direction: *direction,
                        minimum: *minimum,
                        maximum: *maximum,
                        kind: HybridExpansionKind::Trails,
                        relationship_type: relationship_type.as_deref(),
                        properties,
                        start_domain: Self::compile_node_domain(&plan.operators, *left),
                        end_domain: Self::compile_node_domain(&plan.operators, *right),
                    })?;
                }
                MatchOperator::ExpandShortestPath {
                    traversal,
                    left,
                    right,
                    direction,
                    minimum,
                    maximum,
                    all,
                    relationship_type,
                    properties,
                } => {
                    self.compile_hybrid_variable_length(HybridVariableLengthSpec {
                        traversal: *traversal,
                        left: *left,
                        right: *right,
                        direction: *direction,
                        minimum: *minimum,
                        maximum: *maximum,
                        kind: HybridExpansionKind::Shortest(if *all {
                            ShortestMode::All
                        } else {
                            ShortestMode::One
                        }),
                        relationship_type: relationship_type.as_deref(),
                        properties,
                        start_domain: Self::compile_node_domain(&plan.operators, *left),
                        end_domain: Self::compile_node_domain(&plan.operators, *right),
                    })?;
                }
                MatchOperator::DistinctRelationships(relationships) => {
                    self.compile_relationship_uniqueness(relationships)?;
                }
                MatchOperator::BindPath {
                    path,
                    start,
                    segments,
                } => self.compile_path_binding(*path, *start, segments)?,
                MatchOperator::MatchRelationshipList { traversal, path } => {
                    self.compile_relationship_list_match(*traversal, *path)?;
                }
                MatchOperator::Filter(expression) => {
                    for (slot, key, expected) in indexable_equalities(expression, bound.as_ref()) {
                        if indexed.contains(&slot) && probed.insert(slot) {
                            let id = self.entity_field(slot, "id")?;
                            let (value, value_type) = self.scalar_parts(expected)?;
                            let prefilter = if is_inline_operand(expected) {
                                self.property_index_prefilter(&id, key, (&value, &value_type))
                            } else {
                                self.computed_property_index_prefilter(
                                    &id,
                                    key,
                                    (&value, &value_type),
                                )
                            };
                            self.filters.push(prefilter);
                        }
                    }
                    let predicate = self.predicate_expression(expression)?;
                    self.filters.push(format!("({predicate})"));
                }
            }
        }
        Ok(())
    }

    fn compile_path_binding(
        &mut self,
        path: PathId,
        start: SlotId,
        segments: &[PathSegment],
    ) -> Result<()> {
        if self.paths.contains_key(&path) {
            return Err(Error::Data(format!(
                "logical path {} is bound twice",
                path.0
            )));
        }

        let encoded_segments = if segments.is_empty() {
            vec![format!(
                "json_object('nodes', json_array({}), 'relationships', json_array())",
                self.entity_field(start, "id")?
            )]
        } else {
            segments
                .iter()
                .map(|segment| {
                    let (nodes, relationships) = match segment.relationship {
                        RelationshipUse::Single(relationship) => (
                            format!(
                                "json_array({}, {})",
                                self.entity_field(segment.left, "id")?,
                                self.entity_field(segment.right, "id")?
                            ),
                            format!("json_array({})", self.entity_field(relationship, "id")?),
                        ),
                        RelationshipUse::Traversal(traversal) => {
                            let source = self.traversal_source(traversal)?;
                            (
                                format!("json({})", source.node_ids),
                                format!("json({})", source.edge_ids),
                            )
                        }
                    };
                    Ok(format!(
                        "json_object('nodes', {nodes}, 'relationships', {relationships})"
                    ))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let encoded = format!(
            "json_object('segments', json_array({}))",
            encoded_segments.join(", ")
        );
        self.paths.insert(path, encoded);
        Ok(())
    }

    // The traversal's `used_edges` array must equal the bound list's
    // relationship IDs position by position. The bound list is the
    // concatenation of every segment's relationships in the path identity, so
    // a list projected from a multi-segment `relationships(path)` compares as
    // one sequence. Both sides are compared through `json_each` rather than as
    // JSON text because the CTE, the Rust producers, and stored path identities
    // may escape the same IDs differently.
    fn compile_relationship_list_match(
        &mut self,
        traversal: TraversalId,
        path: PathId,
    ) -> Result<()> {
        let matched = self.traversal_edges(traversal)?.to_owned();
        let source = self.path_source(path)?.to_owned();
        let bound = format!(
            "(SELECT json_group_array(bound_edge.value                 ORDER BY bound_segment.key, bound_edge.key)               FROM json_each({source}, '$.segments') AS bound_segment,                    json_each(bound_segment.value, '$.relationships') AS bound_edge)"
        );
        let matched_member = format!("rematch_{}_edge", traversal.0);
        let bound_member = format!("rematch_{}_bound", traversal.0);
        self.filters.push(format!(
            "json_array_length({matched}) = json_array_length({bound})              AND NOT EXISTS (SELECT 1 FROM json_each({matched}) AS {matched_member}                              JOIN json_each({bound}) AS {bound_member}                                ON {matched_member}.key = {bound_member}.key                              WHERE {matched_member}.value IS NOT {bound_member}.value)"
        ));
        Ok(())
    }

    fn compile_variable_length(&mut self, spec: VariableLengthSpec<'_>) -> Result<()> {
        let VariableLengthSpec {
            traversal,
            left,
            right,
            direction,
            minimum,
            maximum,
            relationship_type,
            properties,
            seed_filters,
        } = spec;
        if self.traversals.contains_key(&traversal) {
            return Err(Error::Data(format!(
                "logical traversal {} is expanded twice",
                traversal.0
            )));
        }

        let cte = format!("_crabnet_walk_{}", traversal.0);
        let recursive_row = format!("walk{}", traversal.0);
        let edge = format!("walk_edge{}", traversal.0);
        let used_edge = format!("walk_used{}", traversal.0);
        let (edge_join, next_node) = match direction {
            Direction::Right => (
                format!("{edge}.from_node = {recursive_row}.end_id"),
                format!("{edge}.to_node"),
            ),
            Direction::Left => (
                format!("{edge}.to_node = {recursive_row}.end_id"),
                format!("{edge}.from_node"),
            ),
            Direction::Undirected => (
                format!(
                    "({edge}.from_node = {recursive_row}.end_id OR \
                     {edge}.to_node = {recursive_row}.end_id)"
                ),
                format!(
                    "CASE WHEN {edge}.from_node = {recursive_row}.end_id \
                     THEN {edge}.to_node ELSE {edge}.from_node END"
                ),
            ),
        };

        let mut recursive_filters = vec![
            format!("{recursive_row}.depth < {maximum}"),
            format!(
                "NOT EXISTS (SELECT 1 FROM json_each({recursive_row}.used_edges) \
                 AS {used_edge} WHERE {used_edge}.value = {edge}.id)"
            ),
        ];
        if let Some(relationship_type) = relationship_type {
            let expected = self.push_binding(SqlParameter::Text(relationship_type.to_owned()));
            recursive_filters.push(format!("{edge}.edge_type = {expected}"));
        }
        for (key, expected) in properties {
            let (property, property_type) = self.json_property_parts(format!("{edge}.props"), key);
            recursive_filters.push(self.typed_value_comparison(
                property,
                property_type,
                expected.clone(),
                BinaryOp::Equal,
            ));
        }

        // Seed one identity trail per candidate start node, then append
        // eligible edges with UNION ALL so separate relationship sequences
        // retain bag multiplicity. `used_edges` enforces Cypher trail
        // semantics without forbidding a path from revisiting the same node.
        //
        // The seed repeats the start endpoint's label and inline-property
        // constraints from this MATCH. The outer join still applies them to
        // the start node, so the seed is a superset of the final matches and
        // only limits how many trails the materialized walk enumerates. An
        // unconstrained start, such as a node bound by an earlier clause,
        // still seeds every stored node.
        let seed = if seed_filters.is_empty() {
            format!("nodes AS {WALK_SEED}")
        } else {
            format!("nodes AS {WALK_SEED} WHERE {}", seed_filters.join(" AND "))
        };
        let body = format!(
            "SELECT {WALK_SEED}.id, {WALK_SEED}.id, 0, json_array(), json_array({WALK_SEED}.id) \
             FROM {seed} \
             UNION ALL \
             SELECT {recursive_row}.start_id, {next_node}, {recursive_row}.depth + 1, \
                    json_insert({recursive_row}.used_edges, '$[#]', {edge}.id), \
                    json_insert({recursive_row}.node_ids, '$[#]', {next_node}) \
             FROM {cte} AS {recursive_row} \
             JOIN edges AS {edge} ON ({edge_join}) \
             WHERE {}",
            recursive_filters.join(" AND ")
        );
        self.ctes.push((
            format!("{cte}(start_id, end_id, depth, used_edges, node_ids)"),
            body,
        ));
        self.has_recursive_cte = true;

        let relation = format!("walk_result{}", traversal.0);
        self.from.push(format!("{cte} AS {relation}"));
        self.filters.push(format!("{relation}.depth >= {minimum}"));
        self.filters.push(format!("{relation}.depth <= {maximum}"));
        self.filters.push(format!(
            "{relation}.start_id = {}",
            self.entity_field(left, "id")?
        ));
        self.filters.push(format!(
            "{relation}.end_id = {}",
            self.entity_field(right, "id")?
        ));
        self.traversals.insert(
            traversal,
            TraversalSource {
                edge_ids: format!("{relation}.used_edges"),
                node_ids: format!("{relation}.node_ids"),
            },
        );
        Ok(())
    }

    fn compile_hybrid_variable_length(&mut self, spec: HybridVariableLengthSpec<'_>) -> Result<()> {
        let HybridVariableLengthSpec {
            traversal,
            left,
            right,
            direction,
            minimum,
            maximum,
            kind,
            relationship_type,
            properties,
            start_domain,
            end_domain,
        } = spec;
        if self.traversals.contains_key(&traversal) {
            return Err(Error::Data(format!(
                "logical traversal {} is expanded twice",
                traversal.0
            )));
        }

        let (edge_sql, edge_bindings) =
            Self::compile_eligible_edge_query(relationship_type, properties);
        let output = RuntimeSlot(self.inputs.len());
        let spec = TrailExpansionSpec {
            traversal,
            direction: direction.into(),
            minimum,
            maximum,
            edge_sql,
            edge_bindings,
            start_domain,
            end_domain,
        };
        let operator = match kind {
            HybridExpansionKind::Trails => ValueOperator::TrailExpand { output, spec },
            HybridExpansionKind::Shortest(mode) => {
                ValueOperator::ShortestExpand { output, mode, spec }
            }
        };
        self.inputs.push(operator);
        self.bindings.push(FragmentArgument::Runtime(output));
        let input = format!("?{}", self.bindings.len());
        let relation = format!("trail_result{}", traversal.0);
        let row = format!("{relation}.value");
        self.from.push(format!("json_each({input}) AS {relation}"));
        self.filters
            .push(format!("json_extract({row}, '$.depth') >= {minimum}"));
        if let Some(maximum) = maximum {
            self.filters
                .push(format!("json_extract({row}, '$.depth') <= {maximum}"));
        }
        self.filters.push(format!(
            "json_extract({row}, '$.start_id') = {}",
            self.entity_field(left, "id")?
        ));
        self.filters.push(format!(
            "json_extract({row}, '$.end_id') = {}",
            self.entity_field(right, "id")?
        ));
        self.traversals.insert(
            traversal,
            TraversalSource {
                edge_ids: format!("json_extract({row}, '$.used_edges')"),
                node_ids: format!("json_extract({row}, '$.node_ids')"),
            },
        );
        Ok(())
    }

    fn compile_eligible_edge_query(
        relationship_type: Option<&str>,
        properties: &[(String, ValueExpr)],
    ) -> (String, Vec<SqlParameter>) {
        let mut compiler = Self::new();
        let edge = "trail_edge";
        let mut filters = Vec::new();
        if let Some(relationship_type) = relationship_type {
            let expected = compiler.push_binding(SqlParameter::Text(relationship_type.to_owned()));
            filters.push(format!("{edge}.edge_type = {expected}"));
        }
        for (key, expected) in properties {
            let (property, property_type) =
                compiler.json_property_parts(format!("{edge}.props"), key);
            filters.push(compiler.typed_value_comparison(
                property,
                property_type,
                expected.clone(),
                BinaryOp::Equal,
            ));
        }
        let mut sql =
            format!("SELECT {edge}.id, {edge}.from_node, {edge}.to_node FROM edges AS {edge}");
        if !filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&filters.join(" AND "));
        }
        sql.push_str(&format!(" ORDER BY {edge}.id"));
        (sql, compiler.into_static_bindings())
    }

    /// Compile the label and inline-property constraints that this `MATCH`
    /// places on one traversal endpoint into a standalone node-ID query. The
    /// result is a superset of the endpoint's final matches, so the hybrid
    /// producer can seed or filter by it without changing semantics. An
    /// endpoint without such constraints has no domain query.
    fn compile_node_domain(
        operators: &[MatchOperator],
        endpoint: SlotId,
    ) -> Option<NodeDomainSpec> {
        let mut compiler = Self::new();
        let node = "trail_node";
        let filters = compiler.endpoint_constraint_filters(operators, endpoint, node);
        if filters.is_empty() {
            return None;
        }
        Some(NodeDomainSpec {
            sql: format!(
                "SELECT {node}.id FROM nodes AS {node} WHERE {} ORDER BY {node}.id",
                filters.join(" AND ")
            ),
            bindings: compiler.into_static_bindings(),
        })
    }

    /// Render the label and inline-property constraints that `operators`
    /// place on `endpoint` as predicates over a `nodes` row aliased `node`,
    /// binding their values in this compiler. Constraints from earlier clauses
    /// or a node-local `WHERE` are not included.
    fn endpoint_constraint_filters(
        &mut self,
        operators: &[MatchOperator],
        endpoint: SlotId,
        node: &str,
    ) -> Vec<String> {
        let mut filters = Vec::new();
        let indexed = operators.iter().any(|operator| {
            matches!(operator, MatchOperator::PropertyEquals { slot, .. } if *slot == endpoint)
        });
        for operator in operators {
            match operator {
                MatchOperator::HasLabel { slot, label } if *slot == endpoint => {
                    let expected = self.push_binding(SqlParameter::Text(label.clone()));
                    filters.push(node_label_predicate(
                        &format!("{node}.id"),
                        &expected,
                        indexed,
                    ));
                }
                MatchOperator::PropertyEquals { slot, key, value } if *slot == endpoint => {
                    let (expected, expected_type) = self.probe_value(value);
                    let prefilter = self.property_index_prefilter(
                        &format!("{node}.id"),
                        key,
                        (&expected, &expected_type),
                    );
                    filters.push(prefilter);
                    let (property, property_type) =
                        self.json_property_parts(format!("{node}.props"), key);
                    filters.push(self.typed_value_comparison(
                        property,
                        property_type,
                        value.clone(),
                        BinaryOp::Equal,
                    ));
                }
                _ => {}
            }
        }
        filters
    }

    fn into_static_bindings(self) -> Vec<SqlParameter> {
        self.bindings
            .into_iter()
            .map(|argument| match argument {
                FragmentArgument::Static(parameter) => parameter,
                FragmentArgument::Runtime(_) => {
                    unreachable!("traversal source filters are SQL-only")
                }
            })
            .collect()
    }

    fn compile_relationship_uniqueness(&mut self, relationships: &[RelationshipUse]) -> Result<()> {
        for (index, relationship) in relationships.iter().enumerate() {
            for (previous_index, previous) in relationships[..index].iter().enumerate() {
                let predicate = match (*relationship, *previous) {
                    (RelationshipUse::Single(left), RelationshipUse::Single(right)) => format!(
                        "{} <> {}",
                        self.entity_field(left, "id")?,
                        self.entity_field(right, "id")?
                    ),
                    (RelationshipUse::Single(single), RelationshipUse::Traversal(traversal))
                    | (RelationshipUse::Traversal(traversal), RelationshipUse::Single(single)) => {
                        let used_edges = self.traversal_edges(traversal)?;
                        let member = format!("trail_member_{index}_{previous_index}");
                        format!(
                            "NOT EXISTS (SELECT 1 FROM json_each({used_edges}) AS {member} \
                             WHERE {member}.value = {})",
                            self.entity_field(single, "id")?
                        )
                    }
                    (RelationshipUse::Traversal(left), RelationshipUse::Traversal(right)) => {
                        let left_edges = self.traversal_edges(left)?;
                        let right_edges = self.traversal_edges(right)?;
                        let left_member = format!("left_trail_{index}_{previous_index}");
                        let right_member = format!("right_trail_{index}_{previous_index}");
                        format!(
                            "NOT EXISTS (SELECT 1 FROM json_each({left_edges}) AS {left_member} \
                             JOIN json_each({right_edges}) AS {right_member} \
                               ON {left_member}.value = {right_member}.value)"
                        )
                    }
                };
                self.filters.push(predicate);
            }
        }
        Ok(())
    }

    fn traversal_edges(&self, traversal: TraversalId) -> Result<&str> {
        self.traversal_source(traversal)
            .map(|source| source.edge_ids.as_str())
    }

    fn traversal_source(&self, traversal: TraversalId) -> Result<&TraversalSource> {
        self.traversals.get(&traversal).ok_or_else(|| {
            Error::Data(format!(
                "logical traversal {} has no SQL source",
                traversal.0
            ))
        })
    }

    fn compile_optional_match(&mut self, plan: &MatchPlan) -> Result<()> {
        if plan.has_path_binding() {
            return Err(Error::Unsupported(
                "named paths or relationship-list bindings in OPTIONAL MATCH".into(),
            ));
        }
        if plan.has_variable_length() {
            return Err(Error::Unsupported(
                "variable-length traversal in OPTIONAL MATCH".into(),
            ));
        }
        let stage_index = self.ctes.len();
        let input_cte = format!("_crabnet_optional_input_{stage_index}");
        let input_relation = format!("oi{stage_index}");

        let mut entity_bindings = self
            .entities
            .iter()
            .map(|(slot, source)| (*slot, source.kind()))
            .collect::<Vec<_>>();
        entity_bindings.sort_by_key(|(slot, _)| *slot);
        let mut scalar_bindings = self.scalars.keys().copied().collect::<Vec<_>>();
        scalar_bindings.sort();
        let mut path_bindings = self.paths.keys().copied().collect::<Vec<_>>();
        path_bindings.sort();

        let mut select = entity_bindings
            .iter()
            .map(|(slot, kind)| {
                self.encoded_entity(*slot, *kind)
                    .map(|value| format!("{value} AS e{}", slot.0))
            })
            .collect::<Result<Vec<_>>>()?;
        select.extend(scalar_bindings.iter().map(|slot| {
            format!(
                "{} AS s{}",
                self.scalars
                    .get(slot)
                    .expect("collected scalar slot has a source"),
                slot.0
            )
        }));
        select.extend(path_bindings.iter().map(|path| {
            format!(
                "{} AS p{}",
                self.paths
                    .get(path)
                    .expect("collected path slot has a source"),
                path.0
            )
        }));
        if let Some(ordering) = &self.ordering {
            select.push(format!("{ordering} AS _crabnet_order"));
        }
        if select.is_empty() {
            select.push("1 AS _crabnet_unit".into());
        }

        let input_sql = self.select_sql(&select);
        self.ctes.push((input_cte.clone(), input_sql));
        self.entities = entity_bindings
            .into_iter()
            .map(|(slot, kind)| {
                (
                    slot,
                    EntitySource::Encoded {
                        sql: format!("{input_relation}.e{}", slot.0),
                        kind,
                    },
                )
            })
            .collect();
        self.scalars = scalar_bindings
            .into_iter()
            .map(|slot| (slot, format!("{input_relation}.s{}", slot.0)))
            .collect();
        self.paths = path_bindings
            .into_iter()
            .map(|path| (path, format!("{input_relation}.p{}", path.0)))
            .collect();
        let inherited_ordering = self
            .ordering
            .as_ref()
            .map(|_| format!("{input_relation}._crabnet_order"));
        self.from.clear();
        self.filters.clear();
        self.ordering = inherited_ordering;

        let inline_random = std::mem::replace(&mut self.inline_random, true);
        let compiled = self.compile_match(plan);
        self.inline_random = inline_random;
        compiled?;
        let optional_sources = std::mem::take(&mut self.from);
        let optional_predicates = std::mem::take(&mut self.filters);
        let right = match optional_sources.as_slice() {
            [] => format!("(SELECT 1 AS _crabnet_present) AS om{stage_index}"),
            [source] => source.clone(),
            sources => format!("({})", sources.join(" CROSS JOIN ")),
        };
        let predicate = if optional_predicates.is_empty() {
            "1".to_owned()
        } else {
            optional_predicates.join(" AND ")
        };
        self.from = vec![format!(
            "{input_cte} AS {input_relation} LEFT JOIN {right} ON ({predicate})"
        )];
        Ok(())
    }

    fn compile_with(&mut self, stage: &WithStage) -> Result<()> {
        if !stage.distinct {
            return self.compile_with_rows(stage);
        }

        // Evaluate the projection once before DISTINCT's modifiers. Keep
        // incoming values temporarily so WHERE can still inspect dropped
        // variables; aliases already refer to the projected scalar slots.
        let aggregating = stage.bindings.iter().any(|binding| {
            matches!(binding, WithBinding::Scalar { expression, .. } | WithBinding::Entity { expression: Some(expression), .. } if expression.contains_aggregate())
        });
        let mut projection = WithStage {
            bindings: stage.bindings.clone(),
            distinct: false,
            filter: None,
            order_by: Vec::new(),
            offset: None,
            limit: None,
        };
        if !aggregating {
            let mut entities = self.entities.iter().collect::<Vec<_>>();
            entities.sort_by_key(|(slot, _)| **slot);
            projection
                .bindings
                .extend(
                    entities
                        .into_iter()
                        .map(|(slot, source)| WithBinding::Entity {
                            name: String::new(),
                            slot: *slot,
                            kind: source.kind(),
                            expression: None,
                        }),
                );
            let mut scalars = self.scalars.keys().copied().collect::<Vec<_>>();
            scalars.sort();
            projection
                .bindings
                .extend(scalars.into_iter().map(|slot| WithBinding::Scalar {
                    name: String::new(),
                    slot,
                    expression: Expr::Scalar(slot),
                }));
            let mut paths = self.paths.keys().copied().collect::<Vec<_>>();
            paths.sort();
            projection
                .bindings
                .extend(paths.into_iter().map(|path| WithBinding::Path {
                    name: String::new(),
                    path,
                }));
        }
        self.compile_with_rows(&projection)?;

        let mut modifiers = stage.clone();
        for binding in &mut modifiers.bindings {
            if let WithBinding::Entity {
                slot,
                kind,
                expression,
                ..
            } = binding
                && let Some(expression) = expression.take()
                && !expression.contains_rand()
            {
                self.projected_scalars
                    .push((expression, self.encoded_entity(*slot, *kind)?));
            }
            if let WithBinding::Scalar {
                slot, expression, ..
            } = binding
            {
                // A written expression such as ORDER BY n.name can refer to
                // its projected value after grouping has dropped n. A new
                // rand() call remains a separate evaluation; aliases refer
                // to slots and never require this expression substitution.
                if !expression.contains_rand() {
                    self.projected_scalars
                        .push((expression.clone(), self.scalars[slot].clone()));
                }
                *expression = match expression {
                    Expr::Nested { shape, .. } => Expr::Nested {
                        shape: shape.clone(),
                        expression: Box::new(Expr::Scalar(*slot)),
                    },
                    Expr::EntityList { kind, .. } => Expr::EntityList {
                        kind: *kind,
                        expression: Box::new(Expr::Scalar(*slot)),
                    },
                    _ => Expr::Scalar(*slot),
                };
            }
        }
        let result = self.compile_with_rows(&modifiers);
        self.projected_scalars.clear();
        result
    }

    fn compile_with_rows(&mut self, stage: &WithStage) -> Result<()> {
        let aggregating = stage.bindings.iter().any(|binding| {
            matches!(
                binding,
                WithBinding::Scalar { expression, .. } | WithBinding::Entity { expression: Some(expression), .. } if expression.contains_aggregate()
            )
        });
        if aggregating
            && stage.bindings.iter().any(|binding| {
                matches!(
                    binding,
                    WithBinding::Path { .. } | WithBinding::PathCollection { .. }
                )
            })
        {
            return Err(Error::Unsupported(
                "path values in aggregating WITH projections".into(),
            ));
        }
        if aggregating {
            let expressions = stage
                .bindings
                .iter()
                .map(|binding| match binding {
                    WithBinding::Entity {
                        slot, expression, ..
                    } => expression.clone().unwrap_or(Expr::Entity(*slot)),
                    WithBinding::Scalar { expression, .. } => expression.clone(),
                    WithBinding::Path { .. } | WithBinding::PathCollection { .. } => unreachable!(),
                })
                .collect::<Vec<_>>();
            let modifiers = stage
                .filter
                .iter()
                .chain(stage.order_by.iter().map(|item| &item.expression))
                .collect::<Vec<_>>();
            self.compile_aggregation(&expressions, &modifiers)?;
        }
        if let Some(filter) = &stage.filter {
            let predicate = self.predicate_expression(filter)?;
            self.filters.push(format!("({predicate})"));
        }

        let mut select = Vec::new();
        let mut entity_bindings = Vec::new();
        let mut path_bindings = Vec::new();
        let mut scalar_bindings = Vec::new();
        for binding in &stage.bindings {
            match binding {
                WithBinding::Entity {
                    slot,
                    kind,
                    expression,
                    ..
                } => {
                    if !entity_bindings
                        .iter()
                        .any(|(candidate, _)| candidate == slot)
                    {
                        select.push(format!(
                            "{} AS e{}",
                            match expression {
                                Some(expression) => self.output_expression(expression)?.0,
                                None => self.encoded_entity(*slot, *kind)?,
                            },
                            slot.0
                        ));
                        entity_bindings.push((*slot, *kind));
                    }
                }
                WithBinding::Path { path, .. } | WithBinding::PathCollection { path, .. } => {
                    if !path_bindings.contains(path) {
                        select.push(format!("{} AS p{}", self.path_source(*path)?, path.0));
                        path_bindings.push(*path);
                    }
                }
                WithBinding::Scalar {
                    slot, expression, ..
                } => {
                    let encoded = self.encoded_expression(expression)?;
                    select.push(format!("{encoded} AS s{}", slot.0));
                    scalar_bindings.push(*slot);
                }
            }
        }

        let order_items = stage
            .order_by
            .iter()
            .map(|item| {
                let key = self.order_key(&item.expression)?;
                Ok(order_expression(&key, item.direction))
            })
            .collect::<Result<Vec<_>>>()?;
        let inherited_ordering = (!aggregating).then(|| self.ordering.clone()).flatten();
        if !order_items.is_empty() {
            select.push(format!(
                "row_number() OVER (ORDER BY {}) AS _crabnet_order",
                order_items.join(", ")
            ));
        } else if let Some(ordering) = &inherited_ordering {
            select.push(format!("{ordering} AS _crabnet_order"));
        }

        if select.is_empty() {
            // `WITH *` over an empty scope keeps the rows and binds nothing.
            select.push("1 AS _crabnet_unit".to_owned());
        }
        let mut sql = self.select_sql(&select);
        let ordered = !order_items.is_empty() || inherited_ordering.is_some();
        if stage.distinct {
            sql = self.distinct_with_rows(sql, &stage.bindings, ordered);
        }
        if stage.distinct && ordered {
            sql.push_str(" ORDER BY _crabnet_order");
        } else if !order_items.is_empty() {
            sql.push_str(" ORDER BY ");
            sql.push_str(&order_items.join(", "));
        } else if let Some(ordering) = &inherited_ordering {
            sql.push_str(" ORDER BY ");
            sql.push_str(ordering);
        }
        if stage.limit.is_some() || stage.offset.is_some() {
            sql.push_str(" LIMIT ");
            match &stage.limit {
                Some(limit) => sql.push_str(&self.push_binding(SqlParameter::Page(limit.clone()))),
                None => sql.push_str("-1"),
            }
        }
        if let Some(offset) = &stage.offset {
            sql.push_str(" OFFSET ");
            sql.push_str(&self.push_binding(SqlParameter::Page(offset.clone())));
        }

        let stage_index = self.ctes.len();
        let cte = format!("_crabnet_with_{stage_index}");
        self.ctes.push((cte.clone(), sql));
        let relation = format!("w{stage_index}");

        self.entities = entity_bindings
            .into_iter()
            .map(|(slot, kind)| {
                (
                    slot,
                    EntitySource::Encoded {
                        sql: format!("{relation}.e{}", slot.0),
                        kind,
                    },
                )
            })
            .collect();
        self.scalars = scalar_bindings
            .into_iter()
            .map(|slot| (slot, format!("{relation}.s{}", slot.0)))
            .collect();
        self.paths = path_bindings
            .into_iter()
            .map(|path| (path, format!("{relation}.p{}", path.0)))
            .collect();
        self.from = vec![format!("{cte} AS {relation}")];
        self.ordering = (!order_items.is_empty() || inherited_ordering.is_some())
            .then(|| format!("{relation}._crabnet_order"));
        self.filters.clear();
        self.projected_scalars.clear();
        Ok(())
    }

    /// Compute grouping keys and aggregate leaves before evaluating the
    /// surrounding scalar expressions. This keeps each aggregate in the
    /// input query's scope, including when used inside a list subquery or a
    /// CASE branch, and lets scalar operators bind their operands once.
    fn compile_aggregation(&mut self, expressions: &[Expr], modifiers: &[&Expr]) -> Result<()> {
        fn collect(expression: &Expr, aggregates: &mut Vec<Expr>) {
            if matches!(expression, Expr::Aggregate { .. }) {
                if !aggregates.contains(expression) {
                    aggregates.push(expression.clone());
                }
            } else {
                for child in expression.children() {
                    collect(child, aggregates);
                }
            }
        }

        let mut aggregates = Vec::new();
        for expression in expressions.iter().chain(modifiers.iter().copied()) {
            collect(expression, &mut aggregates);
        }
        let grouping = expressions
            .iter()
            .filter(|expression| !expression.contains_aggregate() && expression.is_row_dependent());
        let stage_index = self.ctes.len();
        let cte = format!("_crabnet_aggregate_{stage_index}");
        let relation = format!("ag{stage_index}");
        let mut select = Vec::new();
        let mut group_by = Vec::new();
        let mut entities = HashMap::new();
        let mut projected = Vec::new();
        for expression in grouping.chain(aggregates.iter()) {
            if !expression.contains_aggregate() {
                group_by.extend(self.grouping_terms(expression)?);
            }
            let column = format!("v{}", select.len());
            let (encoded, _) = self.output_expression(expression)?;
            select.push(format!("{encoded} AS {column}"));
            let source = format!("{relation}.{column}");
            if let Expr::Entity(slot) = expression {
                entities.insert(
                    *slot,
                    EntitySource::Encoded {
                        sql: source,
                        kind: self.entity_kind(*slot)?,
                    },
                );
            } else {
                projected.push((expression.clone(), source));
            }
        }
        let mut sql = self.select_sql(&select);
        if !group_by.is_empty() {
            sql.push_str(" GROUP BY ");
            sql.push_str(&group_by.join(", "));
        }
        self.ctes.push((cte.clone(), sql));
        self.entities = entities;
        self.scalars.clear();
        self.paths.clear();
        self.projected_scalars = projected;
        self.from = vec![format!("{cte} AS {relation}")];
        self.filters.clear();
        self.ordering = None;
        Ok(())
    }

    /// Deduplicate the materialized projection, never hidden ordering columns
    /// or the input bag. Ranking retains a complete representative row and
    /// its incoming order, and runs before the caller applies pagination.
    fn distinct_with_rows(
        &mut self,
        sql: String,
        bindings: &[WithBinding],
        ordered: bool,
    ) -> String {
        let keys = bindings
            .iter()
            .map(|binding| match binding {
                WithBinding::Entity { slot, .. } => {
                    format!("json_extract(e{}, '$.id')", slot.0)
                }
                WithBinding::Scalar {
                    slot, expression, ..
                } => {
                    let function = if matches!(expression, Expr::Nested { .. }) {
                        "crabnet_nested_key"
                    } else if matches!(expression, Expr::EntityList { .. }) {
                        "crabnet_entity_list_key"
                    } else {
                        "crabnet_semantic_key"
                    };
                    format!("{function}(s{})", slot.0)
                }
                WithBinding::Path { path, .. } => {
                    format!("crabnet_path_key(p{}, 'path')", path.0)
                }
                WithBinding::PathCollection { path, kind, .. } => {
                    let kind = match kind {
                        PathCollectionKind::Nodes => "nodes",
                        PathCollectionKind::Relationships => "relationships",
                    };
                    format!("crabnet_path_key(p{}, '{kind}')", path.0)
                }
            })
            .collect::<Vec<_>>();
        let partition = if keys.is_empty() {
            // WITH DISTINCT * over an empty scope retains at most one row.
            "'unit'".to_owned()
        } else {
            keys.join(", ")
        };
        let order = if ordered {
            " ORDER BY _crabnet_order"
        } else {
            ""
        };
        let index = self.ctes.len();
        let projected = format!("_crabnet_distinct_input_{index}");
        let ranked = format!("_crabnet_distinct_rank_{index}");
        self.ctes.push((projected.clone(), sql));
        self.ctes.push((
            ranked.clone(),
            format!(
                "SELECT *, row_number() OVER (PARTITION BY {partition}{order}) \
             AS _crabnet_duplicate FROM {projected}"
            ),
        ));
        format!("SELECT * FROM {ranked} WHERE _crabnet_duplicate = 1")
    }

    fn compile_unwind(&mut self, stage: &UnwindStage) -> Result<()> {
        let stage_index = self.ctes.len();
        let member = format!("u{stage_index}");
        if let Expr::PathFunction {
            kind: PathFunctionKind::Nodes | PathFunctionKind::Relationships,
            path,
        } = &stage.expression
        {
            let UnwindBinding::Entity(slot, kind) = stage.binding else {
                return Err(Error::Data("path UNWIND requires an entity binding".into()));
            };
            let source = self.path_source(*path)?.to_owned();
            let segment = format!("usegment{stage_index}");
            let entity = format!("uentity{stage_index}");
            let (field, table) = match kind {
                EntityKind::Node => ("nodes", "nodes"),
                EntityKind::Relationship => ("relationships", "edges"),
            };
            self.from
                .push(format!("json_each({source}, '$.segments') AS {segment}"));
            self.from.push(format!(
                "json_each({segment}.value, '$.{field}') AS {member}"
            ));
            self.from.push(format!("{table} AS {entity}"));
            self.filters.push(format!("{entity}.id = {member}.value"));
            if kind == EntityKind::Node {
                // Adjacent segments share one endpoint. Skip that duplicate
                // position, but retain repeated identities from cycles and
                // self-loops, including when a segment has zero hops.
                self.filters
                    .push(format!("({segment}.key = 0 OR {member}.key > 0)"));
            }
            self.entities.insert(
                slot,
                EntitySource::Table {
                    alias: entity,
                    kind,
                },
            );
        } else {
            let (list_value, list_type) = self.scalar_parts(&stage.expression)?;
            self.from.push(format!(
                "json_each(CASE WHEN ({list_type}) = 'array' \
                 THEN ({list_value}) ELSE '[]' END) AS {member}"
            ));
            match stage.binding {
                UnwindBinding::Scalar(slot) => {
                    self.scalars.insert(
                        slot,
                        encoded_scalar(&format!("{member}.value"), &format!("{member}.type")),
                    );
                }
                UnwindBinding::Entity(slot, kind) => {
                    self.entities.insert(
                        slot,
                        EntitySource::Encoded {
                            sql: format!("{member}.value"),
                            kind,
                        },
                    );
                }
            }
        }

        let mut entity_bindings = self
            .entities
            .iter()
            .map(|(slot, source)| (*slot, source.kind()))
            .collect::<Vec<_>>();
        entity_bindings.sort_by_key(|(slot, _)| *slot);
        let mut scalar_bindings = self.scalars.keys().copied().collect::<Vec<_>>();
        scalar_bindings.sort();
        let mut path_bindings = self.paths.keys().copied().collect::<Vec<_>>();
        path_bindings.sort();

        let mut select = entity_bindings
            .iter()
            .map(|(slot, kind)| {
                self.encoded_entity(*slot, *kind)
                    .map(|value| format!("{value} AS e{}", slot.0))
            })
            .collect::<Result<Vec<_>>>()?;
        select.extend(scalar_bindings.iter().map(|slot| {
            format!(
                "{} AS s{}",
                self.scalars
                    .get(slot)
                    .expect("collected scalar slot has a source"),
                slot.0
            )
        }));
        select.extend(path_bindings.iter().map(|path| {
            format!(
                "{} AS p{}",
                self.paths
                    .get(path)
                    .expect("collected path slot has a source"),
                path.0
            )
        }));
        let sql = self.select_sql(&select);
        let cte = format!("_crabnet_unwind_{stage_index}");
        self.ctes.push((cte.clone(), sql));
        let relation = format!("urows{stage_index}");

        self.entities = entity_bindings
            .into_iter()
            .map(|(slot, kind)| {
                (
                    slot,
                    EntitySource::Encoded {
                        sql: format!("{relation}.e{}", slot.0),
                        kind,
                    },
                )
            })
            .collect();
        self.scalars = scalar_bindings
            .into_iter()
            .map(|slot| (slot, format!("{relation}.s{}", slot.0)))
            .collect();
        self.paths = path_bindings
            .into_iter()
            .map(|path| (path, format!("{relation}.p{}", path.0)))
            .collect();
        self.from = vec![format!("{cte} AS {relation}")];
        self.filters.clear();
        self.ordering = None;
        Ok(())
    }

    fn insert_scan(&mut self, slot: SlotId, kind: EntityKind, alias: String) -> Result<()> {
        if self
            .entities
            .insert(
                slot,
                EntitySource::Table {
                    alias: alias.clone(),
                    kind,
                },
            )
            .is_some()
        {
            return Err(Error::Data("logical plan scans an entity twice".into()));
        }
        self.scanned.push((slot, kind));
        Ok(())
    }

    fn select_sql(&mut self, select: &[String]) -> String {
        let mut sql = format!("SELECT {}", select.join(", "));
        let random_sources = std::mem::take(&mut self.random_sources);
        if random_sources.is_empty() {
            if !self.from.is_empty() {
                sql.push_str(" FROM ");
                sql.push_str(&self.from.join(", "));
            }
        } else {
            // CROSS JOIN fixes the loop order, so the random sources are
            // innermost and re-read for every row of the sources before them.
            sql.push_str(" FROM ");
            sql.push_str(
                &self
                    .from
                    .iter()
                    .chain(random_sources.iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" CROSS JOIN "),
            );
        }
        if !self.filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&self.filters.join(" AND "));
        }
        sql
    }

    fn with_ctes(&self, sql: String) -> String {
        if self.ctes.is_empty() {
            return sql;
        }
        let ctes = self
            .ctes
            .iter()
            .map(|(name, sql)| format!("{name} AS MATERIALIZED ({sql})"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "WITH {}{ctes} {sql}",
            if self.has_recursive_cte {
                "RECURSIVE "
            } else {
                ""
            }
        )
    }

    fn entity_source(&self, slot: SlotId) -> Result<&EntitySource> {
        self.entities
            .get(&slot)
            .ok_or_else(|| Error::Data(format!("logical slot {} has no SQL source", slot.0)))
    }

    fn entity_kind(&self, slot: SlotId) -> Result<EntityKind> {
        self.entity_source(slot).map(EntitySource::kind)
    }

    fn entity_field(&self, slot: SlotId, field: &str) -> Result<String> {
        let source = self.entity_source(slot)?;
        let column = match (source.kind(), field) {
            (_, "id") => "id",
            (EntityKind::Relationship, "type") => "edge_type",
            (EntityKind::Relationship, "from") => "from_node",
            (EntityKind::Relationship, "to") => "to_node",
            _ => {
                return Err(Error::Data(format!(
                    "field `{field}` does not exist on logical slot {}",
                    slot.0
                )));
            }
        };
        Ok(match source {
            EntitySource::Table { alias, .. } => format!("{alias}.{column}"),
            EntitySource::Encoded { sql, .. } => format!("json_extract({sql}, '$.{field}')"),
        })
    }

    /// The node's label list as JSON array text.
    fn entity_labels(&self, slot: SlotId) -> Result<String> {
        Ok(match self.entity_source(slot)? {
            EntitySource::Table { alias, .. } => format!("{alias}.labels"),
            EntitySource::Encoded { sql, .. } => format!("json_extract({sql}, '$.labels')"),
        })
    }

    /// Whether the node in `slot` carries the label bound at `value`. A
    /// stored node is tested through the `node_labels` index table; an
    /// encoded one through its label list.
    fn label_test(&self, slot: SlotId, value: &str) -> Result<String> {
        Ok(match self.entity_source(slot)? {
            EntitySource::Table { alias, .. } => {
                node_label_predicate(&format!("{alias}.id"), value, false)
            }
            EntitySource::Encoded { sql, .. } => {
                format!("EXISTS (SELECT 1 FROM json_each({sql}, '$.labels') WHERE value = {value})")
            }
        })
    }

    fn entity_props(&self, slot: SlotId) -> Result<String> {
        Ok(match self.entity_source(slot)? {
            EntitySource::Table { alias, .. } => format!("{alias}.props"),
            EntitySource::Encoded { sql, .. } => format!("json_extract({sql}, '$.props')"),
        })
    }

    fn encoded_entity(&self, slot: SlotId, kind: EntityKind) -> Result<String> {
        if self.entity_kind(slot)? != kind {
            return Err(Error::Data(format!(
                "logical slot {} changed entity kind across a WITH boundary",
                slot.0
            )));
        }
        if let EntitySource::Encoded { sql, .. } = self.entity_source(slot)? {
            return Ok(sql.clone());
        }
        let id = self.entity_field(slot, "id")?;
        let props = self.entity_props(slot)?;
        let record = match kind {
            EntityKind::Node => {
                let labels = self.entity_labels(slot)?;
                format!("json_object('id', {id}, 'labels', json({labels}), 'props', json({props}))")
            }
            EntityKind::Relationship => format!(
                "json_object('id', {id}, 'type', {}, 'from', {}, 'to', {}, \
                 'props', json({props}))",
                self.entity_field(slot, "type")?,
                self.entity_field(slot, "from")?,
                self.entity_field(slot, "to")?,
            ),
        };
        Ok(format!(
            "CASE WHEN {id} IS NULL THEN 'null' ELSE {record} END"
        ))
    }

    fn path_source(&self, path: PathId) -> Result<&str> {
        self.paths
            .get(&path)
            .map(String::as_str)
            .ok_or_else(|| Error::Data(format!("logical path {} has no SQL source", path.0)))
    }

    fn output_expression(&mut self, expression: &Expr) -> Result<(String, OutputShape)> {
        if matches!(expression, Expr::Nested { .. }) {
            return Ok((self.encoded_expression(expression)?, OutputShape::Nested));
        }
        if let Expr::EntityValue { kind, .. } = expression {
            return Ok((
                self.encoded_expression(expression)?,
                OutputShape::Entity(*kind),
            ));
        }
        if let Expr::EntityList { kind, .. } = expression {
            return Ok((
                self.encoded_expression(expression)?,
                OutputShape::EntityList(*kind),
            ));
        }
        if let Expr::Entity(slot) = expression {
            let kind = self.entity_kind(*slot)?;
            return Ok((self.encoded_entity(*slot, kind)?, OutputShape::Entity(kind)));
        }
        if let Expr::Path(path) = expression {
            return Ok((
                self.path_source(*path)?.to_owned(),
                OutputShape::PathIdentity,
            ));
        }
        if let Expr::PathFunction { kind, path } = expression {
            let shape = match kind {
                PathFunctionKind::Length => {
                    return Ok((self.encoded_expression(expression)?, OutputShape::Json));
                }
                PathFunctionKind::Nodes => OutputShape::PathNodes,
                PathFunctionKind::Relationships => OutputShape::PathRelationships,
            };
            return Ok((self.path_source(*path)?.to_owned(), shape));
        }

        Ok((self.encoded_expression(expression)?, OutputShape::Json))
    }

    fn grouping_terms(&mut self, expression: &Expr) -> Result<Vec<String>> {
        if let Expr::EntityValue { .. } = expression {
            let record = self.encoded_expression(expression)?;
            return Ok(vec![format!("json_extract({record}, '$.id')")]);
        }
        if let Expr::Entity(slot) = expression {
            return Ok(vec![self.entity_field(*slot, "id")?]);
        }
        if matches!(
            expression,
            Expr::Path(_)
                | Expr::PathFunction {
                    kind: PathFunctionKind::Nodes | PathFunctionKind::Relationships,
                    ..
                }
        ) {
            return Err(Error::Unsupported(
                "path values as grouping keys".to_owned(),
            ));
        }
        let encoded = self.encoded_expression(expression)?;
        let key = if matches!(expression, Expr::Nested { .. }) {
            format!("crabnet_nested_key({encoded})")
        } else if matches!(expression, Expr::EntityList { .. }) {
            format!("crabnet_entity_list_key({encoded})")
        } else {
            format!("crabnet_semantic_key({encoded})")
        };
        Ok(vec![key])
    }

    fn raw_expression(&mut self, expression: &Expr) -> Result<String> {
        // Some values (notably literals and parameters) use a separate bind
        // to carry their JSON type. Keep that bind visible even when ORDER BY
        // needs only the raw payload, or SQLite's parameter count and the
        // planner's binding vector can diverge.
        self.scoped(&[expression], |_, parts| {
            let (value, value_type) = &parts[0];
            Ok(format!(
                "CASE WHEN ({value_type}) IS NULL THEN ({value}) ELSE ({value}) END"
            ))
        })
    }

    /// A sort key whose byte order is Cypher's orderability. The key
    /// function takes the JSON type and the SQL value, so primitives skip
    /// the JSON round trip.
    fn order_key(&mut self, expression: &Expr) -> Result<String> {
        self.scoped(&[expression], |_, parts| {
            let (value, value_type) = &parts[0];
            Ok(format!("crabnet_order_key(({value_type}), ({value}))"))
        })
    }

    /// The JSON text encoding of a scalar, as stored in result columns.
    fn encoded_expression(&mut self, expression: &Expr) -> Result<String> {
        if let Some((_, encoded)) = self
            .projected_scalars
            .iter()
            .find(|(projected, _)| projected == expression)
        {
            return Ok(encoded.clone());
        }
        if matches!(expression, Expr::Nested { .. } | Expr::NestedCall { .. }) {
            return self.encoded_nested(expression);
        }
        if let Expr::Case {
            operand,
            alternatives,
            otherwise,
        } = expression
        {
            return self
                .case_expression(operand.as_deref(), alternatives, otherwise.as_deref(), true)
                .map(|scalar| scalar.value);
        }
        if let Expr::Function {
            function: ScalarFunction::Coalesce,
            arguments,
        } = expression
        {
            return self
                .scalar_function(ScalarFunction::Coalesce, arguments)
                .map(|scalar| scalar.value);
        }
        self.scoped(&[expression], |_, parts| {
            let (value, value_type) = &parts[0];
            Ok(encoded_scalar(value, value_type))
        })
    }

    /// Build tagged constructors iteratively. A deeply nested literal should
    /// not allocate several recursive SQL-compiler frames per container.
    fn encoded_nested(&mut self, expression: &Expr) -> Result<String> {
        use super::nested::Op;
        enum Work<'a> {
            Expr(&'a Expr),
            Call(Op, usize),
            List(usize),
            Map(Vec<String>),
        }
        let mut work = vec![Work::Expr(expression)];
        let mut values: Vec<String> = Vec::new();
        while let Some(next) = work.pop() {
            match next {
                Work::Expr(expression) => {
                    if let Some((_, encoded)) = self
                        .projected_scalars
                        .iter()
                        .find(|(projected, _)| projected == expression)
                    {
                        values.push(encoded.clone());
                        continue;
                    }
                    match expression {
                        Expr::Nested { expression, .. } => work.push(Work::Expr(expression)),
                        Expr::NestedCall { op, arguments } => {
                            work.push(Work::Call(*op, arguments.len()));
                            work.extend(arguments.iter().rev().map(Work::Expr));
                        }
                        Expr::List(items) => {
                            work.push(Work::List(items.len()));
                            work.extend(items.iter().rev().map(Work::Expr));
                        }
                        Expr::Map(entries) => {
                            let keys = entries
                                .iter()
                                .map(|(key, _)| self.push_binding(SqlParameter::Text(key.clone())))
                                .collect();
                            work.push(Work::Map(keys));
                            work.extend(entries.iter().rev().map(|(_, value)| Work::Expr(value)));
                        }
                        Expr::Entity(slot) => {
                            values.push(self.encoded_entity(*slot, self.entity_kind(*slot)?)?)
                        }
                        Expr::Path(path) => {
                            let grouped = super::nested::materialize_path_value(Expr::Path(*path));
                            if let Some((_, source)) = self
                                .projected_scalars
                                .iter()
                                .find(|(value, _)| *value == grouped)
                            {
                                values.push(format!("crabnet_nested('payload', {source})"));
                                continue;
                            }
                            let nodes = self.encoded_expression(&Expr::PathFunction {
                                kind: PathFunctionKind::Nodes,
                                path: *path,
                            })?;
                            let relationships = self.encoded_expression(&Expr::PathFunction {
                                kind: PathFunctionKind::Relationships,
                                path: *path,
                            })?;
                            values.push(format!("json_object('nodes', json({nodes}), 'relationships', json({relationships}))"));
                        }
                        _ => values.push(self.encoded_expression(expression)?),
                    }
                }
                Work::Call(op, count) => {
                    let arguments = values.split_off(values.len() - count);
                    values.push(format!(
                        "crabnet_nested('{}', {})",
                        op.name(),
                        arguments.join(", ")
                    ));
                }
                Work::List(count) => {
                    let items = values.split_off(values.len() - count);
                    values.push(format!(
                        "json_array({})",
                        items
                            .iter()
                            .map(|value| format!("json({value})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                Work::Map(keys) => {
                    let items = values.split_off(values.len() - keys.len());
                    values.push(format!(
                        "json_object({})",
                        keys.iter()
                            .zip(items)
                            .map(|(key, value)| format!("{key}, json({value})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        Ok(values.pop().expect("nested expression result"))
    }

    /// Compiles each operand once and builds `body` over short references to
    /// their values and types. Operators mention an operand several times
    /// (null guards, type gates, the operation itself), and a derived type
    /// repeats its value again, so inlining operand SQL made the generated
    /// text grow exponentially with expression depth. Non-trivial operands
    /// are instead computed in a correlated scalar-subquery scope:
    /// `(SELECT body FROM (SELECT b.v, type(b.v) AS t FROM (SELECT value AS v) AS b) AS s)`.
    /// Leaves are cheap to repeat and stay inline, as do aggregates, which
    /// must remain in the grouping query rather than move into a subquery.
    /// Compiles a `json_each` body, returning the SQL and the sources that
    /// `rand()` calls inside it need joined to that body's FROM clause.
    fn capture_sources<T>(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<(T, String)> {
        self.local_sources.push(Vec::new());
        let result = body(self);
        let sources = self.local_sources.pop().unwrap_or_default();
        let joined = sources
            .iter()
            .map(|source| format!(" CROSS JOIN {source}"))
            .collect::<String>();
        Ok((result?, joined))
    }

    /// A column of the current row, used to correlate a body's random
    /// source with the enclosing row so SQLite re-evaluates it per row.
    fn row_anchor(&self) -> Option<String> {
        let mut entities = self
            .entities
            .iter()
            .map(|(slot, source)| {
                let sql = match source {
                    EntitySource::Table { alias, .. } => format!("{alias}.id"),
                    EntitySource::Encoded { sql, .. } => sql.clone(),
                };
                (slot.0, sql)
            })
            .collect::<Vec<_>>();
        entities.sort();
        if let Some((_, sql)) = entities.into_iter().next() {
            return Some(sql);
        }
        let mut scalars = self
            .scalars
            .iter()
            .map(|(slot, sql)| (slot.0, sql.clone()))
            .collect::<Vec<_>>();
        scalars.sort();
        if let Some((_, sql)) = scalars.into_iter().next() {
            return Some(sql);
        }
        if let Some((_, sql)) = self.projected_scalars.first() {
            return Some(sql.clone());
        }
        let mut elements = self
            .list_elements
            .iter()
            .map(|(variable, (value, _))| (variable.0, value.clone()))
            .collect::<Vec<_>>();
        elements.sort();
        elements.into_iter().next().map(|(_, sql)| sql)
    }

    fn scoped(
        &mut self,
        operands: &[&Expr],
        body: impl FnOnce(&mut Self, &[(String, String)]) -> Result<String>,
    ) -> Result<String> {
        let mut compiled = Vec::with_capacity(operands.len());
        for operand in operands {
            let scalar = self.compile_scalar(operand)?;
            compiled.push((scalar, is_inline_operand(operand)));
        }
        self.scoped_scalars(compiled, body)
    }

    fn scoped_scalars(
        &mut self,
        operands: Vec<(Scalar, bool)>,
        body: impl FnOnce(&mut Self, &[(String, String)]) -> Result<String>,
    ) -> Result<String> {
        let scope = self.scopes;
        self.scopes += 1;
        let mut values = Vec::new();
        let mut typed = Vec::new();
        let mut parts = Vec::with_capacity(operands.len());
        for (index, (scalar, inline)) in operands.into_iter().enumerate() {
            if inline {
                parts.push(scalar.into_parts());
                continue;
            }
            let value = format!("v{scope}_{index}");
            let value_type = format!("t{scope}_{index}");
            let bound_type = scalar.type_of(&format!("b{scope}.{value}"));
            values.push(format!("({}) AS {value}", scalar.value));
            let raw = scalar.raw_value(&format!("b{scope}.{value}"));
            typed.push(format!("{raw} AS {value}, ({bound_type}) AS {value_type}"));
            parts.push((
                format!("s{scope}.{value}"),
                format!("s{scope}.{value_type}"),
            ));
        }
        let body = body(self, &parts)?;
        if values.is_empty() {
            return Ok(body);
        }
        Ok(format!(
            "(SELECT {body} FROM (SELECT {} FROM (SELECT {}) AS b{scope}) AS s{scope})",
            typed.join(", "),
            values.join(", ")
        ))
    }

    fn predicate_expression(&mut self, expression: &Expr) -> Result<String> {
        self.boolean_expression(expression)
    }

    fn boolean_expression(&mut self, expression: &Expr) -> Result<String> {
        // Comparisons, logical connectives, string predicates, membership,
        // and null tests already evaluate to SQLite's three-valued 1/0/NULL,
        // so they need no type gate. Skipping it matters: the gate repeats
        // its operand several times, which made a chain of `n` connectives
        // grow the generated SQL by a factor of about ten per term.
        if is_native_boolean(expression) {
            return self.compile_scalar(expression).map(|scalar| scalar.value);
        }
        self.scoped(&[expression], |_, parts| {
            let (value, value_type) = &parts[0];
            // Retain the value expression as well as its type gate. Most
            // dynamic types derive from the value, but aggregates can have a
            // static result type; dropping their value here would also drop
            // argument bindings.
            let retained_type =
                format!("CASE WHEN ({value}) IS NULL THEN ({value_type}) ELSE ({value_type}) END");
            Ok(boolean_value(&retained_type))
        })
    }

    fn scalar_parts(&mut self, expression: &Expr) -> Result<(String, String)> {
        self.compile_scalar(expression).map(Scalar::into_parts)
    }

    /// Materialize ordered path members only when an expression needs entity
    /// records. Bare path-list outputs still use batch hydration.
    fn path_entity_list(&mut self, path: PathId, kind: PathFunctionKind) -> Result<Scalar> {
        let source = self.path_source(path)?.to_owned();
        let scope = self.scopes;
        self.scopes += 1;
        let segment = format!("pes{scope}");
        let member = format!("pem{scope}");
        let entity = format!("pee{scope}");
        let (field, table, record, filter) = match kind {
            PathFunctionKind::Nodes => (
                "nodes",
                "nodes",
                format!(
                    "json_object('id', {entity}.id, 'labels', json({entity}.labels), 'props', json({entity}.props))"
                ),
                format!("AND ({segment}.key = 0 OR {member}.key > 0)"),
            ),
            PathFunctionKind::Relationships => (
                "relationships",
                "edges",
                format!(
                    "json_object('id', {entity}.id, 'type', {entity}.edge_type, 'from', {entity}.from_node, 'to', {entity}.to_node, 'props', json({entity}.props))"
                ),
                String::new(),
            ),
            PathFunctionKind::Length => unreachable!("length is scalar"),
        };
        Ok(Scalar::nullable(
            format!(
                "CASE WHEN ({source}) IS NULL OR json_type(({source}), '$') = 'null' THEN NULL ELSE \
             (SELECT json_group_array(json({record}) ORDER BY {segment}.key, {member}.key) \
              FROM json_each(({source}), '$.segments') AS {segment}, \
                   json_each({segment}.value, '$.{field}') AS {member} \
              JOIN {table} AS {entity} ON {entity}.id = {member}.value {filter}) END"
            ),
            "array",
        ))
    }

    fn computed_entity_access(&mut self, entity: &Expr, access: &EntityAccess) -> Result<Scalar> {
        let entity_scalar = self.compile_scalar(entity)?;
        if let EntityAccess::Property(key) = access {
            let encoded = self.scoped_scalars(vec![(entity_scalar, false)], |_, parts| {
                Ok(format!("json_extract({}, '$.props')", parts[0].0))
            })?;
            return Ok(Scalar::from(self.json_property_parts(encoded, key)));
        }
        if let EntityAccess::DynamicProperty(key) = access {
            let key_scalar = self.compile_scalar(key)?;
            let operands = vec![(entity_scalar, false), (key_scalar, is_inline_operand(key))];
            let read = |function: &str, parts: &[(String, String)]| {
                let (key, key_type) = &parts[1];
                format!(
                    "{function}(json_extract({}, '$.props'), CASE WHEN ({key_type}) = 'text' THEN '$.' || json_quote({key}) END)",
                    parts[0].0
                )
            };
            let value =
                self.scoped_scalars(operands.clone(), |_, parts| Ok(read("json_extract", parts)))?;
            let kind = self.scoped_scalars(operands, |_, parts| Ok(read("json_type", parts)))?;
            return Ok(Scalar::sql(value, kind));
        }
        let value = self.scoped_scalars(vec![(entity_scalar, false)], |this, parts| {
            let entity = &parts[0].0;
            let id = format!("json_extract({entity}, '$.id')");
            let field = |name: &str| format!("json_extract({entity}, '$.{name}')");
            Ok(match access {
                EntityAccess::ElementId | EntityAccess::Identity(_) => id,
                EntityAccess::Labels => field("labels"),
                EntityAccess::RelationshipType => field("type"),
                EntityAccess::Properties | EntityAccess::Keys => {
                    let alias = format!("cep{}", this.scopes);
                    this.scopes += 1;
                    let aggregate = if matches!(access, EntityAccess::Keys) {
                        format!("json_group_array({alias}.key)")
                    } else {
                        format!("json_group_object({alias}.key, json({}))", element_json(&alias))
                    };
                    format!("CASE WHEN {id} IS NOT NULL THEN (SELECT {aggregate} FROM json_each({}) AS {alias} WHERE {alias}.type <> 'null') END", field("props"))
                }
                EntityAccess::HasLabel(label) => {
                    let label = this.push_binding(SqlParameter::Text(label.clone()));
                    format!("CASE WHEN {id} IS NOT NULL THEN EXISTS (SELECT 1 FROM json_each({}) WHERE value = {label}) END", field("labels"))
                }
                EntityAccess::Property(_) | EntityAccess::DynamicProperty(_) => unreachable!(),
            })
        })?;
        Ok(match access {
            EntityAccess::Identity(EntityKind::Node) => Scalar::nullable(value, "node"),
            EntityAccess::Identity(EntityKind::Relationship) => {
                Scalar::nullable(value, "relationship")
            }
            EntityAccess::Labels | EntityAccess::Keys => Scalar::nullable(value, "array"),
            EntityAccess::Properties => Scalar::nullable(value, "object"),
            EntityAccess::HasLabel(_) => Scalar::boolean(value),
            _ => Scalar::nullable(value, "text"),
        })
    }

    fn compile_scalar(&mut self, expression: &Expr) -> Result<Scalar> {
        if let Some((_, encoded)) = self
            .projected_scalars
            .iter()
            .find(|(projected, _)| projected == expression)
        {
            return Ok(Scalar::sql(
                format!("json_extract({encoded}, '$')"),
                format!("json_type({encoded}, '$')"),
            ));
        }
        if let Expr::PathFunction { kind, path } = expression {
            // Grouped paths are tagged values. Derive their functions from
            // the retained representative after aggregation drops raw paths.
            let path_value = super::nested::materialize_path_value(Expr::Path(*path));
            if self
                .projected_scalars
                .iter()
                .any(|(value, _)| *value == path_value)
            {
                use super::nested::{Op, call};
                let op = match kind {
                    PathFunctionKind::Length => Op::Length,
                    PathFunctionKind::Nodes => Op::PathNodes,
                    PathFunctionKind::Relationships => Op::PathRelationships,
                };
                let value = call(op, vec![path_value]);
                return self.compile_scalar(&if *kind == PathFunctionKind::Length {
                    value
                } else {
                    call(Op::EntityList, vec![value])
                });
            }
            let list = super::nested::materialize_path_value(expression.clone());
            if self
                .projected_scalars
                .iter()
                .any(|(value, _)| *value == list)
            {
                return self.compile_scalar(&super::nested::call(
                    super::nested::Op::EntityList,
                    vec![list],
                ));
            }
        }
        match expression {
            Expr::NestedCall { .. } => Ok(Scalar {
                value: self.encoded_nested(expression)?,
                kind: ScalarType::Encoded,
            }),
            Expr::Nested { expression, .. }
            | Expr::EntityList { expression, .. }
            | Expr::EntityValue { expression, .. } => self.compile_scalar(expression),
            Expr::EntityAccess { entity, access } => self.computed_entity_access(entity, access),
            Expr::Entity(_) => Err(Error::Unsupported(
                "graph entities in scalar expressions".to_owned(),
            )),
            Expr::Path(_) => Err(Error::Unsupported(
                "path values in scalar expressions".to_owned(),
            )),
            Expr::PathFunction { kind, path } => match kind {
                PathFunctionKind::Length => {
                    let source = self.path_source(*path)?;
                    Ok(Scalar::sql(
                        format!(
                            "CASE WHEN ({source}) IS NULL \
                                      OR json_type(({source}), '$') = 'null' \
                             THEN NULL ELSE \
                               (SELECT COALESCE(SUM(json_array_length(\
                                   path_segment.value, '$.relationships')), 0) \
                                FROM json_each(({source}), '$.segments') AS path_segment) \
                             END"
                        ),
                        "'integer'",
                    ))
                }
                PathFunctionKind::Nodes | PathFunctionKind::Relationships => {
                    self.path_entity_list(*path, *kind)
                }
            },
            Expr::Scalar(slot) => {
                let source = self.scalars.get(slot).cloned().ok_or_else(|| {
                    Error::Data(format!("scalar slot {} has no SQL source", slot.0))
                })?;
                Ok(Scalar::sql(
                    format!("json_extract({source}, '$')"),
                    format!("json_type({source}, '$')"),
                ))
            }
            Expr::Value(value) => {
                let json = self.push_binding(SqlParameter::Json(value.clone()));
                let json = format!("json({json})");
                Ok(Scalar::sql(
                    format!("json_extract({json}, '$')"),
                    format!("json_type({json}, '$')"),
                ))
            }
            Expr::Property(slot, key) => self.entity_property_parts(*slot, key).map(Scalar::from),
            Expr::ElementId(slot) => Ok(Scalar::sql(self.entity_field(*slot, "id")?, "'text'")),
            Expr::EntityIdentity(slot) => {
                let kind = match self.entity_kind(*slot)? {
                    EntityKind::Node => "node",
                    EntityKind::Relationship => "relationship",
                };
                Ok(Scalar::nullable(self.entity_field(*slot, "id")?, kind))
            }
            Expr::RelationshipType(slot) => {
                Ok(Scalar::sql(self.entity_field(*slot, "type")?, "'text'"))
            }
            Expr::Labels(slot) => {
                let id = self.entity_field(*slot, "id")?;
                let labels = self.entity_labels(*slot)?;
                Ok(Scalar::sql(
                    format!("CASE WHEN {id} IS NULL THEN NULL ELSE json({labels}) END"),
                    "'array'",
                ))
            }
            Expr::Exists {
                pattern,
                local,
                bindings,
            } => {
                let subquery = self.exists_subquery(pattern, local, bindings)?;
                Ok(Scalar::boolean(subquery))
            }
            Expr::HasLabel(slot, label) => {
                let id = self.entity_field(*slot, "id")?;
                let value = self.push_binding(SqlParameter::Text(label.clone()));
                let test = self.label_test(*slot, &value)?;
                Ok(Scalar::boolean(format!(
                    "(CASE WHEN {id} IS NULL THEN NULL ELSE ({test}) END)"
                )))
            }
            Expr::Binary {
                left,
                operator,
                right,
            } => {
                let predicate = match operator {
                    BinaryOp::Equal | BinaryOp::NotEqual => {
                        self.scoped(&[left, right], |_, parts| {
                            let (left_value, left_type) = &parts[0];
                            let (right_value, right_type) = &parts[1];
                            Ok(typed_sql_comparison(
                                left_value,
                                left_type,
                                right_value,
                                right_type,
                                *operator,
                            ))
                        })?
                    }
                    BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual => self.scoped(&[left, right], |_, parts| {
                        let (left_value, left_type) = &parts[0];
                        let (right_value, right_type) = &parts[1];
                        Ok(typed_sql_ordering(
                            left_value,
                            left_type,
                            right_value,
                            right_type,
                            *operator,
                        ))
                    })?,
                    BinaryOp::In => self.membership_expression(left, right)?,
                    BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
                        self.string_predicate(left, right, *operator)?
                    }
                    BinaryOp::Or | BinaryOp::And => {
                        let left = self.boolean_expression(left)?;
                        let right = self.boolean_expression(right)?;
                        let operator = match operator {
                            BinaryOp::Or => "OR",
                            BinaryOp::And => "AND",
                            _ => unreachable!(),
                        };
                        format!("({left} {operator} {right})")
                    }
                    BinaryOp::Xor => {
                        // Null if either side is null, else whether they
                        // differ. Each side is computed once in a scope.
                        let left = self.boolean_expression(left)?;
                        let right = self.boolean_expression(right)?;
                        format!(
                            "(SELECT CASE WHEN xr.l IS NULL OR xr.r IS NULL THEN NULL \
                             ELSE xr.l <> xr.r END \
                             FROM (SELECT ({left}) AS l, ({right}) AS r) AS xr)"
                        )
                    }
                    BinaryOp::Add
                    | BinaryOp::Subtract
                    | BinaryOp::Multiply
                    | BinaryOp::Divide
                    | BinaryOp::Remainder => {
                        return self.numeric_binary(left, right, *operator);
                    }
                };
                Ok(Scalar::boolean(predicate))
            }
            Expr::Unary { operator, operand } => match operator {
                UnaryOp::Not => {
                    let operand = self.boolean_expression(operand)?;
                    Ok(Scalar::boolean(format!("(NOT ({operand}))")))
                }
                UnaryOp::Plus | UnaryOp::Minus => self.numeric_unary(operand, *operator),
            },
            Expr::IsNull {
                expression,
                negated,
            } => {
                let expression = self.raw_expression(expression)?;
                Ok(Scalar::boolean(format!(
                    "(({expression}) IS {}NULL)",
                    if *negated { "NOT " } else { "" }
                )))
            }
            Expr::Aggregate {
                kind,
                argument,
                distinct,
            } => self.aggregate_parts(*kind, argument.as_deref(), *distinct),
            Expr::Quantified {
                kind,
                variable,
                list,
                predicate,
            } => self
                .quantified_expression(*kind, *variable, list, predicate)
                .map(Scalar::boolean),
            Expr::MapProperty { base, key } => {
                let path = self.push_binding(SqlParameter::Text(key.to_owned()));
                let json_path = format!("'$.' || json_quote({path})");
                let scalar = self.compile_scalar(base)?;
                let inline = is_inline_operand(base);
                // Only maps have keys. Null reads as null; any other kind is
                // a type error.
                let map = |(value, value_type): &(String, String)| {
                    format!(
                        "(CASE WHEN ({value_type}) = 'object' THEN ({value}) \
                          WHEN {} THEN NULL ELSE {} END)",
                        is_null_type(value_type),
                        type_error("property access needs a map, node, or relationship")
                    )
                };
                let value = self.scoped_scalars(vec![(scalar.clone(), inline)], |_, parts| {
                    Ok(format!("json_extract({}, {json_path})", map(&parts[0])))
                })?;
                let value_type = self.scoped_scalars(vec![(scalar, inline)], |_, parts| {
                    Ok(format!("json_type({}, {json_path})", map(&parts[0])))
                })?;
                Ok(Scalar::sql(value, value_type))
            }
            Expr::ListElement(variable) => self
                .list_elements
                .get(variable)
                .cloned()
                .map(Scalar::from)
                .ok_or_else(|| {
                    Error::Data(format!(
                        "list variable #l{} is referenced outside its quantifier",
                        variable.0
                    ))
                }),
            Expr::Size(operand)
                if matches!(operand.as_ref(), Expr::PathFunction {
                    kind: PathFunctionKind::Nodes | PathFunctionKind::Relationships,
                    path,
                } if self.paths.contains_key(path)) =>
            {
                // Retain Size in the logical expression for grouping and
                // projected-value substitution. A deferred list needs only
                // its path's hop count, without fetching entity records.
                let Expr::PathFunction { kind, path } = operand.as_ref() else {
                    unreachable!("matched a deferred path list");
                };
                let length = self.compile_scalar(&Expr::PathFunction {
                    kind: PathFunctionKind::Length,
                    path: *path,
                })?;
                let value = if *kind == PathFunctionKind::Nodes {
                    format!("(({}) + 1)", length.value)
                } else {
                    length.value
                };
                Ok(Scalar::sql(value, "'integer'"))
            }
            Expr::Size(operand) => self.composed(
                &[operand],
                |_, parts| {
                    let (value, value_type) = &parts[0];
                    // Lists count elements and strings count characters;
                    // null and every other kind read as null.
                    Ok(format!(
                        "(CASE WHEN ({value_type}) = 'array' THEN json_array_length(({value})) \
                          WHEN ({value_type}) = 'text' THEN length(({value})) END)"
                    ))
                },
                |types| {
                    format!(
                        "(CASE WHEN ({}) IN ('array', 'text') THEN 'integer' ELSE 'null' END)",
                        types[0]
                    )
                },
            ),
            Expr::ListComprehension {
                variable,
                list,
                predicate,
                projection,
            } => self.list_comprehension(
                *variable,
                list,
                predicate.as_deref(),
                projection.as_deref(),
            ),
            Expr::Case {
                operand,
                alternatives,
                otherwise,
            } => self.case_expression(
                operand.as_deref(),
                alternatives,
                otherwise.as_deref(),
                false,
            ),
            Expr::Function {
                function,
                arguments,
            } => self.scalar_function(*function, arguments),
            // Stored maps keep explicit-null entries (see semantics.md), but a
            // null property does not exist in Cypher, so keys() and
            // properties() leave those entries out.
            Expr::EntityKeys(slot) => {
                let id = self.entity_field(*slot, "id")?;
                let props = self.entity_props(*slot)?;
                let alias = format!("ek{}", self.scopes);
                self.scopes += 1;
                Ok(Scalar::nullable(
                    format!(
                        "(CASE WHEN {id} IS NOT NULL THEN \
                          (SELECT json_group_array({alias}.key) FROM json_each({props}) AS {alias} \
                           WHERE {alias}.type <> 'null') END)"
                    ),
                    "array",
                ))
            }
            Expr::EntityProperties(slot) => {
                let id = self.entity_field(*slot, "id")?;
                let props = self.entity_props(*slot)?;
                let alias = format!("ep{}", self.scopes);
                self.scopes += 1;
                let element = element_json(&alias);
                Ok(Scalar::nullable(
                    format!(
                        "(CASE WHEN {id} IS NOT NULL THEN \
                          (SELECT json_group_object({alias}.key, json({element})) \
                           FROM json_each({props}) AS {alias} WHERE {alias}.type <> 'null') END)"
                    ),
                    "object",
                ))
            }
            Expr::List(items) => {
                let items = items
                    .iter()
                    .map(|item| Ok(format!("json({})", self.encoded_expression(item)?)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Scalar::sql(
                    format!("json_array({})", items.join(", ")),
                    "'array'",
                ))
            }
            Expr::Map(entries) => {
                let entries = entries
                    .iter()
                    .map(|(key, value)| {
                        let key = self.push_binding(SqlParameter::Text(key.clone()));
                        Ok(format!("{key}, json({})", self.encoded_expression(value)?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Scalar::sql(
                    format!("json_object({})", entries.join(", ")),
                    "'object'",
                ))
            }
            Expr::Index { base, index } => {
                // An integer indexes a list (negative counts from the end)
                // and a string keys a map. A null base or index, an
                // out-of-range index, or a missing key reads as null; any
                // other kind of base or index is a type error.
                let base_scalar = self.compile_scalar(base)?;
                let index_scalar = self.compile_scalar(index)?;
                let operands = || {
                    vec![
                        (base_scalar.clone(), is_inline_operand(base)),
                        (index_scalar.clone(), is_inline_operand(index)),
                    ]
                };
                let path = |parts: &[(String, String)]| {
                    let (base, base_type) = &parts[0];
                    let (index, index_type) = &parts[1];
                    let path = format!(
                        "(CASE WHEN {} THEN NULL \
                          WHEN ({base_type}) = 'array' AND ({index_type}) = 'integer' THEN \
                              CASE WHEN ({index}) >= 0 THEN '$[' || ({index}) || ']' \
                                   ELSE '$[#' || ({index}) || ']' END \
                          WHEN ({base_type}) = 'object' AND ({index_type}) = 'text' THEN \
                              '$.' || json_quote(({index})) \
                          WHEN ({base_type}) = 'array' THEN {} \
                          WHEN ({base_type}) = 'object' THEN {} END)",
                        is_null_type(index_type),
                        type_error("a list index must be an integer"),
                        type_error("a map key must be a string"),
                    );
                    // Only JSON containers may reach json_extract; a string
                    // base would be rejected as malformed JSON.
                    let container = format!(
                        "(CASE WHEN ({base_type}) IN ('array', 'object') THEN ({base}) \
                          WHEN {} THEN NULL ELSE {} END)",
                        is_null_type(base_type),
                        type_error("only a list or a map can be indexed")
                    );
                    (container, path)
                };
                let value = self.scoped_scalars(operands(), |_, parts| {
                    let (base, path) = path(parts);
                    Ok(format!("json_extract({base}, {path})"))
                })?;
                let value_type = self.scoped_scalars(operands(), |_, parts| {
                    let (base, path) = path(parts);
                    Ok(format!("json_type({base}, {path})"))
                })?;
                Ok(Scalar::sql(value, value_type))
            }
            Expr::DynamicProperty(slot, key) => {
                let props = self.entity_props(*slot)?;
                let key_scalar = self.compile_scalar(key)?;
                let inline = is_inline_operand(key);
                let path = |parts: &[(String, String)]| {
                    let (key, key_type) = &parts[0];
                    format!(
                        "(CASE WHEN ({key_type}) = 'text' THEN '$.' || json_quote(({key})) END)"
                    )
                };
                let value = self
                    .scoped_scalars(vec![(key_scalar.clone(), inline)], |_, parts| {
                        Ok(format!("json_extract({props}, {})", path(parts)))
                    })?;
                let value_type = self.scoped_scalars(vec![(key_scalar, inline)], |_, parts| {
                    Ok(format!("json_type({props}, {})", path(parts)))
                })?;
                Ok(Scalar::sql(value, value_type))
            }
            Expr::Slice { base, lower, upper } => {
                let mut operands = vec![base.as_ref()];
                operands.extend(lower.as_deref());
                operands.extend(upper.as_deref());
                let invalid_bound = |bound_type: Option<&String>| {
                    bound_type.map_or_else(
                        || "0".to_owned(),
                        |bound_type| {
                            format!("(({bound_type}) IS NULL OR ({bound_type}) <> 'integer')")
                        },
                    )
                };
                let bound_types = |types: &[String]| {
                    let mut bounds = types[1..].iter();
                    let lower = lower.as_ref().and_then(|_| bounds.next());
                    let upper = upper.as_ref().and_then(|_| bounds.next());
                    format!("{} OR {}", invalid_bound(lower), invalid_bound(upper))
                };
                let value = |this: &mut Self, parts: &[(String, String)]| {
                    let (list, list_type) = &parts[0];
                    let types = parts.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>();
                    let mut bounds = parts[1..].iter();
                    let lower = lower.as_ref().and_then(|_| bounds.next());
                    let upper = upper.as_ref().and_then(|_| bounds.next());
                    let alias = format!("sl{}", this.scopes);
                    let length = format!("json_array_length(({list}))");
                    // Negative bounds count from the end and every bound is
                    // clamped to the list, so an inverted or exceeding range
                    // simply yields fewer elements.
                    let normalize = |(bound, _): &(String, String)| {
                        format!(
                            "(CASE WHEN ({bound}) < 0 THEN max({length} + ({bound}), 0) \
                              ELSE ({bound}) END)"
                        )
                    };
                    let from = lower.map_or_else(|| "0".to_owned(), normalize);
                    let to = upper.map_or_else(|| length.clone(), normalize);
                    let element = element_json(&alias);
                    Ok(format!(
                        "(CASE WHEN ({list_type}) IS NULL OR ({list_type}) <> 'array' \
                               OR {} THEN NULL \
                          ELSE (SELECT json_group_array(json({element}) ORDER BY {alias}.key) \
                                FROM json_each(({list})) AS {alias} \
                                WHERE {alias}.key >= {from} AND {alias}.key < {to}) END)",
                        bound_types(&types),
                    ))
                };
                self.composed(&operands, value, |types| {
                    format!(
                        "(CASE WHEN ({}) = 'array' AND NOT ({}) THEN 'array' ELSE 'null' END)",
                        types[0],
                        bound_types(types)
                    )
                })
            }
            Expr::Range { start, end, step } => {
                let mut operands = vec![start.as_ref(), end.as_ref()];
                operands.extend(step.as_deref());
                // A null argument makes the range null; any other
                // non-integer argument is an argument error.
                let any_null = |types: &[String]| {
                    types
                        .iter()
                        .map(|t| is_null_type(t))
                        .collect::<Vec<_>>()
                        .join(" OR ")
                };
                let all_integers = |types: &[String]| {
                    types
                        .iter()
                        .map(|t| format!("({t}) = 'integer'"))
                        .collect::<Vec<_>>()
                        .join(" AND ")
                };
                let invalid = "crabnet_raise('range() arguments must be integers')";
                let value = |this: &mut Self, parts: &[(String, String)]| {
                    let (start, _) = &parts[0];
                    let (end, _) = &parts[1];
                    let step = parts
                        .get(2)
                        .map_or_else(|| "1".to_owned(), |(step, _)| step.clone());
                    let types = parts.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>();
                    let alias = format!("rg{}", this.scopes);
                    // A step pointing away from the end yields an empty list;
                    // a zero step can never finish, so it is an error.
                    Ok(format!(
                        "(CASE WHEN {} THEN NULL WHEN NOT ({}) THEN {invalid} \
                          WHEN ({step}) = 0 THEN crabnet_raise('range() step must not be zero') \
                          ELSE (SELECT json_group_array(v) FROM \
                                (WITH RECURSIVE {alias}(v) AS \
                                   (SELECT ({start}) \
                                    UNION ALL SELECT v + ({step}) FROM {alias} \
                                    WHERE (({step}) > 0 AND v + ({step}) <= ({end})) \
                                       OR (({step}) < 0 AND v + ({step}) >= ({end}))) \
                                 SELECT v FROM {alias} \
                                 WHERE (({step}) > 0 AND v <= ({end})) \
                                    OR (({step}) < 0 AND v >= ({end})))) END)",
                        any_null(&types),
                        all_integers(&types)
                    ))
                };
                self.composed(&operands, value, |types| {
                    format!(
                        "(CASE WHEN {} THEN 'null' WHEN {} THEN 'array' ELSE {invalid} END)",
                        any_null(types),
                        all_integers(types)
                    )
                })
            }
        }
    }

    /// Compiles an operator whose result type follows from its operand
    /// types alone. The value is built in a scope over the operands; the
    /// type is composed from each operand's standalone type SQL, so it never
    /// repeats an operand's value and nested operators stay linear.
    fn composed(
        &mut self,
        operands: &[&Expr],
        value: impl FnOnce(&mut Self, &[(String, String)]) -> Result<String>,
        value_type: impl FnOnce(&[String]) -> String,
    ) -> Result<Scalar> {
        let mut compiled = Vec::with_capacity(operands.len());
        let mut types = Vec::with_capacity(operands.len());
        let mut all_inline = true;
        for operand in operands {
            let scalar = self.compile_scalar(operand)?;
            let inline = is_inline_operand(operand);
            all_inline &= inline;
            types.push(self.standalone_type(&scalar, inline)?);
            compiled.push((scalar, inline));
        }
        let value = self.scoped_scalars(compiled, value)?;
        if all_inline {
            return Ok(Scalar::sql(value, value_type(&types)));
        }
        // The composition mentions each operand type several times, so
        // non-trivial types are bound once in a scope of their own; otherwise
        // a chain of operators would multiply its type text at every level.
        let scope = self.scopes;
        self.scopes += 1;
        let names = (0..types.len())
            .map(|index| format!("ts{scope}.t{index}"))
            .collect::<Vec<_>>();
        let bound = types
            .iter()
            .enumerate()
            .map(|(index, value_type)| format!("({value_type}) AS t{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Scalar::sql(
            value,
            format!(
                "(SELECT {} FROM (SELECT {bound}) AS ts{scope})",
                value_type(&names)
            ),
        ))
    }

    /// Type SQL usable outside a scope. A derived type of a non-trivial
    /// operand is computed in its own scope so the operand's value appears
    /// once rather than the two or three times its derivation needs.
    fn standalone_type(&mut self, scalar: &Scalar, inline: bool) -> Result<String> {
        match &scalar.kind {
            ScalarType::Sql(sql) => Ok(sql.clone()),
            _ if inline => Ok(scalar.type_of(&scalar.value)),
            _ => self.scoped_scalars(vec![(scalar.clone(), false)], |_, parts| {
                Ok(parts[0].1.clone())
            }),
        }
    }

    /// Compiles a built-in scalar function. Arguments of the wrong kind
    /// yield null. Results with one possible type use the `Nullable` kind so
    /// their type follows from the value without repeating the argument.
    fn scalar_function(&mut self, function: ScalarFunction, arguments: &[Expr]) -> Result<Scalar> {
        let operands = arguments.iter().collect::<Vec<_>>();
        let numeric = |value_type: &str| format!("({value_type}) IN ('integer', 'real')");
        match function {
            // random() is a signed 64-bit integer; its top 53 bits become a
            // real in [0, 1). SQLite evaluates an uncorrelated scalar subquery
            // once per statement, so the draw is not left inline: each call
            // site becomes a one-row table-valued source joined after the row
            // sources, and the value is read from it by column.
            ScalarFunction::Rand => {
                let scaled =
                    |draw: &str| format!("((({draw}) >> 11) * 1.1102230246251565e-16 + 0.5)");
                if self.inline_random {
                    return Ok(Scalar::sql(scaled("random()"), "'real'"));
                }
                let alias = format!("rnd{}", self.scopes);
                self.scopes += 1;
                if self.local_sources.is_empty() {
                    self.random_sources
                        .push(format!("json_each(json_array(random())) AS {alias}"));
                } else {
                    // A body source mentions the enclosing row so the body
                    // subquery is correlated and re-run per row.
                    let draw = self
                        .row_anchor()
                        .map(|anchor| {
                            format!("CASE WHEN ({anchor}) IS NULL THEN random() ELSE random() END")
                        })
                        .unwrap_or_else(|| "random()".to_owned());
                    // Keep one array member. Adding the anchor as a second
                    // member would multiply each comprehension input row.
                    let source = format!("json_each(json_array({draw})) AS {alias}");
                    if let Some(local) = self.local_sources.last_mut() {
                        local.push(source);
                    }
                }
                Ok(Scalar::sql(scaled(&format!("{alias}.value")), "'real'"))
            }
            ScalarFunction::Pi => Ok(Scalar::sql("3.141592653589793", "'real'")),
            ScalarFunction::E => Ok(Scalar::sql("2.718281828459045", "'real'")),
            ScalarFunction::Abs => self.composed(
                &operands,
                |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE WHEN {} THEN abs(({value})) END)",
                        numeric(value_type)
                    ))
                },
                |types| {
                    format!(
                        "(CASE WHEN {} THEN ({}) ELSE 'null' END)",
                        numeric(&types[0]),
                        types[0]
                    )
                },
            ),
            ScalarFunction::Sign => {
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE WHEN {} THEN sign(({value})) END)",
                        numeric(value_type)
                    ))
                })?;
                Ok(Scalar::nullable(value, "integer"))
            }
            ScalarFunction::Ceil
            | ScalarFunction::Floor
            | ScalarFunction::Round
            | ScalarFunction::Sqrt
            | ScalarFunction::Exp
            | ScalarFunction::Log
            | ScalarFunction::Log10
            | ScalarFunction::Sin
            | ScalarFunction::Cos
            | ScalarFunction::Tan
            | ScalarFunction::Asin
            | ScalarFunction::Acos
            | ScalarFunction::Atan => {
                // round() is built in; the rest are registered by the
                // database since the bundled SQLite omits its math library.
                let sql_function = match function {
                    ScalarFunction::Round => "round".to_owned(),
                    ScalarFunction::Log => "crabnet_ln".to_owned(),
                    other => format!("crabnet_{}", other.name()),
                };
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE WHEN {} THEN CAST({sql_function}(({value})) AS REAL) END)",
                        numeric(value_type)
                    ))
                })?;
                Ok(Scalar::nullable(value, "real"))
            }
            ScalarFunction::ToInteger => {
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE ({value_type}) WHEN 'integer' THEN ({value}) \
                          WHEN 'real' THEN CAST(({value}) AS INTEGER) \
                          WHEN 'true' THEN 1 WHEN 'false' THEN 0 \
                          WHEN 'text' THEN crabnet_to_integer(({value})) \
                          WHEN 'array' THEN {invalid} WHEN 'object' THEN {invalid} END)",
                        invalid = type_error("toInteger() cannot convert a list or a map")
                    ))
                })?;
                Ok(Scalar::nullable(value, "integer"))
            }
            ScalarFunction::ToFloat => {
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE WHEN {} THEN NULL ELSE (CASE ({value_type}) \
                          WHEN 'real' THEN ({value}) \
                          WHEN 'integer' THEN CAST(({value}) AS REAL) \
                          WHEN 'text' THEN crabnet_to_float(({value})) ELSE {} END) END)",
                        is_null_type(value_type),
                        type_error("toFloat() converts only numbers and strings")
                    ))
                })?;
                Ok(Scalar::nullable(value, "real"))
            }
            ScalarFunction::ToString => {
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE ({value_type}) WHEN 'text' THEN ({value}) \
                          WHEN 'integer' THEN CAST(({value}) AS TEXT) \
                          WHEN 'real' THEN crabnet_json_real(({value})) \
                          WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' \
                          WHEN 'array' THEN {invalid} WHEN 'object' THEN {invalid} END)",
                        invalid = type_error("toString() cannot convert a list or a map")
                    ))
                })?;
                Ok(Scalar::nullable(value, "text"))
            }
            ScalarFunction::ToBoolean => {
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE ({value_type}) WHEN 'true' THEN 1 WHEN 'false' THEN 0 \
                          WHEN 'integer' THEN (({value}) <> 0) \
                          WHEN 'text' THEN (CASE lower(({value})) \
                              WHEN 'true' THEN 1 WHEN 'false' THEN 0 END) \
                          WHEN 'real' THEN {invalid} WHEN 'array' THEN {invalid} \
                          WHEN 'object' THEN {invalid} END)",
                        invalid =
                            type_error("toBoolean() converts only booleans, integers, and strings")
                    ))
                })?;
                Ok(Scalar::boolean(value))
            }
            ScalarFunction::ToLower
            | ScalarFunction::ToUpper
            | ScalarFunction::Trim
            | ScalarFunction::LTrim
            | ScalarFunction::RTrim => {
                let sql_function = match function {
                    ScalarFunction::ToLower => "lower",
                    ScalarFunction::ToUpper => "upper",
                    other => other.name(),
                };
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE WHEN ({value_type}) = 'text' THEN {sql_function}(({value})) END)"
                    ))
                })?;
                Ok(Scalar::nullable(value, "text"))
            }
            ScalarFunction::Reverse => self.composed(
                &operands,
                |this, parts| {
                    let (value, value_type) = &parts[0];
                    let alias = format!("rv{}", this.scopes);
                    let element = element_json(&alias);
                    Ok(format!(
                        "(CASE WHEN ({value_type}) = 'text' THEN crabnet_reverse(({value})) \
                          WHEN ({value_type}) = 'array' THEN \
                              (SELECT json_group_array(json({element}) ORDER BY {alias}.key DESC) \
                               FROM json_each(({value})) AS {alias}) END)"
                    ))
                },
                |types| {
                    format!(
                        "(CASE WHEN ({}) IN ('text', 'array') THEN ({}) ELSE 'null' END)",
                        types[0], types[0]
                    )
                },
            ),
            ScalarFunction::Substring => {
                let value = self.scoped(&operands, |_, parts| {
                    let (text, text_type) = &parts[0];
                    let (start, start_type) = &parts[1];
                    let (length_condition, length) = match parts.get(2) {
                        Some((length, length_type)) => (
                            format!(" AND ({length_type}) = 'integer' AND ({length}) >= 0"),
                            format!(", ({length})"),
                        ),
                        None => (String::new(), String::new()),
                    };
                    // Cypher rejects negative positions; they read as null.
                    Ok(format!(
                        "(CASE WHEN ({text_type}) = 'text' AND ({start_type}) = 'integer' \
                          AND ({start}) >= 0{length_condition} \
                          THEN substr(({text}), ({start}) + 1{length}) END)"
                    ))
                })?;
                Ok(Scalar::nullable(value, "text"))
            }
            ScalarFunction::Left | ScalarFunction::Right => {
                let value = self.scoped(&operands, |_, parts| {
                    let (text, text_type) = &parts[0];
                    let (count, count_type) = &parts[1];
                    let taken = if function == ScalarFunction::Left {
                        format!("substr(({text}), 1, ({count}))")
                    } else {
                        format!(
                            "(CASE WHEN ({count}) <= 0 THEN '' ELSE substr(({text}), -({count})) END)"
                        )
                    };
                    Ok(format!(
                        "(CASE WHEN ({text_type}) = 'text' AND ({count_type}) = 'integer' \
                          THEN {taken} END)"
                    ))
                })?;
                Ok(Scalar::nullable(value, "text"))
            }
            ScalarFunction::Replace => {
                let value = self.scoped(&operands, |_, parts| {
                    let (text, text_type) = &parts[0];
                    let (search, search_type) = &parts[1];
                    let (replacement, replacement_type) = &parts[2];
                    Ok(format!(
                        "(CASE WHEN ({text_type}) = 'text' AND ({search_type}) = 'text' \
                          AND ({replacement_type}) = 'text' \
                          THEN replace(({text}), ({search}), ({replacement})) END)"
                    ))
                })?;
                Ok(Scalar::nullable(value, "text"))
            }
            ScalarFunction::Split => {
                let value = self.scoped(&operands, |_, parts| {
                    let (text, text_type) = &parts[0];
                    let (delimiter, delimiter_type) = &parts[1];
                    Ok(format!(
                        "(CASE WHEN ({text_type}) = 'text' AND ({delimiter_type}) = 'text' \
                          THEN crabnet_split(({text}), ({delimiter})) END)"
                    ))
                })?;
                Ok(Scalar::nullable(value, "array"))
            }
            ScalarFunction::Coalesce => {
                // SQLite's COALESCE is lazy. Pick encoded JSON so its type
                // travels with its value, and evaluate each argument only
                // when the preceding values were null.
                let arguments = arguments
                    .iter()
                    .map(|argument| {
                        self.conditional_expression(|this| this.encoded_expression(argument))
                            .map(|value| format!("nullif(({value}), 'null')"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Scalar {
                    value: format!("coalesce({}, 'null')", arguments.join(", ")),
                    kind: ScalarType::Encoded,
                })
            }
            ScalarFunction::Keys => {
                let value = self.scoped(&operands, |this, parts| {
                    let (value, value_type) = &parts[0];
                    let alias = format!("mk{}", this.scopes);
                    Ok(format!(
                        "(CASE WHEN ({value_type}) = 'object' THEN \
                          (SELECT json_group_array({alias}.key) FROM json_each(({value})) AS {alias}) \
                          WHEN {} THEN NULL ELSE {} END)",
                        is_null_type(value_type),
                        type_error("keys() needs a map, node, or relationship")
                    ))
                })?;
                Ok(Scalar::nullable(value, "array"))
            }
            ScalarFunction::Properties => {
                let value = self.scoped(&operands, |_, parts| {
                    let (value, value_type) = &parts[0];
                    Ok(format!(
                        "(CASE WHEN ({value_type}) = 'object' THEN ({value}) \
                          WHEN {} THEN NULL ELSE {} END)",
                        is_null_type(value_type),
                        type_error("properties() needs a map, node, or relationship")
                    ))
                })?;
                Ok(Scalar::nullable(value, "object"))
            }
        }
    }

    /// Keep random sources inside the conditional branch that uses them.
    /// The surrounding row anchor makes these local sources run per row.
    fn conditional_expression(
        &mut self,
        compile: impl FnOnce(&mut Self) -> Result<String>,
    ) -> Result<String> {
        let (value, sources) = self.capture_sources(compile)?;
        Ok(if sources.is_empty() {
            value
        } else {
            format!("(SELECT ({value}) FROM (SELECT 1){sources})")
        })
    }

    /// Select a branch lazily. Binding only the simple CASE operand avoids
    /// hoisting later conditions out of their conditional scopes. Encoded
    /// results carry their type with their value; scalar operators can instead
    /// request separate value/type SQL to keep nested arithmetic linear.
    fn case_expression(
        &mut self,
        operand: Option<&Expr>,
        alternatives: &[CaseAlternative],
        otherwise: Option<&Expr>,
        encoded: bool,
    ) -> Result<Scalar> {
        let graph = matches!(operand, Some(Expr::Nested { .. }));
        let compiled = operand
            .map(|value| self.compile_scalar(value).map(|scalar| (scalar, false)))
            .transpose()?
            .into_iter()
            .collect();
        let branch = self.scoped_scalars(compiled, |this, operand| {
            let mut arms = String::new();
            for (index, alternative) in alternatives.iter().enumerate() {
                for condition in &alternative.conditions {
                    let condition = if let Some((left, left_type)) = operand.first() {
                        this.scoped(&[condition], |_, parts| {
                            let (right, right_type) = &parts[0];
                            Ok(if graph {
                                format!(
                                    "json_extract(crabnet_nested('equal', {}, {}), '$')",
                                    encoded_scalar(left, left_type),
                                    encoded_scalar(right, right_type)
                                )
                            } else {
                                typed_sql_comparison(
                                    left,
                                    left_type,
                                    right,
                                    right_type,
                                    BinaryOp::Equal,
                                )
                            })
                        })
                    } else {
                        this.boolean_expression(condition)
                    }?;
                    // Separate WHEN arms also short-circuit comma-separated
                    // conditions without duplicating the result expression.
                    arms.push_str(&format!(" WHEN ({condition}) THEN {index}"));
                }
            }
            Ok(format!("(CASE{arms} ELSE -1 END)"))
        })?;
        let scope = self.scopes;
        self.scopes += 1;
        let pick = |choices: Vec<String>, default: &str| {
            let otherwise = choices
                .get(alternatives.len())
                .map_or(default, String::as_str);
            let arms = choices[..alternatives.len()]
                .iter()
                .enumerate()
                .map(|(index, value)| format!(" WHEN {index} THEN ({value})"))
                .collect::<String>();
            format!(
                "(SELECT CASE cs{scope}.k{arms} ELSE ({otherwise}) END FROM (SELECT ({branch}) AS k) AS cs{scope})"
            )
        };
        let results = alternatives.iter().map(|a| &a.result).chain(otherwise);
        if encoded {
            let choices = results
                .map(|value| self.conditional_expression(|this| this.encoded_expression(value)))
                .collect::<Result<Vec<_>>>()?;
            return Ok(Scalar {
                value: pick(choices, "'null'"),
                kind: ScalarType::Encoded,
            });
        }
        // Scalar operators can ask for the value and type separately. Keep
        // their branch types separate from their values so arithmetic around
        // nested CASE results does not double the SQL at every level.
        let mut values = Vec::new();
        let mut types = Vec::new();
        for result in results {
            let scalar = self.compile_scalar(result)?;
            types.push(self.standalone_type(&scalar, is_inline_operand(result))?);
            values.push(scalar.raw_value(&scalar.value));
        }
        Ok(Scalar::sql(pick(values, "NULL"), pick(types, "'null'")))
    }

    /// Compiles `[x IN list WHERE predicate | projection]` to a correlated
    /// `json_each` scan that keeps the elements whose predicate is true and
    /// collects their projections, in list order, back into a JSON array.
    fn list_comprehension(
        &mut self,
        variable: ListVariableId,
        list: &Expr,
        predicate: Option<&Expr>,
        projection: Option<&Expr>,
    ) -> Result<Scalar> {
        let value = |this: &mut Self, parts: &[(String, String)]| {
            let (list_value, list_type) = &parts[0];
            let alias = format!("q{}", variable.0);
            let previous = this.list_elements.insert(
                variable,
                (format!("{alias}.value"), format!("{alias}.type")),
            );
            let compiled = this.capture_sources(|this| {
                let filter = match predicate {
                    Some(predicate) => format!(" WHERE ({})", this.boolean_expression(predicate)?),
                    None => String::new(),
                };
                let element = match projection {
                    Some(projection) => this.output_expression(projection)?.0,
                    None => element_json(&alias),
                };
                Ok::<_, Error>((filter, element))
            });
            match previous {
                Some(previous) => {
                    this.list_elements.insert(variable, previous);
                }
                None => {
                    this.list_elements.remove(&variable);
                }
            }
            let ((filter, element), sources) = compiled?;
            Ok(format!(
                "(CASE WHEN ({list_type}) IS NULL OR ({list_type}) <> 'array' THEN NULL \
                 ELSE (SELECT json_group_array(json({element}) ORDER BY {alias}.key) \
                       FROM json_each({list_value}) AS {alias}{sources}{filter}) END)"
            ))
        };
        self.composed(&[list], value, |types| array_when_list(&types[0]))
    }

    fn aggregate_parts(
        &mut self,
        kind: AggregateKind,
        argument: Option<&Expr>,
        distinct: bool,
    ) -> Result<Scalar> {
        if kind == AggregateKind::Count && argument.is_none() {
            return Ok(Scalar::sql("COUNT(*)", "'integer'"));
        }
        let argument = argument.ok_or_else(|| {
            Error::Data("logical aggregate other than count() is missing its argument".into())
        })?;

        if matches!(argument, Expr::Nested { .. }) {
            let encoded = self.encoded_expression(argument)?;
            let key =
                format!("CASE WHEN ({encoded}) <> 'null' THEN crabnet_nested_key({encoded}) END");
            return match kind {
                AggregateKind::Collect => Ok(self.collect_scalar(encoded, key, distinct)),
                AggregateKind::Count => Ok(Scalar::sql(
                    format!("COUNT({}{key})", if distinct { "DISTINCT " } else { "" }),
                    "'integer'",
                )),
                _ => Err(Error::Data("unsupported graph collection aggregate".into())),
            };
        }

        if let Expr::EntityValue { .. } = argument {
            let encoded = self.encoded_expression(argument)?;
            let key = format!("json_extract(({encoded}), '$.id')");
            return match kind {
                AggregateKind::Collect => Ok(self.collect_scalar(encoded, key, distinct)),
                AggregateKind::Count => Ok(Scalar::sql(
                    format!("COUNT({}{key})", if distinct { "DISTINCT " } else { "" }),
                    "'integer'",
                )),
                _ => Err(Error::Data("unsupported computed entity aggregate".into())),
            };
        }

        if let Expr::Entity(slot) = argument {
            if kind == AggregateKind::Collect {
                let encoded = self.encoded_entity(*slot, self.entity_kind(*slot)?)?;
                let key = self.entity_field(*slot, "id")?;
                return Ok(self.collect_scalar(encoded, key, distinct));
            }
            if kind != AggregateKind::Count {
                return Err(Error::Data(
                    "logical aggregate contains an unsupported entity argument".into(),
                ));
            }
            let value = self.entity_field(*slot, "id")?;
            return Ok(Scalar::sql(
                format!("COUNT({}{value})", if distinct { "DISTINCT " } else { "" }),
                "'integer'",
            ));
        }

        let scalar = self.compile_scalar(argument)?;
        let inline = is_inline_operand(argument);
        // Each place the aggregate mentions its argument (the aggregated
        // value, the null filter, the type filters) is a separate expression
        // context, so each gets its own scope: the argument appears once per
        // use instead of once per mention, and a derived type no longer
        // repeats the value again.
        let mut bind = |body: &dyn Fn(&str, &str) -> String| {
            self.scoped_scalars(vec![(scalar.clone(), inline)], |_, parts| {
                let (value, value_type) = &parts[0];
                Ok(body(value, value_type))
            })
        };
        let non_null_of = |value: &str, value_type: &str| {
            format!(
                "({value}) IS NOT NULL AND ({value_type}) IS NOT NULL \
                 AND ({value_type}) <> 'null'"
            )
        };
        let numeric_of = |value_type: &str| format!("({value_type}) IN ('integer', 'real')");
        let numeric = bind(&|_, value_type| numeric_of(value_type))?;
        let invalid = bind(&|value, value_type| {
            format!(
                "({}) AND NOT ({})",
                non_null_of(value, value_type),
                numeric_of(value_type)
            )
        })?;
        let value = bind(&|value, _| value.to_owned())?;
        let modifier = if distinct { "DISTINCT " } else { "" };

        match kind {
            AggregateKind::Count => {
                let counted = if distinct {
                    bind(&|value, value_type| aggregate_distinct_key(value, value_type))?
                } else {
                    bind(&|value, value_type| {
                        format!(
                            "CASE WHEN {} THEN ({value}) END",
                            non_null_of(value, value_type)
                        )
                    })?
                };
                Ok(Scalar::sql(
                    format!("COUNT({modifier}{counted})"),
                    "'integer'",
                ))
            }
            AggregateKind::Sum => {
                let sum = format!("SUM({modifier}{value}) FILTER (WHERE {numeric})");
                let value = format!(
                    "(CASE WHEN COUNT(*) FILTER (WHERE {invalid}) > 0 THEN NULL \
                     ELSE COALESCE({sum}, 0) END)"
                );
                // The sum is real as soon as one real was summed; deciding
                // this from the argument types keeps the type from repeating
                // the sum itself.
                let real = bind(&|_, value_type| format!("({value_type}) = 'real'"))?;
                let value_type = format!(
                    "(CASE WHEN COUNT(*) FILTER (WHERE {invalid}) > 0 THEN 'null' \
                      WHEN COUNT(*) FILTER (WHERE {real}) > 0 THEN 'real' ELSE 'integer' END)"
                );
                Ok(Scalar::sql(value, value_type))
            }
            AggregateKind::Average => {
                let average = format!("AVG({modifier}{value}) FILTER (WHERE {numeric})");
                let value = format!(
                    "(CASE WHEN COUNT(*) FILTER (WHERE {invalid}) > 0 THEN NULL \
                     ELSE {average} END)"
                );
                Ok(Scalar::sql(value, "'real'"))
            }
            AggregateKind::Minimum | AggregateKind::Maximum => {
                // Cypher orders values of every type against each other, so
                // the extremum is chosen in Rust from each value's JSON text.
                // DISTINCT cannot change it.
                let function = match kind {
                    AggregateKind::Minimum => "crabnet_min",
                    _ => "crabnet_max",
                };
                let encoded = bind(&|value, value_type| encoded_scalar(value, value_type))?;
                let result = format!("{function}({encoded})");
                Ok(Scalar::sql(
                    format!("json_extract({result}, '$')"),
                    format!("json_type({result}, '$')"),
                ))
            }
            AggregateKind::Collect => {
                let encoded = bind(&|value, value_type| encoded_scalar(value, value_type))?;
                let key = format!("crabnet_semantic_key({encoded})");
                Ok(self.collect_scalar(encoded, key, distinct))
            }
        }
    }

    fn collect_scalar(&self, encoded: String, key: String, distinct: bool) -> Scalar {
        let ordering = self
            .ordering
            .as_ref()
            .map(|ordering| format!(" ORDER BY {ordering}"))
            .unwrap_or_default();
        let sql = if distinct {
            format!("crabnet_collect_distinct({encoded}, {key}{ordering})")
        } else {
            format!(
                "json_group_array(json({encoded}){ordering}) FILTER (WHERE ({encoded}) <> 'null')"
            )
        };
        Scalar::sql(sql, "'array'")
    }

    /// `+` adds numbers, concatenates strings, and concatenates lists (a
    /// non-list operand beside a list is appended or prepended as one
    /// element). The result type is decided from the operand types alone, so
    /// it composes without repeating the operand values.
    fn plus(&mut self, left: &Expr, right: &Expr) -> Result<Scalar> {
        let value_type = |types: &[String]| {
            let (left_type, right_type) = (&types[0], &types[1]);
            format!(
                "(CASE WHEN ({left_type}) IS NULL OR ({right_type}) IS NULL \
                        OR ({left_type}) = 'null' OR ({right_type}) = 'null' THEN 'null' \
                  WHEN ({left_type}) = 'integer' AND ({right_type}) = 'integer' THEN 'integer' \
                  WHEN ({left_type}) IN ('integer', 'real') \
                       AND ({right_type}) IN ('integer', 'real') THEN 'real' \
                  WHEN ({left_type}) = 'text' AND ({right_type}) = 'text' THEN 'text' \
                  WHEN ({left_type}) = 'array' OR ({right_type}) = 'array' THEN 'array' \
                  ELSE 'null' END)"
            )
        };
        let value = |this: &mut Self, parts: &[(String, String)]| {
            let (left_value, left_type) = &parts[0];
            let (right_value, right_type) = &parts[1];
            let alias = format!("cat{}", this.scopes);
            let as_list = |(value, value_type): &(String, String)| {
                format!(
                    "(CASE WHEN ({value_type}) = 'array' THEN ({value}) \
                      ELSE json_array(json({})) END)",
                    encoded_scalar(value, value_type)
                )
            };
            let element = element_json(&alias);
            let concatenated = format!(
                "(SELECT json_group_array(json(element) ORDER BY side, position) FROM \
                   (SELECT 0 AS side, {alias}.key AS position, {element} AS element \
                    FROM json_each({}) AS {alias} \
                    UNION ALL \
                    SELECT 1, {alias}.key, {element} FROM json_each({}) AS {alias}))",
                as_list(&parts[0]),
                as_list(&parts[1])
            );
            Ok(format!(
                "(CASE WHEN ({left_value}) IS NULL OR ({right_value}) IS NULL THEN NULL \
                  WHEN ({left_type}) IN ('integer', 'real') \
                       AND ({right_type}) IN ('integer', 'real') \
                       THEN (({left_value}) + ({right_value})) \
                  WHEN ({left_type}) = 'text' AND ({right_type}) = 'text' \
                       THEN (({left_value}) || ({right_value})) \
                  WHEN ({left_type}) = 'array' OR ({right_type}) = 'array' THEN {concatenated} \
                  ELSE NULL END)"
            ))
        };
        self.composed(&[left, right], value, value_type)
    }

    fn numeric_binary(&mut self, left: &Expr, right: &Expr, operator: BinaryOp) -> Result<Scalar> {
        if operator == BinaryOp::Add {
            return self.plus(left, right);
        }
        let value = |_: &mut Self, parts: &[(String, String)]| {
            let (left_value, left_type) = &parts[0];
            let (right_value, right_type) = &parts[1];
            let compatible = format!(
                "({left_type}) IN ('integer', 'real') AND \
             ({right_type}) IN ('integer', 'real')"
            );
            let operation = if operator == BinaryOp::Remainder {
                format!(
                    "CASE WHEN ({left_type}) = 'integer' AND ({right_type}) = 'integer' \
                 THEN (({left_value}) % ({right_value})) \
                 ELSE (({left_value}) - \
                       CAST(({left_value}) / ({right_value}) AS INTEGER) * ({right_value})) END"
                )
            } else {
                let symbol = match operator {
                    BinaryOp::Subtract => "-",
                    BinaryOp::Multiply => "*",
                    BinaryOp::Divide => "/",
                    _ => unreachable!("only arithmetic operators reach numeric_binary"),
                };
                format!("(({left_value}) {symbol} ({right_value}))")
            };
            let value = format!(
                "(CASE \
             WHEN ({left_value}) IS NULL OR ({right_value}) IS NULL THEN NULL \
             WHEN {compatible} THEN ({operation}) \
             ELSE {} END)",
                type_error("arithmetic needs numbers")
            );
            Ok(value)
        };
        self.composed(&[left, right], value, |types| {
            let (left_type, right_type) = (&types[0], &types[1]);
            format!(
                "(CASE WHEN {} OR {} THEN 'null' \
                  WHEN ({left_type}) IN ('integer', 'real') \
                       AND ({right_type}) IN ('integer', 'real') THEN {} \
                  ELSE {} END)",
                is_null_type(left_type),
                is_null_type(right_type),
                numeric_result_type(left_type, right_type),
                type_error("arithmetic needs numbers")
            )
        })
    }

    fn numeric_unary(&mut self, operand: &Expr, operator: UnaryOp) -> Result<Scalar> {
        let value = |_: &mut Self, parts: &[(String, String)]| {
            let (operand_value, operand_type) = &parts[0];
            let symbol = match operator {
                UnaryOp::Plus => "+",
                UnaryOp::Minus => "-",
                UnaryOp::Not => unreachable!("NOT is handled as a boolean operator"),
            };
            let value = format!(
                "(CASE \
             WHEN ({operand_value}) IS NULL THEN NULL \
             WHEN ({operand_type}) IN ('integer', 'real') THEN {symbol}({operand_value}) \
             ELSE {} END)",
                type_error("unary arithmetic needs a number")
            );
            Ok(value)
        };
        self.composed(&[operand], value, |types| {
            format!(
                "(CASE WHEN ({}) IN ('integer', 'real') THEN ({}) WHEN {} THEN 'null' \
                  ELSE {} END)",
                types[0],
                types[0],
                is_null_type(&types[0]),
                type_error("unary arithmetic needs a number")
            )
        })
    }

    fn membership_expression(&mut self, left: &Expr, right: &Expr) -> Result<String> {
        self.scoped(&[left, right], |_, parts| {
            let (left_value, left_type) = &parts[0];
            let (right_value, right_type) = &parts[1];
            let list =
                format!("CASE WHEN ({right_type}) = 'array' THEN ({right_value}) ELSE '[]' END");
            let type_error = type_error("IN needs a list on its right");
            let member_value = "member.value";
            let member_type = "member.type";
            let equal = typed_sql_comparison(
                left_value,
                left_type,
                member_value,
                member_type,
                BinaryOp::Equal,
            );

            Ok(format!(
                "(CASE \
             WHEN ({right_type}) IS NULL OR ({right_type}) = 'null' THEN NULL \
             WHEN ({right_type}) <> 'array' THEN {type_error} \
             WHEN EXISTS (SELECT 1 FROM json_each({list}) AS member WHERE ({equal})) THEN 1 \
             WHEN NOT EXISTS (SELECT 1 FROM json_each({list})) THEN 0 \
             WHEN ({left_type}) IS NULL OR ({left_type}) = 'null' THEN NULL \
             WHEN EXISTS (SELECT 1 FROM json_each({list}) AS member \
                          WHERE ({equal}) IS NULL) THEN NULL \
             ELSE 0 END)"
            ))
        })
    }

    /// Compiles `all`/`any`/`none`/`single(x IN list WHERE predicate)` to a
    /// correlated `json_each` subquery. The predicate is evaluated once per
    /// element into SQLite's three-valued 1/0/NULL; the quantifier then
    /// decides from the counts of true, null, and total outcomes, which is
    /// exactly Cypher's semantics: a null outcome can be either truth value,
    /// so it only matters when the remaining outcomes leave the answer open.
    fn quantified_expression(
        &mut self,
        kind: ListQuantifier,
        variable: ListVariableId,
        list: &Expr,
        predicate: &Expr,
    ) -> Result<String> {
        self.scoped(&[list], |this, parts| {
        let (list_value, list_type) = &parts[0];
        let alias = format!("q{}", variable.0);
        let previous = this.list_elements.insert(
            variable,
            (format!("{alias}.value"), format!("{alias}.type")),
        );
        let outcome = this.capture_sources(|this| this.boolean_expression(predicate));
        match previous {
            Some(previous) => {
                this.list_elements.insert(variable, previous);
            }
            None => {
                this.list_elements.remove(&variable);
            }
        }
        let (outcome, sources) = outcome?;
        let decision = match kind {
            ListQuantifier::All => {
                "WHEN total - trues - nulls > 0 THEN 0 WHEN nulls > 0 THEN NULL ELSE 1"
            }
            ListQuantifier::Any => "WHEN trues > 0 THEN 1 WHEN nulls > 0 THEN NULL ELSE 0",
            ListQuantifier::None => "WHEN trues > 0 THEN 0 WHEN nulls > 0 THEN NULL ELSE 1",
            ListQuantifier::Single => {
                "WHEN trues > 1 THEN 0 WHEN nulls > 0 THEN NULL WHEN trues = 1 THEN 1 ELSE 0"
            }
        };
        Ok(format!(
            "(CASE              WHEN ({list_type}) IS NULL OR ({list_type}) <> 'array' THEN NULL              ELSE (SELECT CASE {decision} END FROM                    (SELECT count(*) AS total,                            count(*) FILTER (WHERE outcome) AS trues,                            count(*) FILTER (WHERE outcome IS NULL) AS nulls                     FROM (SELECT ({outcome}) AS outcome                           FROM json_each({list_value}) AS {alias}{sources}))) END)"
        ))
        })
    }

    fn string_predicate(
        &mut self,
        left: &Expr,
        right: &Expr,
        operator: BinaryOp,
    ) -> Result<String> {
        self.scoped(&[left, right], |_, parts| {
            let (left_value, left_type) = &parts[0];
            let (right_value, right_type) = &parts[1];
            let predicate = match operator {
                BinaryOp::StartsWith => format!("instr(({left_value}), ({right_value})) = 1"),
                BinaryOp::EndsWith => format!(
                    "length(({left_value})) >= length(({right_value})) AND \
                 substr(({left_value}), length(({left_value})) - length(({right_value})) + 1) \
                 = (({right_value}) COLLATE BINARY)"
                ),
                BinaryOp::Contains => format!("instr(({left_value}), ({right_value})) > 0"),
                _ => unreachable!("only string predicates reach string_predicate"),
            };

            Ok(format!(
                "(CASE \
             WHEN ({left_type}) IS NULL OR ({left_type}) = 'null' \
               OR ({right_type}) IS NULL OR ({right_type}) = 'null' THEN NULL \
             WHEN ({left_type}) <> 'text' OR ({right_type}) <> 'text' THEN NULL \
             ELSE ({predicate}) END)"
            ))
        })
    }

    fn entity_property_parts(&mut self, slot: SlotId, key: &str) -> Result<(String, String)> {
        let props = self.entity_props(slot)?;
        Ok(self.json_property_parts(props, key))
    }

    fn json_property_parts(&mut self, props: String, key: &str) -> (String, String) {
        let path = self.push_binding(SqlParameter::Text(key.to_owned()));
        let json_path = format!("'$.' || json_quote({path})");
        (
            format!("json_extract({props}, {json_path})"),
            format!("json_type({props}, {json_path})"),
        )
    }

    fn typed_value_comparison(
        &mut self,
        property: String,
        property_type: String,
        expected: ValueExpr,
        operator: BinaryOp,
    ) -> String {
        let expected_scalar = self.push_binding(SqlParameter::Scalar(expected.clone()));
        let expected_json = self.push_binding(SqlParameter::Json(expected));
        let expected_type = format!("json_type(json({expected_json}))");
        typed_sql_comparison(
            &property,
            &property_type,
            &expected_scalar,
            &expected_type,
            operator,
        )
    }

    /// A necessary condition for `json_extract(props, key) = expected` on the
    /// node whose ID is `id`, answered from the `node_properties` index. The
    /// index holds each top-level non-null property as `json_extract` returns
    /// it, so every node for which the exact comparison holds passes; the
    /// exact, type-aware comparison must still be applied beside it.
    ///
    /// A list or map can equal a property whose JSON text differs (`[1]` and
    /// `[1.0]`), so a composite expected value probes every node that has the
    /// key instead. Its type gate is constant per row, so SQLite tests it
    /// once rather than per index entry.
    fn property_index_prefilter(
        &mut self,
        id: &str,
        key: &str,
        (expected, expected_type): (&str, &str),
    ) -> String {
        let key = self.push_binding(SqlParameter::Text(key.to_owned()));
        format!(
            "{id} IN (SELECT {PROPERTY_INDEX}.node_id FROM node_properties AS {PROPERTY_INDEX} \
             WHERE {PROPERTY_INDEX}.key = {key} AND {PROPERTY_INDEX}.value = ({expected}) \
             UNION ALL SELECT {PROPERTY_INDEX}.node_id FROM node_properties AS {PROPERTY_INDEX} \
             WHERE {PROPERTY_INDEX}.key = {key} AND ({expected_type}) IN ('array', 'object'))"
        )
    }

    /// Freeze a computed key inside the IN relation, so its scalar subqueries
    /// do not run once per property-index entry. CROSS JOIN keeps the one-row
    /// key ahead of the index, especially for the composite-presence branch.
    /// Keep the node ID outside this scope so SQLite can still drive the node
    /// scan from the candidate IDs.
    fn computed_property_index_prefilter(
        &mut self,
        id: &str,
        key: &str,
        (expected, expected_type): (&str, &str),
    ) -> String {
        let key = self.push_binding(SqlParameter::Text(key.to_owned()));
        format!(
            "{id} IN (WITH _crabnet_probe AS MATERIALIZED \
             (SELECT ({expected}) AS value, ({expected_type}) AS kind) \
             SELECT {PROPERTY_INDEX}.node_id FROM _crabnet_probe CROSS JOIN node_properties AS {PROPERTY_INDEX} \
             WHERE {PROPERTY_INDEX}.key = {key} AND {PROPERTY_INDEX}.value = _crabnet_probe.value \
             UNION ALL SELECT {PROPERTY_INDEX}.node_id FROM _crabnet_probe CROSS JOIN node_properties AS {PROPERTY_INDEX} \
             WHERE _crabnet_probe.kind IN ('array', 'object') AND {PROPERTY_INDEX}.key = {key})"
        )
    }

    /// The SQL value and type of a stored-value expression, for an index
    /// probe.
    fn probe_value(&mut self, value: &ValueExpr) -> (String, String) {
        let expected = self.push_binding(SqlParameter::Scalar(value.clone()));
        let json = self.push_binding(SqlParameter::Json(value.clone()));
        (expected, format!("json_type(json({json}))"))
    }

    fn push_binding(&mut self, binding: SqlParameter) -> String {
        self.bindings.push(FragmentArgument::Static(binding));
        format!("?{}", self.bindings.len())
    }
}

/// The JSON text of one `json_each` row, given the table alias. Equivalent to
/// `encoded_scalar` over the row's value and type but far shorter, which
/// matters because list operators embed it inside each other.
fn element_json(alias: &str) -> String {
    format!(
        "(CASE WHEN {alias}.type IN ('array', 'object') THEN {alias}.value \
          WHEN {alias}.type IN ('true', 'false', 'null') THEN {alias}.type \
          WHEN {alias}.type = 'real' THEN crabnet_json_real({alias}.value) \
          ELSE json_quote({alias}.value) END)"
    )
}

fn encoded_scalar(value: &str, value_type: &str) -> String {
    // A statically known type needs no type gates, and the value can be
    // mentioned once. Aggregates rely on this: they stay inline, so every
    // mention of the result repeats the whole aggregate.
    match value_type {
        "'true'" | "'false'" => {
            return format!("(CASE WHEN ({value}) IS NULL THEN 'null' ELSE {value_type} END)");
        }
        "'text'" | "'integer'" => return format!("json_quote(({value}))"),
        "'real'" => return format!("COALESCE(crabnet_json_real(({value})), 'null')"),
        "'array'" | "'object'" => return format!("COALESCE(json(({value})), 'null')"),
        _ => {}
    }
    // Every branch maps a SQL NULL to 'null' on its own, so the type is
    // mentioned once and the value three times.
    format!(
        "(CASE ({value_type}) \
         WHEN 'text' THEN json_quote(({value})) \
         WHEN 'real' THEN COALESCE(crabnet_json_real(({value})), 'null') \
         WHEN 'true' THEN 'true' \
         WHEN 'false' THEN 'false' \
         WHEN 'null' THEN 'null' \
         ELSE COALESCE(json(({value})), 'null') END)"
    )
}

/// An ORDER BY term over a sort key from `order_key`. The key encodes
/// Cypher's global sort order, in which null sorts last, so ascending order
/// puts nulls last and descending order puts them first.
fn order_expression(key: &str, direction: SortDirection) -> String {
    let direction = match direction {
        SortDirection::Ascending => "ASC",
        SortDirection::Descending => "DESC",
    };
    format!("{key} {direction}")
}

fn aggregate_distinct_key(value: &str, value_type: &str) -> String {
    let encoded = encoded_scalar(value, value_type);
    format!(
        "(CASE \
         WHEN ({value}) IS NULL OR ({value_type}) IS NULL OR ({value_type}) = 'null' THEN NULL \
         ELSE crabnet_semantic_key({encoded}) END)"
    )
}

// Expressions whose SQL value is already a three-valued boolean.
fn is_native_boolean(expression: &Expr) -> bool {
    match expression {
        Expr::Binary { operator, .. } => matches!(
            operator,
            BinaryOp::Equal
                | BinaryOp::NotEqual
                | BinaryOp::Less
                | BinaryOp::LessEqual
                | BinaryOp::Greater
                | BinaryOp::GreaterEqual
                | BinaryOp::In
                | BinaryOp::StartsWith
                | BinaryOp::EndsWith
                | BinaryOp::Contains
                | BinaryOp::Or
                | BinaryOp::And
        ),
        Expr::Unary {
            operator: UnaryOp::Not,
            ..
        }
        | Expr::IsNull { .. }
        | Expr::Quantified { .. } => true,
        _ => false,
    }
}

// Whether an operand is cheap enough to repeat inline rather than compute
// once in a scalar-subquery scope. Aggregates must stay inline: moving them
// into a subquery would aggregate over that subquery's single row instead of
// over the group.
fn is_inline_operand(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Aggregate { .. }
            | Expr::Value(_)
            | Expr::Scalar(_)
            | Expr::Property(_, _)
            | Expr::ElementId(_)
            | Expr::EntityIdentity(_)
            | Expr::RelationshipType(_)
            | Expr::ListElement(_)
            | Expr::Function {
                function: ScalarFunction::Rand,
                ..
            }
    )
}

// The JSON type name of a three-valued SQL boolean.
fn boolean_type(value: &str) -> String {
    format!(
        "(CASE WHEN ({value}) IS NULL THEN 'null' \
         WHEN ({value}) THEN 'true' ELSE 'false' END)"
    )
}

/// A three-valued SQL boolean from a JSON type name. Null stays null; any
/// kind other than a boolean is a type error.
fn boolean_value(value_type: &str) -> String {
    format!(
        "(CASE WHEN ({value_type}) = 'true' THEN 1 \
         WHEN ({value_type}) = 'false' THEN 0 \
         WHEN ({value_type}) IS NULL OR ({value_type}) = 'null' THEN NULL \
         ELSE {} END)",
        type_error("expected a boolean")
    )
}

/// SQL that fails the statement with `Error::Type`. Generated SQL places it
/// in a `CASE` branch reached only by a value of the wrong kind.
fn type_error(message: &str) -> String {
    format!("crabnet_type_error('{}')", message.replace('\'', "''"))
}

/// Whether a JSON type name is null or a missing value.
fn is_null_type(value_type: &str) -> String {
    format!("(({value_type}) IS NULL OR ({value_type}) = 'null')")
}

// The result type of a binary numeric operator, from its operand types:
// integer with integer stays integer, any other numeric pairing is real, and
// everything else (including null) is null.
fn numeric_result_type(left_type: &str, right_type: &str) -> String {
    format!(
        "(CASE WHEN ({left_type}) IS NULL OR ({right_type}) IS NULL \
                OR ({left_type}) = 'null' OR ({right_type}) = 'null' THEN 'null' \
          WHEN ({left_type}) = 'integer' AND ({right_type}) = 'integer' THEN 'integer' \
          WHEN ({left_type}) IN ('integer', 'real') AND ({right_type}) IN ('integer', 'real') \
               THEN 'real' \
          ELSE 'null' END)"
    )
}

// The type of a list-producing operator whose only operand must be a list.
fn array_when_list(list_type: &str) -> String {
    format!("(CASE WHEN ({list_type}) = 'array' THEN 'array' ELSE 'null' END)")
}

fn typed_sql_comparison(
    left: &str,
    left_type: &str,
    right: &str,
    right_type: &str,
    operator: BinaryOp,
) -> String {
    let compatible_types = format!(
        "((({left_type}) IN ('integer', 'real') AND ({right_type}) IN ('integer', 'real')) \
         OR (({left_type}) IN ('true', 'false') AND ({right_type}) IN ('true', 'false')) \
         OR ({left_type}) = ({right_type}))"
    );
    let equal = format!("(({left}) = ({right}) AND {compatible_types})");
    // Lists and maps arrive as JSON text. Their equality looks inside them,
    // where nulls propagate and integers equal floats, so it is decided in
    // Rust; a composite beside any other type fails the type check below.
    let negation = match operator {
        BinaryOp::Equal => "",
        BinaryOp::NotEqual => "NOT ",
        _ => unreachable!("typed comparisons are only generated for equality operators"),
    };
    format!(
        "(CASE WHEN ({left}) IS NULL OR ({right}) IS NULL THEN NULL \
         WHEN ({left_type}) IN ('array', 'object') AND ({left_type}) = ({right_type}) \
         THEN {negation}crabnet_equals(({left}), ({right})) \
         ELSE {negation}{equal} END)"
    )
}

fn typed_sql_ordering(
    left: &str,
    left_type: &str,
    right: &str,
    right_type: &str,
    operator: BinaryOp,
) -> String {
    let compatible_types = format!(
        "((({left_type}) IN ('integer', 'real') AND ({right_type}) IN ('integer', 'real')) \
         OR (({left_type}) = 'text' AND ({right_type}) = 'text') \
         OR (({left_type}) IN ('true', 'false') AND ({right_type}) IN ('true', 'false')))"
    );
    let operator = match operator {
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        _ => unreachable!("typed ordering only handles ordering operators"),
    };
    format!(
        "(CASE \
         WHEN ({left}) IS NULL OR ({right}) IS NULL THEN NULL \
         WHEN {compatible_types} THEN (({left}) {operator} ({right})) \
         WHEN ({left_type}) = 'array' AND ({right_type}) = 'array' \
         THEN (crabnet_compare(({left}), ({right})) {operator} 0) \
         ELSE NULL END)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::lower;

    // Exercise the same compiler mode as a MERGE matcher, with read syntax
    // making both equality orientations and rejected self-dependencies easy
    // to express. The integration work-budget test checks the actual MERGE route.
    fn merge_scope_sql(source: &str) -> String {
        let query = lower::lower(source).unwrap();
        compile_merge_read(&query.input, query.projection.as_ref().unwrap())
            .unwrap()
            .sql
            .clone()
    }

    #[test]
    fn computed_key_lookups_are_planned_through_the_index() {
        for expression in [
            "row.id",
            "row.nested.ids[-1]",
            "row[$key]",
            "coalesce(row.id, $fallback)",
            "toLower(trim(row.id))",
            "toInteger(row.id) + 1",
            "-abs(row.id)",
            "size(row.id)",
            "[row.id, $extra]",
            "{key: row.id}",
            "CASE WHEN row.id IS NULL THEN $fallback ELSE row.id END",
        ] {
            for predicate in [
                format!("p.package_id = {expression}"),
                format!("{expression} = p.package_id AND p.enabled IS NOT NULL"),
            ] {
                let query =
                    format!("UNWIND $rows AS row MATCH (p:Package) WHERE {predicate} RETURN p");
                let physical = merge_scope_sql(&query);
                assert!(
                    physical.contains("node_properties AS _crabnet_property"),
                    "{query} should probe the property index:\n{physical}"
                );
            }
        }
    }

    #[test]
    fn probes_do_not_repeat_volatile_expensive_or_self_dependent_keys() {
        for query in [
            "MATCH (p) WHERE p.k = p.other + 1 RETURN p",
            "MATCH (p) WHERE p.k = p.nested.key RETURN p",
            "MATCH (p), (q) WHERE p.k = q.nested.key RETURN p",
            "MATCH (p) WHERE p.k = $values[p.other] RETURN p",
            "MATCH (p) WHERE p.k = coalesce($key, p.other) RETURN p",
            "MATCH (p) WHERE p.k = CASE WHEN $flag THEN $key ELSE p.other END RETURN p",
            "MATCH (p) WHERE p.k = rand() RETURN p",
            "MATCH (p) WHERE p.k = {key: rand()}.key RETURN p",
            "MATCH (p) WHERE p.k = [rand()][0] RETURN p",
            "MATCH (p) WHERE p.k = toInteger(rand() * 10) RETURN p",
            "MATCH (p) WHERE p.k = coalesce($key, rand()) RETURN p",
            "MATCH (p) WHERE p.k = CASE WHEN $flag THEN $key ELSE rand() END RETURN p",
            "MATCH (p) WHERE p.k = [x IN $values | x + 1] RETURN p",
            "MATCH (p) WHERE p.k = range(1, $end) RETURN p",
            "MATCH (p) WHERE p.k = $row.id OR p.other = 1 RETURN p",
        ] {
            let physical = merge_scope_sql(query);
            assert!(
                !physical.contains("node_properties"),
                "{query}:\n{physical}"
            );
        }
        // The WITH stage freezes the random value, so repeating its slot is safe.
        let physical =
            merge_scope_sql("WITH {id: rand()} AS row MATCH (p) WHERE p.k = row.id RETURN p");
        assert!(physical.contains("node_properties"));
        let physical =
            merge_scope_sql("MATCH (q) WITH q MATCH (p) WHERE p.k = q.nested.key RETURN p");
        assert!(physical.contains("node_properties"));
    }

    #[test]
    fn generated_sql_uses_internal_aliases_and_bound_property_names() {
        let query =
            lower::lower("MATCH (`odd variable`:Person) RETURN `odd variable`.`odd.key` AS value")
                .unwrap();
        let projection = query.projection.as_ref().unwrap();
        let plan = compile_read(&query.input, projection).unwrap();

        assert!(plan.sql.contains("nodes AS n0"));
        assert!(!plan.sql.contains("odd variable"));
        assert!(!plan.sql.contains("odd.key"));
        assert_eq!(plan.columns, ["value"]);
    }

    #[test]
    fn relationship_joins_are_lowered_to_endpoint_predicates() {
        let query = lower::lower("MATCH (a)-[r:KNOWS]->(b) RETURN a.name, b.name").unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.sql.contains("edges AS r1"));
        assert!(
            plan.sql
                .contains("r1.from_node = n0.id AND r1.to_node = n2.id")
        );
        assert!(!plan.sql.contains("KNOWS"));
    }

    #[test]
    fn sqlite_walks_seed_from_the_constrained_start_endpoint() {
        let query = lower::lower(
            "MATCH (start:Person {name: $name})-[:NEXT*1..3]->(end) RETURN end.name AS name",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        let seed = plan
            .sql
            .split("FROM nodes AS seed WHERE ")
            .nth(1)
            .and_then(|rest| rest.split(" UNION ALL SELECT walk0.").next())
            .expect("the recursive seed must be filtered:\n{plan.sql}");
        assert!(seed.starts_with("EXISTS (SELECT 1 FROM node_labels WHERE label = ?"));
        assert!(seed.contains("seed.id IN (SELECT _crabnet_property.node_id"));
        assert!(seed.contains("json_extract(seed.props, '$.' || json_quote(?"));
        // The outer join still applies the same constraints to the start node.
        assert!(plan.sql.contains("walk_result0.start_id = n0.id"));
        assert!(plan.sql.contains("AND node_id = n0.id)"));
        assert!(!seed.contains("ORDER BY"));
    }

    #[test]
    fn sqlite_walks_from_an_unconstrained_start_seed_every_node() {
        let query = lower::lower(
            "MATCH (start) WHERE start.name = 'a' \
             MATCH (start)-[:NEXT*1..3]->(end:Person) RETURN end.name AS name",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.sql.contains("FROM nodes AS seed UNION ALL"));
        assert!(
            !plan
                .sql
                .contains("seed.id IN (SELECT node_id FROM node_labels")
        );
    }

    #[test]
    fn whole_entities_have_explicit_output_shapes_and_null_guards() {
        let query = lower::lower("MATCH (a)-[r]->(b) RETURN a AS node, r AS relationship").unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert_eq!(
            plan.output_shapes,
            [
                OutputShape::Entity(EntityKind::Node),
                OutputShape::Entity(EntityKind::Relationship),
            ]
        );
        assert!(
            plan.sql
                .contains("CASE WHEN n0.id IS NULL THEN 'null' ELSE json_object")
        );
        assert!(
            plan.sql
                .contains("CASE WHEN r1.id IS NULL THEN 'null' ELSE json_object")
        );
        assert!(plan.sql.contains("'from', r1.from_node"));
        assert!(plan.sql.contains("'to', r1.to_node"));
    }

    #[test]
    fn distinct_is_a_post_decode_stage_and_defers_pagination() {
        let query = lower::lower(
            "MATCH (node) RETURN DISTINCT node.value AS value \
             ORDER BY value SKIP 2 LIMIT 3",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.distinct());
        assert!(!plan.sql.contains("SELECT DISTINCT"));
        assert!(!plan.sql.contains(" LIMIT "));
        assert!(!plan.sql.contains(" OFFSET "));
    }

    #[test]
    fn path_output_stays_an_identity_until_the_read_plan_hydration_stage() {
        let query = lower::lower(
            "MATCH path = (start)-[:NEXT]->(end) \
             RETURN DISTINCT path, end.name AS name ORDER BY name SKIP 1 LIMIT 2",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert_eq!(
            plan.output_shapes,
            [OutputShape::PathIdentity, OutputShape::Json]
        );
        assert!(plan.has_path_identity_output());
        let branch = plan.render_physical(false);
        assert!(branch.contains("operator 1: semantic-distinct"));
        assert!(branch.contains("operator 2: pagination"));
        assert!(!branch.contains("batch-hydrate-paths"));
        let root = plan.render_physical(true);
        let distinct = root.find(": semantic-distinct").unwrap();
        let pagination = root.find(": pagination").unwrap();
        let hydration = root.find(": batch-hydrate-paths").unwrap();
        assert!(distinct < pagination && pagination < hydration);
    }

    #[test]
    fn path_length_is_sql_pushable_while_entity_lists_stay_pending() {
        let query = lower::lower(
            "MATCH path = (start)-[:NEXT*0..2]->(end) \
             RETURN length(path) AS hops, nodes(path) AS points, \
                    relationships(path) AS links",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert_eq!(
            plan.output_shapes,
            [
                OutputShape::Json,
                OutputShape::PathNodes,
                OutputShape::PathRelationships,
            ]
        );
        assert!(plan.sql.contains("SUM(json_array_length("));
        assert!(plan.sql.contains("'$.relationships'"));
        assert!(plan.sql.contains("json_type("));
        assert!(plan.has_path_identity_output());
    }

    #[test]
    fn hybrid_traversal_is_a_resolved_runtime_input_with_filtered_edges() {
        let query =
            lower::lower("MATCH path = (start)-[:NEXT*2.. {open: $open}]->(end) RETURN path")
                .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.sql.contains("json_each(?1) AS trail_result0"));
        assert!(plan.sql.contains("'$.start_id'"));
        assert!(plan.sql.contains("'$.used_edges'"));
        assert!(!plan.sql.contains("_crabnet_walk_"));
        let physical = plan.render_physical(true);
        assert!(physical.contains(
            "operator 0: trail-expand output=%r0 traversal=~0 direction=right bounds=2..unbounded"
        ));

        let parameters = Parameters::from([("open".to_owned(), Value::Bool(true))]);
        let bindings = plan
            .resolve_bindings(&parameters, |input| {
                let PreparedValueOperator::TrailExpand { spec, .. } = input else {
                    panic!("expected a trail expansion")
                };
                assert_eq!(spec.traversal, 0);
                assert_eq!(spec.direction, TrailDirection::Right);
                assert_eq!((spec.minimum, spec.maximum), (2, None));
                assert!(spec.edge_sql.contains("FROM edges AS trail_edge"));
                assert!(spec.edge_sql.contains("trail_edge.edge_type = ?1"));
                assert!(spec.edge_sql.contains("json_type(trail_edge.props"));
                assert_eq!(
                    spec.edge_bindings.as_slice(),
                    &[
                        SqlValue::Text("NEXT".into()),
                        SqlValue::Text("open".into()),
                        SqlValue::Integer(1),
                        SqlValue::Text("true".into()),
                    ]
                );
                Ok(SqlValue::Text("[]".into()))
            })
            .unwrap();
        assert_eq!(bindings, [SqlValue::Text("[]".into())]);
    }

    #[test]
    fn hybrid_endpoint_constraints_become_bound_node_domains() {
        let query = lower::lower(
            "MATCH (start:Person {name: $name})-[:NEXT*2..]->(end {open: true}) RETURN end",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();
        let physical = plan.render_physical(true);
        assert!(physical.contains("start-domain:\n        sql: SELECT trail_node.id"));
        assert!(physical.contains("end-domain:\n        sql: SELECT trail_node.id"));

        let parameters = Parameters::from([("name".to_owned(), Value::String("Ada".into()))]);
        plan.resolve_bindings(&parameters, |input| {
            let PreparedValueOperator::TrailExpand { spec, .. } = input else {
                panic!("expected a trail expansion")
            };
            let start = spec.start_domain.as_ref().expect("start domain");
            assert!(
                start
                    .sql
                    .starts_with("SELECT trail_node.id FROM nodes AS trail_node WHERE ")
            );
            assert!(start.sql.contains(
                "EXISTS (SELECT 1 FROM node_labels WHERE label = ?1 AND node_id = trail_node.id)"
            ));
            assert!(start.sql.contains("json_type(trail_node.props"));
            assert!(start.sql.ends_with(" ORDER BY trail_node.id"));
            assert_eq!(
                start.bindings.as_slice(),
                &[
                    SqlValue::Text("Person".into()),
                    SqlValue::Text("Ada".into()),
                    SqlValue::Text("\"Ada\"".into()),
                    SqlValue::Text("name".into()),
                    SqlValue::Text("name".into()),
                    SqlValue::Text("Ada".into()),
                    SqlValue::Text("\"Ada\"".into()),
                ]
            );
            let end = spec.end_domain.as_ref().expect("end domain");
            assert!(!end.sql.contains("label"));
            assert_eq!(
                end.bindings.as_slice(),
                &[
                    SqlValue::Integer(1),
                    SqlValue::Text("true".into()),
                    SqlValue::Text("open".into()),
                    SqlValue::Text("open".into()),
                    SqlValue::Integer(1),
                    SqlValue::Text("true".into()),
                ]
            );
            Ok(SqlValue::Text("[]".into()))
        })
        .unwrap();

        let unconstrained = lower::lower("MATCH (start)-[:NEXT*2..]->(end) RETURN end").unwrap();
        let plan = compile_read(
            &unconstrained.input,
            unconstrained.projection.as_ref().unwrap(),
        )
        .unwrap();
        let physical = plan.render_physical(true);
        assert!(physical.contains("start-domain: unrestricted"));
        assert!(physical.contains("end-domain: unrestricted"));
    }

    #[test]
    fn shortest_path_selection_is_exposed_to_the_runtime_and_physical_plan() {
        for (all, expected_mode, expected_name) in [
            (false, ShortestMode::One, "one"),
            (true, ShortestMode::All, "all"),
        ] {
            let input = Input::Match(MatchPlan {
                operators: vec![
                    MatchOperator::ScanNode { slot: SlotId(0) },
                    MatchOperator::ScanNode { slot: SlotId(1) },
                    MatchOperator::ExpandShortestPath {
                        traversal: TraversalId(0),
                        left: SlotId(0),
                        right: SlotId(1),
                        direction: Direction::Right,
                        minimum: 1,
                        maximum: Some(4),
                        all,
                        relationship_type: Some("NEXT".into()),
                        properties: vec![("open".into(), ValueExpr::Parameter("open".into()))],
                    },
                    MatchOperator::BindPath {
                        path: PathId(0),
                        start: SlotId(0),
                        segments: vec![PathSegment {
                            left: SlotId(0),
                            relationship: RelationshipUse::Traversal(TraversalId(0)),
                            right: SlotId(1),
                        }],
                    },
                ],
            });
            let projection = Projection {
                columns: vec!["route".into()],
                expressions: vec![Expr::Path(PathId(0))],
                distinct: false,
                order_by: Vec::new(),
                offset: None,
                limit: None,
            };
            let plan = compile_read(&input, &projection).unwrap();

            assert!(plan.sql.contains("json_each(?1) AS trail_result0"));
            assert!(plan.sql.contains("'$.used_edges'"));
            assert!(!plan.sql.contains("_crabnet_walk_"));
            assert!(plan.render_physical(true).contains(&format!(
                "operator 0: shortest-expand mode={expected_name} output=%r0 traversal=~0 direction=right bounds=1..4"
            )));

            let parameters = Parameters::from([("open".to_owned(), Value::Bool(true))]);
            let bindings = plan
                .resolve_bindings(&parameters, |input| {
                    let PreparedValueOperator::ShortestExpand { mode, spec, .. } = input else {
                        panic!("expected a shortest expansion")
                    };
                    assert_eq!(*mode, expected_mode);
                    assert_eq!(spec.direction, TrailDirection::Right);
                    assert_eq!((spec.minimum, spec.maximum), (1, Some(4)));
                    assert!(spec.edge_sql.contains("FROM edges AS trail_edge"));
                    assert!(spec.edge_sql.contains("trail_edge.edge_type = ?1"));
                    assert!(spec.edge_sql.contains("json_type(trail_edge.props"));
                    Ok(SqlValue::Text("[]".into()))
                })
                .unwrap();
            assert_eq!(bindings, [SqlValue::Text("[]".into())]);
        }
    }

    #[test]
    fn all_parameters_resolve_before_a_runtime_input_is_materialized() {
        let query = lower::lower(
            "MATCH (start)-[:NEXT* {open: $open}]->(end) \
             RETURN $missing AS value",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();
        let parameters = Parameters::from([("open".to_owned(), Value::Bool(true))]);
        let mut materialized = false;

        let error = plan
            .resolve_bindings(&parameters, |_| {
                materialized = true;
                Ok(SqlValue::Text("[]".into()))
            })
            .unwrap_err();

        assert!(matches!(error, Error::MissingParameter(name) if name == "missing"));
        assert!(!materialized);
    }

    #[test]
    fn membership_and_string_predicates_use_typed_json_operations() {
        let query = lower::lower(
            "MATCH (node) \
             WHERE node.name STARTS WITH $prefix AND node.value IN $allowed \
             RETURN node.name",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.sql.contains("instr("));
        assert!(plan.sql.contains("json_each("));
        assert!(plan.sql.contains("json_type("));
        assert!(!plan.sql.contains(" LIKE "));
        assert!(!plan.sql.contains("prefix"));
        assert!(!plan.sql.contains("allowed"));
    }

    #[test]
    fn with_boundaries_use_ctes_and_carried_entity_accessors() {
        let query = lower::lower(
            "MATCH (source:Person) \
             WITH source WHERE source.active = true \
             MATCH (source)-[edge:LINK]->(target) \
             RETURN source.name, type(edge), target.name",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.sql.starts_with("WITH _crabnet_with_0 AS MATERIALIZED"));
        assert!(plan.sql.contains("_crabnet_with_0 AS w0"));
        assert!(plan.sql.contains("json_extract(w0.e0, '$.id')"));
        assert!(plan.sql.contains("edges AS r1"));
        assert!(plan.sql.contains("nodes AS n2"));
        assert!(
            plan.sql
                .contains("r1.from_node = json_extract(w0.e0, '$.id')")
        );
    }

    #[test]
    fn bind_positions_are_shared_across_chained_ctes() {
        let query = lower::lower(
            "WITH $start AS value WHERE value >= $floor \
             WITH value + $step AS total LIMIT $limit \
             RETURN total",
        )
        .unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.sql.contains("_crabnet_with_1 AS MATERIALIZED"));
        for index in 1..=plan.arguments.len() {
            assert!(
                plan.sql.contains(&format!("?{index}")),
                "missing bind position ?{index} in {}",
                plan.sql
            );
        }
        assert!(!plan.sql.contains(&format!("?{}", plan.arguments.len() + 1)));
    }
}
