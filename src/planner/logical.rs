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
    Xor,
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

/// A list-bound variable introduced by a quantified predicate such as
/// `any(x IN list WHERE ...)`. Ids are unique within one lowered query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ListVariableId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListQuantifier {
    All,
    Any,
    None,
    Single,
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
    /// Whether the node has the label; null for a null node.
    HasLabel(SlotId, String),
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
    /// `all`/`any`/`none`/`single(variable IN list WHERE predicate)`. The
    /// predicate refers to the current element through `ListElement`.
    Quantified {
        kind: ListQuantifier,
        variable: ListVariableId,
        list: Box<Self>,
        predicate: Box<Self>,
    },
    ListElement(ListVariableId),
    /// `.key` on a map-valued scalar such as a projected map, a parameter, a
    /// nested property, or a quantified list element.
    MapProperty {
        base: Box<Self>,
        key: String,
    },
    /// `size(list)` or `size(string)`.
    Size(Box<Self>),
    /// A list literal with at least one computed element; all-literal lists
    /// stay `Value`.
    List(Vec<Self>),
    /// A map literal with at least one computed entry; all-literal maps stay
    /// `Value`.
    Map(Vec<(String, Self)>),
    /// `base[index]`: an integer index into a list or a string key of a map.
    Index {
        base: Box<Self>,
        index: Box<Self>,
    },
    /// `entity[key]`: a property of a graph entity named at runtime.
    DynamicProperty(SlotId, Box<Self>),
    /// `list[lower..upper]` with either bound optional.
    Slice {
        base: Box<Self>,
        lower: Option<Box<Self>>,
        upper: Option<Box<Self>>,
    },
    /// `range(start, end[, step])`.
    Range {
        start: Box<Self>,
        end: Box<Self>,
        step: Option<Box<Self>>,
    },
    /// `[variable IN list WHERE predicate | projection]`. The predicate and
    /// projection refer to the current element through `ListElement`; a
    /// missing projection yields the element itself.
    ListComprehension {
        variable: ListVariableId,
        list: Box<Self>,
        predicate: Option<Box<Self>>,
        projection: Option<Box<Self>>,
    },
    /// `CASE operand WHEN a, b THEN x ... ELSE y END`, where each `WHEN`
    /// value is compared with the operand for equality, or the generic
    /// `CASE WHEN predicate THEN x ... END` without an operand. The first
    /// alternative whose condition is true selects the result; a missing
    /// `ELSE` yields null.
    Case {
        operand: Option<Box<Self>>,
        alternatives: Vec<CaseAlternative>,
        otherwise: Option<Box<Self>>,
    },
    /// A built-in scalar function over scalar arguments.
    Function {
        function: ScalarFunction,
        arguments: Vec<Self>,
    },
    /// `keys(entity)`: the entity's property names as a list.
    EntityKeys(SlotId),
    /// `properties(entity)`: the entity's properties as a map.
    EntityProperties(SlotId),
}

/// Built-in scalar functions with fixed SQL translations. Arguments of the
/// wrong kind yield null, matching the arithmetic operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalarFunction {
    Rand,
    Pi,
    E,
    Abs,
    Sign,
    Ceil,
    Floor,
    Round,
    Sqrt,
    Exp,
    Log,
    Log10,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    ToInteger,
    ToFloat,
    ToString,
    ToBoolean,
    ToLower,
    ToUpper,
    Trim,
    LTrim,
    RTrim,
    Reverse,
    Substring,
    Left,
    Right,
    Replace,
    Split,
    Coalesce,
    Keys,
    Properties,
}

impl ScalarFunction {
    /// Looks a function up by its lower-cased Cypher name.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "rand" => Self::Rand,
            "pi" => Self::Pi,
            "e" => Self::E,
            "abs" => Self::Abs,
            "sign" => Self::Sign,
            "ceil" => Self::Ceil,
            "floor" => Self::Floor,
            "round" => Self::Round,
            "sqrt" => Self::Sqrt,
            "exp" => Self::Exp,
            "log" => Self::Log,
            "log10" => Self::Log10,
            "sin" => Self::Sin,
            "cos" => Self::Cos,
            "tan" => Self::Tan,
            "asin" => Self::Asin,
            "acos" => Self::Acos,
            "atan" => Self::Atan,
            "tointeger" => Self::ToInteger,
            "tofloat" => Self::ToFloat,
            "tostring" => Self::ToString,
            "toboolean" => Self::ToBoolean,
            "tolower" => Self::ToLower,
            "toupper" => Self::ToUpper,
            "trim" => Self::Trim,
            "ltrim" => Self::LTrim,
            "rtrim" => Self::RTrim,
            "reverse" => Self::Reverse,
            "substring" => Self::Substring,
            "left" => Self::Left,
            "right" => Self::Right,
            "replace" => Self::Replace,
            "split" => Self::Split,
            "coalesce" => Self::Coalesce,
            "keys" => Self::Keys,
            "properties" => Self::Properties,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Rand => "rand",
            Self::Pi => "pi",
            Self::E => "e",
            Self::Abs => "abs",
            Self::Sign => "sign",
            Self::Ceil => "ceil",
            Self::Floor => "floor",
            Self::Round => "round",
            Self::Sqrt => "sqrt",
            Self::Exp => "exp",
            Self::Log => "log",
            Self::Log10 => "log10",
            Self::Sin => "sin",
            Self::Cos => "cos",
            Self::Tan => "tan",
            Self::Asin => "asin",
            Self::Acos => "acos",
            Self::Atan => "atan",
            Self::ToInteger => "toInteger",
            Self::ToFloat => "toFloat",
            Self::ToString => "toString",
            Self::ToBoolean => "toBoolean",
            Self::ToLower => "toLower",
            Self::ToUpper => "toUpper",
            Self::Trim => "trim",
            Self::LTrim => "ltrim",
            Self::RTrim => "rtrim",
            Self::Reverse => "reverse",
            Self::Substring => "substring",
            Self::Left => "left",
            Self::Right => "right",
            Self::Replace => "replace",
            Self::Split => "split",
            Self::Coalesce => "coalesce",
            Self::Keys => "keys",
            Self::Properties => "properties",
        }
    }

    /// The accepted argument counts, inclusive.
    pub(crate) fn arity(self) -> (usize, usize) {
        match self {
            Self::Rand | Self::Pi | Self::E => (0, 0),
            Self::Substring => (2, 3),
            Self::Left | Self::Right | Self::Split => (2, 2),
            Self::Replace => (3, 3),
            Self::Coalesce => (1, usize::MAX),
            _ => (1, 1),
        }
    }
}

/// One `WHEN ... THEN ...` arm of a `CASE` expression. Several comma
/// separated `WHEN` values share a result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CaseAlternative {
    pub(crate) conditions: Vec<Expr>,
    pub(crate) result: Expr,
}

impl Expr {
    /// The sub-expressions of a `CASE` expression, in evaluation order.
    fn case_parts<'a>(
        operand: &'a Option<Box<Self>>,
        alternatives: &'a [CaseAlternative],
        otherwise: &'a Option<Box<Self>>,
    ) -> impl Iterator<Item = &'a Self> {
        operand
            .iter()
            .map(Box::as_ref)
            .chain(alternatives.iter().flat_map(|alternative| {
                alternative
                    .conditions
                    .iter()
                    .chain(std::iter::once(&alternative.result))
            }))
            .chain(otherwise.iter().map(Box::as_ref))
    }

    /// The direct sub-expressions, in evaluation order.
    pub(crate) fn children(&self) -> Vec<&Self> {
        match self {
            Self::Value(_)
            | Self::Entity(_)
            | Self::Scalar(_)
            | Self::Path(_)
            | Self::PathFunction { .. }
            | Self::Property(_, _)
            | Self::ElementId(_)
            | Self::Labels(_)
            | Self::HasLabel(_, _)
            | Self::RelationshipType(_)
            | Self::ListElement(_)
            | Self::EntityKeys(_)
            | Self::EntityProperties(_) => Vec::new(),
            Self::Binary { left, right, .. } => vec![left, right],
            Self::Unary { operand, .. } => vec![operand],
            Self::IsNull { expression, .. } => vec![expression],
            Self::Aggregate { argument, .. } => argument.iter().map(Box::as_ref).collect(),
            Self::Quantified {
                list, predicate, ..
            } => vec![list, predicate],
            Self::MapProperty { base, .. } => vec![base],
            Self::Size(operand) => vec![operand],
            Self::List(items) => items.iter().collect(),
            Self::Map(entries) => entries.iter().map(|(_, value)| value).collect(),
            Self::Index { base, index } => vec![base, index],
            Self::DynamicProperty(_, key) => vec![key],
            Self::Slice { base, lower, upper } => std::iter::once(base.as_ref())
                .chain(lower.as_deref())
                .chain(upper.as_deref())
                .collect(),
            Self::Range { start, end, step } => [start, end]
                .into_iter()
                .map(Box::as_ref)
                .chain(step.as_deref())
                .collect(),
            Self::ListComprehension {
                list,
                predicate,
                projection,
                ..
            } => std::iter::once(list.as_ref())
                .chain(predicate.as_deref())
                .chain(projection.as_deref())
                .collect(),
            Self::Case {
                operand,
                alternatives,
                otherwise,
            } => Self::case_parts(operand, alternatives, otherwise).collect(),
            Self::Function { arguments, .. } => arguments.iter().collect(),
        }
    }

    /// Whether `rand()` occurs anywhere in the expression.
    pub(crate) fn contains_rand(&self) -> bool {
        matches!(
            self,
            Self::Function {
                function: ScalarFunction::Rand,
                ..
            }
        ) || self.children().into_iter().any(Self::contains_rand)
    }

    pub(crate) fn contains_aggregate(&self) -> bool {
        match self {
            Self::Function { arguments, .. } => arguments.iter().any(Self::contains_aggregate),
            Self::Case {
                operand,
                alternatives,
                otherwise,
            } => Self::case_parts(operand, alternatives, otherwise).any(Self::contains_aggregate),
            Self::Aggregate { .. } => true,
            Self::Binary { left, right, .. } => {
                left.contains_aggregate() || right.contains_aggregate()
            }
            Self::Unary { operand, .. } => operand.contains_aggregate(),
            Self::IsNull { expression, .. } => expression.contains_aggregate(),
            Self::Quantified {
                list, predicate, ..
            } => list.contains_aggregate() || predicate.contains_aggregate(),
            Self::MapProperty { base, .. } => base.contains_aggregate(),
            Self::Size(operand) => operand.contains_aggregate(),
            Self::List(items) => items.iter().any(Self::contains_aggregate),
            Self::Map(entries) => entries.iter().any(|(_, value)| value.contains_aggregate()),
            Self::Index { base, index } => base.contains_aggregate() || index.contains_aggregate(),
            Self::DynamicProperty(_, key) => key.contains_aggregate(),
            Self::Slice { base, lower, upper } => {
                base.contains_aggregate()
                    || lower.as_deref().is_some_and(Self::contains_aggregate)
                    || upper.as_deref().is_some_and(Self::contains_aggregate)
            }
            Self::Range { start, end, step } => {
                start.contains_aggregate()
                    || end.contains_aggregate()
                    || step.as_deref().is_some_and(Self::contains_aggregate)
            }
            Self::ListComprehension {
                list,
                predicate,
                projection,
                ..
            } => {
                list.contains_aggregate()
                    || predicate.as_deref().is_some_and(Self::contains_aggregate)
                    || projection.as_deref().is_some_and(Self::contains_aggregate)
            }
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
            Self::Quantified {
                list, predicate, ..
            } => list.is_row_dependent() || predicate.is_row_dependent(),
            // An element varies per list member, not per row; the enclosing
            // quantifier reports its own row dependence through its operands.
            Self::ListElement(_) => false,
            Self::MapProperty { base, .. } => base.is_row_dependent(),
            Self::Size(operand) => operand.is_row_dependent(),
            Self::List(items) => items.iter().any(Self::is_row_dependent),
            Self::Map(entries) => entries.iter().any(|(_, value)| value.is_row_dependent()),
            Self::Index { base, index } => base.is_row_dependent() || index.is_row_dependent(),
            Self::DynamicProperty(_, _) => true,
            Self::Slice { base, lower, upper } => {
                base.is_row_dependent()
                    || lower.as_deref().is_some_and(Self::is_row_dependent)
                    || upper.as_deref().is_some_and(Self::is_row_dependent)
            }
            Self::Range { start, end, step } => {
                start.is_row_dependent()
                    || end.is_row_dependent()
                    || step.as_deref().is_some_and(Self::is_row_dependent)
            }
            Self::ListComprehension {
                list,
                predicate,
                projection,
                ..
            } => {
                list.is_row_dependent()
                    || predicate.as_deref().is_some_and(Self::is_row_dependent)
                    || projection.as_deref().is_some_and(Self::is_row_dependent)
            }
            Self::Case {
                operand,
                alternatives,
                otherwise,
            } => Self::case_parts(operand, alternatives, otherwise).any(Self::is_row_dependent),
            // `rand()` is a fresh value per evaluation, not a row attribute,
            // so it never becomes an implicit grouping key on its own.
            Self::Function { arguments, .. } => arguments.iter().any(Self::is_row_dependent),
            Self::EntityKeys(_) | Self::EntityProperties(_) => true,
            Self::Entity(_)
            | Self::Scalar(_)
            | Self::Path(_)
            | Self::PathFunction { .. }
            | Self::Property(_, _)
            | Self::ElementId(_)
            | Self::Labels(_)
            | Self::HasLabel(_, _)
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
            Self::Quantified {
                list, predicate, ..
            } => {
                list.uses_only_grouped_values(grouping)
                    && predicate.uses_only_grouped_values(grouping)
            }
            Self::ListElement(_) => true,
            Self::MapProperty { base, .. } => base.uses_only_grouped_values(grouping),
            Self::Size(operand) => operand.uses_only_grouped_values(grouping),
            Self::List(items) => items
                .iter()
                .all(|item| item.uses_only_grouped_values(grouping)),
            Self::Map(entries) => entries
                .iter()
                .all(|(_, value)| value.uses_only_grouped_values(grouping)),
            Self::Index { base, index } => {
                base.uses_only_grouped_values(grouping) && index.uses_only_grouped_values(grouping)
            }
            Self::DynamicProperty(slot, key) => {
                key.uses_only_grouped_values(grouping)
                    && grouping.iter().any(
                        |expression| matches!(expression, Self::Entity(grouped) if grouped == slot),
                    )
            }
            Self::Slice { base, lower, upper } => {
                base.uses_only_grouped_values(grouping)
                    && lower
                        .as_deref()
                        .is_none_or(|lower| lower.uses_only_grouped_values(grouping))
                    && upper
                        .as_deref()
                        .is_none_or(|upper| upper.uses_only_grouped_values(grouping))
            }
            Self::Range { start, end, step } => {
                start.uses_only_grouped_values(grouping)
                    && end.uses_only_grouped_values(grouping)
                    && step
                        .as_deref()
                        .is_none_or(|step| step.uses_only_grouped_values(grouping))
            }
            Self::ListComprehension {
                list,
                predicate,
                projection,
                ..
            } => {
                list.uses_only_grouped_values(grouping)
                    && predicate
                        .as_deref()
                        .is_none_or(|predicate| predicate.uses_only_grouped_values(grouping))
                    && projection
                        .as_deref()
                        .is_none_or(|projection| projection.uses_only_grouped_values(grouping))
            }
            Self::Case {
                operand,
                alternatives,
                otherwise,
            } => Self::case_parts(operand, alternatives, otherwise)
                .all(|part| part.uses_only_grouped_values(grouping)),
            Self::Function { arguments, .. } => arguments
                .iter()
                .all(|argument| argument.uses_only_grouped_values(grouping)),
            Self::EntityKeys(slot) | Self::EntityProperties(slot) => grouping
                .iter()
                .any(|expression| matches!(expression, Self::Entity(grouped) if grouped == slot)),
            Self::Property(slot, _)
            | Self::ElementId(slot)
            | Self::Labels(slot)
            | Self::HasLabel(slot, _)
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
    /// Rows already materialized by a mutation pipeline, exposed to the SQL
    /// compiler through the `_crabnet_rows` temporary table with one encoded
    /// entity column per entity slot, one JSON column per scalar slot, and
    /// the row index as `ROW_INDEX_SLOT`. The post-mutation `RETURN` and
    /// computed stored values run over it directly; a `MERGE` matcher and a
    /// read segment continue from it through further read stages.
    Rows {
        slots: Vec<(SlotId, EntityKind)>,
        scalars: Vec<ScalarSlotId>,
        stages: Vec<ReadStage>,
    },
}

/// One read-then-write step of a clause sequence that continues after its
/// first mutations: `stages` read from the rows the earlier mutations left,
/// seeing only the variables live at that point (`slots` and `scalars`), and
/// `mutations` then apply to the rows the stages produce.
#[derive(Clone, Debug)]
pub(crate) struct Segment {
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    pub(crate) scalars: Vec<ScalarSlotId>,
    pub(crate) stages: Vec<ReadStage>,
    pub(crate) mutations: Vec<Mutation>,
}

/// The scalar slot through which a read over materialized rows sees each
/// row's index.
pub(crate) const ROW_INDEX_SLOT: ScalarSlotId = ScalarSlotId(usize::MAX);

/// A value written by `CREATE` or `SET`: a literal or parameter shape that
/// is resolved once per statement, or an expression evaluated per input row
/// over the materialized rows.
#[derive(Clone, Debug)]
pub(crate) enum StoredValue {
    Static(ValueExpr),
    Computed(Expr),
}

#[derive(Clone, Debug)]
pub(crate) enum Mutation {
    CreateNode {
        slot: SlotId,
        labels: Vec<String>,
        properties: StoredValue,
    },
    CreateRelationship {
        slot: SlotId,
        relationship_type: String,
        from: SlotId,
        to: SlotId,
        properties: StoredValue,
    },
    SetProperty {
        slot: SlotId,
        key: String,
        value: StoredValue,
    },
    ReplaceProperties {
        slot: SlotId,
        properties: StoredValue,
    },
    MergeProperties {
        slot: SlotId,
        properties: StoredValue,
    },
    RemoveProperty {
        slot: SlotId,
        key: String,
    },
    /// `SET n:A:B`: adds the labels the node does not have yet.
    SetLabels {
        slot: SlotId,
        labels: Vec<String>,
    },
    /// `REMOVE n:A:B`: removes the labels the node has.
    RemoveLabels {
        slot: SlotId,
        labels: Vec<String>,
    },
    Delete {
        targets: Vec<(SlotId, EntityKind)>,
        detach: bool,
    },
    /// `MERGE pattern [ON CREATE SET ...] [ON MATCH SET ...]`. `pattern`
    /// matches the whole pattern from each input row with already-bound slots
    /// fixed; `fresh` are the slots the pattern introduces, which `creates`
    /// builds when a row has no match.
    Merge {
        pattern: MatchPlan,
        fresh: Vec<(SlotId, EntityKind)>,
        creates: Vec<Mutation>,
        on_create: Vec<Mutation>,
        on_match: Vec<Mutation>,
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
    pub(crate) segments: Vec<Segment>,
    pub(crate) projection: Option<Projection>,
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

pub(crate) fn validate_storable_properties(value: &Value) -> Result<()> {
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
