use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Write},
};

use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
#[cfg(test)]
use serde_json::{Value, json};

use crate::{
    Error, Result,
    planner::{ResolvedTrailExpansion, ShortestMode, TrailDirection},
};

// These query-wide limits bound explored work and the exact compact JSON bytes
// passed from hybrid traversal operators into SQLite. They fail closed rather
// than silently truncating a result.
pub(super) const DEFAULT_TRAIL_STATE_LIMIT: usize = 100_000;
pub(super) const DEFAULT_TRAIL_PAYLOAD_LIMIT: usize = 128 * 1024 * 1024;

pub(super) struct TrailBudget {
    state_limit: usize,
    state_used: usize,
    payload_limit: usize,
    payload_used: usize,
}

impl Default for TrailBudget {
    fn default() -> Self {
        Self::with_limits(DEFAULT_TRAIL_STATE_LIMIT, DEFAULT_TRAIL_PAYLOAD_LIMIT)
    }
}

impl TrailBudget {
    #[cfg(test)]
    pub(super) fn new(state_limit: usize) -> Self {
        Self::with_limits(state_limit, DEFAULT_TRAIL_PAYLOAD_LIMIT)
    }

    pub(super) fn with_limits(state_limit: usize, payload_limit: usize) -> Self {
        Self {
            state_limit,
            state_used: 0,
            payload_limit,
            payload_used: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn payload_used(&self) -> usize {
        self.payload_used
    }

    fn admit(&mut self, traversal: usize) -> Result<()> {
        if self.state_used >= self.state_limit {
            return Err(Error::ResourceLimit(format!(
                "trail expansion ~{traversal} exceeded the {}-state budget",
                self.state_limit
            )));
        }
        self.state_used += 1;
        Ok(())
    }

    fn admit_payload(&mut self, traversal: usize, bytes: usize) -> Result<()> {
        let Some(next) = self.payload_used.checked_add(bytes) else {
            return Err(Error::ResourceLimit(format!(
                "trail expansion ~{traversal} exceeded the {}-byte serialized-payload budget",
                self.payload_limit
            )));
        };
        if next > self.payload_limit {
            return Err(Error::ResourceLimit(format!(
                "trail expansion ~{traversal} exceeded the {}-byte serialized-payload budget",
                self.payload_limit
            )));
        }
        self.payload_used = next;
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

struct EncodedRows {
    bytes: Vec<u8>,
    has_rows: bool,
}

impl EncodedRows {
    fn new(traversal: usize, budget: &mut TrailBudget) -> Result<Self> {
        budget.admit_payload(traversal, 1)?;
        Ok(Self {
            bytes: vec![b'['],
            has_rows: false,
        })
    }

    fn push(
        &mut self,
        start: &str,
        state: &TrailState,
        traversal: usize,
        budget: &mut TrailBudget,
    ) -> Result<()> {
        let mut writer = PayloadWriter::new(&mut self.bytes, traversal, budget);
        if self.has_rows {
            writer.write_bytes(b",")?;
        }
        writer.write_bytes(b"{\"depth\":")?;
        writer.write_json_usize(state.edge_ids.len())?;
        writer.write_bytes(b",\"end_id\":")?;
        writer.write_json_string(&state.end_node)?;
        writer.write_bytes(b",\"node_ids\":")?;
        writer.write_string_array(&state.node_ids)?;
        writer.write_bytes(b",\"start_id\":")?;
        writer.write_json_string(start)?;
        writer.write_bytes(b",\"used_edges\":")?;
        writer.write_string_array(&state.edge_ids)?;
        writer.write_bytes(b"}")?;
        self.has_rows = true;
        Ok(())
    }

    fn finish(mut self, traversal: usize, budget: &mut TrailBudget) -> Result<String> {
        PayloadWriter::new(&mut self.bytes, traversal, budget).write_bytes(b"]")?;
        String::from_utf8(self.bytes)
            .map_err(|_| Error::Data("traversal runtime JSON was not valid UTF-8".into()))
    }
}

struct PayloadWriter<'a> {
    bytes: &'a mut Vec<u8>,
    traversal: usize,
    budget: &'a mut TrailBudget,
    resource_error: Option<Error>,
}

impl<'a> PayloadWriter<'a> {
    fn new(bytes: &'a mut Vec<u8>, traversal: usize, budget: &'a mut TrailBudget) -> Self {
        Self {
            bytes,
            traversal,
            budget,
            resource_error: None,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.budget.admit_payload(self.traversal, bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn write_json_string(&mut self, value: &str) -> Result<()> {
        let result = serde_json::to_writer(&mut *self, value);
        self.finish_json(result)
    }

    fn write_json_usize(&mut self, value: usize) -> Result<()> {
        let result = serde_json::to_writer(&mut *self, &value);
        self.finish_json(result)
    }

    fn write_string_array(&mut self, values: &[String]) -> Result<()> {
        self.write_bytes(b"[")?;
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                self.write_bytes(b",")?;
            }
            self.write_json_string(value)?;
        }
        self.write_bytes(b"]")
    }

    fn finish_json(&mut self, result: serde_json::Result<()>) -> Result<()> {
        match result {
            Ok(()) => Ok(()),
            Err(_) if self.resource_error.is_some() => {
                Err(self.resource_error.take().expect("checked resource error"))
            }
            Err(error) => Err(Error::Data(format!(
                "could not encode traversal runtime JSON: {error}"
            ))),
        }
    }
}

impl Write for PayloadWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.write_bytes(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.resource_error = Some(error);
                Err(io::Error::other("traversal payload budget exhausted"))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// The consuming SQLite fragment reads this ordered trail relation through
// json_each(), keeping endpoint joins and downstream row operators in SQL.
pub(super) fn materialize_trails(
    connection: &Connection,
    spec: &ResolvedTrailExpansion,
    budget: &mut TrailBudget,
) -> Result<SqlValue> {
    let Some((edges, nodes)) = load_expansion_inputs(connection, spec)? else {
        let rows = EncodedRows::new(spec.traversal, budget)?.finish(spec.traversal, budget)?;
        return Ok(SqlValue::Text(rows));
    };
    let rows = enumerate_trails(
        &nodes,
        &edges,
        spec.direction,
        spec.minimum,
        spec.maximum,
        spec.traversal,
        budget,
    )?;
    Ok(SqlValue::Text(rows))
}

pub(super) fn materialize_shortest(
    connection: &Connection,
    spec: &ResolvedTrailExpansion,
    mode: ShortestMode,
    budget: &mut TrailBudget,
) -> Result<SqlValue> {
    let Some((edges, nodes)) = load_expansion_inputs(connection, spec)? else {
        let rows = EncodedRows::new(spec.traversal, budget)?.finish(spec.traversal, budget)?;
        return Ok(SqlValue::Text(rows));
    };
    let rows = enumerate_shortest_paths(
        &nodes,
        &edges,
        spec.direction,
        (spec.minimum, spec.maximum),
        mode,
        spec.traversal,
        budget,
    )?;
    Ok(SqlValue::Text(rows))
}

fn load_expansion_inputs(
    connection: &Connection,
    spec: &ResolvedTrailExpansion,
) -> Result<Option<(Vec<StoredEdge>, Vec<String>)>> {
    if spec.maximum.is_some_and(|maximum| spec.minimum > maximum) {
        return Ok(None);
    }
    Ok(Some((
        load_edges(connection, spec)?,
        load_nodes(connection)?,
    )))
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
) -> Result<String> {
    let mut rows = EncodedRows::new(traversal, budget)?;
    let effective_maximum = maximum.unwrap_or(edges.len()).min(edges.len());
    if minimum > effective_maximum {
        return rows.finish(traversal, budget);
    }

    let adjacency = adjacency(edges, direction);
    for start in nodes {
        let seed = TrailState {
            end_node: start.clone(),
            edge_ids: Vec::new(),
            node_ids: vec![start.clone()],
        };
        if minimum == 0 {
            budget.admit(traversal)?;
            rows.push(start, &seed, traversal, budget)?;
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
                    rows.push(start, &next, traversal, budget)?;
                }
                pending.push(next);
            }
        }
    }
    rows.finish(traversal, budget)
}

// Run one BFS per stored start node. The first depth recorded for an end node
// is its minimum; equal-depth states are retained only for allShortestPaths.
// Eligible edges arrive in ID order, making shortestPath's unspecified tied
// choice stable for an unchanged snapshot.
fn enumerate_shortest_paths(
    nodes: &[String],
    edges: &[StoredEdge],
    direction: TrailDirection,
    bounds: (usize, Option<usize>),
    mode: ShortestMode,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<String> {
    let mut rows = EncodedRows::new(traversal, budget)?;
    let (minimum, maximum) = bounds;
    let effective_maximum = maximum.unwrap_or(edges.len()).min(edges.len());
    if minimum > effective_maximum {
        return rows.finish(traversal, budget);
    }

    let adjacency = adjacency(edges, direction);
    let include_all = mode == ShortestMode::All;
    for start in nodes {
        let seed = TrailState {
            end_node: start.clone(),
            edge_ids: Vec::new(),
            node_ids: vec![start.clone()],
        };
        if minimum == 0 {
            budget.admit(traversal)?;
            rows.push(start, &seed, traversal, budget)?;
        }

        let mut best_depth = BTreeMap::from([(start.clone(), 0_usize)]);
        let mut pending = VecDeque::from([seed]);
        while let Some(state) = pending.pop_front() {
            if state.edge_ids.len() >= effective_maximum {
                continue;
            }
            let Some(steps) = adjacency.get(&state.end_node) else {
                continue;
            };
            for step in steps {
                if state.edge_ids.contains(&step.edge_id) {
                    continue;
                }
                budget.admit(traversal)?;
                let depth = state.edge_ids.len() + 1;
                match best_depth.get(&step.next_node) {
                    Some(best) if depth > *best => continue,
                    Some(best) if depth == *best && !include_all => continue,
                    None => {
                        best_depth.insert(step.next_node.clone(), depth);
                    }
                    Some(_) => {}
                }

                let mut next = state.clone();
                next.end_node.clone_from(&step.next_node);
                next.edge_ids.push(step.edge_id.clone());
                next.node_ids.push(step.next_node.clone());
                if depth >= minimum {
                    rows.push(start, &next, traversal, budget)?;
                }
                pending.push_back(next);
            }
        }
        // A positive cycle back to `start` needs its own search: a longer
        // arrival at an intermediate node can preserve the edge that closes
        // the cycle, so ordinary node-distance pruning is not sound for it.
        if minimum > 0 {
            for cycle in enumerate_shortest_cycles(
                start,
                &adjacency,
                effective_maximum,
                include_all,
                traversal,
                budget,
            )? {
                rows.push(start, &cycle, traversal, budget)?;
            }
        }
    }
    rows.finish(traversal, budget)
}

fn enumerate_shortest_cycles(
    start: &str,
    adjacency: &BTreeMap<String, Vec<TrailStep>>,
    maximum: usize,
    include_all: bool,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<Vec<TrailState>> {
    let Some(first_steps) = adjacency.get(start) else {
        return Ok(Vec::new());
    };
    let mut best_cycle_depth = None;
    let mut cycles = Vec::new();

    for first in first_steps {
        budget.admit(traversal)?;
        let first_state = TrailState {
            end_node: first.next_node.clone(),
            edge_ids: vec![first.edge_id.clone()],
            node_ids: vec![start.to_owned(), first.next_node.clone()],
        };
        if first.next_node == start {
            retain_shortest_cycle(&mut cycles, &mut best_cycle_depth, first_state, include_all);
            continue;
        }
        if maximum <= 1 || best_cycle_depth.is_some_and(|best| best <= 1) {
            continue;
        }

        let mut best_depth = BTreeMap::from([(first.next_node.clone(), 1_usize)]);
        let mut pending = VecDeque::from([first_state]);
        while let Some(state) = pending.pop_front() {
            let depth = state.edge_ids.len();
            if depth >= maximum || best_cycle_depth.is_some_and(|best| depth >= best) {
                continue;
            }
            let Some(steps) = adjacency.get(&state.end_node) else {
                continue;
            };
            for step in steps {
                if state.edge_ids.contains(&step.edge_id) {
                    continue;
                }
                budget.admit(traversal)?;
                let next_depth = depth + 1;
                let mut next = state.clone();
                next.end_node.clone_from(&step.next_node);
                next.edge_ids.push(step.edge_id.clone());
                next.node_ids.push(step.next_node.clone());
                if next.end_node == start {
                    retain_shortest_cycle(&mut cycles, &mut best_cycle_depth, next, include_all);
                    continue;
                }

                match best_depth.get(&next.end_node) {
                    Some(best) if next_depth > *best => continue,
                    Some(best) if next_depth == *best && !include_all => continue,
                    None => {
                        best_depth.insert(next.end_node.clone(), next_depth);
                    }
                    Some(_) => {}
                }
                pending.push_back(next);
            }
        }
    }
    Ok(cycles)
}

fn retain_shortest_cycle(
    cycles: &mut Vec<TrailState>,
    best_depth: &mut Option<usize>,
    candidate: TrailState,
    include_all: bool,
) {
    let depth = candidate.edge_ids.len();
    match *best_depth {
        Some(best) if depth > best => {}
        Some(best) if depth == best => {
            if include_all {
                cycles.push(candidate);
            }
        }
        Some(_) | None => {
            cycles.clear();
            cycles.push(candidate);
            *best_depth = Some(depth);
        }
    }
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

#[cfg(test)]
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

    fn decode_rows(encoded: &str) -> Vec<Value> {
        serde_json::from_str(encoded).unwrap()
    }

    #[test]
    fn undirected_self_loop_is_emitted_once_per_depth() {
        let nodes = vec!["a".into()];
        let edges = vec![edge("loop", "a", "a")];
        let mut budget = TrailBudget::new(10);
        let encoded = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Undirected,
            0,
            None,
            0,
            &mut budget,
        )
        .unwrap();
        let rows = decode_rows(&encoded);

        assert_eq!(budget.payload_used, encoded.len());
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
    fn shortest_selection_shares_the_fail_closed_work_budget() {
        let nodes = vec!["a".into(), "b".into()];
        let edges = vec![edge("ab", "a", "b")];
        let mut budget = TrailBudget::new(0);
        let error = enumerate_shortest_paths(
            &nodes,
            &edges,
            TrailDirection::Right,
            (1, None),
            ShortestMode::One,
            9,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(error, Error::ResourceLimit(message) if message.contains("~9")));
    }

    #[test]
    fn reversed_bounds_are_empty_without_spending_state_budget() {
        let nodes = vec!["a".into(), "b".into()];
        let edges = vec![edge("ab", "a", "b")];
        let mut budget = TrailBudget::with_limits(0, 2);
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

        assert_eq!(rows, "[]");
        assert_eq!(budget.state_used, 0);
        assert_eq!(budget.payload_used, 2);
    }

    #[test]
    fn serialized_payload_boundary_is_exact_for_escaped_utf8_ids() {
        let nodes = vec!["a\n".into(), "β".into()];
        let edges = vec![edge("edge\"", "a\n", "β")];
        let row = encoded_trail(
            "a\n",
            &TrailState {
                end_node: "β".into(),
                edge_ids: vec!["edge\"".into()],
                node_ids: vec!["a\n".into(), "β".into()],
            },
        );
        let expected = Value::Array(vec![row]).to_string();
        assert!(expected.len() > expected.chars().count());

        let mut exact = TrailBudget::with_limits(10, expected.len());
        let actual = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Right,
            1,
            None,
            11,
            &mut exact,
        )
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(exact.payload_used, expected.len());

        let mut short = TrailBudget::with_limits(10, expected.len() - 1);
        let error = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Right,
            1,
            None,
            12,
            &mut short,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            Error::ResourceLimit(message)
                if message.contains("~12")
                    && message.contains(&format!("{}-byte serialized-payload budget", expected.len() - 1))
        ));
    }

    #[test]
    fn empty_relation_requires_its_two_serialized_bytes() {
        let mut exact = TrailBudget::with_limits(0, 2);
        let rows =
            enumerate_trails(&[], &[], TrailDirection::Right, 1, None, 13, &mut exact).unwrap();
        assert_eq!(rows, "[]");

        let mut short = TrailBudget::with_limits(0, 1);
        let error =
            enumerate_trails(&[], &[], TrailDirection::Right, 1, None, 13, &mut short).unwrap_err();
        assert!(matches!(
            error,
            Error::ResourceLimit(message)
                if message.contains("~13")
                    && message.contains("1-byte serialized-payload budget")
        ));
    }

    #[test]
    fn emitted_relations_consume_one_shared_payload_budget() {
        let nodes = vec!["a".into(), "b".into()];
        let edges = vec![edge("ab", "a", "b")];
        let expected = Value::Array(vec![encoded_trail(
            "a",
            &TrailState {
                end_node: "b".into(),
                edge_ids: vec!["ab".into()],
                node_ids: vec!["a".into(), "b".into()],
            },
        )])
        .to_string();
        let mut budget = TrailBudget::with_limits(10, expected.len());

        let first = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Right,
            1,
            None,
            14,
            &mut budget,
        )
        .unwrap();
        assert_eq!(first, expected);

        let error = enumerate_trails(
            &nodes,
            &edges,
            TrailDirection::Right,
            1,
            None,
            15,
            &mut budget,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            Error::ResourceLimit(message)
                if message.contains("~15") && message.contains("serialized-payload budget")
        ));
    }

    #[test]
    fn shortest_paths_use_the_same_fail_closed_payload_guard() {
        let nodes = vec!["a".into(), "b".into()];
        let edges = vec![edge("ab", "a", "b")];
        let expected = Value::Array(vec![encoded_trail(
            "a",
            &TrailState {
                end_node: "b".into(),
                edge_ids: vec!["ab".into()],
                node_ids: vec!["a".into(), "b".into()],
            },
        )])
        .to_string();
        let mut budget = TrailBudget::with_limits(10, expected.len() - 1);

        let error = enumerate_shortest_paths(
            &nodes,
            &edges,
            TrailDirection::Right,
            (1, None),
            ShortestMode::One,
            16,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::ResourceLimit(message)
                if message.contains("~16")
                    && message.contains("serialized-payload budget")
        ));
    }
}
