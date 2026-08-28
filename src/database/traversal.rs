use std::collections::BTreeMap;

use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};

use crate::{
    Error, Result,
    planner::{ResolvedTrailExpansion, TrailDirection},
};

// This limits explored work and fails closed; it is deliberately not presented
// as a bound on the fully materialized graph input or JSON payload.
pub(super) const DEFAULT_TRAIL_STATE_LIMIT: usize = 100_000;

pub(super) struct TrailBudget {
    limit: usize,
    used: usize,
}

impl Default for TrailBudget {
    fn default() -> Self {
        Self::new(DEFAULT_TRAIL_STATE_LIMIT)
    }
}

impl TrailBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    fn admit(&mut self, traversal: usize) -> Result<()> {
        if self.used >= self.limit {
            return Err(Error::ResourceLimit(format!(
                "trail expansion ~{traversal} exceeded the {}-state budget",
                self.limit
            )));
        }
        self.used += 1;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct StoredEdge {
    id: String,
    from: String,
    to: String,
}

#[derive(Clone, Debug)]
struct TrailStep {
    edge_id: String,
    next_node: String,
}

#[derive(Clone, Debug)]
struct TrailState {
    end_node: String,
    edge_ids: Vec<String>,
    node_ids: Vec<String>,
}

// The consuming SQLite fragment reads this ordered trail relation through
// json_each(), keeping endpoint joins and downstream row operators in SQL.
pub(super) fn materialize(
    connection: &Connection,
    spec: &ResolvedTrailExpansion,
    budget: &mut TrailBudget,
) -> Result<SqlValue> {
    if spec.maximum.is_some_and(|maximum| spec.minimum > maximum) {
        return Ok(SqlValue::Text("[]".into()));
    }

    let edges = load_edges(connection, spec)?;
    let nodes = load_nodes(connection)?;
    let rows = enumerate_trails(
        &nodes,
        &edges,
        spec.direction,
        spec.minimum,
        spec.maximum,
        spec.traversal,
        budget,
    )?;
    Ok(SqlValue::Text(Value::Array(rows).to_string()))
}

fn load_nodes(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT id FROM nodes ORDER BY id")?;
    debug_assert!(statement.readonly());
    statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn load_edges(connection: &Connection, spec: &ResolvedTrailExpansion) -> Result<Vec<StoredEdge>> {
    let mut statement = connection.prepare(&spec.edge_sql)?;
    debug_assert!(statement.readonly());
    statement
        .query_map(params_from_iter(spec.edge_bindings.iter()), |row| {
            Ok(StoredEdge {
                id: row.get(0)?,
                from: row.get(1)?,
                to: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn enumerate_trails(
    nodes: &[String],
    edges: &[StoredEdge],
    direction: TrailDirection,
    minimum: usize,
    maximum: Option<usize>,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<Vec<Value>> {
    let effective_maximum = maximum.unwrap_or(edges.len()).min(edges.len());
    if minimum > effective_maximum {
        return Ok(Vec::new());
    }

    let adjacency = adjacency(edges, direction);
    let mut rows = Vec::new();
    for start in nodes {
        let seed = TrailState {
            end_node: start.clone(),
            edge_ids: Vec::new(),
            node_ids: vec![start.clone()],
        };
        if minimum == 0 {
            budget.admit(traversal)?;
            rows.push(encoded_trail(start, &seed));
        }

        let mut pending = vec![seed];
        while let Some(state) = pending.pop() {
            if state.edge_ids.len() >= effective_maximum {
                continue;
            }
            let Some(steps) = adjacency.get(&state.end_node) else {
                continue;
            };
            for step in steps.iter().rev() {
                if state.edge_ids.contains(&step.edge_id) {
                    continue;
                }
                budget.admit(traversal)?;
                let mut next = state.clone();
                next.end_node.clone_from(&step.next_node);
                next.edge_ids.push(step.edge_id.clone());
                next.node_ids.push(step.next_node.clone());
                if next.edge_ids.len() >= minimum {
                    rows.push(encoded_trail(start, &next));
                }
                pending.push(next);
            }
        }
    }
    Ok(rows)
}

fn adjacency(edges: &[StoredEdge], direction: TrailDirection) -> BTreeMap<String, Vec<TrailStep>> {
    let mut adjacency = BTreeMap::<String, Vec<TrailStep>>::new();
    for edge in edges {
        match direction {
            TrailDirection::Right => add_step(&mut adjacency, &edge.from, &edge.to, &edge.id),
            TrailDirection::Left => add_step(&mut adjacency, &edge.to, &edge.from, &edge.id),
            TrailDirection::Undirected => {
                add_step(&mut adjacency, &edge.from, &edge.to, &edge.id);
                if edge.from != edge.to {
                    add_step(&mut adjacency, &edge.to, &edge.from, &edge.id);
                }
            }
        }
    }
    adjacency
}

fn add_step(adjacency: &mut BTreeMap<String, Vec<TrailStep>>, from: &str, to: &str, edge_id: &str) {
    adjacency
        .entry(from.to_owned())
        .or_default()
        .push(TrailStep {
            edge_id: edge_id.to_owned(),
            next_node: to.to_owned(),
        });
}

fn encoded_trail(start: &str, state: &TrailState) -> Value {
    json!({
        "start_id": start,
        "end_id": state.end_node,
        "depth": state.edge_ids.len(),
        "used_edges": state.edge_ids,
        "node_ids": state.node_ids,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(id: &str, from: &str, to: &str) -> StoredEdge {
        StoredEdge {
            id: id.into(),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn undirected_self_loop_is_emitted_once_per_depth() {
        let nodes = vec!["a".into()];
        let edges = vec![edge("loop", "a", "a")];
        let mut budget = TrailBudget::new(10);
        let rows = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Undirected,
            0,
            None,
            0,
            &mut budget,
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["depth"], 0);
        assert_eq!(rows[1]["depth"], 1);
    }

    #[test]
    fn trail_state_budget_fails_instead_of_truncating() {
        let nodes = vec!["a".into(), "b".into(), "c".into()];
        let edges = vec![edge("ab", "a", "b"), edge("bc", "b", "c")];
        let mut budget = TrailBudget::new(2);
        let error = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Right,
            1,
            None,
            7,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(error, Error::ResourceLimit(message) if message.contains("~7")));
    }

    #[test]
    fn reversed_bounds_are_empty_without_spending_budget() {
        let nodes = vec!["a".into(), "b".into()];
        let edges = vec![edge("ab", "a", "b")];
        let mut budget = TrailBudget::new(0);
        let rows = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Right,
            34,
            Some(33),
            0,
            &mut budget,
        )
        .unwrap();

        assert!(rows.is_empty());
    }
}
