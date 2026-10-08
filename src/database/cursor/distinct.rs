//! Hash buckets narrow comparisons; the existing Cypher equivalence remains
//! authoritative, including for collisions and non-reflexive floating values.
use std::{
    collections::HashMap,
    hash::{BuildHasher, Hash, Hasher},
};

use super::super::{ExecutionContext, cypher_rows_equivalent};
use crate::{CypherValue, Result};

#[derive(Default)]
pub(super) struct SeenRows {
    buckets: HashMap<u64, Vec<Vec<CypherValue>>>,
}

impl SeenRows {
    pub(super) fn insert(
        &mut self,
        row: &[CypherValue],
        context: &ExecutionContext,
    ) -> Result<bool> {
        let mut hasher = self.buckets.hasher().build_hasher();
        hash_values(row, &mut hasher, context)?;
        let bucket = self.buckets.entry(hasher.finish()).or_default();
        for previous in bucket.iter() {
            context.check()?;
            if cypher_rows_equivalent(previous, row) {
                return Ok(false);
            }
        }
        context.check()?;
        bucket.push(row.to_vec());
        Ok(true)
    }
}

fn hash_values(
    values: &[CypherValue],
    hasher: &mut impl Hasher,
    context: &ExecutionContext,
) -> Result<()> {
    values.len().hash(hasher);
    for value in values {
        hash_value(value, hasher, context)?;
    }
    Ok(())
}

/// Equal values must hash alike; unequal values may share a bucket. Numeric
/// families share a tag, and graph values use ordered identities, not records.
fn hash_value(
    value: &CypherValue,
    hasher: &mut impl Hasher,
    context: &ExecutionContext,
) -> Result<()> {
    context.check()?;
    match value {
        CypherValue::Null => 0_u8.hash(hasher),
        CypherValue::Boolean(value) => {
            1_u8.hash(hasher);
            value.hash(hasher);
        }
        CypherValue::Integer(value) => {
            2_u8.hash(hasher);
            value.hash(hasher);
        }
        CypherValue::Float(value) => {
            // The checked integer conversion also distinguishes i64::MAX
            // from its rounded f64 value (2^63), and canonicalizes signed zero.
            let integer = (value.is_finite() && value.fract() == 0.0)
                .then_some(*value as i128)
                .and_then(|value| i64::try_from(value).ok());
            if let Some(integer) = integer {
                2_u8.hash(hasher);
                integer.hash(hasher);
            } else {
                3_u8.hash(hasher);
                value.to_bits().hash(hasher);
            }
        }
        CypherValue::String(value) => {
            4_u8.hash(hasher);
            value.hash(hasher);
        }
        CypherValue::List(values) => {
            5_u8.hash(hasher);
            hash_values(values, hasher, context)?;
        }
        CypherValue::Map(values) => {
            6_u8.hash(hasher);
            values.len().hash(hasher);
            // Cypher maps use BTreeMap, so iteration is independent of insertion order.
            for (key, value) in values {
                key.hash(hasher);
                hash_value(value, hasher, context)?;
            }
        }
        CypherValue::Node(node) => {
            7_u8.hash(hasher);
            node.element_id.hash(hasher);
        }
        CypherValue::Relationship(edge) => {
            8_u8.hash(hasher);
            edge.element_id.hash(hasher);
        }
        CypherValue::Path(path) => {
            9_u8.hash(hasher);
            path.nodes.len().hash(hasher);
            for node in &path.nodes {
                context.check()?;
                node.element_id.hash(hasher);
            }
            path.relationships.len().hash(hasher);
            for edge in &path.relationships {
                context.check()?;
                edge.element_id.hash(hasher);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeValue, PathValue, QueryOptions, RelationshipValue};
    use serde_json::json;

    #[test]
    fn fingerprint_buckets_agree_with_equality_for_numeric_and_graph_values() {
        let context = ExecutionContext::new(&QueryOptions::default());
        let mut values = vec![
            CypherValue::Null,
            CypherValue::Boolean(false),
            CypherValue::Boolean(true),
            CypherValue::String("1".into()),
            CypherValue::String("\0é".into()),
        ];
        for number in [
            i64::MIN,
            -9007199254740993,
            -1,
            0,
            1,
            9007199254740992,
            9007199254740993,
            i64::MAX,
        ] {
            values.push(CypherValue::Integer(number));
            values.push(CypherValue::Float(number as f64));
        }
        for number in [
            -0.0,
            0.0,
            0.5,
            -0.5,
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            values.push(CypherValue::Float(number));
        }
        let node = NodeValue {
            element_id: "a".into(),
            labels: vec![],
            properties: Default::default(),
        };
        let mut changed = node.clone();
        changed.labels.push("Different".into());
        changed
            .properties
            .insert("value".into(), CypherValue::Integer(9));
        let edge = RelationshipValue {
            element_id: "a".into(),
            relationship_type: "R".into(),
            start_node_element_id: "a".into(),
            end_node_element_id: "a".into(),
            properties: Default::default(),
        };
        values.extend([
            CypherValue::Node(node.clone()),
            CypherValue::Node(changed.clone()),
            CypherValue::Relationship(edge.clone()),
            CypherValue::Path(PathValue {
                nodes: vec![node.clone(), node],
                relationships: vec![edge.clone()],
            }),
            CypherValue::Path(PathValue {
                nodes: vec![changed.clone(), changed],
                relationships: vec![edge],
            }),
            CypherValue::try_from(json!({"id":"a"})).unwrap(),
        ]);
        let mut rows: Vec<_> = values.iter().cloned().map(|v| vec![v]).collect();
        for value in values {
            rows.push(vec![CypherValue::List(vec![
                value.clone(),
                CypherValue::Null,
            ])]);
            rows.push(vec![CypherValue::Map([("v".into(), value.clone())].into())]);
            rows.push(vec![value, CypherValue::Null]);
        }
        let mut seen = SeenRows::default();
        let mut oracle: Vec<Vec<CypherValue>> = Vec::new();
        for row in rows.iter().chain(rows.iter().rev()) {
            let new = !oracle.iter().any(|old| cypher_rows_equivalent(old, row));
            assert_eq!(seen.insert(row, &context).unwrap(), new, "{row:?}");
            if new {
                oracle.push(row.clone());
            }
        }
    }

    #[test]
    fn a_fingerprint_collision_still_compares_the_complete_row() {
        let context = ExecutionContext::new(&QueryOptions::default());
        let mut seen = SeenRows::default();
        let row = vec![CypherValue::Integer(2)];
        let mut hasher = seen.buckets.hasher().build_hasher();
        hash_values(&row, &mut hasher, &context).unwrap();
        // Inject a different value into the candidate bucket to force a collision.
        seen.buckets
            .insert(hasher.finish(), vec![vec![CypherValue::Integer(1)]]);
        assert!(seen.insert(&row, &context).unwrap());
        assert!(!seen.insert(&[CypherValue::Float(2.0)], &context).unwrap());
    }
}
