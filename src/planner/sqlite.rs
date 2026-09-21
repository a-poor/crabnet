use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
#[cfg(test)]
use serde_json::Value;

use crate::{Error, Parameters, Result};

use super::logical::{
    AggregateKind, BinaryOp, Direction, EntityKind, Expr, Input, ListQuantifier, ListVariableId,
    MatchOperator, MatchPlan, PathFunctionKind, PathId, PathSegment, Projection, ReadStage,
    RelationshipUse, ScalarSlotId, SlotId, SortDirection, TraversalId, UnaryOp, UnwindStage,
    ValueExpr, WithBinding, WithStage,
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

pub(crate) struct RowSource {
    pub(crate) sql: String,
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    bindings: Vec<SqlParameter>,
}

pub(crate) struct ResolvedRowSource {
    pub(crate) sql: String,
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    pub(crate) bindings: Vec<SqlValue>,
}

impl RowSource {
    pub(crate) fn resolve(self, parameters: &Parameters) -> Result<ResolvedRowSource> {
        Ok(ResolvedRowSource {
            bindings: resolve_bindings(&self.bindings, parameters)?,
            sql: self.sql,
            slots: self.slots,
        })
    }
}

fn resolve_bindings(bindings: &[SqlParameter], parameters: &Parameters) -> Result<Vec<SqlValue>> {
    bindings
        .iter()
        .map(|binding| binding.resolve(parameters))
        .collect()
}

pub(crate) fn compile_read(input: &Input, projection: &Projection) -> Result<CompiledRead> {
    let mut compiler = Compiler::new();
    match input {
        Input::Unit => {}
        Input::Match(plan) => compiler.compile_match(plan)?,
        Input::Pipeline(pipeline) => {
            for stage in &pipeline.stages {
                match stage {
                    ReadStage::Match(plan) => compiler.compile_match(plan)?,
                    ReadStage::OptionalMatch(plan) => compiler.compile_optional_match(plan)?,
                    ReadStage::With(stage) => compiler.compile_with(stage)?,
                    ReadStage::Unwind(stage) => compiler.compile_unwind(stage)?,
                }
            }
        }
    }

    let aggregating = projection.expressions.iter().any(Expr::contains_aggregate);
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
    let mut sql = compiler.select_sql(&select);

    if aggregating {
        let group_by = projection
            .expressions
            .iter()
            .filter(|expression| !expression.contains_aggregate() && expression.is_row_dependent())
            .map(|expression| compiler.grouping_terms(expression))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if !group_by.is_empty() {
            sql.push_str(" GROUP BY ");
            sql.push_str(&group_by.join(", "));
        }
    }

    if !projection.order_by.is_empty() {
        let items = projection
            .order_by
            .iter()
            .map(|item| {
                let expression = compiler.raw_expression(&item.expression)?;
                Ok(order_expression(expression, item.direction))
            })
            .collect::<Result<Vec<_>>>()?;
        sql.push_str(" ORDER BY ");
        sql.push_str(&items.join(", "));
    } else if !aggregating && let Some(ordering) = &compiler.ordering {
        sql.push_str(" ORDER BY ");
        sql.push_str(ordering);
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

pub(crate) fn compile_row_source(plan: &MatchPlan) -> Result<RowSource> {
    if plan.has_variable_length() {
        return Err(Error::Unsupported(
            "writes fed by variable-length traversal".into(),
        ));
    }
    let mut compiler = Compiler::new();
    compiler.compile_match(plan)?;
    compiler.scanned.sort_by_key(|(slot, _)| *slot);

    let select = compiler
        .scanned
        .iter()
        .map(|(slot, kind)| compiler.encoded_entity(*slot, *kind))
        .collect::<Result<Vec<_>>>()?;
    let sql = compiler.select_sql(&select);

    let bindings = compiler
        .bindings
        .into_iter()
        .map(|argument| match argument {
            FragmentArgument::Static(parameter) => Ok(parameter),
            FragmentArgument::Runtime(_) => Err(Error::Data(
                "mutation row source contains a runtime input".into(),
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RowSource {
        sql,
        slots: compiler.scanned,
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

struct VariableLengthSpec<'a> {
    traversal: TraversalId,
    left: SlotId,
    right: SlotId,
    direction: Direction,
    minimum: usize,
    maximum: usize,
    relationship_type: Option<&'a str>,
    properties: &'a [(String, ValueExpr)],
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
    /// Type SQL that stands on its own, such as `json_type(...)` or a
    /// constant like `'integer'`.
    Sql(String),
    /// A three-valued SQL boolean whose type name follows from its value.
    Boolean,
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

    /// The type SQL, given SQL that yields this scalar's value. Derived
    /// kinds are computed from `value` rather than from the scalar's own
    /// value text, so a scope can compute the value once and type it by name.
    fn type_of(&self, value: &str) -> String {
        match &self.kind {
            ScalarType::Sql(sql) => sql.clone(),
            ScalarType::Boolean => boolean_type(value),
        }
    }

    fn into_parts(self) -> (String, String) {
        let value_type = self.type_of(&self.value);
        (self.value, value_type)
    }
}

impl From<(String, String)> for Scalar {
    fn from((value, value_type): (String, String)) -> Self {
        Self::sql(value, value_type)
    }
}

struct Compiler {
    entities: HashMap<SlotId, EntitySource>,
    scalars: HashMap<ScalarSlotId, String>,
    // Value and type SQL for each list-bound variable currently in scope.
    list_elements: HashMap<ListVariableId, (String, String)>,
    // Scalar-subquery scopes opened so far; each gets unique column names.
    scopes: usize,
    traversals: HashMap<TraversalId, TraversalSource>,
    paths: HashMap<PathId, String>,
    scanned: Vec<(SlotId, EntityKind)>,
    from: Vec<String>,
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
            entities: HashMap::new(),
            scalars: HashMap::new(),
            list_elements: HashMap::new(),
            scopes: 0,
            traversals: HashMap::new(),
            paths: HashMap::new(),
            scanned: Vec::new(),
            from: Vec::new(),
            filters: Vec::new(),
            inputs: Vec::new(),
            ordering: None,
            bindings: Vec::new(),
            ctes: Vec::new(),
            has_recursive_cte: false,
        }
    }

    fn compile_match(&mut self, plan: &MatchPlan) -> Result<()> {
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
                    let label_sql = self.entity_field(*slot, "label")?;
                    let value = self.push_binding(SqlParameter::Text(label.clone()));
                    self.filters.push(format!("{label_sql} = {value}"));
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
                    self.compile_variable_length(VariableLengthSpec {
                        traversal: *traversal,
                        left: *left,
                        right: *right,
                        direction: *direction,
                        minimum: *minimum,
                        maximum: *maximum,
                        relationship_type: relationship_type.as_deref(),
                        properties,
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

        // Seed one identity trail per node, then append eligible edges with
        // UNION ALL so separate relationship sequences retain bag
        // multiplicity. `used_edges` enforces Cypher trail semantics without
        // forbidding a path from revisiting the same node.
        let body = format!(
            "SELECT seed.id, seed.id, 0, json_array(), json_array(seed.id) FROM nodes AS seed \
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
        let mut filters = Vec::new();
        for operator in operators {
            match operator {
                MatchOperator::HasLabel { slot, label } if *slot == endpoint => {
                    let expected = compiler.push_binding(SqlParameter::Text(label.clone()));
                    filters.push(format!("{node}.label = {expected}"));
                }
                MatchOperator::PropertyEquals { slot, key, value } if *slot == endpoint => {
                    let (property, property_type) =
                        compiler.json_property_parts(format!("{node}.props"), key);
                    filters.push(compiler.typed_value_comparison(
                        property,
                        property_type,
                        value.clone(),
                        BinaryOp::Equal,
                    ));
                }
                _ => {}
            }
        }
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

        self.compile_match(plan)?;
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
        let aggregating = stage.bindings.iter().any(|binding| {
            matches!(
                binding,
                WithBinding::Scalar { expression, .. } if expression.contains_aggregate()
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
        if !aggregating && let Some(filter) = &stage.filter {
            let predicate = self.predicate_expression(filter)?;
            self.filters.push(format!("({predicate})"));
        }

        let mut select = Vec::new();
        let mut entity_bindings = Vec::new();
        let mut path_bindings = Vec::new();
        let mut scalar_bindings = Vec::new();
        let mut group_by = Vec::new();
        for binding in &stage.bindings {
            match binding {
                WithBinding::Entity { slot, kind, .. } => {
                    if !entity_bindings
                        .iter()
                        .any(|(candidate, _)| candidate == slot)
                    {
                        select.push(format!(
                            "{} AS e{}",
                            self.encoded_entity(*slot, *kind)?,
                            slot.0
                        ));
                        entity_bindings.push((*slot, *kind));
                    }
                    if aggregating {
                        group_by.extend(self.grouping_terms(&Expr::Entity(*slot))?);
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
                    if aggregating
                        && !expression.contains_aggregate()
                        && expression.is_row_dependent()
                    {
                        group_by.extend(self.grouping_terms(expression)?);
                    }
                }
            }
        }

        let order_items = stage
            .order_by
            .iter()
            .map(|item| {
                let expression = self.raw_expression(&item.expression)?;
                Ok(order_expression(expression, item.direction))
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

        let mut sql = self.select_sql(&select);
        if aggregating && !group_by.is_empty() {
            sql.push_str(" GROUP BY ");
            sql.push_str(&group_by.join(", "));
        }
        if aggregating && let Some(filter) = &stage.filter {
            let predicate = self.predicate_expression(filter)?;
            sql.push_str(" HAVING (");
            sql.push_str(&predicate);
            sql.push(')');
        }
        if !order_items.is_empty() {
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
        Ok(())
    }

    fn compile_unwind(&mut self, stage: &UnwindStage) -> Result<()> {
        let (list_value, list_type) = self.scalar_parts(&stage.expression)?;
        let stage_index = self.ctes.len();
        let member = format!("u{stage_index}");
        self.from.push(format!(
            "json_each(CASE WHEN ({list_type}) = 'array' \
             THEN ({list_value}) ELSE '[]' END) AS {member}"
        ));

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
        select.push(format!(
            "{} AS s{}",
            encoded_scalar(&format!("{member}.value"), &format!("{member}.type")),
            stage.slot.0
        ));

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
        scalar_bindings.push(stage.slot);
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

    fn select_sql(&self, select: &[String]) -> String {
        let mut sql = format!("SELECT {}", select.join(", "));
        if !self.from.is_empty() {
            sql.push_str(" FROM ");
            sql.push_str(&self.from.join(", "));
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
            (EntityKind::Node, "label") => "label",
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
                let label = self.entity_field(slot, "label")?;
                format!("json_object('id', {id}, 'label', {label}, 'props', json({props}))")
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
        Ok(vec![format!("crabnet_semantic_key({encoded})")])
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

    /// The JSON text encoding of a scalar, as stored in result columns.
    fn encoded_expression(&mut self, expression: &Expr) -> Result<String> {
        self.scoped(&[expression], |_, parts| {
            let (value, value_type) = &parts[0];
            Ok(encoded_scalar(value, value_type))
        })
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
            typed.push(format!("b{scope}.{value}, ({bound_type}) AS {value_type}"));
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

    fn compile_scalar(&mut self, expression: &Expr) -> Result<Scalar> {
        match expression {
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
                PathFunctionKind::Nodes | PathFunctionKind::Relationships => Err(
                    Error::Unsupported("typed path collections in scalar expressions".to_owned()),
                ),
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
            Expr::RelationshipType(slot) => {
                Ok(Scalar::sql(self.entity_field(*slot, "type")?, "'text'"))
            }
            Expr::Labels(slot) => {
                let id = self.entity_field(*slot, "id")?;
                let label = self.entity_field(*slot, "label")?;
                Ok(Scalar::sql(
                    format!(
                        "CASE WHEN {id} IS NULL THEN NULL \
                         WHEN {label} IS NULL THEN '[]' ELSE json_array({label}) END"
                    ),
                    "'array'",
                ))
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
                // Only maps have keys; null and every other kind read as null.
                let map = |(value, value_type): &(String, String)| {
                    format!("(CASE WHEN ({value_type}) = 'object' THEN ({value}) END)")
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
                // and a string keys a map; anything else reads as null, as
                // does an out-of-range index or a missing key.
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
                        "(CASE WHEN ({base_type}) = 'array' AND ({index_type}) = 'integer' THEN \
                              CASE WHEN ({index}) >= 0 THEN '$[' || ({index}) || ']' \
                                   ELSE '$[#' || ({index}) || ']' END \
                          WHEN ({base_type}) = 'object' AND ({index_type}) = 'text' THEN \
                              '$.' || json_quote(({index})) END)"
                    );
                    // Only JSON containers may reach json_extract; a string
                    // base would be rejected as malformed JSON.
                    let container = format!(
                        "(CASE WHEN ({base_type}) IN ('array', 'object') THEN ({base}) END)"
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
                let all_integers = |types: &[String]| {
                    types
                        .iter()
                        .map(|t| format!("(({t}) IS NOT NULL AND ({t}) = 'integer')"))
                        .collect::<Vec<_>>()
                        .join(" AND ")
                };
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
                        "(CASE WHEN NOT ({}) THEN NULL \
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
                        all_integers(&types)
                    ))
                };
                self.composed(&operands, value, |types| {
                    format!(
                        "(CASE WHEN {} THEN 'array' ELSE 'null' END)",
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
            let compiled = (|| {
                let filter = match predicate {
                    Some(predicate) => format!(" WHERE ({})", this.boolean_expression(predicate)?),
                    None => String::new(),
                };
                let element = match projection {
                    Some(projection) => this.encoded_expression(projection)?,
                    None => element_json(&alias),
                };
                Ok::<_, Error>((filter, element))
            })();
            match previous {
                Some(previous) => {
                    this.list_elements.insert(variable, previous);
                }
                None => {
                    this.list_elements.remove(&variable);
                }
            }
            let (filter, element) = compiled?;
            Ok(format!(
                "(CASE WHEN ({list_type}) IS NULL OR ({list_type}) <> 'array' THEN NULL \
                 ELSE (SELECT json_group_array(json({element}) ORDER BY {alias}.key) \
                       FROM json_each({list_value}) AS {alias}{filter}) END)"
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

        if let Expr::Entity(slot) = argument {
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
        let non_null = bind(&|value, value_type| non_null_of(value, value_type))?;
        let numeric = bind(&|_, value_type| numeric_of(value_type))?;
        let invalid = bind(&|value, value_type| {
            format!(
                "({}) AND NOT ({})",
                non_null_of(value, value_type),
                numeric_of(value_type)
            )
        })?;
        let value = bind(&|value, _| value.to_owned())?;
        let value_type = bind(&|_, value_type| value_type.to_owned())?;
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
                Ok(min_max_parts(kind, &value, &value_type, modifier).into())
            }
            AggregateKind::Collect => {
                if distinct {
                    return Err(Error::Unsupported("collect(DISTINCT ...)".into()));
                }
                let encoded = bind(&|value, value_type| encoded_scalar(value, value_type))?;
                let ordering = self
                    .ordering
                    .as_ref()
                    .map(|ordering| format!(" ORDER BY {ordering}"))
                    .unwrap_or_default();
                Ok(Scalar::sql(
                    format!(
                        "json_group_array(json({encoded}){ordering}) FILTER (WHERE {non_null})"
                    ),
                    "'array'",
                ))
            }
        }
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
             ELSE NULL END)"
            );
            Ok(value)
        };
        self.composed(&[left, right], value, |types| {
            numeric_result_type(&types[0], &types[1])
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
             ELSE NULL END)"
            );
            Ok(value)
        };
        self.composed(&[operand], value, |types| {
            format!(
                "(CASE WHEN ({}) IN ('integer', 'real') THEN ({}) ELSE 'null' END)",
                types[0], types[0]
            )
        })
    }

    fn membership_expression(&mut self, left: &Expr, right: &Expr) -> Result<String> {
        self.scoped(&[left, right], |_, parts| {
            let (left_value, left_type) = &parts[0];
            let (right_value, right_type) = &parts[1];
            let list =
                format!("CASE WHEN ({right_type}) = 'array' THEN ({right_value}) ELSE '[]' END");
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
             WHEN ({right_type}) <> 'array' THEN NULL \
             WHEN EXISTS (SELECT 1 FROM json_each({list}) AS member WHERE ({equal})) THEN 1 \
             WHEN NOT EXISTS (SELECT 1 FROM json_each({list})) THEN 0 \
             WHEN ({left_type}) IS NULL OR ({left_type}) = 'null' THEN NULL \
             WHEN EXISTS (SELECT 1 FROM json_each({list}) AS member \
                          WHERE member.type = 'null') THEN NULL \
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
        let outcome = this.boolean_expression(predicate);
        match previous {
            Some(previous) => {
                this.list_elements.insert(variable, previous);
            }
            None => {
                this.list_elements.remove(&variable);
            }
        }
        let outcome = outcome?;
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
            "(CASE              WHEN ({list_type}) IS NULL OR ({list_type}) <> 'array' THEN NULL              ELSE (SELECT CASE {decision} END FROM                    (SELECT count(*) AS total,                            count(*) FILTER (WHERE outcome) AS trues,                            count(*) FILTER (WHERE outcome IS NULL) AS nulls                     FROM (SELECT ({outcome}) AS outcome                           FROM json_each({list_value}) AS {alias}))) END)"
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

fn order_expression(expression: String, direction: SortDirection) -> String {
    let direction = match direction {
        // Cypher's default is NULLS LAST for ascending order.
        SortDirection::Ascending => "ASC",
        // Cypher's default is NULLS FIRST for descending order.
        SortDirection::Descending => "DESC",
    };
    format!("(({expression}) IS NULL) {direction}, ({expression}) {direction}")
}

fn aggregate_distinct_key(value: &str, value_type: &str) -> String {
    let encoded = encoded_scalar(value, value_type);
    format!(
        "(CASE \
         WHEN ({value}) IS NULL OR ({value_type}) IS NULL OR ({value_type}) = 'null' THEN NULL \
         ELSE crabnet_semantic_key({encoded}) END)"
    )
}

fn min_max_parts(
    kind: AggregateKind,
    value: &str,
    value_type: &str,
    modifier: &str,
) -> (String, String) {
    let operator = match kind {
        AggregateKind::Minimum => "MIN",
        AggregateKind::Maximum => "MAX",
        _ => unreachable!("only min/max aggregates use min_max_parts"),
    };
    let non_null = format!(
        "({value}) IS NOT NULL AND ({value_type}) IS NOT NULL \
         AND ({value_type}) <> 'null'"
    );
    let numeric = format!("({value_type}) IN ('integer', 'real')");
    let text = format!("({value_type}) = 'text'");
    let boolean = format!("({value_type}) IN ('true', 'false')");
    let invalid = format!("({non_null}) AND NOT (({numeric}) OR ({text}) OR ({boolean}))");
    let numeric_count = format!("COUNT(*) FILTER (WHERE {numeric})");
    let text_count = format!("COUNT(*) FILTER (WHERE {text})");
    let boolean_count = format!("COUNT(*) FILTER (WHERE {boolean})");
    let invalid_count = format!("COUNT(*) FILTER (WHERE {invalid})");
    let family_count =
        format!("(({numeric_count} > 0) + ({text_count} > 0) + ({boolean_count} > 0))");
    let numeric_result = format!("{operator}({modifier}{value}) FILTER (WHERE {numeric})");
    let text_result = format!("{operator}({modifier}{value}) FILTER (WHERE {text})");
    let boolean_result = format!("{operator}({modifier}{value}) FILTER (WHERE {boolean})");
    let result = format!(
        "(CASE \
         WHEN {invalid_count} > 0 OR {family_count} > 1 THEN NULL \
         WHEN {numeric_count} > 0 THEN {numeric_result} \
         WHEN {text_count} > 0 THEN {text_result} \
         WHEN {boolean_count} > 0 THEN {boolean_result} \
         ELSE NULL END)"
    );
    let result_type = format!(
        "(CASE \
         WHEN {invalid_count} > 0 OR {family_count} > 1 THEN 'null' \
         WHEN {numeric_count} > 0 THEN \
           CASE WHEN typeof(({numeric_result})) = 'integer' THEN 'integer' ELSE 'real' END \
         WHEN {text_count} > 0 THEN 'text' \
         WHEN {boolean_count} > 0 THEN \
           CASE WHEN ({boolean_result}) THEN 'true' ELSE 'false' END \
         ELSE 'null' END)"
    );
    (result, result_type)
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
    expression.contains_aggregate()
        || matches!(
            expression,
            Expr::Value(_)
                | Expr::Scalar(_)
                | Expr::Property(_, _)
                | Expr::ElementId(_)
                | Expr::RelationshipType(_)
                | Expr::ListElement(_)
        )
}

// The JSON type name of a three-valued SQL boolean.
fn boolean_type(value: &str) -> String {
    format!(
        "(CASE WHEN ({value}) IS NULL THEN 'null' \
         WHEN ({value}) THEN 'true' ELSE 'false' END)"
    )
}

fn boolean_value(value_type: &str) -> String {
    format!(
        "(CASE WHEN ({value_type}) = 'true' THEN 1 \
         WHEN ({value_type}) = 'false' THEN 0 ELSE NULL END)"
    )
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
    let comparison = match operator {
        BinaryOp::Equal => equal,
        BinaryOp::NotEqual => format!("NOT {equal}"),
        _ => unreachable!("typed comparisons are only generated for equality operators"),
    };
    format!("(CASE WHEN ({left}) IS NULL OR ({right}) IS NULL THEN NULL ELSE {comparison} END)")
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
         ELSE NULL END)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::lower;

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
            assert!(start.sql.contains("trail_node.label = ?1"));
            assert!(start.sql.contains("json_type(trail_node.props"));
            assert!(start.sql.ends_with(" ORDER BY trail_node.id"));
            assert_eq!(
                start.bindings.as_slice(),
                &[
                    SqlValue::Text("Person".into()),
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
