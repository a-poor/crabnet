//! Private tagged transport for graph-bearing collections. Ordinary JSON is
//! explicitly wrapped, so user properties cannot impersonate graph records.
use std::collections::BTreeMap;

use rusqlite::{Connection, functions::FunctionFlags};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    EntityValue, comparison, decode_entity, encode_entity, json_from_cypher, semantic_json_key,
};
use crate::{CypherValue, Error, PathValue, Result, planner::EntityKind};

#[derive(Clone)]
pub(super) enum NestedValue {
    Json(CypherValue),
    Entity(EntityValue),
    List(Vec<Self>),
    Map(BTreeMap<String, Self>),
    Path(Vec<EntityValue>, Vec<EntityValue>),
}

impl NestedValue {
    pub(super) fn decode(encoded: &str) -> Result<Self> {
        let value = parse_json(encoded)?;
        Self::from_tagged(&value, 0)
    }

    fn from_tagged(value: &Value, depth: usize) -> Result<Self> {
        if depth > 100 {
            return Err(Error::Type("nested collection depth exceeds 100".into()));
        }
        if value.is_null() {
            return Ok(Self::Json(CypherValue::Null));
        }
        let (tag, value) = parts(value)?;
        Ok(match tag {
            "v" => Self::Json(CypherValue::try_from(value.clone())?),
            "n" => Self::Entity(decode_entity(&value.to_string(), EntityKind::Node)?),
            "r" => Self::Entity(decode_entity(&value.to_string(), EntityKind::Relationship)?),
            "l" => Self::List(
                array(value)?
                    .iter()
                    .map(|v| Self::from_tagged(v, depth + 1))
                    .collect::<Result<_>>()?,
            ),
            "m" => Self::Map(
                object(value)?
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), Self::from_tagged(v, depth + 1)?)))
                    .collect::<Result<_>>()?,
            ),
            "p" => {
                let nodes = array(&value["nodes"])?
                    .iter()
                    .map(|v| decode_entity(&v.to_string(), EntityKind::Node))
                    .collect::<Result<Vec<_>>>()?;
                let relationships = array(&value["relationships"])?
                    .iter()
                    .map(|v| decode_entity(&v.to_string(), EntityKind::Relationship))
                    .collect::<Result<Vec<_>>>()?;
                if nodes.len() != relationships.len() + 1 {
                    return Err(Error::Data("malformed nested path".into()));
                }
                Self::Path(nodes, relationships)
            }
            _ => return Err(Error::Data("unknown nested value tag".into())),
        })
    }

    pub(super) fn value(&self) -> CypherValue {
        match self {
            Self::Json(value) => value.clone(),
            Self::Entity(value) => value.to_cypher_value(),
            Self::List(items) => CypherValue::List(items.iter().map(Self::value).collect()),
            Self::Map(entries) => CypherValue::Map(
                entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.value()))
                    .collect(),
            ),
            Self::Path(nodes, relationships) => CypherValue::Path(PathValue {
                nodes: nodes
                    .iter()
                    .map(|n| match n {
                        EntityValue::Node(n) => n.clone(),
                        _ => unreachable!(),
                    })
                    .collect(),
                relationships: relationships
                    .iter()
                    .map(|r| match r {
                        EntityValue::Relationship(r) => r.clone(),
                        _ => unreachable!(),
                    })
                    .collect(),
            }),
        }
    }

    pub(super) fn encode(&self) -> Value {
        match self {
            Self::Json(value) => tagged("v", json_from_cypher(value)),
            Self::Entity(value) => tagged(
                if value.kind() == EntityKind::Node {
                    "n"
                } else {
                    "r"
                },
                serde_json::from_str(&encode_entity(value)).expect("entity JSON"),
            ),
            Self::List(items) => {
                tagged("l", Value::Array(items.iter().map(Self::encode).collect()))
            }
            Self::Map(entries) => tagged(
                "m",
                Value::Object(
                    entries
                        .iter()
                        .map(|(k, v)| (k.clone(), v.encode()))
                        .collect(),
                ),
            ),
            Self::Path(nodes, relationships) => tagged(
                "p",
                json!({
                    "nodes": nodes.iter().map(|v| serde_json::from_str::<Value>(&encode_entity(v)).expect("entity JSON")).collect::<Vec<_>>(),
                    "relationships": relationships.iter().map(|v| serde_json::from_str::<Value>(&encode_entity(v)).expect("entity JSON")).collect::<Vec<_>>()
                }),
            ),
        }
    }

    pub(super) fn entities_mut(&mut self) -> Vec<&mut EntityValue> {
        match self {
            Self::Json(_) => Vec::new(),
            Self::Entity(entity) => vec![entity],
            Self::List(items) => items.iter_mut().flat_map(Self::entities_mut).collect(),
            Self::Map(entries) => entries.values_mut().flat_map(Self::entities_mut).collect(),
            Self::Path(nodes, relationships) => nodes.iter_mut().chain(relationships).collect(),
        }
    }
}

fn sql_error(error: Error) -> rusqlite::Error {
    match error {
        Error::Type(message) => rusqlite::Error::UserFunctionError(Box::new(
            std::io::Error::other(format!("{}{message}", crate::error::TYPE_ERROR_TAG)),
        )),
        error => rusqlite::Error::UserFunctionError(Box::new(error)),
    }
}

/// Tags add transport containers beyond the public 100-container value
/// limit. Bound the byte-level syntax before allowing serde to parse it.
/// This also fails closed for corrupt database records.
pub(super) fn parse_json(encoded: &str) -> Result<Value> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in encoded.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > 320 {
                        return Err(Error::Type(
                            "nested value transport exceeds the supported depth".into(),
                        ));
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    let mut parser = serde_json::Deserializer::from_str(encoded);
    parser.disable_recursion_limit();
    let value = Value::deserialize(&mut parser)
        .map_err(|e| Error::Data(format!("malformed nested JSON: {e}")))?;
    parser
        .end()
        .map_err(|e| Error::Data(format!("malformed nested JSON: {e}")))?;
    Ok(value)
}

fn tagged(tag: &str, value: Value) -> Value {
    if value.is_null() {
        Value::Null
    } else {
        json!([tag, value])
    }
}

fn validate_depth(value: &Value) -> Result<()> {
    let mut work = vec![(value, 0, true)];
    while let Some((value, depth, tagged)) = work.pop() {
        if value.is_null() {
            continue;
        }
        if tagged {
            let (tag, payload) = parts(value)?;
            match tag {
                "v" => work.push((payload, depth, false)),
                "l" | "m" => {
                    if depth >= 100 {
                        return Err(Error::Type("nested collection depth exceeds 100".into()));
                    }
                    if tag == "l" {
                        work.extend(array(payload)?.iter().map(|v| (v, depth + 1, true)));
                    } else {
                        work.extend(object(payload)?.values().map(|v| (v, depth + 1, true)));
                    }
                }
                _ => {}
            }
        } else {
            let children: Box<dyn Iterator<Item = &Value>> = match value {
                Value::Array(items) => Box::new(items.iter()),
                Value::Object(items) => Box::new(items.values()),
                _ => continue,
            };
            if depth >= 100 {
                return Err(Error::Type("nested collection depth exceeds 100".into()));
            }
            work.extend(children.map(|v| (v, depth + 1, false)));
        }
    }
    Ok(())
}

fn parts(value: &Value) -> Result<(&str, &Value)> {
    let values = array(value)?;
    if values.len() != 2 {
        return Err(Error::Data("malformed nested value".into()));
    }
    Ok((
        values[0]
            .as_str()
            .ok_or_else(|| Error::Data("missing nested value tag".into()))?,
        &values[1],
    ))
}

fn array(value: &Value) -> Result<&Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| Error::Type("expected a list".into()))
}

fn object(value: &Value) -> Result<&serde_json::Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| Error::Type("expected a map".into()))
}

/// Normalize both the wrapped-JSON and graph-collection representations to
/// the same recursively typed identity tree before equality or grouping.
fn identity(value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let (tag, payload) = parts(value)?;
    Ok(match tag {
        "v" => json_identity(payload),
        "n" | "r" => tagged(tag, payload["id"].clone()),
        "l" => tagged(
            "l",
            Value::Array(
                array(payload)?
                    .iter()
                    .map(identity)
                    .collect::<Result<_>>()?,
            ),
        ),
        "m" => tagged(
            "m",
            Value::Object(
                object(payload)?
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), identity(v)?)))
                    .collect::<Result<_>>()?,
            ),
        ),
        "p" => tagged(
            "p",
            json!([
                array(&payload["nodes"])?
                    .iter()
                    .map(|n| n["id"].clone())
                    .collect::<Vec<_>>(),
                array(&payload["relationships"])?
                    .iter()
                    .map(|r| r["id"].clone())
                    .collect::<Vec<_>>()
            ]),
        ),
        _ => return Err(Error::Data("unknown nested value tag".into())),
    })
}

fn json_identity(value: &Value) -> Value {
    match value {
        Value::Array(items) => tagged("l", Value::Array(items.iter().map(json_identity).collect())),
        Value::Object(entries) => tagged(
            "m",
            Value::Object(
                entries
                    .iter()
                    .map(|(k, v)| (k.clone(), json_identity(v)))
                    .collect(),
            ),
        ),
        _ => tagged("v", value.clone()),
    }
}

fn to_json(value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let (tag, payload) = parts(value)?;
    Ok(match tag {
        "v" => payload.clone(),
        "l" => Value::Array(array(payload)?.iter().map(to_json).collect::<Result<_>>()?),
        "m" => Value::Object(
            object(payload)?
                .iter()
                .map(|(k, v)| Ok((k.clone(), to_json(v)?)))
                .collect::<Result<_>>()?,
        ),
        _ => {
            return Err(Error::Type(
                "graph values cannot be used as property values".into(),
            ));
        }
    })
}

fn elements(value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let (tag, payload) = parts(value)?;
    match (tag, payload) {
        ("l", _) => Ok(payload.clone()),
        ("v", Value::Array(items)) => Ok(Value::Array(
            items.iter().map(|v| tagged("v", v.clone())).collect(),
        )),
        _ => Ok(Value::Null),
    }
}

/// Use the ordinary `+` rules for scalar members selected from mixed
/// collections, and preserve tags when either operand is a list.
fn add(left: &Value, right: &Value) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let left_items = elements(left)?;
    let right_items = elements(right)?;
    if left_items.is_array() || right_items.is_array() {
        let mut items = match left_items {
            Value::Array(items) => items,
            _ => vec![left.clone()],
        };
        match right_items {
            Value::Array(right) => items.extend(right),
            _ => items.push(right.clone()),
        }
        return Ok(tagged("l", Value::Array(items)));
    }
    let (left_tag, left) = parts(left)?;
    let (right_tag, right) = parts(right)?;
    if left_tag != "v" || right_tag != "v" {
        return Err(Error::Type(
            "adding graph values requires a list operand".into(),
        ));
    }
    let result = match (left, right) {
        (Value::String(left), Value::String(right)) => Value::String(format!("{left}{right}")),
        (Value::Number(left), Value::Number(right)) => {
            if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64())
                && let Some(sum) = left.checked_add(right)
            {
                json!(sum)
            } else {
                json!(left.as_f64().unwrap() + right.as_f64().unwrap())
            }
        }
        _ => Value::Null,
    };
    Ok(tagged("v", result))
}

fn get(value: &Value, index: &Value, property: bool) -> Result<Value> {
    if value.is_null() || index.is_null() {
        return Ok(Value::Null);
    }
    let (tag, payload) = parts(value)?;
    match (tag, payload, index) {
        ("n" | "r", _, Value::String(key)) => Ok(tagged("v", payload["props"].get(key).cloned().unwrap_or(Value::Null))),
        ("n" | "r", _, _) if !property => Ok(Value::Null),
        ("m", _, Value::String(key)) => Ok(payload.get(key).cloned().unwrap_or(Value::Null)),
        ("v", Value::Object(map), Value::String(key)) => Ok(tagged("v", map.get(key).cloned().unwrap_or(Value::Null))),
        ("l" | "v", Value::Array(items), Value::Number(index)) if !property => {
            let index = index.as_i64().ok_or_else(|| Error::Type("list index must be an integer".into()))?;
            let position = if index < 0 { items.len() as i128 + index as i128 } else { index as i128 };
            let selected = usize::try_from(position).ok().and_then(|i| items.get(i)).cloned().unwrap_or(Value::Null);
            Ok(if tag == "v" { tagged("v", selected) } else { selected })
        }
        _ => Err(Error::Type("index/property access requires a list, map, node, or relationship and a compatible key".into())),
    }
}

fn evaluate(op: &str, args: &[Value]) -> Result<Value> {
    let value = &args[0];
    match op {
        "json" => return Ok(tagged("v", value.clone())),
        "node" => return Ok(tagged("n", value.clone())),
        "relationship" => return Ok(tagged("r", value.clone())),
        "list" => return Ok(tagged("l", value.clone())),
        "map" => return Ok(tagged("m", value.clone())),
        "path" => return Ok(tagged("p", value.clone())),
        "nodes" | "relationships" => {
            return Ok(if value.is_null() {
                Value::Null
            } else {
                tagged(
                    "l",
                    Value::Array(
                        array(value)?
                            .iter()
                            .map(|v| tagged(if op == "nodes" { "n" } else { "r" }, v.clone()))
                            .collect(),
                    ),
                )
            });
        }
        "equal" => {
            return Ok(comparison::equals(&identity(value)?, &identity(&args[1])?)
                .map_or(Value::Null, Value::Bool));
        }
        "add" => return add(value, &args[1]),
        "get" | "property" => return get(value, &args[1], op == "property"),
        "to_json" => return to_json(value),
        "elements" => return elements(value),
        "in" => {
            if args[1].is_null() {
                return Ok(Value::Null);
            }
            let list = elements(&args[1])?;
            let mut unknown = false;
            for member in array(&list)? {
                match comparison::equals(&identity(value)?, &identity(member)?) {
                    Some(true) => return Ok(Value::Bool(true)),
                    None => unknown = true,
                    _ => {}
                }
            }
            return Ok(if unknown {
                Value::Null
            } else {
                Value::Bool(false)
            });
        }
        _ => {}
    }
    if value.is_null() {
        return Ok(Value::Null);
    }
    let (tag, payload) = parts(value)?;
    Ok(match op {
        "payload" => payload.clone(),
        "entity_list" => Value::Array(
            array(&elements(value)?)?
                .iter()
                .map(|v| {
                    if v.is_null() {
                        Ok(Value::Null)
                    } else {
                        parts(v).map(|(_, p)| p.clone())
                    }
                })
                .collect::<Result<_>>()?,
        ),
        "size" => match (tag, payload) {
            ("l" | "v", Value::Array(items)) => json!(items.len()),
            ("v", Value::String(s)) => json!(s.chars().count()),
            _ => return Err(Error::Type("size() requires a list or string".into())),
        },
        "list_input" => match (tag, payload) {
            ("l" | "v", Value::Array(_)) => value.clone(),
            _ => return Err(Error::Type("list operation requires a list".into())),
        },
        "reverse" => match (tag, payload) {
            ("l" | "v", Value::Array(items)) => {
                tagged(tag, Value::Array(items.iter().rev().cloned().collect()))
            }
            ("v", Value::String(s)) => tagged("v", Value::String(s.chars().rev().collect())),
            _ => return Err(Error::Type("reverse() requires a list or string".into())),
        },
        "element_id" if tag == "n" || tag == "r" => payload["id"].clone(),
        "labels" if tag == "n" => payload["labels"].clone(),
        "relationship_type" if tag == "r" => payload["type"].clone(),
        "has_label" if tag == "n" => Value::Bool(array(&payload["labels"])?.contains(&args[1])),
        "has_label" if tag == "r" => Value::Bool(payload["type"] == args[1]),
        "keys" | "properties"
            if tag == "n" || tag == "r" || tag == "m" || (tag == "v" && payload.is_object()) =>
        {
            let fields = if tag == "n" || tag == "r" {
                &payload["props"]
            } else {
                payload
            };
            if op == "keys" {
                json!(
                    object(fields)?
                        .iter()
                        .filter(|(_, v)| tag == "m" || tag == "v" || !v.is_null())
                        .map(|(k, _)| k)
                        .collect::<Vec<_>>()
                )
            } else if tag == "m" {
                value.clone()
            } else if tag == "n" || tag == "r" {
                tagged(
                    "v",
                    Value::Object(
                        object(fields)?
                            .iter()
                            .filter(|(_, v)| !v.is_null())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    ),
                )
            } else {
                tagged("v", fields.clone())
            }
        }
        "length" if tag == "p" => json!(array(&payload["relationships"])?.len()),
        "path_nodes" | "path_relationships" if tag == "p" => {
            let (key, tag) = if op == "path_nodes" {
                ("nodes", "n")
            } else {
                ("relationships", "r")
            };
            tagged(
                "l",
                Value::Array(
                    array(&payload[key])?
                        .iter()
                        .map(|v| tagged(tag, v.clone()))
                        .collect(),
                ),
            )
        }
        _ => {
            return Err(Error::Type(format!(
                "{op}() received an incompatible value"
            )));
        }
    })
}

pub(super) fn register(connection: &Connection) -> Result<()> {
    connection.create_scalar_function(
        "crabnet_nested",
        -1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let op: String = context.get(0)?;
            let args = (1..context.len())
                .map(|i| super::functions::json_argument(context, i))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            evaluate(&op, &args)
                .and_then(|v| {
                    if matches!(
                        op.as_str(),
                        "json"
                            | "node"
                            | "relationship"
                            | "list"
                            | "map"
                            | "path"
                            | "nodes"
                            | "relationships"
                            | "add"
                            | "reverse"
                            | "get"
                            | "property"
                            | "properties"
                            | "path_nodes"
                            | "path_relationships"
                    ) {
                        validate_depth(&v)?;
                    }
                    Ok(v.to_string())
                })
                .map_err(sql_error)
        },
    )?;
    connection.create_scalar_function(
        "crabnet_nested_key",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let value = super::functions::json_argument(context, 0)?;
            identity(&value)
                .map(|value| semantic_json_key(&value))
                .map_err(sql_error)
        },
    )?;
    Ok(())
}
