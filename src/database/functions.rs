//! SQLite functions implementing Cypher value, numeric, and text semantics.
//! Generated SQL handles operand kinds before calling these helpers.

use std::{cmp::Ordering, collections::HashSet};

use rusqlite::{
    Connection,
    functions::{Aggregate, Context, FunctionFlags},
    types::ValueRef,
};

use super::{comparison, decode_path_identity, nested, semantic_json_key, text};
use crate::Result;

pub(super) fn register(connection: &Connection) -> Result<()> {
    nested::register(connection)?;
    connection.create_aggregate_function(
        "crabnet_collect_distinct",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        CollectDistinct,
    )?;
    connection.create_scalar_function(
        "crabnet_entity_list_key",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let value = json_argument(context, 0)?;
            if value.is_null() {
                return Ok(semantic_json_key(&value));
            }
            let ids = value
                .as_array()
                .ok_or_else(|| {
                    rusqlite::Error::UserFunctionError(Box::new(crate::Error::Data(
                        "entity list is not an array".into(),
                    )))
                })?
                .iter()
                .map(|entity| {
                    if entity.is_null() {
                        return Ok(serde_json::Value::Null);
                    }
                    entity
                        .get("id")
                        .filter(|id| id.is_string())
                        .cloned()
                        .ok_or_else(|| {
                            rusqlite::Error::UserFunctionError(Box::new(crate::Error::Data(
                                "collected entity is missing its identity".into(),
                            )))
                        })
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(semantic_json_key(&serde_json::Value::Array(ids)))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_path_key",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let encoded = context.get::<String>(0)?;
            let kind = context.get::<String>(1)?;
            let identity = decode_path_identity(&encoded)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
            // Ignore segment boundaries and, for graph lists, the identities
            // belonging only to the other half of the underlying path.
            let value = match kind.as_str() {
                "nodes" => serde_json::json!(identity.node_ids),
                "relationships" => serde_json::json!(identity.relationship_ids),
                _ => serde_json::json!([identity.node_ids, identity.relationship_ids]),
            };
            Ok(semantic_json_key(&value))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_semantic_key",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let encoded = context.get::<String>(0)?;
            let value = serde_json::from_str(&encoded)
                .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
            Ok(semantic_json_key(&value))
        },
    )?;
    // Cypher equality, comparison, and sort order for values SQLite cannot
    // compare natively. Each takes JSON text; generated SQL routes lists and
    // maps here and keeps primitives in plain SQL.
    connection.create_scalar_function(
        "crabnet_equals",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let left = json_argument(context, 0)?;
            let right = json_argument(context, 1)?;
            Ok(comparison::equals(&left, &right))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_compare",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let left = json_argument(context, 0)?;
            let right = json_argument(context, 1)?;
            Ok(comparison::compare(&left, &right).map(|ordering| ordering as i64))
        },
    )?;
    // The sort key takes a JSON type name and the SQL value, so primitives
    // need no JSON round trip; lists and maps arrive as JSON text.
    connection.create_scalar_function(
        "crabnet_order_key",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let value_type = context.get::<Option<String>>(0)?;
            let value = match (value_type.as_deref(), context.get_raw(1)) {
                (_, ValueRef::Null) | (None | Some("null"), _) => serde_json::Value::Null,
                (Some("true"), _) => serde_json::Value::Bool(true),
                (Some("false"), _) => serde_json::Value::Bool(false),
                (Some("integer"), ValueRef::Integer(integer)) => integer.into(),
                (Some("real"), ValueRef::Real(real)) => real.into(),
                (Some("real"), ValueRef::Integer(integer)) => (integer as f64).into(),
                (Some("text"), ValueRef::Text(text)) => {
                    serde_json::Value::String(String::from_utf8_lossy(text).into_owned())
                }
                _ => json_argument(context, 1)?,
            };
            Ok(comparison::order_key(&value))
        },
    )?;
    for (name, keep) in [
        ("crabnet_min", Ordering::Less),
        ("crabnet_max", Ordering::Greater),
    ] {
        connection.create_aggregate_function(
            name,
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            Extremum(keep),
        )?;
    }
    // SQLite renders REAL values into JSON text with fifteen significant
    // digits, which silently perturbs any float that needs more. Every scalar
    // leaves SQL as JSON text, so the encoder routes reals through this
    // shortest round-trip formatter instead of `json()`.
    connection.create_scalar_function(
        "crabnet_json_real",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| Ok(context.get::<Option<f64>>(0)?.map(json_real_text)),
    )?;
    // Check arithmetic before SQLite's JSON transport or a later operator
    // can turn an unsupported non-finite result into a different value. SQL
    // calls this only after handling null operands / empty aggregate bags.
    connection.create_scalar_function(
        "crabnet_finite",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let value = context.get::<rusqlite::types::Value>(0)?;
            match value {
                rusqlite::types::Value::Integer(_) => Ok(value),
                rusqlite::types::Value::Real(real) => {
                    finite_number(real).map(rusqlite::types::Value::Real)
                }
                // SUM/AVG can return NULL for an indeterminate accumulation
                // even when their input contains non-null finite numbers.
                _ => Err(non_finite_error()),
            }
        },
    )?;
    // A quotient cast to i64 saturates for large ratios and cannot implement
    // floating remainder. Rust's remainder handles finite operands directly.
    connection.create_scalar_function(
        "crabnet_remainder",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| finite_number(context.get::<f64>(0)? % context.get::<f64>(1)?),
    )?;
    // The bundled SQLite is built without its math library, so the numeric
    // functions come from Rust. Null input stays null; non-finite output fails.
    type MathFunction = fn(f64) -> f64;
    let math: [(&str, MathFunction); 12] = [
        ("crabnet_ceil", f64::ceil),
        ("crabnet_floor", f64::floor),
        ("crabnet_sqrt", f64::sqrt),
        ("crabnet_exp", f64::exp),
        ("crabnet_ln", f64::ln),
        ("crabnet_log10", f64::log10),
        ("crabnet_sin", f64::sin),
        ("crabnet_cos", f64::cos),
        ("crabnet_tan", f64::tan),
        ("crabnet_asin", f64::asin),
        ("crabnet_acos", f64::acos),
        ("crabnet_atan", f64::atan),
    ];
    for (name, function) in math {
        connection.create_scalar_function(
            name,
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            move |context| {
                context
                    .get::<Option<f64>>(0)?
                    .map(|value| finite_number(function(value)))
                    .transpose()
            },
        )?;
    }
    // SQLite's integer cast saturates out-of-range reals; Cypher conversion
    // instead returns null when the truncated value cannot fit.
    connection.create_scalar_function(
        "crabnet_float_to_integer",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| Ok(context.get::<Option<f64>>(0)?.and_then(float_to_integer)),
    )?;
    // Text conversions and string helpers that SQLite lacks. Each takes text
    // (the generated SQL gates the argument type) and returns NULL when the
    // text does not convert.
    connection.create_scalar_function(
        "crabnet_to_integer",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            Ok(context
                .get::<Option<String>>(0)?
                .and_then(|text| text_to_integer(&text)))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_to_float",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            Ok(context
                .get::<Option<String>>(0)?
                .and_then(|text| text_to_float(&text)))
        },
    )?;
    connection.create_scalar_function(
        "crabnet_reverse",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            Ok(context
                .get::<Option<String>>(0)?
                .map(|text| text.chars().rev().collect::<String>()))
        },
    )?;
    text::register(connection)?;
    connection.create_scalar_function(
        "crabnet_split",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let text = context.get::<Option<String>>(0)?;
            let delimiter = context.get::<Option<String>>(1)?;
            Ok(match (text, delimiter) {
                (Some(text), Some(delimiter)) => Some(split_text(&text, &delimiter)),
                _ => None,
            })
        },
    )?;
    // Generated SQL calls this from a CASE branch to raise a runtime argument
    // error (for example a zero `range()` step) instead of producing a value.
    connection.create_scalar_function(
        "crabnet_raise",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| -> rusqlite::Result<Option<i64>> {
            let message = context.get::<String>(0)?;
            Err(rusqlite::Error::UserFunctionError(
                format!("{}{message}", crate::error::ARGUMENT_ERROR_TAG).into(),
            ))
        },
    )?;
    // Generated SQL raises a runtime type error, such as `NOT` of a number,
    // the same way.
    connection.create_scalar_function(
        "crabnet_type_error",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| -> rusqlite::Result<Option<i64>> {
            let message = context.get::<String>(0)?;
            Err(rusqlite::Error::UserFunctionError(
                format!("{}{message}", crate::error::TYPE_ERROR_TAG).into(),
            ))
        },
    )?;
    Ok(())
}

fn non_finite_error() -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(
        format!(
            "{}numeric result is not finite",
            crate::error::ARGUMENT_ERROR_TAG
        )
        .into(),
    )
}

fn finite_number(value: f64) -> rusqlite::Result<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(non_finite_error())
    }
}

pub(super) fn json_argument(
    context: &Context<'_>,
    index: usize,
) -> rusqlite::Result<serde_json::Value> {
    let encoded = context.get::<String>(index)?;
    nested::parse_json(&encoded)
        .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))
}

/// `min()` and `max()` under Cypher orderability: the argument is a value's
/// JSON text, nulls are skipped, and the result is the JSON text of the
/// least or greatest value, or NULL when there was none.
struct Extremum(Ordering);

/// Keeps the first representative of each typed value in input order.
struct CollectDistinct;

#[derive(Default)]
struct CollectedValues {
    keys: HashSet<Vec<u8>>,
    values: Vec<serde_json::Value>,
}

impl Aggregate<CollectedValues, String> for CollectDistinct {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<CollectedValues> {
        Ok(CollectedValues::default())
    }

    fn step(&self, context: &mut Context<'_>, state: &mut CollectedValues) -> rusqlite::Result<()> {
        let value = json_argument(context, 0)?;
        if value.is_null() {
            return Ok(());
        }
        let key = match context.get_raw(1) {
            ValueRef::Blob(key) | ValueRef::Text(key) => key.to_vec(),
            _ => {
                return Err(rusqlite::Error::UserFunctionError(Box::new(
                    crate::Error::Data("collected value has no identity key".into()),
                )));
            }
        };
        if state.keys.insert(key) {
            state.values.push(value);
        }
        Ok(())
    }

    fn finalize(
        &self,
        _: &mut Context<'_>,
        state: Option<CollectedValues>,
    ) -> rusqlite::Result<String> {
        Ok(serde_json::Value::Array(state.unwrap_or_default().values).to_string())
    }
}

impl Aggregate<Option<(Vec<u8>, String)>, Option<String>> for Extremum {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<Option<(Vec<u8>, String)>> {
        Ok(None)
    }

    fn step(
        &self,
        context: &mut Context<'_>,
        best: &mut Option<(Vec<u8>, String)>,
    ) -> rusqlite::Result<()> {
        let Some(encoded) = context.get::<Option<String>>(0)? else {
            return Ok(());
        };
        let value: serde_json::Value = serde_json::from_str(&encoded)
            .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
        if value.is_null() {
            return Ok(());
        }
        let key = comparison::order_key(&value);
        if best
            .as_ref()
            .is_none_or(|(best, _)| key.cmp(best) == self.0)
        {
            *best = Some((key, encoded));
        }
        Ok(())
    }

    fn finalize(
        &self,
        _: &mut Context<'_>,
        best: Option<Option<(Vec<u8>, String)>>,
    ) -> rusqlite::Result<Option<String>> {
        Ok(best.flatten().map(|(_, encoded)| encoded))
    }
}

/// `toInteger()` of a string: an integer literal, or a finite float literal
/// truncated toward zero; anything else is null.
fn text_to_integer(text: &str) -> Option<i64> {
    let text = text.trim();
    let digits = text
        .strip_prefix('+')
        .or_else(|| text.strip_prefix('-'))
        .unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        // Never round an out-of-range integer string through f64: values just
        // below i64::MIN would round back into range.
        return text.parse::<i64>().ok();
    }
    float_to_integer(text.parse::<f64>().ok()?)
}

fn float_to_integer(value: f64) -> Option<i64> {
    let truncated = value.trunc();
    // The exact f64 upper bound is 2^63 (exclusive); i64::MAX rounds up to it.
    // The lower bound is exactly representable and inclusive. Range membership
    // also rejects NaN and infinities before Rust's saturating cast can run.
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&truncated) {
        Some(truncated as i64)
    } else {
        None
    }
}

/// `toFloat()` of a string: a finite float literal, else null.
fn text_to_float(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|float| float.is_finite())
}

/// `split()`: the pieces between delimiters as a JSON array of strings. An
/// empty delimiter splits into characters.
fn split_text(text: &str, delimiter: &str) -> String {
    let pieces = if delimiter.is_empty() {
        text.chars().map(String::from).collect::<Vec<_>>()
    } else {
        text.split(delimiter).map(str::to_owned).collect()
    };
    serde_json::Value::Array(pieces.into_iter().map(serde_json::Value::String).collect())
        .to_string()
}

/// Formats a float as the shortest JSON number text that parses back to the
/// same `f64`. Non-finite values mirror SQLite's own `json()` spelling so the
/// transport stays parseable.
fn json_real_text(value: f64) -> String {
    match serde_json::Number::from_f64(value) {
        Some(number) => number.to_string(),
        None if value.is_nan() => "null".into(),
        None if value.is_sign_negative() => "-9.0e999".into(),
        None => "9.0e999".into(),
    }
}
