use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
use serde_json::Value;

use crate::{Error, Parameters, Result};

use super::logical::{
    AggregateKind, BinaryOp, Direction, EntityKind, Expr, Input, MatchOperator, MatchPlan,
    PathFunctionKind, PathId, PathSegment, Projection, ReadStage, RelationshipUse, ScalarSlotId,
    SlotId, SortDirection, TraversalId, UnaryOp, UnwindStage, ValueExpr, WithBinding, WithStage,
    validate_storable_value,
};

#[derive(Clone, Debug)]
enum SqlBinding {
    Text(String),
    Scalar(ValueExpr),
    Json(ValueExpr),
    Page(ValueExpr),
}

impl SqlBinding {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Scalar(_) => "scalar",
            Self::Json(_) => "json",
            Self::Page(_) => "page",
        }
    }

    fn resolve(&self, parameters: &Parameters) -> Result<SqlValue> {
        match self {
            Self::Text(value) => Ok(SqlValue::Text(value.clone())),
            Self::Scalar(value) => json_to_sql(value.resolve(parameters)?),
            Self::Json(value) => {
                let value = value.resolve(parameters)?;
                validate_storable_value(&value)?;
                Ok(SqlValue::Text(value.to_string()))
            }
            Self::Page(value) => {
                let value = value.resolve(parameters)?;
                let Value::Number(number) = value else {
                    return Err(Error::Semantic(
                        "SKIP and LIMIT require nonnegative integers".into(),
                    ));
                };
                if let Some(integer) = number.as_i64()
                    && integer >= 0
                {
                    return Ok(SqlValue::Integer(integer));
                }
                if let Some(unsigned) = number.as_u64()
                    && let Ok(integer) = i64::try_from(unsigned)
                {
                    return Ok(SqlValue::Integer(integer));
                }
                Err(Error::Semantic(
                    "SKIP and LIMIT require nonnegative integers".into(),
                ))
            }
        }
    }
}

fn json_to_sql(value: Value) -> Result<SqlValue> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(value) => Ok(SqlValue::Integer(i64::from(value))),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(SqlValue::Integer(value))
            } else if let Some(value) = number.as_u64() {
                i64::try_from(value).map(SqlValue::Integer).map_err(|_| {
                    Error::Semantic("integer parameter is outside the i64 range".into())
                })
            } else {
                number
                    .as_f64()
                    .map(SqlValue::Real)
                    .ok_or_else(|| Error::Semantic("non-finite floats are not supported".into()))
            }
        }
        Value::String(value) => Ok(SqlValue::Text(value)),
        Value::Array(_) | Value::Object(_) => Ok(SqlValue::Text(value.to_string())),
    }
}

pub(crate) struct SqlQuery {
    pub(crate) sql: String,
    pub(crate) columns: Vec<String>,
    pub(crate) output_shapes: Vec<OutputShape>,
    bindings: Vec<SqlBinding>,
    distinct: bool,
    post_offset: Option<ValueExpr>,
    post_limit: Option<ValueExpr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputShape {
    Json,
    Entity(EntityKind),
    PathIdentity,
    PathNodes,
    PathRelationships,
}

impl SqlQuery {
    pub(crate) fn resolve_bindings(&self, parameters: &Parameters) -> Result<Vec<SqlValue>> {
        resolve_bindings(&self.bindings, parameters)
    }

    pub(crate) fn distinct(&self) -> bool {
        self.distinct
    }

    pub(crate) fn has_path_identity_output(&self) -> bool {
        self.output_shapes.iter().any(|shape| {
            matches!(
                shape,
                OutputShape::PathIdentity | OutputShape::PathNodes | OutputShape::PathRelationships
            )
        })
    }

    pub(crate) fn render_physical(&self, batch_hydrate_paths: bool) -> String {
        let mut output = String::from("physical-query\n  sqlite-fragment:\n    sql: ");
        output.push_str(&self.sql);
        output.push_str("\n    bindings:\n");
        if self.bindings.is_empty() {
            output.push_str("      none\n");
        } else {
            for (index, binding) in self.bindings.iter().enumerate() {
                output.push_str(&format!("      ?{} {}\n", index + 1, binding.kind_name()));
            }
        }

        let mut rust_stages = Vec::new();
        if self.distinct {
            rust_stages.push("semantic-distinct");
            if self.post_offset.is_some() || self.post_limit.is_some() {
                rust_stages.push("pagination");
            }
        }
        if batch_hydrate_paths && self.has_path_identity_output() {
            rust_stages.push("batch-hydrate-paths");
        }

        output.push_str("  rust-stage: ");
        if rust_stages.is_empty() {
            output.push_str("none");
        } else {
            output.push_str(&rust_stages.join(" then "));
        }
        output.push('\n');
        output
    }

    pub(crate) fn resolve_post_pagination(
        &self,
        parameters: &Parameters,
    ) -> Result<(usize, Option<usize>)> {
        let offset = self
            .post_offset
            .as_ref()
            .map(|value| resolve_page(value, parameters))
            .transpose()?
            .unwrap_or(0);
        let limit = self
            .post_limit
            .as_ref()
            .map(|value| resolve_page(value, parameters))
            .transpose()?;
        Ok((offset, limit))
    }
}

fn resolve_page(value: &ValueExpr, parameters: &Parameters) -> Result<usize> {
    let SqlValue::Integer(value) = SqlBinding::Page(value.clone()).resolve(parameters)? else {
        unreachable!("page bindings always resolve to SQLite integers")
    };
    usize::try_from(value)
        .map_err(|_| Error::Semantic("SKIP and LIMIT are too large for this platform".into()))
}

pub(crate) struct RowSource {
    pub(crate) sql: String,
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    bindings: Vec<SqlBinding>,
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

fn resolve_bindings(bindings: &[SqlBinding], parameters: &Parameters) -> Result<Vec<SqlValue>> {
    bindings
        .iter()
        .map(|binding| binding.resolve(parameters))
        .collect()
}

pub(crate) fn compile_read(input: &Input, projection: &Projection) -> Result<SqlQuery> {
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
            Some(limit) => sql.push_str(&compiler.push_binding(SqlBinding::Page(limit.clone()))),
            None => sql.push_str("-1"),
        }
    }
    if !projection.distinct
        && let Some(offset) = &projection.offset
    {
        sql.push_str(" OFFSET ");
        sql.push_str(&compiler.push_binding(SqlBinding::Page(offset.clone())));
    }

    sql = compiler.with_ctes(sql);

    debug_assert_eq!(projection.columns.len(), output_shapes.len());
    Ok(SqlQuery {
        sql,
        columns: projection.columns.clone(),
        output_shapes,
        bindings: compiler.bindings,
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

    Ok(RowSource {
        sql,
        slots: compiler.scanned,
        bindings: compiler.bindings,
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

#[derive(Clone, Debug)]
struct TraversalSource {
    edge_ids: String,
    node_ids: String,
}

struct Compiler {
    entities: HashMap<SlotId, EntitySource>,
    scalars: HashMap<ScalarSlotId, String>,
    traversals: HashMap<TraversalId, TraversalSource>,
    paths: HashMap<PathId, String>,
    scanned: Vec<(SlotId, EntityKind)>,
    from: Vec<String>,
    filters: Vec<String>,
    ordering: Option<String>,
    bindings: Vec<SqlBinding>,
    ctes: Vec<(String, String)>,
    has_recursive_cte: bool,
}

impl Compiler {
    fn new() -> Self {
        Self {
            entities: HashMap::new(),
            scalars: HashMap::new(),
            traversals: HashMap::new(),
            paths: HashMap::new(),
            scanned: Vec::new(),
            from: Vec::new(),
            filters: Vec::new(),
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
                    let value = self.push_binding(SqlBinding::Text(label.clone()));
                    self.filters.push(format!("{label_sql} = {value}"));
                }
                MatchOperator::HasRelationshipType {
                    slot,
                    relationship_type,
                } => {
                    let type_sql = self.entity_field(*slot, "type")?;
                    let value = self.push_binding(SqlBinding::Text(relationship_type.clone()));
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
                MatchOperator::DistinctRelationships(relationships) => {
                    self.compile_relationship_uniqueness(relationships)?;
                }
                MatchOperator::BindPath {
                    path,
                    start,
                    segments,
                } => self.compile_path_binding(*path, *start, segments)?,
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
            let expected = self.push_binding(SqlBinding::Text(relationship_type.to_owned()));
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
            return Err(Error::Unsupported("named paths in OPTIONAL MATCH".into()));
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
                    let (value, value_type) = self.scalar_parts(expression)?;
                    select.push(format!(
                        "{} AS s{}",
                        encoded_scalar(&value, &value_type),
                        slot.0
                    ));
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
                Some(limit) => sql.push_str(&self.push_binding(SqlBinding::Page(limit.clone()))),
                None => sql.push_str("-1"),
            }
        }
        if let Some(offset) = &stage.offset {
            sql.push_str(" OFFSET ");
            sql.push_str(&self.push_binding(SqlBinding::Page(offset.clone())));
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
                    let (value, value_type) = self.scalar_parts(expression)?;
                    return Ok((encoded_scalar(&value, &value_type), OutputShape::Json));
                }
                PathFunctionKind::Nodes => OutputShape::PathNodes,
                PathFunctionKind::Relationships => OutputShape::PathRelationships,
            };
            return Ok((self.path_source(*path)?.to_owned(), shape));
        }

        let (value, value_type) = self.scalar_parts(expression)?;
        Ok((encoded_scalar(&value, &value_type), OutputShape::Json))
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
        let (value, value_type) = self.scalar_parts(expression)?;
        Ok(vec![format!(
            "crabnet_semantic_key({})",
            encoded_scalar(&value, &value_type)
        )])
    }

    fn raw_expression(&mut self, expression: &Expr) -> Result<String> {
        let (value, value_type) = self.scalar_parts(expression)?;
        // Some values (notably literals and parameters) use a separate bind
        // to carry their JSON type. Keep that bind visible even when ORDER BY
        // needs only the raw payload, or SQLite's parameter count and the
        // planner's binding vector can diverge.
        Ok(format!(
            "CASE WHEN ({value_type}) IS NULL THEN ({value}) ELSE ({value}) END"
        ))
    }

    fn predicate_expression(&mut self, expression: &Expr) -> Result<String> {
        self.boolean_expression(expression)
    }

    fn boolean_expression(&mut self, expression: &Expr) -> Result<String> {
        let (value, value_type) = self.scalar_parts(expression)?;
        // Retain the value expression as well as its type gate. Most dynamic
        // types derive from the value, but aggregates can have a static result
        // type; dropping their value here would also drop argument bindings.
        let retained_type =
            format!("CASE WHEN ({value}) IS NULL THEN ({value_type}) ELSE ({value_type}) END");
        Ok(boolean_value(&retained_type))
    }

    fn scalar_parts(&mut self, expression: &Expr) -> Result<(String, String)> {
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
                    Ok((
                        format!(
                            "CASE WHEN ({source}) IS NULL \
                                      OR json_type(({source}), '$') = 'null' \
                             THEN NULL ELSE \
                               (SELECT COALESCE(SUM(json_array_length(\
                                   path_segment.value, '$.relationships')), 0) \
                                FROM json_each(({source}), '$.segments') AS path_segment) \
                             END"
                        ),
                        "'integer'".into(),
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
                Ok((
                    format!("json_extract({source}, '$')"),
                    format!("json_type({source}, '$')"),
                ))
            }
            Expr::Value(value) => {
                let json = self.push_binding(SqlBinding::Json(value.clone()));
                let json = format!("json({json})");
                Ok((
                    format!("json_extract({json}, '$')"),
                    format!("json_type({json}, '$')"),
                ))
            }
            Expr::Property(slot, key) => self.entity_property_parts(*slot, key),
            Expr::ElementId(slot) => Ok((self.entity_field(*slot, "id")?, "'text'".into())),
            Expr::RelationshipType(slot) => {
                Ok((self.entity_field(*slot, "type")?, "'text'".into()))
            }
            Expr::Labels(slot) => {
                let id = self.entity_field(*slot, "id")?;
                let label = self.entity_field(*slot, "label")?;
                Ok((
                    format!(
                        "CASE WHEN {id} IS NULL THEN NULL \
                         WHEN {label} IS NULL THEN '[]' ELSE json_array({label}) END"
                    ),
                    "'array'".into(),
                ))
            }
            Expr::Binary {
                left,
                operator,
                right,
            } => {
                let predicate = match operator {
                    BinaryOp::Equal | BinaryOp::NotEqual => {
                        let (left_value, left_type) = self.scalar_parts(left)?;
                        let (right_value, right_type) = self.scalar_parts(right)?;
                        typed_sql_comparison(
                            &left_value,
                            &left_type,
                            &right_value,
                            &right_type,
                            *operator,
                        )
                    }
                    BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual => {
                        let (left_value, left_type) = self.scalar_parts(left)?;
                        let (right_value, right_type) = self.scalar_parts(right)?;
                        typed_sql_ordering(
                            &left_value,
                            &left_type,
                            &right_value,
                            &right_type,
                            *operator,
                        )
                    }
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
                Ok(boolean_parts(predicate))
            }
            Expr::Unary { operator, operand } => match operator {
                UnaryOp::Not => {
                    let operand = self.boolean_expression(operand)?;
                    Ok(boolean_parts(format!("(NOT ({operand}))")))
                }
                UnaryOp::Plus | UnaryOp::Minus => self.numeric_unary(operand, *operator),
            },
            Expr::IsNull {
                expression,
                negated,
            } => {
                let expression = self.raw_expression(expression)?;
                Ok(boolean_parts(format!(
                    "(({expression}) IS {}NULL)",
                    if *negated { "NOT " } else { "" }
                )))
            }
            Expr::Aggregate {
                kind,
                argument,
                distinct,
            } => self.aggregate_parts(*kind, argument.as_deref(), *distinct),
        }
    }

    fn aggregate_parts(
        &mut self,
        kind: AggregateKind,
        argument: Option<&Expr>,
        distinct: bool,
    ) -> Result<(String, String)> {
        if kind == AggregateKind::Count && argument.is_none() {
            return Ok(("COUNT(*)".into(), "'integer'".into()));
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
            return Ok((
                format!("COUNT({}{value})", if distinct { "DISTINCT " } else { "" }),
                "'integer'".into(),
            ));
        }

        let (value, value_type) = self.scalar_parts(argument)?;
        let non_null = format!(
            "({value}) IS NOT NULL AND ({value_type}) IS NOT NULL \
             AND ({value_type}) <> 'null'"
        );
        let numeric = format!("({value_type}) IN ('integer', 'real')");
        let modifier = if distinct { "DISTINCT " } else { "" };

        match kind {
            AggregateKind::Count => {
                let counted = if distinct {
                    aggregate_distinct_key(&value, &value_type)
                } else {
                    format!("CASE WHEN {non_null} THEN ({value}) END")
                };
                Ok((format!("COUNT({modifier}{counted})"), "'integer'".into()))
            }
            AggregateKind::Sum => {
                let invalid = format!("({non_null}) AND NOT ({numeric})");
                let sum = format!("SUM({modifier}{value}) FILTER (WHERE {numeric})");
                let value = format!(
                    "(CASE WHEN COUNT(*) FILTER (WHERE {invalid}) > 0 THEN NULL \
                     ELSE COALESCE({sum}, 0) END)"
                );
                Ok(numeric_parts(value))
            }
            AggregateKind::Average => {
                let invalid = format!("({non_null}) AND NOT ({numeric})");
                let average = format!("AVG({modifier}{value}) FILTER (WHERE {numeric})");
                let value = format!(
                    "(CASE WHEN COUNT(*) FILTER (WHERE {invalid}) > 0 THEN NULL \
                     ELSE {average} END)"
                );
                Ok((value, "'real'".into()))
            }
            AggregateKind::Minimum | AggregateKind::Maximum => {
                Ok(min_max_parts(kind, &value, &value_type, modifier))
            }
            AggregateKind::Collect => {
                if distinct {
                    return Err(Error::Unsupported("collect(DISTINCT ...)".into()));
                }
                let encoded = encoded_scalar(&value, &value_type);
                let ordering = self
                    .ordering
                    .as_ref()
                    .map(|ordering| format!(" ORDER BY {ordering}"))
                    .unwrap_or_default();
                Ok((
                    format!(
                        "json_group_array(json({encoded}){ordering}) FILTER (WHERE {non_null})"
                    ),
                    "'array'".into(),
                ))
            }
        }
    }

    fn numeric_binary(
        &mut self,
        left: &Expr,
        right: &Expr,
        operator: BinaryOp,
    ) -> Result<(String, String)> {
        let (left_value, left_type) = self.scalar_parts(left)?;
        let (right_value, right_type) = self.scalar_parts(right)?;
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
                BinaryOp::Add => "+",
                BinaryOp::Subtract => "-",
                BinaryOp::Multiply => "*",
                BinaryOp::Divide => "/",
                BinaryOp::Remainder => unreachable!(),
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
        Ok(numeric_parts(value))
    }

    fn numeric_unary(&mut self, operand: &Expr, operator: UnaryOp) -> Result<(String, String)> {
        let (operand_value, operand_type) = self.scalar_parts(operand)?;
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
        Ok(numeric_parts(value))
    }

    fn membership_expression(&mut self, left: &Expr, right: &Expr) -> Result<String> {
        let (left_value, left_type) = self.scalar_parts(left)?;
        let (right_value, right_type) = self.scalar_parts(right)?;
        let list = format!("CASE WHEN ({right_type}) = 'array' THEN ({right_value}) ELSE '[]' END");
        let member_value = "member.value";
        let member_type = "member.type";
        let equal = typed_sql_comparison(
            &left_value,
            &left_type,
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
    }

    fn string_predicate(
        &mut self,
        left: &Expr,
        right: &Expr,
        operator: BinaryOp,
    ) -> Result<String> {
        let (left_value, left_type) = self.scalar_parts(left)?;
        let (right_value, right_type) = self.scalar_parts(right)?;
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
    }

    fn entity_property_parts(&mut self, slot: SlotId, key: &str) -> Result<(String, String)> {
        let props = self.entity_props(slot)?;
        Ok(self.json_property_parts(props, key))
    }

    fn json_property_parts(&mut self, props: String, key: &str) -> (String, String) {
        let path = self.push_binding(SqlBinding::Text(key.to_owned()));
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
        let expected_scalar = self.push_binding(SqlBinding::Scalar(expected.clone()));
        let expected_json = self.push_binding(SqlBinding::Json(expected));
        let expected_type = format!("json_type(json({expected_json}))");
        typed_sql_comparison(
            &property,
            &property_type,
            &expected_scalar,
            &expected_type,
            operator,
        )
    }

    fn push_binding(&mut self, binding: SqlBinding) -> String {
        self.bindings.push(binding);
        format!("?{}", self.bindings.len())
    }
}

fn encoded_scalar(value: &str, value_type: &str) -> String {
    format!(
        "(CASE \
         WHEN ({value}) IS NULL OR ({value_type}) IS NULL OR ({value_type}) = 'null' THEN 'null' \
         WHEN ({value_type}) = 'true' THEN 'true' \
         WHEN ({value_type}) = 'false' THEN 'false' \
         WHEN ({value_type}) = 'text' THEN json_quote(({value})) \
         ELSE json(({value})) END)"
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

fn boolean_parts(value: String) -> (String, String) {
    let value_type = format!(
        "(CASE WHEN ({value}) IS NULL THEN 'null' \
         WHEN ({value}) THEN 'true' ELSE 'false' END)"
    );
    (value, value_type)
}

fn boolean_value(value_type: &str) -> String {
    format!(
        "(CASE WHEN ({value_type}) = 'true' THEN 1 \
         WHEN ({value_type}) = 'false' THEN 0 ELSE NULL END)"
    )
}

fn numeric_parts(value: String) -> (String, String) {
    let value_type = format!(
        "(CASE WHEN ({value}) IS NULL THEN 'null' \
         WHEN typeof(({value})) = 'integer' THEN 'integer' ELSE 'real' END)"
    );
    (value, value_type)
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
        assert!(branch.contains("rust-stage: semantic-distinct then pagination\n"));
        assert!(!branch.contains("batch-hydrate-paths"));
        let root = plan.render_physical(true);
        assert!(
            root.contains(
                "rust-stage: semantic-distinct then pagination then batch-hydrate-paths\n"
            )
        );
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
        for index in 1..=plan.bindings.len() {
            assert!(
                plan.sql.contains(&format!("?{index}")),
                "missing bind position ?{index} in {}",
                plan.sql
            );
        }
        assert!(!plan.sql.contains(&format!("?{}", plan.bindings.len() + 1)));
    }
}
