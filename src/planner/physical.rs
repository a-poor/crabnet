use std::collections::{HashMap, HashSet};

use rusqlite::types::Value as SqlValue;
use serde_json::Value;

use crate::{Error, Parameters, Result};

use super::{
    UnionKind,
    logical::{Direction, EntityKind, TraversalId, ValueExpr, validate_storable_value},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RuntimeSlot(pub(crate) usize);

#[derive(Clone, Debug)]
pub(crate) enum SqlParameter {
    Text(String),
    Scalar(ValueExpr),
    Json(ValueExpr),
    Page(ValueExpr),
}

impl SqlParameter {
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Scalar(_) => "scalar",
            Self::Json(_) => "json",
            Self::Page(_) => "page",
        }
    }

    pub(crate) fn resolve(&self, parameters: &Parameters) -> Result<SqlValue> {
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

pub(crate) fn resolve_page(value: &ValueExpr, parameters: &Parameters) -> Result<usize> {
    let SqlValue::Integer(value) = SqlParameter::Page(value.clone()).resolve(parameters)? else {
        unreachable!("page parameters always resolve to SQLite integers")
    };
    usize::try_from(value)
        .map_err(|_| Error::Semantic("SKIP and LIMIT are too large for this platform".into()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrailDirection {
    Right,
    Left,
    Undirected,
}

impl TrailDirection {
    fn name(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Left => "left",
            Self::Undirected => "undirected",
        }
    }
}

impl From<Direction> for TrailDirection {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::Right => Self::Right,
            Direction::Left => Self::Left,
            Direction::Undirected => Self::Undirected,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShortestMode {
    One,
    All,
}

impl ShortestMode {
    fn name(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::All => "all",
        }
    }
}

/// A parameterized SQLite query selecting the node IDs that one traversal
/// endpoint may take. It is always a superset of the endpoint's final
/// matches: the consuming fragment still joins the endpoint slot, so the
/// domain only prunes seeds and emitted rows, never semantics.
#[derive(Clone, Debug)]
pub(crate) struct NodeDomainSpec {
    pub(crate) sql: String,
    pub(crate) bindings: Vec<SqlParameter>,
}

impl NodeDomainSpec {
    fn resolve(&self, parameters: &Parameters) -> Result<ResolvedNodeDomain> {
        Ok(ResolvedNodeDomain {
            sql: self.sql.clone(),
            bindings: resolve_bindings(&self.bindings, parameters)?,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedNodeDomain {
    pub(crate) sql: String,
    pub(crate) bindings: Vec<SqlValue>,
}

fn resolve_bindings(bindings: &[SqlParameter], parameters: &Parameters) -> Result<Vec<SqlValue>> {
    bindings
        .iter()
        .map(|binding| binding.resolve(parameters))
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct TrailExpansionSpec {
    pub(crate) traversal: TraversalId,
    pub(crate) direction: TrailDirection,
    pub(crate) minimum: usize,
    pub(crate) maximum: Option<usize>,
    pub(crate) edge_sql: String,
    pub(crate) edge_bindings: Vec<SqlParameter>,
    pub(crate) start_domain: Option<NodeDomainSpec>,
    pub(crate) end_domain: Option<NodeDomainSpec>,
}

impl TrailExpansionSpec {
    fn resolve(&self, parameters: &Parameters) -> Result<ResolvedTrailExpansion> {
        Ok(ResolvedTrailExpansion {
            traversal: self.traversal.0,
            direction: self.direction,
            minimum: self.minimum,
            maximum: self.maximum,
            edge_sql: self.edge_sql.clone(),
            edge_bindings: resolve_bindings(&self.edge_bindings, parameters)?,
            start_domain: self
                .start_domain
                .as_ref()
                .map(|domain| domain.resolve(parameters))
                .transpose()?,
            end_domain: self
                .end_domain
                .as_ref()
                .map(|domain| domain.resolve(parameters))
                .transpose()?,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedTrailExpansion {
    pub(crate) traversal: usize,
    pub(crate) direction: TrailDirection,
    pub(crate) minimum: usize,
    pub(crate) maximum: Option<usize>,
    pub(crate) edge_sql: String,
    pub(crate) edge_bindings: Vec<SqlValue>,
    pub(crate) start_domain: Option<ResolvedNodeDomain>,
    pub(crate) end_domain: Option<ResolvedNodeDomain>,
}

#[derive(Clone, Debug)]
pub(crate) enum ValueOperator {
    TrailExpand {
        output: RuntimeSlot,
        spec: TrailExpansionSpec,
    },
    ShortestExpand {
        output: RuntimeSlot,
        mode: ShortestMode,
        spec: TrailExpansionSpec,
    },
}

impl ValueOperator {
    fn output(&self) -> RuntimeSlot {
        match self {
            Self::TrailExpand { output, .. } | Self::ShortestExpand { output, .. } => *output,
        }
    }

    fn prepare(self, parameters: &Parameters) -> Result<PreparedValueOperator> {
        match self {
            Self::TrailExpand { output, spec } => Ok(PreparedValueOperator::TrailExpand {
                output,
                spec: spec.resolve(parameters)?,
            }),
            Self::ShortestExpand { output, mode, spec } => {
                Ok(PreparedValueOperator::ShortestExpand {
                    output,
                    mode,
                    spec: spec.resolve(parameters)?,
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum FragmentArgument {
    Static(SqlParameter),
    Runtime(RuntimeSlot),
}

impl FragmentArgument {
    fn describe(&self) -> String {
        match self {
            Self::Static(parameter) => parameter.kind_name().to_owned(),
            Self::Runtime(slot) => format!("runtime(%r{})", slot.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputShape {
    Json,
    Entity(EntityKind),
    PathIdentity,
    PathNodes,
    PathRelationships,
}

#[derive(Clone, Debug)]
pub(crate) struct SqlFragment {
    pub(crate) sql: String,
    pub(crate) columns: Vec<String>,
    pub(crate) output_shapes: Vec<OutputShape>,
    pub(crate) arguments: Vec<FragmentArgument>,
    pub(crate) inputs: Vec<ValueOperator>,
}

impl SqlFragment {
    pub(crate) fn has_path_identity_output(&self) -> bool {
        self.output_shapes.iter().any(|shape| {
            matches!(
                shape,
                OutputShape::PathIdentity | OutputShape::PathNodes | OutputShape::PathRelationships
            )
        })
    }

    fn prepare(self, parameters: &Parameters) -> Result<PreparedSqlFragment> {
        let mut inputs = HashMap::with_capacity(self.inputs.len());
        for input in self.inputs {
            let output = input.output();
            if inputs.insert(output, input).is_some() {
                return Err(Error::Data(format!(
                    "physical plan produces runtime slot %r{} more than once",
                    output.0
                )));
            }
        }

        let mut prepared_inputs = Vec::new();
        let mut consumed_slots = HashSet::with_capacity(inputs.len());
        let mut arguments = Vec::with_capacity(self.arguments.len());
        for argument in self.arguments {
            match argument {
                FragmentArgument::Static(parameter) => arguments.push(
                    PreparedFragmentArgument::Value(parameter.resolve(parameters)?),
                ),
                FragmentArgument::Runtime(slot) => {
                    if !consumed_slots.insert(slot) {
                        return Err(Error::Data(format!(
                            "physical SQL fragment consumes runtime slot %r{} more than once",
                            slot.0
                        )));
                    }
                    let input = inputs.remove(&slot).ok_or_else(|| {
                        Error::Data(format!(
                            "physical SQL fragment reads missing runtime slot %r{}",
                            slot.0
                        ))
                    })?;
                    prepared_inputs.push(input.prepare(parameters)?);
                    arguments.push(PreparedFragmentArgument::Runtime(slot));
                }
            }
        }
        if let Some(slot) = inputs.keys().next() {
            return Err(Error::Data(format!(
                "physical plan produces unreferenced runtime slot %r{}",
                slot.0
            )));
        }

        Ok(PreparedSqlFragment {
            sql: self.sql,
            columns: self.columns,
            output_shapes: self.output_shapes,
            arguments,
            inputs: prepared_inputs,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PhysicalOperator {
    Sqlite(SqlFragment),
    SemanticDistinct {
        input: Box<Self>,
    },
    Paginate {
        input: Box<Self>,
        offset: Option<ValueExpr>,
        limit: Option<ValueExpr>,
    },
    Union {
        left: Box<Self>,
        right: Box<Self>,
        kind: UnionKind,
    },
    BatchHydrate {
        input: Box<Self>,
    },
}

impl PhysicalOperator {
    pub(crate) fn prepare(self, parameters: &Parameters) -> Result<PreparedPhysicalOperator> {
        match self {
            Self::Sqlite(fragment) => fragment
                .prepare(parameters)
                .map(PreparedPhysicalOperator::Sqlite),
            Self::SemanticDistinct { input } => Ok(PreparedPhysicalOperator::SemanticDistinct {
                input: Box::new(input.prepare(parameters)?),
            }),
            Self::Paginate {
                input,
                offset,
                limit,
            } => {
                // Preserve the old branch-preflight ordering: deferred page
                // values are checked before any fragment argument.
                let offset = offset
                    .as_ref()
                    .map(|value| resolve_page(value, parameters))
                    .transpose()?
                    .unwrap_or(0);
                let limit = limit
                    .as_ref()
                    .map(|value| resolve_page(value, parameters))
                    .transpose()?;
                Ok(PreparedPhysicalOperator::Paginate {
                    input: Box::new(input.prepare(parameters)?),
                    offset,
                    limit,
                })
            }
            Self::Union { left, right, kind } => Ok(PreparedPhysicalOperator::Union {
                left: Box::new(left.prepare(parameters)?),
                right: Box::new(right.prepare(parameters)?),
                kind,
            }),
            Self::BatchHydrate { input } => Ok(PreparedPhysicalOperator::BatchHydrate {
                input: Box::new(input.prepare(parameters)?),
            }),
        }
    }

    pub(crate) fn render(&self) -> String {
        let mut output = String::from("physical-plan\n");
        let (body, hydrate) = match self {
            Self::BatchHydrate { input } => (input.as_ref(), true),
            body => (body, false),
        };
        let mut operator_index = 0;
        let mut branch_index = 0;
        render_union_tree(body, &mut output, &mut operator_index, &mut branch_index);
        if hydrate {
            output.push_str(&format!(
                "  operator {}: batch-hydrate-paths\n",
                operator_index
            ));
        }
        output
    }
}

fn render_union_tree(
    operator: &PhysicalOperator,
    output: &mut String,
    operator_index: &mut usize,
    branch_index: &mut usize,
) {
    match operator {
        PhysicalOperator::Union { left, right, kind } => {
            render_union_tree(left, output, operator_index, branch_index);
            render_union_tree(right, output, operator_index, branch_index);
            output.push_str(&format!(
                "  operator {}: union mode={}\n",
                *operator_index,
                match kind {
                    UnionKind::All => "all",
                    UnionKind::Distinct => "semantic-distinct",
                }
            ));
            *operator_index += 1;
        }
        branch => {
            output.push_str(&format!("  branch {}:\n", *branch_index));
            *branch_index += 1;
            render_branch(branch, output, operator_index);
        }
    }
}

fn render_branch(operator: &PhysicalOperator, output: &mut String, index: &mut usize) {
    match operator {
        PhysicalOperator::Sqlite(fragment) => {
            for input in &fragment.inputs {
                render_value_operator(input, output, index);
            }
            output.push_str(&format!("    operator {}: sqlite-fragment\n", *index));
            *index += 1;
            output.push_str("      sql: ");
            output.push_str(&fragment.sql);
            output.push_str("\n      bindings:\n");
            if fragment.arguments.is_empty() {
                output.push_str("        none\n");
            } else {
                for (argument_index, argument) in fragment.arguments.iter().enumerate() {
                    output.push_str(&format!(
                        "        ?{} {}\n",
                        argument_index + 1,
                        argument.describe()
                    ));
                }
            }
        }
        PhysicalOperator::SemanticDistinct { input } => {
            render_branch(input, output, index);
            output.push_str(&format!("    operator {}: semantic-distinct\n", *index));
            *index += 1;
        }
        PhysicalOperator::Paginate { input, .. } => {
            render_branch(input, output, index);
            output.push_str(&format!("    operator {}: pagination\n", *index));
            *index += 1;
        }
        PhysicalOperator::Union { .. } | PhysicalOperator::BatchHydrate { .. } => {
            unreachable!("union and hydration are rendered outside branch scopes")
        }
    }
}

fn render_value_operator(operator: &ValueOperator, output: &mut String, index: &mut usize) {
    let (name, output_slot, spec) = match operator {
        ValueOperator::TrailExpand { output, spec } => ("trail-expand".to_owned(), output, spec),
        ValueOperator::ShortestExpand { output, mode, spec } => (
            format!("shortest-expand mode={}", mode.name()),
            output,
            spec,
        ),
    };
    let maximum = spec
        .maximum
        .map_or_else(|| "unbounded".to_owned(), |value| value.to_string());
    output.push_str(&format!(
        "    operator {}: {name} output=%r{} traversal=~{} direction={} bounds={}..{}\n",
        *index,
        output_slot.0,
        spec.traversal.0,
        spec.direction.name(),
        spec.minimum,
        maximum
    ));
    render_source(output, "edge-source", &spec.edge_sql, &spec.edge_bindings);
    render_domain(output, "start-domain", spec.start_domain.as_ref());
    render_domain(output, "end-domain", spec.end_domain.as_ref());
    *index += 1;
}

fn render_domain(output: &mut String, name: &str, domain: Option<&NodeDomainSpec>) {
    match domain {
        Some(domain) => render_source(output, name, &domain.sql, &domain.bindings),
        None => output.push_str(&format!("      {name}: unrestricted\n")),
    }
}

fn render_source(output: &mut String, name: &str, sql: &str, bindings: &[SqlParameter]) {
    output.push_str(&format!(
        "      {name}:\n        sql: {sql}\n        bindings:\n"
    ));
    if bindings.is_empty() {
        output.push_str("          none\n");
    } else {
        for (binding_index, binding) in bindings.iter().enumerate() {
            output.push_str(&format!(
                "          ?{} {}\n",
                binding_index + 1,
                binding.kind_name()
            ));
        }
    }
}

#[derive(Clone)]
pub(crate) enum PreparedFragmentArgument {
    Value(SqlValue),
    Runtime(RuntimeSlot),
}

#[derive(Clone)]
pub(crate) enum PreparedValueOperator {
    TrailExpand {
        output: RuntimeSlot,
        spec: ResolvedTrailExpansion,
    },
    ShortestExpand {
        output: RuntimeSlot,
        mode: ShortestMode,
        spec: ResolvedTrailExpansion,
    },
}

impl PreparedValueOperator {
    #[cfg(test)]
    pub(crate) fn output(&self) -> RuntimeSlot {
        match self {
            Self::TrailExpand { output, .. } | Self::ShortestExpand { output, .. } => *output,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PreparedSqlFragment {
    pub(crate) sql: String,
    pub(crate) columns: Vec<String>,
    pub(crate) output_shapes: Vec<OutputShape>,
    pub(crate) arguments: Vec<PreparedFragmentArgument>,
    pub(crate) inputs: Vec<PreparedValueOperator>,
}

#[derive(Clone)]
pub(crate) enum PreparedPhysicalOperator {
    Sqlite(PreparedSqlFragment),
    SemanticDistinct {
        input: Box<Self>,
    },
    Paginate {
        input: Box<Self>,
        offset: usize,
        limit: Option<usize>,
    },
    Union {
        left: Box<Self>,
        right: Box<Self>,
        kind: UnionKind,
    },
    BatchHydrate {
        input: Box<Self>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trail_input(slot: usize) -> ValueOperator {
        ValueOperator::TrailExpand {
            output: RuntimeSlot(slot),
            spec: TrailExpansionSpec {
                traversal: TraversalId(slot),
                direction: TrailDirection::Right,
                minimum: 1,
                maximum: Some(2),
                edge_sql: "SELECT id, from_node, to_node FROM edges WHERE edge_type = ?1".into(),
                edge_bindings: vec![SqlParameter::Text(format!("edge-{slot}"))],
                start_domain: None,
                end_domain: None,
            },
        }
    }

    fn fragment(arguments: Vec<FragmentArgument>, inputs: Vec<ValueOperator>) -> SqlFragment {
        SqlFragment {
            sql: "SELECT 1".into(),
            columns: vec!["value".into()],
            output_shapes: vec![OutputShape::Json],
            arguments,
            inputs,
        }
    }

    fn permutations(values: &[usize]) -> Vec<Vec<usize>> {
        fn append(
            prefix: &mut Vec<usize>,
            remaining: &mut Vec<usize>,
            output: &mut Vec<Vec<usize>>,
        ) {
            if remaining.is_empty() {
                output.push(prefix.clone());
                return;
            }
            for index in 0..remaining.len() {
                let value = remaining.remove(index);
                prefix.push(value);
                append(prefix, remaining, output);
                prefix.pop();
                remaining.insert(index, value);
            }
        }

        let mut output = Vec::new();
        append(&mut Vec::new(), &mut values.to_vec(), &mut output);
        output
    }

    fn prepared_input(prepared: &PreparedValueOperator) -> (usize, usize, &SqlValue) {
        let PreparedValueOperator::TrailExpand { output, spec } = prepared else {
            panic!("expected a trail expansion")
        };
        (output.0, spec.traversal, &spec.edge_bindings[0])
    }

    #[test]
    fn valid_runtime_layouts_prepare_in_consumer_order() {
        for slot_count in 0..=4 {
            let slots = (0..slot_count).collect::<Vec<_>>();
            for producer_order in permutations(&slots) {
                for consumer_order in permutations(&slots) {
                    for static_mask in 0..(1 << (slot_count + 1)) {
                        let inputs = producer_order.iter().copied().map(trail_input).collect();
                        let mut arguments = Vec::new();
                        for gap in 0..=slot_count {
                            if static_mask & (1 << gap) != 0 {
                                arguments.push(FragmentArgument::Static(SqlParameter::Text(
                                    format!("static-{gap}"),
                                )));
                            }
                            if let Some(slot) = consumer_order.get(gap) {
                                arguments.push(FragmentArgument::Runtime(RuntimeSlot(*slot)));
                            }
                        }

                        let prepared = fragment(arguments, inputs)
                            .prepare(&Parameters::new())
                            .unwrap();

                        let actual_input_order = prepared
                            .inputs
                            .iter()
                            .map(|input| prepared_input(input).0)
                            .collect::<Vec<_>>();
                        assert_eq!(actual_input_order, consumer_order);
                        for input in &prepared.inputs {
                            let (output, traversal, edge_binding) = prepared_input(input);
                            assert_eq!(traversal, output);
                            assert_eq!(edge_binding, &SqlValue::Text(format!("edge-{output}")));
                        }

                        let mut expected_argument_index = 0;
                        for gap in 0..=slot_count {
                            if static_mask & (1 << gap) != 0 {
                                let PreparedFragmentArgument::Value(SqlValue::Text(value)) =
                                    &prepared.arguments[expected_argument_index]
                                else {
                                    panic!("expected static argument in gap {gap}")
                                };
                                assert_eq!(value, &format!("static-{gap}"));
                                expected_argument_index += 1;
                            }
                            if let Some(slot) = consumer_order.get(gap) {
                                let PreparedFragmentArgument::Runtime(actual_slot) =
                                    prepared.arguments[expected_argument_index]
                                else {
                                    panic!("expected runtime argument after gap {gap}")
                                };
                                assert_eq!(actual_slot, RuntimeSlot(*slot));
                                expected_argument_index += 1;
                            }
                        }
                        assert_eq!(prepared.arguments.len(), expected_argument_index);
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_runtime_slot_layouts_are_rejected() {
        let cases = [
            (
                "duplicate producer",
                vec![FragmentArgument::Runtime(RuntimeSlot(0))],
                vec![trail_input(0), trail_input(0)],
                "physical plan produces runtime slot %r0 more than once",
            ),
            (
                "missing producer",
                vec![FragmentArgument::Runtime(RuntimeSlot(0))],
                vec![],
                "physical SQL fragment reads missing runtime slot %r0",
            ),
            (
                "unreferenced producer",
                vec![],
                vec![trail_input(0)],
                "physical plan produces unreferenced runtime slot %r0",
            ),
            (
                "reused slot",
                vec![
                    FragmentArgument::Runtime(RuntimeSlot(0)),
                    FragmentArgument::Runtime(RuntimeSlot(0)),
                ],
                vec![trail_input(0)],
                "physical SQL fragment consumes runtime slot %r0 more than once",
            ),
        ];

        for (name, arguments, inputs, expected) in cases {
            let error = match fragment(arguments, inputs).prepare(&Parameters::new()) {
                Ok(_) => panic!("{name}: expected an invalid runtime-slot layout"),
                Err(error) => error,
            };
            let Error::Data(message) = error else {
                panic!("{name}: expected Error::Data, found {error}")
            };
            assert_eq!(message, expected, "{name}");
        }
    }
}
