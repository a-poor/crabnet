//! Planning-time kind checks. Where an operand's kind is fixed by the query
//! text (a literal, or a `WITH`/`UNWIND` variable bound to one), a misuse
//! such as `NOT 1`, `1 IN 'a'`, or `'a' % 2` is rejected before execution,
//! as Cypher requires. Kinds that are only known at runtime (parameters,
//! properties) are checked by the generated SQL instead.

use std::collections::HashMap;

use serde_json::Value;

use super::logical::{
    BinaryOp, EntityKind, Expr, ListVariableId, ScalarFunction, ScalarSlotId, UnaryOp, ValueExpr,
};

/// A statically known value kind. Lists remember their element kind when
/// every non-null element has the same one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Null,
    Boolean,
    Integer,
    Float,
    String,
    List(Option<Box<Kind>>),
    EntityList(EntityKind),
    Nested(Box<super::nested::Shape>),
    Entity(EntityKind),
    Map,
}

impl Kind {
    fn name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "a boolean",
            Self::Integer => "an integer",
            Self::Float => "a float",
            Self::String => "a string",
            Self::List(_) => "a list",
            Self::Entity(_) => "a graph entity",
            Self::EntityList(_) => "a list of graph entities",
            Self::Nested(_) => "a graph collection",
            Self::Map => "a map",
        }
    }

    fn is_numeric(&self) -> bool {
        matches!(self, Self::Integer | Self::Float)
    }

    fn element(&self) -> Option<Kind> {
        match self {
            Self::List(element) => element.as_deref().cloned(),
            Self::EntityList(kind) => Some(Self::Entity(*kind)),
            _ => None,
        }
    }
}

/// The kinds of the variables in scope.
#[derive(Clone, Default)]
pub(crate) struct Kinds {
    pub(crate) scalars: HashMap<ScalarSlotId, Kind>,
    elements: HashMap<ListVariableId, Kind>,
}

/// Checks an expression and returns its kind, if known. The error is the
/// message for a semantic error at the expression's span.
pub(crate) fn check(expression: &Expr, kinds: &mut Kinds) -> Result<Option<Kind>, String> {
    Ok(match expression {
        Expr::Nested { shape, expression } => {
            check(expression, kinds)?;
            Some(Kind::Nested(shape.clone()))
        }
        Expr::Value(value) => value_kind(value),
        Expr::HasLabel(..) => Some(Kind::Boolean),
        Expr::Exists { bindings, .. } => {
            for (_, _, value) in bindings.iter() {
                check(value, kinds)?;
            }
            Some(Kind::Boolean)
        }
        Expr::Scalar(slot) => kinds.scalars.get(slot).cloned(),
        Expr::EntityValue { kind, expression } => {
            check(expression, kinds)?;
            Some(Kind::Entity(*kind))
        }
        Expr::EntityList { kind, expression } => {
            check(expression, kinds)?;
            Some(Kind::EntityList(*kind))
        }
        Expr::ListElement(variable) => kinds.elements.get(variable).cloned(),
        Expr::Binary {
            left,
            operator,
            right,
        } => {
            let left = check(left, kinds)?;
            let right = check(right, kinds)?;
            binary_kind(*operator, left, right)?
        }
        Expr::Unary { operator, operand } => {
            let operand = check(operand, kinds)?;
            match operator {
                UnaryOp::Not => {
                    expect_boolean(operand.as_ref(), "NOT")?;
                    Some(Kind::Boolean)
                }
                UnaryOp::Plus | UnaryOp::Minus => {
                    expect_numeric(operand.as_ref(), "unary arithmetic")?;
                    operand.filter(Kind::is_numeric)
                }
            }
        }
        Expr::IsNull { expression, .. } => {
            check(expression, kinds)?;
            Some(Kind::Boolean)
        }
        Expr::MapProperty { base, .. } => {
            if let Some(kind) = check(base, kinds)?
                && !matches!(kind, Kind::Map | Kind::Null)
            {
                return Err(format!(
                    "property access needs a map, node, or relationship, not {}",
                    kind.name()
                ));
            }
            None
        }
        Expr::Quantified {
            variable,
            list,
            predicate,
            ..
        } => {
            let list = check(list, kinds)?;
            expect_list(list.as_ref(), false, "a quantifier")?;
            with_element(kinds, *variable, list, |kinds| {
                let predicate = check(predicate, kinds)?;
                expect_boolean(predicate.as_ref(), "a quantifier predicate")
            })?;
            Some(Kind::Boolean)
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => {
            let list = check(list, kinds)?;
            expect_list(list.as_ref(), false, "a list comprehension")?;
            let element = with_element(kinds, *variable, list.clone(), |kinds| {
                if let Some(predicate) = predicate {
                    let predicate = check(predicate, kinds)?;
                    expect_boolean(predicate.as_ref(), "a list comprehension predicate")?;
                }
                match projection {
                    Some(projection) => check(projection, kinds),
                    None => Ok(list.as_ref().and_then(Kind::element)),
                }
            })?;
            Some(Kind::List(element.map(Box::new)))
        }
        Expr::Case {
            operand,
            alternatives,
            otherwise,
        } => {
            if let Some(operand) = operand {
                check(operand, kinds)?;
            }
            for alternative in alternatives {
                for condition in &alternative.conditions {
                    let condition = check(condition, kinds)?;
                    if operand.is_none() {
                        expect_boolean(condition.as_ref(), "a CASE condition")?;
                    }
                }
                check(&alternative.result, kinds)?;
            }
            if let Some(otherwise) = otherwise {
                check(otherwise, kinds)?;
            }
            None
        }
        Expr::Function {
            function,
            arguments,
        } if function.requires_numeric_input() => {
            check_numeric_function(*function, &arguments[0], kinds)?;
            None
        }
        Expr::Function {
            function: function @ (ScalarFunction::Keys | ScalarFunction::Properties),
            arguments,
        } => {
            for argument in arguments {
                if let Some(kind) = check(argument, kinds)?
                    && !matches!(kind, Kind::Map | Kind::Null)
                {
                    let name = match function {
                        ScalarFunction::Keys => "keys",
                        _ => "properties",
                    };
                    return Err(format!(
                        "{name}() needs a map, node, or relationship, not {}",
                        kind.name()
                    ));
                }
            }
            None
        }
        Expr::NestedCall {
            op: super::nested::Op::ListInput,
            arguments,
        } => {
            let kind = check(&arguments[0], kinds)?;
            expect_list(kind.as_ref(), false, "list operation")?;
            kind
        }
        Expr::Size(operand) => {
            let kind = check(operand, kinds)?;
            expect_list(kind.as_ref(), true, "size()")?;
            None
        }
        Expr::Function {
            function: function @ (ScalarFunction::ListInput | ScalarFunction::Reverse),
            arguments,
        } => {
            let kind = check(&arguments[0], kinds)?;
            expect_list(
                kind.as_ref(),
                *function == ScalarFunction::Reverse,
                function.name(),
            )?;
            kind
        }
        Expr::List(items) => {
            let mut element = None;
            let mut uniform = true;
            for item in items {
                let kind = check(item, kinds)?;
                uniform &= merge_element(&mut element, kind);
            }
            Some(Kind::List(element.filter(|_| uniform).map(Box::new)))
        }
        Expr::Map(_) => {
            for child in expression.children() {
                check(child, kinds)?;
            }
            Some(Kind::Map)
        }
        _ => {
            for child in expression.children() {
                check(child, kinds)?;
            }
            None
        }
    })
}

// Keep function diagnostics out of the recursive check frame so deep graph
// collections still fit on a normal debug-build thread stack.
fn check_numeric_function(
    function: ScalarFunction,
    argument: &Expr,
    kinds: &mut Kinds,
) -> Result<(), String> {
    let kind = check(argument, kinds)?;
    expect_numeric(kind.as_ref(), &format!("{}()", function.name()))
}

/// Checks a predicate such as a `WHERE` clause, which must be a boolean.
pub(crate) fn check_predicate(expression: &Expr, kinds: &mut Kinds) -> Result<(), String> {
    let kind = check(expression, kinds)?;
    expect_boolean(kind.as_ref(), "a predicate")
}

fn binary_kind(
    operator: BinaryOp,
    left: Option<Kind>,
    right: Option<Kind>,
) -> Result<Option<Kind>, String> {
    Ok(match operator {
        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
            let name = match operator {
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
                _ => "XOR",
            };
            expect_boolean(left.as_ref(), name)?;
            expect_boolean(right.as_ref(), name)?;
            Some(Kind::Boolean)
        }
        BinaryOp::In => {
            if let Some(kind) = &right
                && !matches!(kind, Kind::List(_) | Kind::Null)
            {
                return Err(format!("IN needs a list on its right, not {}", kind.name()));
            }
            Some(Kind::Boolean)
        }
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual
        | BinaryOp::StartsWith
        | BinaryOp::EndsWith
        | BinaryOp::Contains => Some(Kind::Boolean),
        BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Remainder => {
            expect_numeric(left.as_ref(), "arithmetic")?;
            expect_numeric(right.as_ref(), "arithmetic")?;
            match (left, right) {
                (Some(Kind::Integer), Some(Kind::Integer)) => Some(Kind::Integer),
                (Some(left), Some(right)) if left.is_numeric() && right.is_numeric() => {
                    Some(Kind::Float)
                }
                _ => None,
            }
        }
        // `+` also concatenates strings and lists, so only numbers with
        // numbers have a kind worth tracking.
        BinaryOp::Add => match (left, right) {
            (Some(Kind::Integer), Some(Kind::Integer)) => Some(Kind::Integer),
            (Some(left), Some(right)) if left.is_numeric() && right.is_numeric() => {
                Some(Kind::Float)
            }
            (Some(Kind::String), Some(Kind::String)) => Some(Kind::String),
            _ => None,
        },
    })
}

fn expect_boolean(kind: Option<&Kind>, context: &str) -> Result<(), String> {
    match kind {
        Some(kind) if !matches!(kind, Kind::Boolean | Kind::Null) => {
            Err(format!("{context} needs a boolean, not {}", kind.name()))
        }
        _ => Ok(()),
    }
}

fn expect_list(kind: Option<&Kind>, allow_string: bool, context: &str) -> Result<(), String> {
    if let Some(kind) = kind {
        let valid = match kind {
            Kind::List(_) | Kind::EntityList(_) | Kind::Null => true,
            Kind::String => allow_string,
            Kind::Nested(shape) => matches!(
                shape.as_ref(),
                super::nested::Shape::List(_)
                    | super::nested::Shape::Tuple(_)
                    | super::nested::Shape::Null
                    | super::nested::Shape::Json
                    | super::nested::Shape::Mixed
            ),
            _ => false,
        };
        if !valid {
            let expected = if allow_string {
                "a list or string"
            } else {
                "a list"
            };
            return Err(format!(
                "{context} requires {expected}, not {}",
                kind.name()
            ));
        }
    }
    Ok(())
}

fn expect_numeric(kind: Option<&Kind>, context: &str) -> Result<(), String> {
    match kind {
        Some(kind) if !kind.is_numeric() && *kind != Kind::Null => {
            Err(format!("{context} needs numbers, not {}", kind.name()))
        }
        _ => Ok(()),
    }
}

/// Runs `body` with a list variable bound to the elements of `list`.
fn with_element<T>(
    kinds: &mut Kinds,
    variable: ListVariableId,
    list: Option<Kind>,
    body: impl FnOnce(&mut Kinds) -> Result<T, String>,
) -> Result<T, String> {
    let previous = match list.as_ref().and_then(Kind::element) {
        Some(element) => kinds.elements.insert(variable, element),
        None => kinds.elements.remove(&variable),
    };
    let result = body(kinds);
    match previous {
        Some(previous) => kinds.elements.insert(variable, previous),
        None => kinds.elements.remove(&variable),
    };
    result
}

/// Folds one element kind into a list's element kind; false once the
/// elements disagree or one is unknown. Nulls fit any element kind.
fn merge_element(element: &mut Option<Kind>, kind: Option<Kind>) -> bool {
    match kind {
        None => false,
        Some(Kind::Null) => true,
        Some(kind) => match element {
            None => {
                *element = Some(kind);
                true
            }
            Some(existing) => *existing == kind,
        },
    }
}

fn value_kind(value: &ValueExpr) -> Option<Kind> {
    match value {
        ValueExpr::Literal(value) => Some(json_kind(value)),
        ValueExpr::Parameter(_) => None,
        ValueExpr::Negated(inner) => value_kind(inner).filter(Kind::is_numeric),
        ValueExpr::List(items) => {
            let mut element = None;
            let mut uniform = true;
            for item in items {
                uniform &= merge_element(&mut element, value_kind(item));
            }
            Some(Kind::List(element.filter(|_| uniform).map(Box::new)))
        }
        ValueExpr::Map(_) => Some(Kind::Map),
    }
}

fn json_kind(value: &Value) -> Kind {
    match value {
        Value::Null => Kind::Null,
        Value::Bool(_) => Kind::Boolean,
        Value::Number(number) if number.is_i64() => Kind::Integer,
        Value::Number(_) => Kind::Float,
        Value::String(_) => Kind::String,
        Value::Array(items) => {
            let mut element = None;
            let mut uniform = true;
            for item in items {
                uniform &= merge_element(&mut element, Some(json_kind(item)));
            }
            Kind::List(element.filter(|_| uniform).map(Box::new))
        }
        Value::Object(_) => Kind::Map,
    }
}
