use serde_json::{Map, Number, Value};

use crate::{Error, Parameters, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SlotId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ScalarSlotId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TraversalId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PathId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathFunctionKind {
    Length,
    Nodes,
    Relationships,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathCollectionKind {
    Nodes,
    Relationships,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntityKind {
    Node,
    Relationship,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ValueExpr {
    Literal(Value),
    Parameter(String),
    List(Vec<Self>),
    Map(Vec<(String, Self)>),
    Negated(Box<Self>),
}

impl ValueExpr {
    pub(crate) fn resolve(&self, parameters: &Parameters) -> Result<Value> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Parameter(name) => {
                let value = parameters
                    .get(name)
                    .ok_or_else(|| Error::MissingParameter(name.clone()))?;
                validate_storable_value(value)?;
                Ok(value.clone())
            }
            Self::List(items) => items
                .iter()
                .map(|item| item.resolve(parameters))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array),
            Self::Map(entries) => {
                let mut values = Map::new();
                for (key, value) in entries {
                    if values
                        .insert(key.clone(), value.resolve(parameters)?)
                        .is_some()
                    {
                        return Err(Error::Semantic(format!(
                            "property map contains duplicate key `{key}`"
                        )));
                    }
                }
                Ok(Value::Object(values))
            }
            Self::Negated(value) => negate(value.resolve(parameters)?),
        }
    }

    pub(crate) fn resolve_properties(
        &self,
        parameters: &Parameters,
        description: &str,
    ) -> Result<Value> {
        if let Self::Parameter(name) = self {
            let value = parameters
                .get(name)
                .ok_or_else(|| Error::MissingParameter(name.clone()))?;
            require_object(value, description)?;
            validate_storable_properties(value)?;
            return Ok(value.clone());
        }

        let value = self.resolve(parameters)?;
        require_object(&value, description)?;
        validate_storable_properties(&value)?;
        Ok(value)
    }
}

fn negate(value: Value) -> Result<Value> {
    let Value::Number(number) = value else {
        return Err(Error::Semantic(
            "unary minus requires an integer or floating-point value".to_owned(),
        ));
    };

    if let Some(integer) = number.as_i64() {
        return integer
            .checked_neg()
            .map(Value::from)
            .ok_or_else(|| Error::Semantic("integer literal is outside the i64 range".to_owned()));
    }
    if let Some(unsigned) = number.as_u64() {
        let integer = i64::try_from(unsigned)
            .map_err(|_| Error::Semantic("integer literal is outside the i64 range".to_owned()))?;
        return Ok(Value::from(-integer));
    }

    let float = -number
        .as_f64()
        .ok_or_else(|| Error::Semantic("invalid floating-point value".to_owned()))?;
    Number::from_f64(float)
        .map(Value::Number)
        .ok_or_else(|| Error::Semantic("non-finite floats are not supported".to_owned()))
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Direction {
    Right,
    Left,
    Undirected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinaryOp {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    In,
    StartsWith,
    EndsWith,
    Contains,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnaryOp {
    Plus,
    Minus,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggregateKind {
    Count,
    Sum,
    Average,
    Minimum,
    Maximum,
    Collect,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Value(ValueExpr),
    Entity(SlotId),
    Scalar(ScalarSlotId),
    Path(PathId),
    PathFunction {
        kind: PathFunctionKind,
        path: PathId,
    },
    Property(SlotId, String),
    ElementId(SlotId),
    Labels(SlotId),
    RelationshipType(SlotId),
    Binary {
        left: Box<Self>,
        operator: BinaryOp,
        right: Box<Self>,
    },
    Unary {
        operator: UnaryOp,
        operand: Box<Self>,
    },
    IsNull {
        expression: Box<Self>,
        negated: bool,
    },
    Aggregate {
        kind: AggregateKind,
        argument: Option<Box<Self>>,
        distinct: bool,
    },
}

impl Expr {
    pub(crate) fn contains_aggregate(&self) -> bool {
        match self {
            Self::Aggregate { .. } => true,
            Self::Binary { left, right, .. } => {
                left.contains_aggregate() || right.contains_aggregate()
            }
            Self::Unary { operand, .. } => operand.contains_aggregate(),
            Self::IsNull { expression, .. } => expression.contains_aggregate(),
            _ => false,
        }
    }

    pub(crate) fn is_row_dependent(&self) -> bool {
        match self {
            Self::Value(_) => false,
            Self::Aggregate { .. } => false,
            Self::Binary { left, right, .. } => left.is_row_dependent() || right.is_row_dependent(),
            Self::Unary { operand, .. } => operand.is_row_dependent(),
            Self::IsNull { expression, .. } => expression.is_row_dependent(),
            Self::Entity(_)
            | Self::Scalar(_)
            | Self::Path(_)
            | Self::PathFunction { .. }
            | Self::Property(_, _)
            | Self::ElementId(_)
            | Self::Labels(_)
            | Self::RelationshipType(_) => true,
        }
    }

    pub(crate) fn uses_only_grouped_values(&self, grouping: &[Self]) -> bool {
        if grouping.contains(self) {
            return true;
        }
        match self {
            Self::Value(_) | Self::Aggregate { .. } => true,
            Self::Binary { left, right, .. } => {
                left.uses_only_grouped_values(grouping) && right.uses_only_grouped_values(grouping)
            }
            Self::Unary { operand, .. } => operand.uses_only_grouped_values(grouping),
            Self::IsNull { expression, .. } => expression.uses_only_grouped_values(grouping),
            Self::Property(slot, _)
            | Self::ElementId(slot)
            | Self::Labels(slot)
            | Self::RelationshipType(slot) => grouping
                .iter()
                .any(|expression| matches!(expression, Self::Entity(grouped) if grouped == slot)),
            Self::Entity(_) | Self::Scalar(_) | Self::Path(_) | Self::PathFunction { .. } => false,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum MatchOperator {
    ScanNode {
        slot: SlotId,
    },
    ScanRelationship {
        slot: SlotId,
    },
    HasLabel {
        slot: SlotId,
        label: String,
    },
    HasRelationshipType {
        slot: SlotId,
        relationship_type: String,
    },
    PropertyEquals {
        slot: SlotId,
        key: String,
        value: ValueExpr,
    },
    ExpandRelationship {
        relationship: SlotId,
        left: SlotId,
        right: SlotId,
        direction: Direction,
    },
    ExpandVariableLength {
        traversal: TraversalId,
        left: SlotId,
        right: SlotId,
        direction: Direction,
        minimum: usize,
        maximum: usize,
        relationship_type: Option<String>,
        properties: Vec<(String, ValueExpr)>,
    },
    ExpandHybridVariableLength {
        traversal: TraversalId,
        left: SlotId,
        right: SlotId,
        direction: Direction,
        minimum: usize,
        maximum: Option<usize>,
        relationship_type: Option<String>,
        properties: Vec<(String, ValueExpr)>,
    },
    ExpandShortestPath {
        traversal: TraversalId,
        left: SlotId,
        right: SlotId,
        direction: Direction,
        minimum: usize,
        maximum: Option<usize>,
        all: bool,
        relationship_type: Option<String>,
        properties: Vec<(String, ValueExpr)>,
    },
    DistinctRelationships(Vec<RelationshipUse>),
    BindPath {
        path: PathId,
        start: SlotId,
        segments: Vec<PathSegment>,
    },
    /// The traversal's ordered relationship sequence must equal the
    /// relationship list already bound to `path`, element by element. The
    /// traversal keeps its own direction, bounds, type, and property filters.
    MatchRelationshipList {
        traversal: TraversalId,
        path: PathId,
    },
    Filter(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelationshipUse {
    Single(SlotId),
    Traversal(TraversalId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PathSegment {
    pub(crate) left: SlotId,
    pub(crate) relationship: RelationshipUse,
    pub(crate) right: SlotId,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MatchPlan {
    pub(crate) operators: Vec<MatchOperator>,
}

impl MatchPlan {
    pub(crate) fn has_variable_length(&self) -> bool {
        self.operators.iter().any(|operator| {
            matches!(
                operator,
                MatchOperator::ExpandVariableLength { .. }
                    | MatchOperator::ExpandHybridVariableLength { .. }
                    | MatchOperator::ExpandShortestPath { .. }
            )
        })
    }

    pub(crate) fn has_path_binding(&self) -> bool {
        self.operators
            .iter()
            .any(|operator| matches!(operator, MatchOperator::BindPath { .. }))
    }
}

#[derive(Clone, Debug)]
pub(crate) enum WithBinding {
    Entity {
        name: String,
        slot: SlotId,
        kind: EntityKind,
    },
    Path {
        name: String,
        path: PathId,
    },
    PathCollection {
        name: String,
        path: PathId,
        kind: PathCollectionKind,
    },
    Scalar {
        name: String,
        slot: ScalarSlotId,
        expression: Expr,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct WithStage {
    pub(crate) bindings: Vec<WithBinding>,
    pub(crate) filter: Option<Expr>,
    pub(crate) order_by: Vec<SortItem>,
    pub(crate) offset: Option<ValueExpr>,
    pub(crate) limit: Option<ValueExpr>,
}

#[derive(Clone, Debug)]
pub(crate) struct UnwindStage {
    pub(crate) name: String,
    pub(crate) slot: ScalarSlotId,
    pub(crate) expression: Expr,
}

#[derive(Clone, Debug)]
pub(crate) enum ReadStage {
    Match(MatchPlan),
    OptionalMatch(MatchPlan),
    With(WithStage),
    Unwind(UnwindStage),
}

#[derive(Clone, Debug)]
pub(crate) struct ReadPipeline {
    pub(crate) stages: Vec<ReadStage>,
}

#[derive(Clone, Debug)]
pub(crate) enum Input {
    Unit,
    Match(MatchPlan),
    Pipeline(ReadPipeline),
}

#[derive(Clone, Debug)]
pub(crate) enum Mutation {
    CreateNode {
        slot: SlotId,
        label: Option<String>,
        properties: ValueExpr,
    },
    CreateRelationship {
        slot: SlotId,
        relationship_type: String,
        from: SlotId,
        to: SlotId,
        properties: ValueExpr,
    },
    SetProperty {
        slot: SlotId,
        key: String,
        value: ValueExpr,
    },
    ReplaceProperties {
        slot: SlotId,
        properties: ValueExpr,
    },
    MergeProperties {
        slot: SlotId,
        properties: ValueExpr,
    },
    RemoveProperty {
        slot: SlotId,
        key: String,
    },
    Delete {
        targets: Vec<(SlotId, EntityKind)>,
        detach: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Debug)]
pub(crate) struct SortItem {
    pub(crate) expression: Expr,
    pub(crate) direction: SortDirection,
}

#[derive(Clone, Debug)]
pub(crate) struct Projection {
    pub(crate) columns: Vec<String>,
    pub(crate) expressions: Vec<Expr>,
    pub(crate) distinct: bool,
    pub(crate) order_by: Vec<SortItem>,
    pub(crate) offset: Option<ValueExpr>,
    pub(crate) limit: Option<ValueExpr>,
}

#[derive(Clone, Debug)]
pub(crate) struct Query {
    pub(crate) slots: Vec<EntityKind>,
    pub(crate) scalar_slots: usize,
    pub(crate) path_slots: usize,
    pub(crate) input: Input,
    pub(crate) mutations: Vec<Mutation>,
    pub(crate) projection: Option<Projection>,
}

#[derive(Clone, Debug)]
pub(crate) enum ResolvedExpr {
    Value(Value),
    Entity(SlotId),
    Property(SlotId, String),
    ElementId(SlotId),
    Labels(SlotId),
    RelationshipType(SlotId),
}

impl Expr {
    pub(crate) fn resolve_output(&self, parameters: &Parameters) -> Result<ResolvedExpr> {
        match self {
            Self::Value(value) => value.resolve(parameters).map(ResolvedExpr::Value),
            Self::Entity(slot) => Ok(ResolvedExpr::Entity(*slot)),
            Self::Scalar(_) => Err(Error::Data(
                "a write projection contained an intermediate scalar slot".into(),
            )),
            Self::Path(_) => Err(Error::Data(
                "a write projection contained a path slot".into(),
            )),
            Self::PathFunction { .. } => Err(Error::Data(
                "a write projection contained a path function".into(),
            )),
            Self::Property(slot, key) => Ok(ResolvedExpr::Property(*slot, key.clone())),
            Self::ElementId(slot) => Ok(ResolvedExpr::ElementId(*slot)),
            Self::Labels(slot) => Ok(ResolvedExpr::Labels(*slot)),
            Self::RelationshipType(slot) => Ok(ResolvedExpr::RelationshipType(*slot)),
            _ => Err(Error::Data(
                "a write projection contained a non-output expression".into(),
            )),
        }
    }
}

pub(crate) fn require_object(value: &Value, description: &str) -> Result<()> {
    if value.is_object() {
        Ok(())
    } else {
        Err(Error::Semantic(format!("{description} must be a map")))
    }
}

pub(crate) const MAX_USER_VALUE_NESTING: usize = 100;

pub(crate) fn validate_storable_value(value: &Value) -> Result<()> {
    validate_json_value(value, MAX_USER_VALUE_NESTING)
}

fn validate_storable_properties(value: &Value) -> Result<()> {
    let Value::Object(properties) = value else {
        return Err(Error::Semantic("properties must be a map".into()));
    };

    // The stored `props` object is an internal envelope, not a property value.
    // Give every top-level property the same nesting allowance as a standalone
    // value regardless of whether the map came from a parameter or literal.
    for value in properties.values() {
        validate_storable_value(value)?;
    }
    Ok(())
}

fn validate_json_value(value: &Value, maximum_nesting: usize) -> Result<()> {
    let mut pending = vec![(value, 0usize)];
    while let Some((value, nesting)) = pending.pop() {
        match value {
            Value::Number(number) if number.as_u64().is_some() && number.as_i64().is_none() => {
                return Err(Error::Semantic(
                    "Cypher integers must fit SQLite's signed 64-bit range".into(),
                ));
            }
            Value::Array(values) => {
                let nesting = nesting + 1;
                if nesting > maximum_nesting {
                    return Err(Error::Semantic(format!(
                        "JSON value nesting exceeds the supported maximum of \
                         {MAX_USER_VALUE_NESTING} list/map containers"
                    )));
                }
                pending.extend(values.iter().map(|value| (value, nesting)));
            }
            Value::Object(values) => {
                let nesting = nesting + 1;
                if nesting > maximum_nesting {
                    return Err(Error::Semantic(format!(
                        "JSON value nesting exceeds the supported maximum of \
                         {MAX_USER_VALUE_NESTING} list/map containers"
                    )));
                }
                pending.extend(values.values().map(|value| (value, nesting)));
            }
            _ => {}
        }
    }
    Ok(())
}
