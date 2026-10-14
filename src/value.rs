use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::{Error, Result};

/// A value returned by a Cypher query.
///
/// Unlike JSON, this type can distinguish graph entities from ordinary maps.
/// Lists may contain nodes and relationships as well as property values.
/// Lists and maps preserve the types of their nested values.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CypherValue {
    /// An absent or unknown value.
    Null,
    /// A boolean value.
    Boolean(bool),
    /// A signed 64-bit integer.
    Integer(i64),
    /// A binary64 floating-point value. Query results are finite.
    Float(f64),
    /// A UTF-8 string, including any embedded NUL characters.
    String(String),
    /// An ordered list that preserves duplicate and null members.
    List(Vec<Self>),
    /// A map from string keys to recursively typed values.
    Map(BTreeMap<String, Self>),
    /// A node snapshot with identity, labels, and properties.
    Node(NodeValue),
    /// A relationship snapshot with identity and stored endpoints.
    Relationship(RelationshipValue),
    /// An ordered graph path.
    Path(PathValue),
}

/// A node returned by a Cypher query.
///
/// Labels are distinct and appear in insertion order.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NodeValue {
    /// Opaque identity within this graph, preserved by reopen and supported copies.
    pub element_id: String,
    /// Distinct labels in insertion order.
    pub labels: Vec<String>,
    /// The properties captured in this result snapshot.
    pub properties: BTreeMap<String, CypherValue>,
}

/// A relationship returned by a Cypher query.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct RelationshipValue {
    /// Opaque identity within this graph, preserved by reopen and supported copies.
    pub element_id: String,
    /// The relationship type.
    pub relationship_type: String,
    /// The stored start-node identity, independent of traversal direction.
    pub start_node_element_id: String,
    /// The stored end-node identity, independent of traversal direction.
    pub end_node_element_id: String,
    /// The properties captured in this result snapshot.
    pub properties: BTreeMap<String, CypherValue>,
}

/// A path returned by a Cypher query.
///
/// Nodes follow traversal order. Relationships retain their stored start/end
/// direction, which may be opposite the traversal order for incoming or
/// undirected patterns. Values produced by Crabnet always contain exactly one
/// more node than relationship.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PathValue {
    /// Node snapshots in traversal order, including both endpoints.
    pub nodes: Vec<NodeValue>,
    /// Relationship snapshots in traversal order, retaining stored direction.
    pub relationships: Vec<RelationshipValue>,
}

impl CypherValue {
    /// Returns whether this value is [`Self::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Returns the boolean payload, or `None` for another variant.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the integer payload; floats are not coerced.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// Returns the float payload; integers are not coerced.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }

    /// Borrows the string payload, or returns `None` for another variant.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// Borrows the ordered list members, or returns `None` for another variant.
    pub fn as_list(&self) -> Option<&[Self]> {
        match self {
            Self::List(values) => Some(values),
            _ => None,
        }
    }

    /// Borrows the map entries, or returns `None` for another variant.
    pub fn as_map(&self) -> Option<&BTreeMap<String, Self>> {
        match self {
            Self::Map(values) => Some(values),
            _ => None,
        }
    }

    /// Borrows the node snapshot, or returns `None` for another variant.
    pub fn as_node(&self) -> Option<&NodeValue> {
        match self {
            Self::Node(node) => Some(node),
            _ => None,
        }
    }

    /// Borrows the relationship snapshot, or returns `None` for another variant.
    pub fn as_relationship(&self) -> Option<&RelationshipValue> {
        match self {
            Self::Relationship(relationship) => Some(relationship),
            _ => None,
        }
    }

    /// Borrows the path snapshot, or returns `None` for another variant.
    pub fn as_path(&self) -> Option<&PathValue> {
        match self {
            Self::Path(path) => Some(path),
            _ => None,
        }
    }
}

impl TryFrom<JsonValue> for CypherValue {
    type Error = Error;

    fn try_from(value: JsonValue) -> Result<Self> {
        Self::try_from(&value)
    }
}

impl TryFrom<&JsonValue> for CypherValue {
    type Error = Error;

    fn try_from(value: &JsonValue) -> Result<Self> {
        match value {
            JsonValue::Null => Ok(Self::Null),
            JsonValue::Bool(value) => Ok(Self::Boolean(*value)),
            JsonValue::Number(number) => {
                if let Some(value) = number.as_i64() {
                    Ok(Self::Integer(value))
                } else if number.as_u64().is_some() {
                    Err(Error::Semantic(
                        "Cypher integers must fit the signed 64-bit range".into(),
                    ))
                } else {
                    number
                        .as_f64()
                        .map(|value| {
                            // SQLite's JSON scalar extraction canonicalizes
                            // negative zero. Normalize every public transport
                            // so whole entities and scalar projections agree.
                            Self::Float(if value == 0.0 { 0.0 } else { value })
                        })
                        .ok_or_else(|| {
                            Error::Data(
                                "JSON number has no supported numeric representation".into(),
                            )
                        })
                }
            }
            JsonValue::String(value) => Ok(Self::String(value.clone())),
            JsonValue::Array(values) => values
                .iter()
                .map(Self::try_from)
                .collect::<Result<Vec<_>>>()
                .map(Self::List),
            JsonValue::Object(values) => values
                .iter()
                .map(|(key, value)| Ok((key.clone(), Self::try_from(value)?)))
                .collect::<Result<BTreeMap<_, _>>>()
                .map(Self::Map),
        }
    }
}

impl PartialEq<JsonValue> for CypherValue {
    fn eq(&self, other: &JsonValue) -> bool {
        match (self, other) {
            (Self::Null, JsonValue::Null) => true,
            (Self::Boolean(left), JsonValue::Bool(right)) => left == right,
            (Self::Integer(left), JsonValue::Number(right)) => right.as_i64() == Some(*left),
            (Self::Float(left), JsonValue::Number(right)) => {
                right.is_f64() && right.as_f64() == Some(*left)
            }
            (Self::String(left), JsonValue::String(right)) => left == right,
            (Self::List(left), JsonValue::Array(right)) => {
                left.len() == right.len()
                    && left.iter().zip(right).all(|(left, right)| left.eq(right))
            }
            (Self::Map(left), JsonValue::Object(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .all(|(key, left)| right.get(key).is_some_and(|right| left.eq(right)))
            }
            (Self::Path(_), _) => false,
            _ => false,
        }
    }
}

impl PartialEq<CypherValue> for JsonValue {
    fn eq(&self, other: &CypherValue) -> bool {
        other.eq(self)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{CypherValue, NodeValue, PathValue, RelationshipValue};

    #[test]
    fn json_conversion_preserves_every_supported_shape() {
        let json = json!({
            "null": null,
            "boolean": true,
            "integer": -7,
            "float": 1.5,
            "string": "value",
            "list": [1, {"nested": false}],
        });

        let value = CypherValue::try_from(&json).unwrap();
        assert_eq!(value, json);
        assert_eq!(json, value);

        let map = value.as_map().unwrap();
        assert!(map["null"].is_null());
        assert_eq!(map["boolean"].as_bool(), Some(true));
        assert_eq!(map["integer"].as_i64(), Some(-7));
        assert_eq!(map["float"].as_f64(), Some(1.5));
        assert_eq!(map["string"].as_str(), Some("value"));
        assert_eq!(map["list"].as_list().unwrap().len(), 2);
    }

    #[test]
    fn json_conversion_rejects_integers_outside_cyphers_domain() {
        let error = CypherValue::try_from(json!(u64::MAX)).unwrap_err();
        assert!(matches!(error, crate::Error::Semantic(_)));

        let nested = json!({"value": [u64::MAX]});
        assert!(matches!(
            CypherValue::try_from(nested),
            Err(crate::Error::Semantic(_))
        ));
    }

    #[test]
    fn json_conversion_canonicalizes_negative_zero() {
        let CypherValue::Float(value) = CypherValue::try_from(json!(-0.0)).unwrap() else {
            panic!("expected a float")
        };

        assert_eq!(value.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn entity_accessors_preserve_graph_metadata() {
        let node = NodeValue {
            element_id: "node-id".into(),
            labels: vec!["Person".into()],
            properties: BTreeMap::from([("name".into(), CypherValue::String("Ada".into()))]),
        };
        let relationship = RelationshipValue {
            element_id: "relationship-id".into(),
            relationship_type: "KNOWS".into(),
            start_node_element_id: "node-id".into(),
            end_node_element_id: "other-node-id".into(),
            properties: BTreeMap::new(),
        };

        let node_value = CypherValue::Node(node.clone());
        let relationship_value = CypherValue::Relationship(relationship.clone());
        let path = PathValue {
            nodes: vec![node.clone()],
            relationships: Vec::new(),
        };
        let path_value = CypherValue::Path(path.clone());

        assert_eq!(node_value.as_node(), Some(&node));
        assert_eq!(relationship_value.as_relationship(), Some(&relationship));
        assert_eq!(path_value.as_path(), Some(&path));
        assert!(node_value.as_relationship().is_none());
        assert!(relationship_value.as_node().is_none());
        assert!(node_value.as_path().is_none());
    }

    #[test]
    fn graph_entities_never_compare_equal_to_json_maps() {
        let node = CypherValue::Node(NodeValue {
            element_id: "node-id".into(),
            labels: vec![],
            properties: BTreeMap::new(),
        });
        let lookalike = json!({
            "element_id": "node-id",
            "labels": [],
            "properties": {},
        });

        assert_ne!(node, lookalike);
        assert_ne!(lookalike, node);
    }

    #[test]
    fn json_compatibility_equality_preserves_numeric_variants() {
        assert_eq!(CypherValue::Integer(1), json!(1));
        assert_ne!(CypherValue::Integer(1), json!(1.0));
        assert_eq!(CypherValue::Float(1.0), json!(1.0));
        assert_ne!(CypherValue::Float(1.0), json!(1));
    }
}
