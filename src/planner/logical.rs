use serde_json::{Map, Number, Value};

use crate::{Error, Parameters, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SlotId(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ScalarSlotId(pub(crate) usize);

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
            Self::Parameter(name) => parameters
                .get(name)
                .cloned()
                .ok_or_else(|| Error::MissingParameter(name.clone())),
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

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Value(ValueExpr),
    Entity(SlotId),
    Scalar(ScalarSlotId),
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
    DistinctRelationships(Vec<SlotId>),
    Filter(Expr),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MatchPlan {
    pub(crate) operators: Vec<MatchOperator>,
}

#[derive(Clone, Debug)]
pub(crate) enum WithBinding {
    Entity {
        name: String,
        slot: SlotId,
        kind: EntityKind,
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
pub(crate) enum ReadStage {
    Match(MatchPlan),
    With(WithStage),
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

pub(crate) fn validate_storable_value(value: &Value) -> Result<()> {
    match value {
        Value::Number(number) if number.as_u64().is_some() && number.as_i64().is_none() => Err(
            Error::Semantic("property integers must fit SQLite's signed 64-bit range".into()),
        ),
        Value::Array(values) => values.iter().try_for_each(validate_storable_value),
        Value::Object(values) => values.values().try_for_each(validate_storable_value),
        _ => Ok(()),
    }
}
