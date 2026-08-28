use std::collections::HashMap;

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

#[derive(Clone, Debug)]
pub(crate) struct TrailExpansionSpec {
    pub(crate) traversal: TraversalId,
    pub(crate) direction: TrailDirection,
    pub(crate) minimum: usize,
    pub(crate) maximum: Option<usize>,
    pub(crate) edge_sql: String,
    pub(crate) edge_bindings: Vec<SqlParameter>,
}

impl TrailExpansionSpec {
    fn resolve(&self, parameters: &Parameters) -> Result<ResolvedTrailExpansion> {
        Ok(ResolvedTrailExpansion {
            traversal: self.traversal.0,
            direction: self.direction,
            minimum: self.minimum,
            maximum: self.maximum,
            edge_sql: self.edge_sql.clone(),
            edge_bindings: self
                .edge_bindings
                .iter()
                .map(|binding| binding.resolve(parameters))
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedTrailExpansion {
    pub(crate) traversal: usize,
    pub(crate) direction: TrailDirection,
    pub(crate) minimum: usize,
    pub(crate) maximum: Option<usize>,
    pub(crate) edge_sql: String,
    pub(crate) edge_bindings: Vec<SqlValue>,
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
        let mut arguments = Vec::with_capacity(self.arguments.len());
        for argument in self.arguments {
            match argument {
                FragmentArgument::Static(parameter) => arguments.push(
                    PreparedFragmentArgument::Value(parameter.resolve(parameters)?),
                ),
                FragmentArgument::Runtime(slot) => {
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
    output.push_str("      edge-source:\n");
    output.push_str("        sql: ");
    output.push_str(&spec.edge_sql);
    output.push_str("\n        bindings:\n");
    if spec.edge_bindings.is_empty() {
        output.push_str("          none\n");
    } else {
        for (binding_index, binding) in spec.edge_bindings.iter().enumerate() {
            output.push_str(&format!(
                "          ?{} {}\n",
                binding_index + 1,
                binding.kind_name()
            ));
        }
    }
    *index += 1;
}

pub(crate) enum PreparedFragmentArgument {
    Value(SqlValue),
    Runtime(RuntimeSlot),
}

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

pub(crate) struct PreparedSqlFragment {
    pub(crate) sql: String,
    pub(crate) columns: Vec<String>,
    pub(crate) output_shapes: Vec<OutputShape>,
    pub(crate) arguments: Vec<PreparedFragmentArgument>,
    pub(crate) inputs: Vec<PreparedValueOperator>,
}

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
