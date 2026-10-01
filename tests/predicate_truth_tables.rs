use std::cmp::Ordering;

use crabnet::{CypherValue, Database, Error, Parameters, Value};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Truth {
    False,
    True,
    Null,
}

impl Truth {
    fn not(self) -> Self {
        match self {
            Self::False => Self::True,
            Self::True => Self::False,
            Self::Null => Self::Null,
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Null,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Null,
        }
    }

    fn value(self) -> CypherValue {
        match self {
            Self::False => CypherValue::Boolean(false),
            Self::True => CypherValue::Boolean(true),
            Self::Null => CypherValue::Null,
        }
    }
}

#[derive(Clone, Debug)]
struct Operand {
    name: &'static str,
    value: Option<Value>,
}

impl Operand {
    fn missing(name: &'static str) -> Self {
        Self { name, value: None }
    }

    fn present(name: &'static str, value: Value) -> Self {
        Self {
            name,
            value: Some(value),
        }
    }

    fn display_value(&self) -> String {
        self.value
            .as_ref()
            .map_or_else(|| "<missing>".to_owned(), Value::to_string)
    }
}

fn operands() -> Vec<Operand> {
    vec![
        Operand::missing("missing"),
        Operand::present("null", Value::Null),
        Operand::present("false", json!(false)),
        Operand::present("true", json!(true)),
        Operand::present("int_min", json!(i64::MIN)),
        Operand::present("int_neg_one", json!(-1)),
        Operand::present("int_zero", json!(0)),
        Operand::present("int_one", json!(1)),
        Operand::present("int_2pow53", json!(9_007_199_254_740_992_i64)),
        Operand::present("int_2pow53_plus_one", json!(9_007_199_254_740_993_i64)),
        Operand::present("int_max", json!(i64::MAX)),
        Operand::present("float_neg_zero", json!(-0.0)),
        Operand::present("float_one", json!(1.0)),
        Operand::present("float_one_half", json!(1.5)),
        Operand::present("float_2pow53", json!(9_007_199_254_740_992.0_f64)),
        Operand::present("float_2pow63", json!(9_223_372_036_854_775_808.0_f64)),
        Operand::present("text_empty", json!("")),
        Operand::present("text_one", json!("1")),
        Operand::present("text_a", json!("a")),
        Operand::present("text_ab", json!("ab")),
        Operand::present("list_empty", json!([])),
        Operand::present("list_null", json!([null])),
        Operand::present("list_int", json!([1])),
        Operand::present("list_float", json!([1.0])),
        Operand::present("map_empty", json!({})),
        Operand::present("map_null", json!({"x": null})),
        Operand::present("map_int", json!({"x": 1})),
        Operand::present("map_float", json!({"x": 1.0})),
    ]
}

fn seed_operands(database: &mut Database, operands: &[Operand]) {
    for operand in operands {
        let mut parameters = Parameters::from([("name".to_owned(), json!(operand.name))]);
        let query = if let Some(value) = &operand.value {
            parameters.insert("value".to_owned(), value.clone());
            "CREATE (:PredicateOperand {name: $name, value: $value})"
        } else {
            "CREATE (:PredicateOperand {name: $name})"
        };
        database.execute_with(query, &parameters).unwrap();
    }
}

fn is_nullish(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

fn compare_integer_float(integer: i64, float: f64) -> Ordering {
    debug_assert!(float.is_finite());

    const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;
    if float >= TWO_TO_63 {
        return Ordering::Less;
    }
    if float < -TWO_TO_63 {
        return Ordering::Greater;
    }

    let truncated = float.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal if float.fract().is_sign_positive() && float.fract() != 0.0 => {
            Ordering::Less
        }
        Ordering::Equal if float.fract().is_sign_negative() && float.fract() != 0.0 => {
            Ordering::Greater
        }
        ordering => ordering,
    }
}

fn compare_numbers(left: &serde_json::Number, right: &serde_json::Number) -> Ordering {
    match (left.as_i64(), right.as_i64()) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(left), None) => compare_integer_float(left, right.as_f64().unwrap()),
        (None, Some(right)) => compare_integer_float(right, left.as_f64().unwrap()).reverse(),
        (None, None) => left
            .as_f64()
            .unwrap()
            .partial_cmp(&right.as_f64().unwrap())
            .unwrap(),
    }
}

fn equality(left: Option<&Value>, right: Option<&Value>) -> Truth {
    if is_nullish(left) || is_nullish(right) {
        return Truth::Null;
    }

    match (left.unwrap(), right.unwrap()) {
        (Value::Number(left), Value::Number(right)) => {
            if compare_numbers(left, right) == Ordering::Equal {
                Truth::True
            } else {
                Truth::False
            }
        }
        (Value::Bool(left), Value::Bool(right)) => Truth::from(*left == *right),
        (Value::String(left), Value::String(right)) => Truth::from(left == right),
        // Collections compare element by element: a definite difference
        // decides false, and otherwise a null element leaves it unknown.
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Truth::False;
            }
            left.iter()
                .zip(right)
                .fold(Truth::True, |truth, (left, right)| {
                    truth.and(equality(Some(left), Some(right)))
                })
        }
        (Value::Object(left), Value::Object(right)) => {
            if left.len() != right.len() || left.keys().any(|key| !right.contains_key(key)) {
                return Truth::False;
            }
            left.iter().fold(Truth::True, |truth, (key, left)| {
                truth.and(equality(Some(left), right.get(key)))
            })
        }
        _ => Truth::False,
    }
}

fn ordering(left: Option<&Value>, right: Option<&Value>) -> Option<Ordering> {
    if is_nullish(left) || is_nullish(right) {
        return None;
    }

    match (left.unwrap(), right.unwrap()) {
        (Value::Number(left), Value::Number(right)) => Some(compare_numbers(left, right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        // Lists compare lexicographically, a shorter prefix first, and are
        // incomparable from the first incomparable element pair on.
        (Value::Array(left), Value::Array(right)) => {
            for (left, right) in left.iter().zip(right) {
                match ordering(Some(left), Some(right))? {
                    Ordering::Equal => {}
                    decided => return Some(decided),
                }
            }
            Some(left.len().cmp(&right.len()))
        }
        _ => None,
    }
}

fn ordered(comparison: Option<Ordering>, accepts: impl FnOnce(Ordering) -> bool) -> Truth {
    comparison.map_or(Truth::Null, |ordering| Truth::from(accepts(ordering)))
}

fn membership(left: Option<&Value>, right: Option<&Value>) -> Outcome {
    let members = match right {
        Some(Value::Array(members)) => members,
        None | Some(Value::Null) => return Outcome::Truth(Truth::Null),
        Some(_) => return Outcome::TypeError,
    };
    if members.is_empty() {
        return Outcome::Truth(Truth::False);
    }

    let comparisons = members
        .iter()
        .map(|member| equality(left, Some(member)))
        .collect::<Vec<_>>();
    Outcome::Truth(if comparisons.contains(&Truth::True) {
        Truth::True
    } else if comparisons.contains(&Truth::Null) {
        Truth::Null
    } else {
        Truth::False
    })
}

/// What a predicate that can fail produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Truth(Truth),
    TypeError,
    /// Either the truth value or a type error. `AND` and `OR` may stop at an
    /// operand that decides the result without checking the other one.
    TruthOrTypeError(Truth),
}

/// A boolean operand, or `None` for a value of another kind.
fn boolean_operand(value: Option<&Value>) -> Option<Truth> {
    match value {
        None | Some(Value::Null) => Some(Truth::Null),
        Some(Value::Bool(value)) => Some(Truth::from(*value)),
        Some(_) => None,
    }
}

fn connective(
    left: Option<&Value>,
    right: Option<&Value>,
    deciding: Truth,
    combine: impl FnOnce(Truth, Truth) -> Truth,
) -> Outcome {
    match (boolean_operand(left), boolean_operand(right)) {
        (Some(left), Some(right)) => Outcome::Truth(combine(left, right)),
        (Some(decided), None) | (None, Some(decided)) if decided == deciding => {
            Outcome::TruthOrTypeError(deciding)
        }
        _ => Outcome::TypeError,
    }
}

fn negation(value: Option<&Value>) -> Outcome {
    boolean_operand(value).map_or(Outcome::TypeError, |truth| Outcome::Truth(truth.not()))
}

/// Runs a single-cell query and checks it against the expected outcome.
fn assert_outcome(
    failures: &mut Vec<String>,
    database: &mut Database,
    query: &str,
    parameters: &Parameters,
    label: String,
    expected: Outcome,
) {
    let actual = database
        .execute_with(query, parameters)
        .map(|result| result.rows[0][0].clone());
    let accepted = match (expected, &actual) {
        (Outcome::Truth(truth) | Outcome::TruthOrTypeError(truth), Ok(value)) => {
            *value == truth.value()
        }
        (Outcome::TypeError | Outcome::TruthOrTypeError(_), Err(Error::Type(_))) => true,
        _ => false,
    };
    if !accepted {
        failures.push(format!("{label}; expected {expected:?}, got {actual:?}"));
    }
}

fn string_predicate(
    left: Option<&Value>,
    right: Option<&Value>,
    predicate: impl FnOnce(&str, &str) -> bool,
) -> Truth {
    match (left, right) {
        (Some(Value::String(left)), Some(Value::String(right))) => {
            Truth::from(predicate(left, right))
        }
        _ => Truth::Null,
    }
}

impl From<bool> for Truth {
    fn from(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }
}

fn case_index(operands: &[Operand], value: &CypherValue) -> usize {
    let name = value.as_str().expect("operand name should be text");
    operands
        .iter()
        .position(|operand| operand.name == name)
        .unwrap_or_else(|| panic!("query returned unknown operand name {name:?}"))
}

fn assert_truth(
    failures: &mut Vec<String>,
    operator: &str,
    left: &Operand,
    right: Option<&Operand>,
    expected: Truth,
    actual: &CypherValue,
) {
    let expected_value = expected.value();
    if actual != &expected_value {
        let right = right.map_or_else(String::new, |right| {
            format!("; right={} ({})", right.name, right.display_value())
        });
        failures.push(format!(
            "{operator}: left={} ({}){right}; expected {expected_value:?}, got {actual:?}",
            left.name,
            left.display_value(),
        ));
    }
}

#[test]
fn binary_predicates_match_the_finite_typed_oracle() {
    let operands = operands();
    let mut database = Database::in_memory().unwrap();
    seed_operands(&mut database, &operands);

    let result = database
        .execute(
            "MATCH (lhs:PredicateOperand), (rhs:PredicateOperand) \
             RETURN lhs.name AS lhs_name, rhs.name AS rhs_name, \
                    lhs.value = rhs.value AS equal, \
                    lhs.value <> rhs.value AS not_equal, \
                    lhs.value < rhs.value AS less, \
                    lhs.value <= rhs.value AS less_equal, \
                    lhs.value > rhs.value AS greater, \
                    lhs.value >= rhs.value AS greater_equal, \
                    lhs.value STARTS WITH rhs.value AS starts_with, \
                    lhs.value CONTAINS rhs.value AS contains, \
                    lhs.value ENDS WITH rhs.value AS ends_with",
        )
        .unwrap();

    assert_eq!(
        result.columns,
        [
            "lhs_name",
            "rhs_name",
            "equal",
            "not_equal",
            "less",
            "less_equal",
            "greater",
            "greater_equal",
            "starts_with",
            "contains",
            "ends_with",
        ]
    );
    assert_eq!(result.rows.len(), operands.len() * operands.len());
    assert_eq!(result.summary, Default::default());

    let mut seen = vec![vec![false; operands.len()]; operands.len()];
    let mut failures = Vec::new();
    for row in &result.rows {
        assert_eq!(row.len(), result.columns.len());
        let left_index = case_index(&operands, &row[0]);
        let right_index = case_index(&operands, &row[1]);
        let left = &operands[left_index];
        let right = &operands[right_index];
        if std::mem::replace(&mut seen[left_index][right_index], true) {
            failures.push(format!(
                "duplicate pair: left={}, right={}",
                left.name, right.name
            ));
            continue;
        }

        let left_value = left.value.as_ref();
        let right_value = right.value.as_ref();
        let comparison = ordering(left_value, right_value);
        let expected = [
            ("=", equality(left_value, right_value)),
            ("<>", equality(left_value, right_value).not()),
            ("<", ordered(comparison, |value| value.is_lt())),
            ("<=", ordered(comparison, |value| value.is_le())),
            (">", ordered(comparison, |value| value.is_gt())),
            (">=", ordered(comparison, |value| value.is_ge())),
            (
                "STARTS WITH",
                string_predicate(left_value, right_value, |left, right| {
                    left.starts_with(right)
                }),
            ),
            (
                "CONTAINS",
                string_predicate(left_value, right_value, |left, right| left.contains(right)),
            ),
            (
                "ENDS WITH",
                string_predicate(left_value, right_value, |left, right| left.ends_with(right)),
            ),
        ];

        for (offset, (operator, expected)) in expected.into_iter().enumerate() {
            assert_truth(
                &mut failures,
                operator,
                left,
                Some(right),
                expected,
                &row[offset + 2],
            );
        }
    }

    // Connectives and membership can fail on a value of the wrong kind, so
    // each pair runs alone.
    for left in &operands {
        for right in &operands {
            let parameters = Parameters::from([
                ("lhs".to_owned(), json!(left.name)),
                ("rhs".to_owned(), json!(right.name)),
            ]);
            let (left_value, right_value) = (left.value.as_ref(), right.value.as_ref());
            for (operator, expected) in [
                (
                    "AND",
                    connective(left_value, right_value, Truth::False, Truth::and),
                ),
                (
                    "OR",
                    connective(left_value, right_value, Truth::True, Truth::or),
                ),
                ("IN", membership(left_value, right_value)),
            ] {
                assert_outcome(
                    &mut failures,
                    &mut database,
                    &format!(
                        "MATCH (lhs:PredicateOperand {{name: $lhs}}), \
                               (rhs:PredicateOperand {{name: $rhs}}) \
                         RETURN lhs.value {operator} rhs.value AS result"
                    ),
                    &parameters,
                    format!(
                        "{operator}: left={} ({}); right={} ({})",
                        left.name,
                        left.display_value(),
                        right.name,
                        right.display_value()
                    ),
                    expected,
                );
            }
        }
    }

    for (left_index, row) in seen.iter().enumerate() {
        for (right_index, seen) in row.iter().enumerate() {
            if !seen {
                failures.push(format!(
                    "missing pair: left={}, right={}",
                    operands[left_index].name, operands[right_index].name
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} predicate-oracle failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn unary_predicates_match_the_finite_typed_oracle() {
    let operands = operands();
    let mut database = Database::in_memory().unwrap();
    seed_operands(&mut database, &operands);

    let result = database
        .execute(
            "MATCH (operand:PredicateOperand) \
             RETURN operand.name AS name, \
                    operand.value IS NULL AS is_null, \
                    operand.value IS NOT NULL AS is_not_null",
        )
        .unwrap();

    assert_eq!(result.columns, ["name", "is_null", "is_not_null"]);
    assert_eq!(result.rows.len(), operands.len());
    assert_eq!(result.summary, Default::default());

    let mut seen = vec![false; operands.len()];
    let mut failures = Vec::new();
    for row in &result.rows {
        assert_eq!(row.len(), result.columns.len());
        let index = case_index(&operands, &row[0]);
        let operand = &operands[index];
        if std::mem::replace(&mut seen[index], true) {
            failures.push(format!("duplicate operand: {}", operand.name));
            continue;
        }

        let value = operand.value.as_ref();
        assert_truth(
            &mut failures,
            "IS NULL",
            operand,
            None,
            Truth::from(is_nullish(value)),
            &row[1],
        );
        assert_truth(
            &mut failures,
            "IS NOT NULL",
            operand,
            None,
            Truth::from(!is_nullish(value)),
            &row[2],
        );
    }
    for operand in &operands {
        assert_outcome(
            &mut failures,
            &mut database,
            "MATCH (operand:PredicateOperand {name: $name}) RETURN NOT operand.value AS result",
            &Parameters::from([("name".to_owned(), json!(operand.name))]),
            format!("NOT: {} ({})", operand.name, operand.display_value()),
            negation(operand.value.as_ref()),
        );
    }

    for (index, seen) in seen.iter().enumerate() {
        if !seen {
            failures.push(format!("missing operand: {}", operands[index].name));
        }
    }

    assert!(
        failures.is_empty(),
        "{} predicate-oracle failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
