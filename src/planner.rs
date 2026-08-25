use std::collections::{HashMap, HashSet};

use open_cypher::{
    Span,
    ast::{
        BinaryOperator, ClauseKind, CreateClause, Expr, ExprKind, FunctionInvocation, IntegerRadix,
        IsPredicate, LabelExpression, LabelExpressionKind, LiteralKind, MatchClause, NodePattern,
        ParameterName, PathFactor, PathFactorKind, Pattern, ProjectionClause, ProjectionItemKind,
        RelationshipDirection, RelationshipPattern, ReturnClause, SetQuantifier, SortDirection,
        StatementKind, UnaryOperator,
    },
    parse,
};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Number, Value};

use crate::{Error, Parameters, Result};

#[derive(Clone, Debug)]
enum InputValue {
    Literal(Value),
    Parameter(String),
    List(Vec<Self>),
    Map(Vec<(String, Self)>),
    Negated(Box<Self>),
}

impl InputValue {
    fn resolve(&self, parameters: &Parameters) -> Result<Value> {
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

#[derive(Clone, Debug)]
enum SqlBinding {
    Text(String),
    Scalar(InputValue),
    Json(InputValue),
    Page(InputValue),
}

impl SqlBinding {
    fn resolve(&self, parameters: &Parameters) -> Result<SqlValue> {
        match self {
            Self::Text(value) => Ok(SqlValue::Text(value.clone())),
            Self::Scalar(value) => json_to_sql(value.resolve(parameters)?),
            Self::Json(value) => Ok(SqlValue::Text(value.resolve(parameters)?.to_string())),
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

pub(crate) enum Plan {
    Read(ReadPlan),
    Create(CreatePlan),
}

pub(crate) struct ReadPlan {
    pub(crate) sql: String,
    pub(crate) columns: Vec<String>,
    bindings: Vec<SqlBinding>,
}

impl ReadPlan {
    pub(crate) fn resolve_bindings(&self, parameters: &Parameters) -> Result<Vec<SqlValue>> {
        self.bindings
            .iter()
            .map(|binding| binding.resolve(parameters))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntityRef {
    Node(usize),
    Relationship(usize),
}

pub(crate) struct CreateNode {
    pub(crate) label: Option<String>,
    properties: InputValue,
}

pub(crate) struct CreateRelationship {
    pub(crate) relationship_type: String,
    pub(crate) from: usize,
    pub(crate) to: usize,
    properties: InputValue,
}

enum CreateOutput {
    Property(EntityRef, String),
    Id(EntityRef),
    RelationshipType(usize),
    Labels(usize),
    Value(InputValue),
}

struct CreateProjection {
    columns: Vec<String>,
    outputs: Vec<CreateOutput>,
}

pub(crate) struct CreatePlan {
    pub(crate) nodes: Vec<CreateNode>,
    pub(crate) relationships: Vec<CreateRelationship>,
    projection: Option<CreateProjection>,
}

pub(crate) struct ResolvedNode {
    pub(crate) label: Option<String>,
    pub(crate) properties: Value,
}

pub(crate) struct ResolvedRelationship {
    pub(crate) relationship_type: String,
    pub(crate) from: usize,
    pub(crate) to: usize,
    pub(crate) properties: Value,
}

enum ResolvedCreateOutput {
    Property(EntityRef, String),
    Id(EntityRef),
    RelationshipType(usize),
    Labels(usize),
    Value(Value),
}

pub(crate) struct ResolvedCreatePlan {
    pub(crate) nodes: Vec<ResolvedNode>,
    pub(crate) relationships: Vec<ResolvedRelationship>,
    columns: Vec<String>,
    outputs: Vec<ResolvedCreateOutput>,
}

impl CreatePlan {
    pub(crate) fn resolve(self, parameters: &Parameters) -> Result<ResolvedCreatePlan> {
        let nodes = self
            .nodes
            .into_iter()
            .map(|node| {
                let properties = node.properties.resolve(parameters)?;
                require_object(&properties, "node properties")?;
                validate_storable_value(&properties)?;
                Ok(ResolvedNode {
                    label: node.label,
                    properties,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let relationships = self
            .relationships
            .into_iter()
            .map(|relationship| {
                let properties = relationship.properties.resolve(parameters)?;
                require_object(&properties, "relationship properties")?;
                validate_storable_value(&properties)?;
                Ok(ResolvedRelationship {
                    relationship_type: relationship.relationship_type,
                    from: relationship.from,
                    to: relationship.to,
                    properties,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let (columns, outputs) = match self.projection {
            Some(projection) => {
                let outputs = projection
                    .outputs
                    .into_iter()
                    .map(|output| match output {
                        CreateOutput::Property(entity, key) => {
                            Ok(ResolvedCreateOutput::Property(entity, key))
                        }
                        CreateOutput::Id(entity) => Ok(ResolvedCreateOutput::Id(entity)),
                        CreateOutput::RelationshipType(index) => {
                            Ok(ResolvedCreateOutput::RelationshipType(index))
                        }
                        CreateOutput::Labels(index) => Ok(ResolvedCreateOutput::Labels(index)),
                        CreateOutput::Value(value) => {
                            value.resolve(parameters).map(ResolvedCreateOutput::Value)
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                (projection.columns, outputs)
            }
            None => (Vec::new(), Vec::new()),
        };

        Ok(ResolvedCreatePlan {
            nodes,
            relationships,
            columns,
            outputs,
        })
    }
}

impl ResolvedCreatePlan {
    pub(crate) fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }

    pub(crate) fn result_row(
        &self,
        node_ids: &[String],
        relationship_ids: &[String],
    ) -> Result<Option<Vec<Value>>> {
        if self.outputs.is_empty() {
            return Ok(None);
        }

        self.outputs
            .iter()
            .map(|output| match output {
                ResolvedCreateOutput::Property(entity, key) => self
                    .properties(*entity)
                    .and_then(Value::as_object)
                    .and_then(|properties| properties.get(key))
                    .cloned()
                    .map_or(Ok(Value::Null), Ok),
                ResolvedCreateOutput::Id(EntityRef::Node(index)) => node_ids
                    .get(*index)
                    .cloned()
                    .map(Value::String)
                    .ok_or_else(|| Error::Data("missing generated node id".into())),
                ResolvedCreateOutput::Id(EntityRef::Relationship(index)) => relationship_ids
                    .get(*index)
                    .cloned()
                    .map(Value::String)
                    .ok_or_else(|| Error::Data("missing generated relationship id".into())),
                ResolvedCreateOutput::RelationshipType(index) => self
                    .relationships
                    .get(*index)
                    .map(|relationship| Value::String(relationship.relationship_type.clone()))
                    .ok_or_else(|| Error::Data("missing created relationship".into())),
                ResolvedCreateOutput::Labels(index) => self
                    .nodes
                    .get(*index)
                    .map(|node| {
                        Value::Array(node.label.iter().cloned().map(Value::String).collect())
                    })
                    .ok_or_else(|| Error::Data("missing created node".into())),
                ResolvedCreateOutput::Value(value) => Ok(value.clone()),
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
    }

    fn properties(&self, entity: EntityRef) -> Option<&Value> {
        match entity {
            EntityRef::Node(index) => self.nodes.get(index).map(|node| &node.properties),
            EntityRef::Relationship(index) => self
                .relationships
                .get(index)
                .map(|relationship| &relationship.properties),
        }
    }
}

fn require_object(value: &Value, description: &str) -> Result<()> {
    if value.is_object() {
        Ok(())
    } else {
        Err(Error::Semantic(format!("{description} must be a map")))
    }
}

fn validate_storable_value(value: &Value) -> Result<()> {
    match value {
        Value::Number(number) if number.as_u64().is_some() && number.as_i64().is_none() => Err(
            Error::Semantic("property integers must fit SQLite's signed 64-bit range".into()),
        ),
        Value::Array(values) => values.iter().try_for_each(validate_storable_value),
        Value::Object(values) => values.values().try_for_each(validate_storable_value),
        _ => Ok(()),
    }
}

pub(crate) fn compile(source: &str) -> Result<Plan> {
    let trimmed = source.trim();
    if trimmed.is_empty() || trimmed == ";" {
        return Err(Error::EmptyQuery);
    }

    let parsed = parse(source).map_err(|errors| Error::Parse(errors.render(source)))?;
    let [statement] = parsed.program.statements.as_slice() else {
        return Err(Error::EmptyQuery);
    };
    let StatementKind::Query(query_statement) = &statement.kind else {
        return Err(unsupported(statement.span, "query statement"));
    };
    if query_statement.mode.is_some() {
        return Err(unsupported(
            query_statement.query.span,
            "EXPLAIN and PROFILE",
        ));
    }
    let open_cypher::ast::QueryKind::Regular(query) = &query_statement.query.kind else {
        return Err(unsupported(query_statement.query.span, "query kind"));
    };
    if !query.unions.is_empty() {
        return Err(unsupported(query_statement.query.span, "UNION"));
    }

    let clauses = &query.head.kind.clauses;
    match clauses.as_slice() {
        [clause] => match &clause.kind {
            ClauseKind::Create(create) => compile_create(source, create, None).map(Plan::Create),
            ClauseKind::Return(return_clause) => ReadCompiler::new(source)
                .compile(None, return_clause)
                .map(Plan::Read),
            kind => Err(unsupported(clause.span, clause_name(kind))),
        },
        [first, second] => match (&first.kind, &second.kind) {
            (ClauseKind::Match(match_clause), ClauseKind::Return(return_clause)) => {
                ReadCompiler::new(source)
                    .compile(Some(match_clause), return_clause)
                    .map(Plan::Read)
            }
            (ClauseKind::Create(create), ClauseKind::Return(return_clause)) => {
                compile_create(source, create, Some(return_clause)).map(Plan::Create)
            }
            (_, kind) => Err(unsupported(second.span, clause_name(kind))),
        },
        [] => Err(Error::EmptyQuery),
        _ => {
            let unsupported_clause = clauses
                .iter()
                .find(|clause| {
                    !matches!(
                        clause.kind,
                        ClauseKind::Match(_) | ClauseKind::Create(_) | ClauseKind::Return(_)
                    )
                })
                .unwrap_or(&clauses[1]);
            Err(unsupported(
                unsupported_clause.span,
                clause_name(&unsupported_clause.kind),
            ))
        }
    }
}

fn clause_name(clause: &ClauseKind) -> &'static str {
    match clause {
        ClauseKind::Use(_) => "USE",
        ClauseKind::Match(_) => "this MATCH clause sequence",
        ClauseKind::Unwind(_) => "UNWIND",
        ClauseKind::Let(_) => "LET",
        ClauseKind::Create(_) => "this CREATE clause sequence",
        ClauseKind::Insert(_) => "INSERT",
        ClauseKind::Merge(_) => "MERGE",
        ClauseKind::Delete(_) => "DELETE",
        ClauseKind::Set(_) => "SET",
        ClauseKind::Remove(_) => "REMOVE",
        ClauseKind::Foreach(_) => "FOREACH",
        ClauseKind::LoadCsv(_) => "LOAD CSV",
        ClauseKind::Call(_) => "CALL",
        ClauseKind::With(_) => "WITH",
        ClauseKind::Return(_) => "this RETURN clause sequence",
        ClauseKind::Select(_) => "SELECT",
        ClauseKind::Next(_) => "NEXT",
        ClauseKind::Filter(_) => "FILTER",
        ClauseKind::Finish => "FINISH",
        ClauseKind::Error(_) => "recovered syntax",
        _ => "clause",
    }
}

fn unsupported(span: Span, feature: impl AsRef<str>) -> Error {
    Error::Unsupported(format!(
        "{} at bytes {}..{}",
        feature.as_ref(),
        span.start,
        span.end
    ))
}

fn semantic(span: Span, message: impl AsRef<str>) -> Error {
    Error::Semantic(format!(
        "{} at bytes {}..{}",
        message.as_ref(),
        span.start,
        span.end
    ))
}

struct CreateCompiler<'source> {
    source: &'source str,
    nodes: Vec<CreateNode>,
    relationships: Vec<CreateRelationship>,
    symbols: HashMap<String, EntityRef>,
}

fn compile_create(
    source: &str,
    create: &CreateClause,
    return_clause: Option<&ReturnClause>,
) -> Result<CreatePlan> {
    let mut compiler = CreateCompiler {
        source,
        nodes: Vec::new(),
        relationships: Vec::new(),
        symbols: HashMap::new(),
    };
    compiler.pattern(&create.pattern)?;
    let projection = return_clause
        .map(|return_clause| compiler.projection(&return_clause.projection))
        .transpose()?;
    Ok(CreatePlan {
        nodes: compiler.nodes,
        relationships: compiler.relationships,
        projection,
    })
}

impl CreateCompiler<'_> {
    fn pattern(&mut self, pattern: &Pattern) -> Result<()> {
        for part in &pattern.parts {
            if part.kind.binding.is_some() || part.kind.selector.is_some() {
                return Err(unsupported(
                    part.span,
                    "path bindings and selectors in CREATE",
                ));
            }
            self.path(&part.kind.path.kind.factors, part.span)?;
        }
        Ok(())
    }

    fn path(&mut self, factors: &[PathFactor], span: Span) -> Result<()> {
        let Some(first) = factors.first() else {
            return Err(semantic(span, "a CREATE path cannot be empty"));
        };
        let PathFactorKind::Node(first_node) = &first.kind else {
            return Err(unsupported(
                first.span,
                "a CREATE path not beginning with a node",
            ));
        };
        let mut left = self.node(first_node, first.span)?;
        let mut remaining = &factors[1..];

        while !remaining.is_empty() {
            let [relationship, node, rest @ ..] = remaining else {
                return Err(semantic(
                    span,
                    "CREATE paths must alternate relationships and nodes",
                ));
            };
            let PathFactorKind::Relationship(relationship_pattern) = &relationship.kind else {
                return Err(unsupported(
                    relationship.span,
                    "non-relationship path factor",
                ));
            };
            let PathFactorKind::Node(node_pattern) = &node.kind else {
                return Err(unsupported(node.span, "non-node path factor"));
            };
            let right = self.node(node_pattern, node.span)?;
            self.relationship(relationship_pattern, relationship.span, left, right)?;
            left = right;
            remaining = rest;
        }
        Ok(())
    }

    fn node(&mut self, node: &NodePattern, span: Span) -> Result<usize> {
        if node.where_clause.is_some() {
            return Err(unsupported(span, "element WHERE in CREATE"));
        }
        if let Some(variable) = &node.variable
            && let Some(entity) = self.symbols.get(&variable.kind.text).copied()
        {
            let EntityRef::Node(index) = entity else {
                return Err(semantic(
                    variable.span,
                    "variable is already a relationship",
                ));
            };
            if node.labels.is_some() || node.properties.is_some() {
                return Err(semantic(
                    span,
                    "a repeated CREATE node variable must be a bare `(variable)` reference",
                ));
            }
            return Ok(index);
        }

        let label = node
            .labels
            .as_ref()
            .map(|label| single_label(label, "multiple or boolean node labels"))
            .transpose()?;
        let properties = properties_input(node.properties.as_deref())?;
        let index = self.nodes.len();
        self.nodes.push(CreateNode { label, properties });
        if let Some(variable) = &node.variable {
            self.symbols
                .insert(variable.kind.text.clone(), EntityRef::Node(index));
        }
        Ok(index)
    }

    fn relationship(
        &mut self,
        relationship: &RelationshipPattern,
        span: Span,
        left: usize,
        right: usize,
    ) -> Result<usize> {
        if relationship.legacy_quantifier.is_some()
            || relationship.graph_quantifier.is_some()
            || relationship.where_clause.is_some()
        {
            return Err(unsupported(
                span,
                "quantified or filtered relationships in CREATE",
            ));
        }
        let relationship_type = relationship
            .labels
            .as_ref()
            .ok_or_else(|| semantic(span, "a created relationship must have one type"))
            .and_then(|label| single_label(label, "multiple or boolean relationship types"))?;
        let (from, to) = match relationship.direction.kind {
            RelationshipDirection::Right => (left, right),
            RelationshipDirection::Left => (right, left),
            RelationshipDirection::Undirected | RelationshipDirection::Both => {
                return Err(unsupported(span, "undirected relationships in CREATE"));
            }
            _ => return Err(unsupported(span, "relationship direction")),
        };
        if let Some(variable) = &relationship.variable
            && self.symbols.contains_key(&variable.kind.text)
        {
            return Err(semantic(variable.span, "variable is already bound"));
        }
        let index = self.relationships.len();
        self.relationships.push(CreateRelationship {
            relationship_type,
            from,
            to,
            properties: properties_input(relationship.properties.as_deref())?,
        });
        if let Some(variable) = &relationship.variable {
            self.symbols
                .insert(variable.kind.text.clone(), EntityRef::Relationship(index));
        }
        Ok(index)
    }

    fn projection(&self, projection: &ProjectionClause) -> Result<CreateProjection> {
        require_simple_projection(projection)?;
        let mut columns = Vec::with_capacity(projection.items.len());
        let mut outputs = Vec::with_capacity(projection.items.len());
        for item in &projection.items {
            let ProjectionItemKind::Expression { expression, alias } = &item.kind else {
                return Err(unsupported(item.span, "RETURN *"));
            };
            columns.push(column_name(self.source, expression, alias.as_ref()));
            outputs.push(self.output(expression)?);
        }
        Ok(CreateProjection { columns, outputs })
    }

    fn output(&self, expression: &Expr) -> Result<CreateOutput> {
        match &expression.kind {
            ExprKind::Property {
                expression: base,
                key,
            } => {
                let entity = self.resolve_variable_expr(base)?;
                Ok(CreateOutput::Property(entity, key.kind.text.clone()))
            }
            ExprKind::Function(function) => self.function_output(function, expression.span),
            ExprKind::Parenthesized(inner) => self.output(inner),
            _ => input_value(expression).map(CreateOutput::Value),
        }
    }

    fn function_output(&self, function: &FunctionInvocation, span: Span) -> Result<CreateOutput> {
        let name = simple_function_name(function, span)?;
        let entity = single_entity_argument(function, &self.symbols, span)?;
        match name.as_str() {
            "elementid" => Ok(CreateOutput::Id(entity)),
            "type" => match entity {
                EntityRef::Relationship(index) => Ok(CreateOutput::RelationshipType(index)),
                EntityRef::Node(_) => Err(semantic(span, "type() requires a relationship")),
            },
            "labels" => match entity {
                EntityRef::Node(index) => Ok(CreateOutput::Labels(index)),
                EntityRef::Relationship(_) => Err(semantic(span, "labels() requires a node")),
            },
            _ => Err(unsupported(span, format!("function `{name}`"))),
        }
    }

    fn resolve_variable_expr(&self, expression: &Expr) -> Result<EntityRef> {
        let ExprKind::Variable(variable) = &expression.kind else {
            return Err(unsupported(expression.span, "nested property access"));
        };
        self.symbols
            .get(&variable.kind.text)
            .copied()
            .ok_or_else(|| {
                semantic(
                    variable.span,
                    format!("unknown variable `{}`", variable.kind.text),
                )
            })
    }
}

fn properties_input(properties: Option<&Expr>) -> Result<InputValue> {
    match properties {
        None => Ok(InputValue::Map(Vec::new())),
        Some(expression) => match &expression.kind {
            ExprKind::Map(_) | ExprKind::Parameter(_) | ExprKind::Parenthesized(_) => {
                input_value(expression)
            }
            _ => Err(unsupported(expression.span, "non-map pattern properties")),
        },
    }
}

fn input_value(expression: &Expr) -> Result<InputValue> {
    match &expression.kind {
        ExprKind::Literal(literal) => literal_value(&literal.kind).map(InputValue::Literal),
        ExprKind::Parameter(parameter) => match &parameter.name {
            ParameterName::Named(name) => Ok(InputValue::Parameter(name.clone())),
            ParameterName::Positional(_) => {
                Err(unsupported(expression.span, "positional parameters"))
            }
            _ => Err(unsupported(expression.span, "parameter form")),
        },
        ExprKind::List(items) => items
            .iter()
            .map(input_value)
            .collect::<Result<Vec<_>>>()
            .map(InputValue::List),
        ExprKind::Map(entries) => entries
            .iter()
            .map(|entry| Ok((entry.key.kind.text.clone(), input_value(&entry.value)?)))
            .collect::<Result<Vec<_>>>()
            .map(InputValue::Map),
        ExprKind::Unary { operator, operand } => match operator.kind {
            UnaryOperator::Minus => {
                if let ExprKind::Literal(literal) = &operand.kind
                    && let LiteralKind::Integer(integer) = &literal.kind
                {
                    negative_integer_literal(integer).map(InputValue::Literal)
                } else {
                    input_value(operand).map(Box::new).map(InputValue::Negated)
                }
            }
            UnaryOperator::Plus => input_value(operand),
            UnaryOperator::Not => Err(unsupported(expression.span, "NOT in a stored value")),
            _ => Err(unsupported(expression.span, "unary value expression")),
        },
        ExprKind::Parenthesized(inner) => input_value(inner),
        _ => Err(unsupported(
            expression.span,
            "non-literal stored value expression",
        )),
    }
}

fn literal_value(literal: &LiteralKind) -> Result<Value> {
    match literal {
        LiteralKind::Null => Ok(Value::Null),
        LiteralKind::Boolean(value) => Ok(Value::Bool(*value)),
        LiteralKind::String(value) => Ok(Value::String(value.value.clone())),
        LiteralKind::Integer(integer) => {
            let (digits, radix) = integer_parts(integer)?;
            i64::from_str_radix(&digits, radix)
                .map(Value::from)
                .map_err(|_| {
                    Error::Semantic(format!(
                        "integer `{}` is outside the i64 range",
                        integer.text
                    ))
                })
        }
        LiteralKind::Float(float) => {
            let parsed = float.text.parse::<f64>().map_err(|_| {
                Error::Semantic(format!("invalid floating-point literal `{}`", float.text))
            })?;
            Number::from_f64(parsed)
                .map(Value::Number)
                .ok_or_else(|| Error::Semantic("non-finite floats are not supported".into()))
        }
        _ => Err(Error::Unsupported("literal type".into())),
    }
}

fn negative_integer_literal(integer: &open_cypher::ast::IntegerLiteral) -> Result<Value> {
    let (digits, radix) = integer_parts(integer)?;
    let magnitude = u64::from_str_radix(&digits, radix).map_err(|_| {
        Error::Semantic(format!(
            "integer `-{}` is outside the i64 range",
            integer.text
        ))
    })?;
    let minimum_magnitude = (i64::MAX as u64) + 1;
    if magnitude == minimum_magnitude {
        Ok(Value::from(i64::MIN))
    } else {
        i64::try_from(magnitude)
            .map(|value| Value::from(-value))
            .map_err(|_| {
                Error::Semantic(format!(
                    "integer `-{}` is outside the i64 range",
                    integer.text
                ))
            })
    }
}

fn integer_parts(integer: &open_cypher::ast::IntegerLiteral) -> Result<(String, u32)> {
    let text = integer.text.replace('_', "");
    match integer.radix {
        IntegerRadix::Decimal => Ok((text, 10)),
        IntegerRadix::Hexadecimal => Ok((
            text.strip_prefix("0x")
                .or_else(|| text.strip_prefix("0X"))
                .unwrap_or(&text)
                .to_owned(),
            16,
        )),
        IntegerRadix::Octal => Ok((
            text.strip_prefix("0o")
                .or_else(|| text.strip_prefix("0O"))
                .unwrap_or(&text)
                .to_owned(),
            8,
        )),
        _ => Err(Error::Unsupported("integer radix".into())),
    }
}

fn single_label(label: &LabelExpression, feature: &str) -> Result<String> {
    match &label.kind {
        LabelExpressionKind::Name(name) => Ok(name.kind.text.clone()),
        _ => Err(unsupported(label.span, feature)),
    }
}

#[derive(Clone, Debug)]
struct EntityBinding {
    entity: EntityRef,
    alias: String,
}

struct ReadCompiler<'source> {
    source: &'source str,
    symbols: HashMap<String, EntityBinding>,
    from: Vec<String>,
    filters: Vec<String>,
    bindings: Vec<SqlBinding>,
    node_count: usize,
    relationship_count: usize,
}

type CompiledProjection = (Vec<String>, Vec<String>, HashMap<String, Expr>);

impl<'source> ReadCompiler<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            source,
            symbols: HashMap::new(),
            from: Vec::new(),
            filters: Vec::new(),
            bindings: Vec::new(),
            node_count: 0,
            relationship_count: 0,
        }
    }

    fn compile(
        mut self,
        match_clause: Option<&MatchClause>,
        return_clause: &ReturnClause,
    ) -> Result<ReadPlan> {
        if let Some(match_clause) = match_clause {
            self.match_clause(match_clause)?;
        }
        let projection = &return_clause.projection;
        let (select, columns, aliases) = self.projection(projection)?;

        let mut sql = String::from("SELECT ");
        sql.push_str(&select.join(", "));
        if !self.from.is_empty() {
            sql.push_str(" FROM ");
            sql.push_str(&self.from.join(", "));
        }
        if !self.filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&self.filters.join(" AND "));
        }

        if let Some(order_by) = &projection.order_by {
            let mut items = Vec::with_capacity(order_by.items.len());
            for item in &order_by.items {
                if item.nulls.is_some() {
                    return Err(unsupported(item.span, "ORDER BY NULLS FIRST/LAST"));
                }
                let expression = if let ExprKind::Variable(variable) = &item.expression.kind {
                    if let Some(aliased_expression) = aliases.get(&variable.kind.text) {
                        self.raw_expression(aliased_expression)?
                    } else {
                        self.raw_expression(&item.expression)?
                    }
                } else {
                    self.raw_expression(&item.expression)?
                };
                let direction = match item.direction.as_ref().map(|value| value.kind) {
                    None | Some(SortDirection::Ascending) => " ASC",
                    Some(SortDirection::Descending) => " DESC",
                    _ => return Err(unsupported(item.span, "sort direction")),
                };
                items.push(format!("{expression}{direction}"));
            }
            sql.push_str(" ORDER BY ");
            sql.push_str(&items.join(", "));
        }

        let offset = match (&projection.offset, &projection.skip) {
            (Some(_), Some(_)) => {
                return Err(semantic(
                    projection.span,
                    "RETURN cannot contain OFFSET and SKIP",
                ));
            }
            (Some(offset), None) | (None, Some(offset)) => Some(offset),
            (None, None) => None,
        };
        if projection.limit.is_some() || offset.is_some() {
            sql.push_str(" LIMIT ");
            match &projection.limit {
                Some(limit) => sql.push_str(&self.page_expression(limit)?),
                None => sql.push_str("-1"),
            }
        }
        if let Some(offset) = offset {
            sql.push_str(" OFFSET ");
            sql.push_str(&self.page_expression(offset)?);
        }

        Ok(ReadPlan {
            sql,
            columns,
            bindings: self.bindings,
        })
    }

    fn match_clause(&mut self, match_clause: &MatchClause) -> Result<()> {
        if match_clause.optional || match_clause.mode.is_some() || !match_clause.hints.is_empty() {
            return Err(unsupported(
                match_clause.pattern.span,
                "OPTIONAL/path-mode/hinted MATCH",
            ));
        }
        for part in &match_clause.pattern.parts {
            if part.kind.binding.is_some() || part.kind.selector.is_some() {
                return Err(unsupported(part.span, "path bindings and selectors"));
            }
            self.match_path(&part.kind.path.kind.factors, part.span)?;
        }
        if let Some(predicate) = &match_clause.where_clause {
            let predicate = self.raw_expression(predicate)?;
            self.filters.push(format!("({predicate})"));
        }
        Ok(())
    }

    fn match_path(&mut self, factors: &[PathFactor], span: Span) -> Result<()> {
        let Some(first) = factors.first() else {
            return Err(semantic(span, "a MATCH path cannot be empty"));
        };
        let PathFactorKind::Node(first_node) = &first.kind else {
            return Err(unsupported(
                first.span,
                "a MATCH path not beginning with a node",
            ));
        };
        let mut left = self.bind_node(first_node, first.span)?;
        let mut relationship_aliases = Vec::<String>::new();
        let mut remaining = &factors[1..];
        while !remaining.is_empty() {
            let [relationship, node, rest @ ..] = remaining else {
                return Err(semantic(
                    span,
                    "MATCH paths must alternate relationships and nodes",
                ));
            };
            let PathFactorKind::Relationship(relationship_pattern) = &relationship.kind else {
                return Err(unsupported(
                    relationship.span,
                    "non-relationship path factor",
                ));
            };
            let PathFactorKind::Node(node_pattern) = &node.kind else {
                return Err(unsupported(node.span, "non-node path factor"));
            };
            let relationship_span = relationship.span;
            let relationship = self.bind_relationship(relationship_pattern, relationship_span)?;
            if relationship_aliases.contains(&relationship.alias) {
                return Err(semantic(
                    relationship_span,
                    "a relationship variable cannot be reused in one path",
                ));
            }
            for previous in &relationship_aliases {
                self.filters
                    .push(format!("{}.id <> {previous}.id", relationship.alias));
            }
            relationship_aliases.push(relationship.alias.clone());
            let right = self.bind_node(node_pattern, node.span)?;
            let forward = format!(
                "({}.from_node = {}.id AND {}.to_node = {}.id)",
                relationship.alias, left.alias, relationship.alias, right.alias
            );
            let reverse = format!(
                "({}.from_node = {}.id AND {}.to_node = {}.id)",
                relationship.alias, right.alias, relationship.alias, left.alias
            );
            let connection = match relationship_pattern.direction.kind {
                RelationshipDirection::Right => forward,
                RelationshipDirection::Left => reverse,
                RelationshipDirection::Undirected => format!("({forward} OR {reverse})"),
                RelationshipDirection::Both => {
                    return Err(unsupported(
                        relationship_pattern.direction.span,
                        "bidirectional relationship patterns",
                    ));
                }
                _ => {
                    return Err(unsupported(
                        relationship_pattern.direction.span,
                        "relationship direction",
                    ));
                }
            };
            self.filters.push(connection);
            left = right;
            remaining = rest;
        }
        Ok(())
    }

    fn bind_node(&mut self, node: &NodePattern, span: Span) -> Result<EntityBinding> {
        let binding = self.bind_entity(
            node.variable.as_ref().map(|name| &name.kind.text),
            true,
            span,
        )?;
        if let Some(label) = &node.labels {
            let label = single_label(label, "multiple or boolean node labels")?;
            let placeholder = self.push_binding(SqlBinding::Text(label));
            self.filters
                .push(format!("{}.label = {placeholder}", binding.alias));
        }
        self.property_filters(&binding.alias, node.properties.as_deref())?;
        if let Some(predicate) = &node.where_clause {
            let predicate = self.raw_expression(predicate)?;
            self.filters.push(format!("({predicate})"));
        }
        Ok(binding)
    }

    fn bind_relationship(
        &mut self,
        relationship: &RelationshipPattern,
        span: Span,
    ) -> Result<EntityBinding> {
        if relationship.legacy_quantifier.is_some() || relationship.graph_quantifier.is_some() {
            return Err(unsupported(span, "variable-length relationship patterns"));
        }
        let binding = self.bind_entity(
            relationship.variable.as_ref().map(|name| &name.kind.text),
            false,
            span,
        )?;
        if let Some(label) = &relationship.labels {
            let relationship_type = single_label(label, "multiple or boolean relationship types")?;
            let placeholder = self.push_binding(SqlBinding::Text(relationship_type));
            self.filters
                .push(format!("{}.path_type = {placeholder}", binding.alias));
        }
        self.property_filters(&binding.alias, relationship.properties.as_deref())?;
        if let Some(predicate) = &relationship.where_clause {
            let predicate = self.raw_expression(predicate)?;
            self.filters.push(format!("({predicate})"));
        }
        Ok(binding)
    }

    fn bind_entity(
        &mut self,
        variable: Option<&String>,
        node: bool,
        span: Span,
    ) -> Result<EntityBinding> {
        if let Some(variable) = variable
            && let Some(binding) = self.symbols.get(variable)
        {
            let kind_matches = matches!(binding.entity, EntityRef::Node(_)) == node;
            if !kind_matches {
                return Err(semantic(
                    span,
                    format!("variable `{variable}` changes entity kind"),
                ));
            }
            return Ok(binding.clone());
        }

        let binding = if node {
            let index = self.node_count;
            self.node_count += 1;
            let alias = format!("n{index}");
            self.from.push(format!("nodes AS {alias}"));
            EntityBinding {
                entity: EntityRef::Node(index),
                alias,
            }
        } else {
            let index = self.relationship_count;
            self.relationship_count += 1;
            let alias = format!("r{index}");
            self.from.push(format!("paths AS {alias}"));
            EntityBinding {
                entity: EntityRef::Relationship(index),
                alias,
            }
        };
        if let Some(variable) = variable {
            self.symbols.insert(variable.clone(), binding.clone());
        }
        Ok(binding)
    }

    fn property_filters(&mut self, alias: &str, properties: Option<&Expr>) -> Result<()> {
        let Some(properties) = properties else {
            return Ok(());
        };
        let ExprKind::Map(entries) = &properties.kind else {
            return Err(unsupported(
                properties.span,
                "parameter-map pattern matching",
            ));
        };
        let mut keys = HashSet::with_capacity(entries.len());
        for entry in entries {
            if !keys.insert(&entry.key.kind.text) {
                return Err(semantic(
                    entry.key.span,
                    format!(
                        "property map contains duplicate key `{}`",
                        entry.key.kind.text
                    ),
                ));
            }
            let expected = input_value(&entry.value)?;
            let (property, property_type) =
                self.property_expression_parts(alias, &entry.key.kind.text);
            let predicate = self.typed_input_comparison(
                property,
                property_type,
                expected,
                BinaryOperator::Equal,
            );
            self.filters.push(predicate);
        }
        Ok(())
    }

    fn projection(&mut self, projection: &ProjectionClause) -> Result<CompiledProjection> {
        if let Some(quantifier) = &projection.quantifier {
            match quantifier.kind {
                SetQuantifier::All => {}
                _ => return Err(unsupported(quantifier.span, "RETURN DISTINCT")),
            }
        }

        let mut select = Vec::with_capacity(projection.items.len());
        let mut columns = Vec::with_capacity(projection.items.len());
        let mut aliases = HashMap::new();
        for (index, item) in projection.items.iter().enumerate() {
            let ProjectionItemKind::Expression { expression, alias } = &item.kind else {
                return Err(unsupported(item.span, "RETURN *"));
            };
            let sql_alias = format!("c{index}");
            let output = self.output_expression(expression)?;
            select.push(format!("{output} AS {sql_alias}"));
            columns.push(column_name(self.source, expression, alias.as_ref()));
            if let Some(alias) = alias {
                aliases.insert(alias.kind.text.clone(), expression.clone());
            }
        }
        if select.is_empty() {
            return Err(semantic(
                projection.span,
                "RETURN must contain at least one item",
            ));
        }
        Ok((select, columns, aliases))
    }

    fn output_expression(&mut self, expression: &Expr) -> Result<String> {
        match &expression.kind {
            ExprKind::Property {
                expression: base,
                key,
            } => {
                let binding = self.resolve_variable(base)?;
                let path = self.push_binding(SqlBinding::Text(key.kind.text.clone()));
                Ok(format!(
                    "COALESCE({}.props -> ('$.' || json_quote({path})), 'null')",
                    binding.alias
                ))
            }
            ExprKind::Function(function) => self.function_output(function, expression.span),
            ExprKind::Parenthesized(inner) => self.output_expression(inner),
            _ => {
                let value = input_value(expression)?;
                Ok(self.push_binding(SqlBinding::Json(value)))
            }
        }
    }

    fn function_output(&mut self, function: &FunctionInvocation, span: Span) -> Result<String> {
        let name = simple_function_name(function, span)?;
        let entity = single_entity_argument(function, &self.symbols, span)?;
        let binding = self
            .symbols
            .values()
            .find(|binding| binding.entity == entity)
            .ok_or_else(|| semantic(span, "unknown function argument"))?;
        match name.as_str() {
            "elementid" => Ok(format!("json_quote({}.id)", binding.alias)),
            "type" => match entity {
                EntityRef::Relationship(_) => {
                    Ok(format!("json_quote({}.path_type)", binding.alias))
                }
                EntityRef::Node(_) => Err(semantic(span, "type() requires a relationship")),
            },
            "labels" => match entity {
                EntityRef::Node(_) => Ok(format!(
                    "CASE WHEN {}.label IS NULL THEN '[]' ELSE json_array({}.label) END",
                    binding.alias, binding.alias
                )),
                EntityRef::Relationship(_) => Err(semantic(span, "labels() requires a node")),
            },
            _ => Err(unsupported(span, format!("function `{name}`"))),
        }
    }

    fn raw_expression(&mut self, expression: &Expr) -> Result<String> {
        match &expression.kind {
            ExprKind::Literal(_)
            | ExprKind::Parameter(_)
            | ExprKind::List(_)
            | ExprKind::Map(_) => {
                let value = input_value(expression)?;
                Ok(self.push_binding(SqlBinding::Scalar(value)))
            }
            ExprKind::Property {
                expression: base,
                key,
            } => {
                let binding = self.resolve_variable(base)?;
                Ok(self.property_expression(&binding.alias, &key.kind.text))
            }
            ExprKind::Binary {
                left,
                operator,
                right,
            } => {
                if matches!(
                    operator.kind,
                    BinaryOperator::Equal | BinaryOperator::NotEqual
                ) {
                    return self
                        .typed_comparison(left, right, operator.kind)?
                        .ok_or_else(|| unsupported(operator.span, "complex equality expression"));
                }
                let left = self.raw_expression(left)?;
                let right = self.raw_expression(right)?;
                let operator = match operator.kind {
                    BinaryOperator::Or => "OR",
                    BinaryOperator::And => "AND",
                    BinaryOperator::Equal => "=",
                    BinaryOperator::NotEqual => "<>",
                    BinaryOperator::Less => "<",
                    BinaryOperator::LessEqual => "<=",
                    BinaryOperator::Greater => ">",
                    BinaryOperator::GreaterEqual => ">=",
                    _ => return Err(unsupported(operator.span, "binary operator")),
                };
                Ok(format!("({left} {operator} {right})"))
            }
            ExprKind::Unary { operator, operand } => {
                let operand = self.raw_expression(operand)?;
                let operator = match operator.kind {
                    UnaryOperator::Plus => "+",
                    UnaryOperator::Minus => "-",
                    UnaryOperator::Not => "NOT ",
                    _ => return Err(unsupported(operator.span, "unary operator")),
                };
                Ok(format!("({operator}{operand})"))
            }
            ExprKind::Is {
                expression,
                negated,
                predicate: IsPredicate::Null,
            } => {
                let expression = self.raw_expression(expression)?;
                Ok(format!(
                    "({expression} IS {}NULL)",
                    if *negated { "NOT " } else { "" }
                ))
            }
            ExprKind::Is { .. } => Err(unsupported(expression.span, "IS predicate")),
            ExprKind::Function(function) => self.raw_function(function, expression.span),
            ExprKind::Parenthesized(inner) => {
                self.raw_expression(inner).map(|value| format!("({value})"))
            }
            ExprKind::Variable(variable) => Err(semantic(
                variable.span,
                format!(
                    "graph variable `{}` is not a scalar expression",
                    variable.kind.text
                ),
            )),
            _ => Err(unsupported(expression.span, "expression")),
        }
    }

    fn page_expression(&mut self, expression: &Expr) -> Result<String> {
        let value = page_input_value(expression)?;
        Ok(self.push_binding(SqlBinding::Page(value)))
    }

    fn resolve_variable(&self, expression: &Expr) -> Result<EntityBinding> {
        let ExprKind::Variable(variable) = &expression.kind else {
            return Err(unsupported(expression.span, "nested property access"));
        };
        self.symbols
            .get(&variable.kind.text)
            .cloned()
            .ok_or_else(|| {
                semantic(
                    variable.span,
                    format!("unknown variable `{}`", variable.kind.text),
                )
            })
    }

    fn raw_function(&self, function: &FunctionInvocation, span: Span) -> Result<String> {
        let name = simple_function_name(function, span)?;
        let entity = single_entity_argument(function, &self.symbols, span)?;
        let binding = self
            .symbols
            .values()
            .find(|binding| binding.entity == entity)
            .ok_or_else(|| semantic(span, "unknown function argument"))?;
        match name.as_str() {
            "elementid" => Ok(format!("{}.id", binding.alias)),
            "type" => match entity {
                EntityRef::Relationship(_) => Ok(format!("{}.path_type", binding.alias)),
                EntityRef::Node(_) => Err(semantic(span, "type() requires a relationship")),
            },
            "labels" => match entity {
                EntityRef::Node(_) => Ok(format!(
                    "CASE WHEN {}.label IS NULL THEN '[]' ELSE json_array({}.label) END",
                    binding.alias, binding.alias
                )),
                EntityRef::Relationship(_) => Err(semantic(span, "labels() requires a node")),
            },
            _ => Err(unsupported(span, format!("function `{name}`"))),
        }
    }

    fn property_expression(&mut self, alias: &str, key: &str) -> String {
        self.property_expression_parts(alias, key).0
    }

    fn property_expression_parts(&mut self, alias: &str, key: &str) -> (String, String) {
        let path = self.push_binding(SqlBinding::Text(key.to_owned()));
        let json_path = format!("'$.' || json_quote({path})");
        (
            format!("json_extract({alias}.props, {json_path})"),
            format!("json_type({alias}.props, {json_path})"),
        )
    }

    fn typed_comparison(
        &mut self,
        left: &Expr,
        right: &Expr,
        operator: BinaryOperator,
    ) -> Result<Option<String>> {
        if let Some((base, key)) = property_reference(left) {
            let binding = self.resolve_variable(base)?;
            let (property, property_type) =
                self.property_expression_parts(&binding.alias, &key.kind.text);
            if let Ok(expected) = input_value(right) {
                return Ok(Some(self.typed_input_comparison(
                    property,
                    property_type,
                    expected,
                    operator,
                )));
            }
            if let Some((right_base, right_key)) = property_reference(right) {
                let right_binding = self.resolve_variable(right_base)?;
                let (right_value, right_type) =
                    self.property_expression_parts(&right_binding.alias, &right_key.kind.text);
                return Ok(Some(typed_sql_comparison(
                    &property,
                    &property_type,
                    &right_value,
                    &right_type,
                    operator,
                )));
            }
        }

        if let Some((base, key)) = property_reference(right)
            && let Ok(expected) = input_value(left)
        {
            let binding = self.resolve_variable(base)?;
            let (property, property_type) =
                self.property_expression_parts(&binding.alias, &key.kind.text);
            return Ok(Some(self.typed_input_comparison(
                property,
                property_type,
                expected,
                operator,
            )));
        }

        if let (Ok(left), Ok(right)) = (input_value(left), input_value(right)) {
            let left_scalar = self.push_binding(SqlBinding::Scalar(left.clone()));
            let left_json = self.push_binding(SqlBinding::Json(left));
            let right_scalar = self.push_binding(SqlBinding::Scalar(right.clone()));
            let right_json = self.push_binding(SqlBinding::Json(right));
            return Ok(Some(typed_sql_comparison(
                &left_scalar,
                &format!("json_type(json({left_json}))"),
                &right_scalar,
                &format!("json_type(json({right_json}))"),
                operator,
            )));
        }

        Ok(None)
    }

    fn typed_input_comparison(
        &mut self,
        property: String,
        property_type: String,
        expected: InputValue,
        operator: BinaryOperator,
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

fn page_input_value(expression: &Expr) -> Result<InputValue> {
    match &expression.kind {
        ExprKind::Literal(literal) if matches!(literal.kind, LiteralKind::Integer(_)) => {
            input_value(expression)
        }
        ExprKind::Parameter(_) => input_value(expression),
        ExprKind::Unary { operator, operand }
            if matches!(operator.kind, UnaryOperator::Plus | UnaryOperator::Minus) =>
        {
            page_input_value(operand)?;
            input_value(expression)
        }
        ExprKind::Parenthesized(inner) => page_input_value(inner),
        _ => Err(semantic(
            expression.span,
            "SKIP and LIMIT require an integer literal or named parameter",
        )),
    }
}

fn property_reference(expression: &Expr) -> Option<(&Expr, &open_cypher::ast::Name)> {
    match &expression.kind {
        ExprKind::Property { expression, key } => Some((expression, key)),
        ExprKind::Parenthesized(inner) => property_reference(inner),
        _ => None,
    }
}

fn typed_sql_comparison(
    left: &str,
    left_type: &str,
    right: &str,
    right_type: &str,
    operator: BinaryOperator,
) -> String {
    let compatible_types = format!(
        "((({left_type}) IN ('integer', 'real') AND ({right_type}) IN ('integer', 'real')) \
         OR (({left_type}) IN ('true', 'false') AND ({right_type}) IN ('true', 'false')) \
         OR ({left_type}) = ({right_type}))"
    );
    let equal = format!("(({left}) = ({right}) AND {compatible_types})");
    let comparison = match operator {
        BinaryOperator::Equal => equal,
        BinaryOperator::NotEqual => format!("NOT {equal}"),
        _ => unreachable!("typed comparisons are only generated for equality operators"),
    };
    format!("(CASE WHEN ({left}) IS NULL OR ({right}) IS NULL THEN NULL ELSE {comparison} END)")
}

fn require_simple_projection(projection: &ProjectionClause) -> Result<()> {
    if projection.quantifier.is_some()
        || projection.order_by.is_some()
        || projection.offset.is_some()
        || projection.skip.is_some()
        || projection.limit.is_some()
    {
        Err(unsupported(
            projection.span,
            "projection modifiers after CREATE",
        ))
    } else {
        Ok(())
    }
}

fn column_name(source: &str, expression: &Expr, alias: Option<&open_cypher::ast::Name>) -> String {
    alias.map_or_else(
        || {
            expression
                .span
                .text(source)
                .unwrap_or("<expression>")
                .to_owned()
        },
        |alias| alias.kind.text.clone(),
    )
}

fn simple_function_name(function: &FunctionInvocation, span: Span) -> Result<String> {
    if function.quantifier.is_some() || function.name.parts.len() != 1 {
        return Err(unsupported(span, "qualified or quantified function call"));
    }
    Ok(function.name.parts[0].kind.text.to_ascii_lowercase())
}

fn single_entity_argument<T>(
    function: &FunctionInvocation,
    symbols: &HashMap<String, T>,
    span: Span,
) -> Result<EntityRef>
where
    T: EntitySymbol,
{
    let [argument] = function.arguments.as_slice() else {
        return Err(semantic(
            span,
            "graph function requires exactly one argument",
        ));
    };
    let ExprKind::Variable(variable) = &argument.kind else {
        return Err(semantic(
            argument.span,
            "graph function argument must be a variable",
        ));
    };
    symbols
        .get(&variable.kind.text)
        .map(EntitySymbol::entity_ref)
        .ok_or_else(|| {
            semantic(
                variable.span,
                format!("unknown variable `{}`", variable.kind.text),
            )
        })
}

trait EntitySymbol {
    fn entity_ref(&self) -> EntityRef;
}

impl EntitySymbol for EntityRef {
    fn entity_ref(&self) -> EntityRef {
        *self
    }
}

impl EntitySymbol for EntityBinding {
    fn entity_ref(&self) -> EntityRef {
        self.entity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_sql_uses_internal_aliases_and_bound_property_names() {
        let Plan::Read(plan) =
            compile("MATCH (`odd variable`:Person) RETURN `odd variable`.`odd.key` AS value")
                .unwrap()
        else {
            panic!("expected a read plan");
        };

        assert!(plan.sql.contains("nodes AS n0"));
        assert!(!plan.sql.contains("odd variable"));
        assert!(!plan.sql.contains("odd.key"));
        assert_eq!(plan.columns, ["value"]);
    }

    #[test]
    fn relationship_joins_are_lowered_to_endpoint_predicates() {
        let Plan::Read(plan) = compile("MATCH (a)-[r:KNOWS]->(b) RETURN a.name, b.name").unwrap()
        else {
            panic!("expected a read plan");
        };

        assert!(plan.sql.contains("paths AS r0"));
        assert!(
            plan.sql
                .contains("r0.from_node = n0.id AND r0.to_node = n1.id")
        );
        assert!(!plan.sql.contains("KNOWS"));
    }
}
