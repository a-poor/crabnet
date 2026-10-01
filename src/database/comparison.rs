//! Cypher equality, comparison, and orderability over the JSON encoding of
//! scalar values. Generated SQL compares primitives natively and calls these
//! through SQL functions for lists and maps, whose rules (element-wise null
//! propagation, integer and float equality at any depth, lexicographic
//! ordering) SQLite's JSON text comparison cannot express.

use std::cmp::Ordering;

use serde_json::{Map, Number, Value};

/// Cypher `=`: `None` is null. A null on either side, or a null met while
/// comparing elements that are otherwise equal, makes the result null; any
/// definite difference makes it false.
pub(super) fn equals(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Bool(left), Value::Bool(right)) => Some(left == right),
        (Value::Number(left), Value::Number(right)) => {
            Some(compare_numbers(left, right) == Some(Ordering::Equal))
        }
        (Value::String(left), Value::String(right)) => Some(left == right),
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Some(false);
            }
            all_equal(left.iter().zip(right))
        }
        (Value::Object(left), Value::Object(right)) => {
            if left.len() != right.len() || left.keys().any(|key| !right.contains_key(key)) {
                return Some(false);
            }
            all_equal(left.iter().map(|(key, value)| (value, &right[key])))
        }
        _ => Some(false),
    }
}

fn all_equal<'a>(pairs: impl Iterator<Item = (&'a Value, &'a Value)>) -> Option<bool> {
    let mut unknown = false;
    for (left, right) in pairs {
        match equals(left, right) {
            Some(false) => return Some(false),
            None => unknown = true,
            Some(true) => {}
        }
    }
    (!unknown).then_some(true)
}

/// Cypher comparability for `<`, `<=`, `>`, and `>=`: `None` is null.
/// Numbers, strings, and booleans compare within their own family; lists
/// compare lexicographically, a shorter prefix first, and become null at the
/// first element pair that is not comparable. Everything else is null.
pub(super) fn compare(left: &Value, right: &Value) -> Option<Ordering> {
    match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::Number(left), Value::Number(right)) => compare_numbers(left, right),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Array(left), Value::Array(right)) => {
            for (left, right) in left.iter().zip(right) {
                match compare(left, right)? {
                    Ordering::Equal => {}
                    decided => return Some(decided),
                }
            }
            Some(left.len().cmp(&right.len()))
        }
        _ => None,
    }
}

fn compare_numbers(left: &Number, right: &Number) -> Option<Ordering> {
    match (left.as_i64(), right.as_i64()) {
        (Some(left), Some(right)) => Some(left.cmp(&right)),
        (Some(left), None) => compare_integer_float(left, right.as_f64()?),
        (None, Some(right)) => compare_integer_float(right, left.as_f64()?).map(Ordering::reverse),
        (None, None) => left.as_f64()?.partial_cmp(&right.as_f64()?),
    }
}

/// Compares an integer with a float exactly, even where the integer has no
/// exact `f64` representation.
fn compare_integer_float(integer: i64, float: f64) -> Option<Ordering> {
    match (integer as f64).partial_cmp(&float)? {
        // Equal after rounding, so the float is integral and within a step
        // of the i64 range; i128 holds both exactly.
        Ordering::Equal => Some(i128::from(integer).cmp(&(float as i128))),
        decided => Some(decided),
    }
}

// Orderability ranks, lowest first. Graph entities and paths would sit
// between maps and strings; they never reach the scalar sort key.
const LIST_END: u8 = 0x00;
const MAP_TAG: u8 = 0x10;
const LIST_TAG: u8 = 0x30;
const STRING_TAG: u8 = 0x50;
const BOOLEAN_TAG: u8 = 0x60;
const NUMBER_TAG: u8 = 0x70;
const NULL_TAG: u8 = 0x80;

/// A byte string whose lexicographic order is Cypher's global sort order
/// (orderability): maps, lists, strings, booleans, numbers, then null.
/// Lists order element by element with a shorter prefix first, so a null
/// element sorts after every value in its position; integers and floats
/// interleave by exact numeric value. Values that are equal under `=` share
/// a key.
pub(super) fn order_key(value: &Value) -> Vec<u8> {
    let mut key = Vec::new();
    append_order_key(&mut key, value);
    key
}

fn append_order_key(key: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => key.push(NULL_TAG),
        Value::Bool(value) => {
            key.push(BOOLEAN_TAG);
            key.push(u8::from(*value));
        }
        Value::Number(number) => {
            key.push(NUMBER_TAG);
            append_number_key(key, number);
        }
        Value::String(value) => {
            key.push(STRING_TAG);
            append_string_key(key, value);
        }
        Value::Array(values) => {
            key.push(LIST_TAG);
            for value in values {
                append_order_key(key, value);
            }
            key.push(LIST_END);
        }
        Value::Object(entries) => {
            key.push(MAP_TAG);
            append_map_key(key, entries);
        }
    }
}

fn append_map_key(key: &mut Vec<u8>, entries: &Map<String, Value>) {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_unstable_by_key(|(name, _)| *name);
    for (name, value) in entries {
        // A tag before each entry keeps "no more entries" (LIST_END) below
        // any entry.
        key.push(STRING_TAG);
        append_string_key(key, name);
        append_order_key(key, value);
    }
    key.push(LIST_END);
}

/// UTF-8 bytes order strings by code point. Escaping NUL as `00 FF` and
/// terminating with `00 00` keeps a string's key from running into the key
/// of whatever follows it.
fn append_string_key(key: &mut Vec<u8>, value: &str) {
    for byte in value.bytes() {
        key.push(byte);
        if byte == 0 {
            key.push(0xFF);
        }
    }
    key.extend_from_slice(&[0, 0]);
}

/// The value rounded to `f64`, as bits that sort like the float, then the
/// exact distance from that rounding for integers it cannot hold, so every
/// integer and float lands in exact numeric order.
fn append_number_key(key: &mut Vec<u8>, number: &Number) {
    let (rounded, remainder) = match number.as_i64() {
        Some(integer) => {
            let rounded = integer as f64;
            (rounded, i128::from(integer) - rounded as i128)
        }
        None => (number.as_f64().unwrap_or(f64::NAN), 0),
    };
    // Both zeros are the same number.
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    let bits = rounded.to_bits();
    let sortable = if bits >> 63 == 1 {
        !bits
    } else {
        bits | 1 << 63
    };
    key.extend_from_slice(&sortable.to_be_bytes());
    let remainder = i64::try_from(remainder).expect("an i64 rounds to f64 within 2^11");
    key.extend_from_slice(&((remainder as u64) ^ 1 << 63).to_be_bytes());
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn equality_propagates_nulls_only_when_nothing_differs() {
        assert_eq!(equals(&json!([1, 2]), &json!([1])), Some(false));
        assert_eq!(equals(&json!([null]), &json!([1])), None);
        assert_eq!(equals(&json!([[1], [2]]), &json!([[1], [null]])), None);
        assert_eq!(
            equals(&json!([[1], [2, 3]]), &json!([[1], [null]])),
            Some(false)
        );
        assert_eq!(equals(&json!([1, 2]), &json!([1.0, 2])), Some(true));
        assert_eq!(equals(&json!({"k": null}), &json!({"k": null})), None);
        assert_eq!(
            equals(&json!({"k": null}), &json!({"k": null, "l": null})),
            Some(false)
        );
        assert_eq!(
            equals(&json!({"a": 1, "b": [2]}), &json!({"b": [2.0], "a": 1})),
            Some(true)
        );
        assert_eq!(equals(&json!([true]), &json!([1])), Some(false));
    }

    #[test]
    fn lists_compare_lexicographically_until_an_incomparable_pair() {
        let compare = |left, right| compare(&left, &right);
        assert_eq!(compare(json!([1, 0]), json!([1])), Some(Ordering::Greater));
        assert_eq!(
            compare(json!([1, null]), json!([1])),
            Some(Ordering::Greater)
        );
        assert_eq!(compare(json!([1, 2]), json!([1, null])), None);
        assert_eq!(
            compare(json!([1, 2]), json!([3, null])),
            Some(Ordering::Less)
        );
        assert_eq!(compare(json!(['a']), json!([1])), None);
        assert_eq!(compare(json!({"a": 1}), json!({"a": 1})), None);
    }

    #[test]
    fn integers_and_floats_compare_exactly() {
        let big = 9_007_199_254_740_993_i64; // 2^53 + 1 has no f64
        let float = json!(9_007_199_254_740_992.0);
        assert_eq!(compare(&json!(big), &float), Some(Ordering::Greater));
        assert_eq!(compare(&float, &json!(big)), Some(Ordering::Less));
        assert!(order_key(&json!(big)) > order_key(&float));
        assert!(order_key(&json!(big - 1)) == order_key(&float));
        assert_eq!(order_key(&json!(-0.0)), order_key(&json!(0)));
    }

    #[test]
    fn order_keys_follow_global_orderability() {
        let ascending = [
            json!({}),
            json!({"a": 1}),
            json!({"b": 0}),
            json!([]),
            json!(["a"]),
            json!(["a", 1]),
            json!([1]),
            json!([1, "a"]),
            json!([1, null]),
            json!([null, 1]),
            json!(""),
            json!("a"),
            json!("a\u{0}"),
            json!("ab"),
            json!(false),
            json!(true),
            json!(-1.5),
            json!(-1),
            json!(0),
            json!(0.5),
            json!(1),
            json!(null),
        ];
        for pair in ascending.windows(2) {
            assert!(
                order_key(&pair[0]) < order_key(&pair[1]),
                "{} should sort before {}",
                pair[0],
                pair[1]
            );
        }
    }
}
