use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, Write},
};

use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
#[cfg(test)]
use serde_json::{Value, json};

use crate::{
    Error, Result,
    planner::{ResolvedNodeDomain, ResolvedTrailExpansion, ShortestMode, TrailDirection},
};

// These query-wide limits bound explored work, the exact compact JSON bytes
// passed from hybrid traversal operators into SQLite, and the graph inputs a
// hybrid producer retains while it runs. They fail closed rather than silently
// truncating a result.
pub(super) const DEFAULT_TRAIL_STATE_LIMIT: usize = 100_000;
pub(super) const DEFAULT_TRAIL_PAYLOAD_LIMIT: usize = 128 * 1024 * 1024;
pub(super) const DEFAULT_TRAIL_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// The deterministic cost charged for every retained ID string on top of its
/// UTF-8 length. It stands in for the owning `String` header so one-byte IDs
/// cannot make a huge retained graph look almost free. The constant is fixed
/// rather than derived from `size_of::<String>()` so the accounting, and the
/// point at which a query fails, is identical on every platform.
const RETAINED_ID_OVERHEAD: usize = 24;
/// The cost of one node-depth entry in a shortest-path visited map beyond its
/// node ID.
const RETAINED_DEPTH_OVERHEAD: usize = 8;

pub(super) struct TrailBudget {
    state_limit: usize,
    state_used: usize,
    payload_limit: usize,
    payload_used: usize,
    memory_limit: usize,
    memory_used: usize,
    memory_peak: usize,
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
            memory_limit: DEFAULT_TRAIL_MEMORY_LIMIT,
            memory_used: 0,
            memory_peak: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn with_memory_limit(mut self, memory_limit: usize) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    #[cfg(test)]
    pub(super) fn payload_used(&self) -> usize {
        self.payload_used
    }

    /// The retained-memory bytes currently charged to live hybrid inputs.
    #[cfg(test)]
    pub(super) fn memory_used(&self) -> usize {
        self.memory_used
    }

    /// The highest retained-memory charge reached so far.
    #[cfg(test)]
    pub(super) fn memory_peak(&self) -> usize {
        self.memory_peak
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

    // Charge `bytes` of retained graph input. Unlike the state and payload
    // counters this one is released again, so it tracks what is live rather
    // than what was ever allocated, and it is compared against the limit
    // before the bytes are retained.
    fn retain(&mut self, traversal: usize, bytes: usize) -> Result<()> {
        let next = self.memory_used.saturating_add(bytes);
        if next > self.memory_limit {
            return Err(Error::ResourceLimit(format!(
                "trail expansion ~{traversal} exceeded the {}-byte retained-memory budget",
                self.memory_limit
            )));
        }
        self.memory_used = next;
        self.memory_peak = self.memory_peak.max(next);
        Ok(())
    }

    fn release(&mut self, bytes: usize) {
        debug_assert!(bytes <= self.memory_used);
        self.memory_used = self.memory_used.saturating_sub(bytes);
    }

    fn memory_checkpoint(&self) -> usize {
        self.memory_used
    }

    // Release everything retained since `checkpoint`, which is how a producer
    // returns the inputs it drops on completion.
    fn release_to(&mut self, checkpoint: usize) {
        debug_assert!(checkpoint <= self.memory_used);
        self.memory_used = checkpoint;
    }
}

// The deterministic retained cost of one ID string.
fn id_cost(id: &str) -> usize {
    id.len() + RETAINED_ID_OVERHEAD
}

fn edge_cost(edge: &StoredEdge) -> usize {
    id_cost(&edge.id) + id_cost(&edge.from) + id_cost(&edge.to)
}

fn step_cost(step: &TrailStep) -> usize {
    id_cost(&step.edge_id) + id_cost(&step.next_node)
}

// The cost of one frontier state: its end node plus every retained edge and
// node ID along the trail.
fn state_cost(state: &TrailState) -> usize {
    id_cost(&state.end_node)
        + state.edge_ids.iter().map(|id| id_cost(id)).sum::<usize>()
        + state.node_ids.iter().map(|id| id_cost(id)).sum::<usize>()
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
    let checkpoint = budget.memory_checkpoint();
    let Some(inputs) = load_expansion_inputs(connection, spec, budget)? else {
        let rows = EncodedRows::new(spec.traversal, budget)?.finish(spec.traversal, budget)?;
        return Ok(SqlValue::Text(rows));
    };
    let rows = enumerate_trails(
        inputs.graph(),
        spec.direction,
        spec.minimum,
        spec.maximum,
        spec.traversal,
        budget,
    )?;
    drop(inputs);
    budget.release_to(checkpoint);
    Ok(SqlValue::Text(rows))
}

pub(super) fn materialize_shortest(
    connection: &Connection,
    spec: &ResolvedTrailExpansion,
    mode: ShortestMode,
    budget: &mut TrailBudget,
) -> Result<SqlValue> {
    let checkpoint = budget.memory_checkpoint();
    let Some(inputs) = load_expansion_inputs(connection, spec, budget)? else {
        let rows = EncodedRows::new(spec.traversal, budget)?.finish(spec.traversal, budget)?;
        return Ok(SqlValue::Text(rows));
    };
    let rows = enumerate_shortest_paths(
        inputs.graph(),
        spec.direction,
        (spec.minimum, spec.maximum),
        mode,
        spec.traversal,
        budget,
    )?;
    drop(inputs);
    budget.release_to(checkpoint);
    Ok(SqlValue::Text(rows))
}

/// The graph inputs one hybrid expansion retains in memory.
///
/// `seeds` are the start nodes to expand, in ID order. A planner-provided
/// start domain restricts them to nodes satisfying the start endpoint's label
/// and inline-property constraints. Without a domain, a traversal requiring
/// at least one hop seeds only nodes that begin an eligible edge, so no
/// whole-node listing is loaded; a zero-hop traversal must still seed every
/// stored node because each one is its own identity trail. `end_domain`
/// restricts emitted rows the same way without pruning intermediate states.
///
/// Every ID retained here is charged to the query's retained-memory budget as
/// it is read, so an oversized eligible-edge set fails before it is fully
/// loaded. The producer releases the whole charge when it finishes.
struct ExpansionInputs {
    seeds: Vec<String>,
    edges: Vec<StoredEdge>,
    end_domain: Option<BTreeSet<String>>,
}

impl ExpansionInputs {
    fn graph(&self) -> ExpansionGraph<'_> {
        ExpansionGraph {
            seeds: &self.seeds,
            edges: &self.edges,
            end_domain: self.end_domain.as_ref(),
        }
    }
}

/// A borrowed view of the seeds, eligible edges, and optional end domain one
/// enumerator runs over.
#[derive(Clone, Copy)]
struct ExpansionGraph<'a> {
    seeds: &'a [String],
    edges: &'a [StoredEdge],
    end_domain: Option<&'a BTreeSet<String>>,
}

fn load_expansion_inputs(
    connection: &Connection,
    spec: &ResolvedTrailExpansion,
    budget: &mut TrailBudget,
) -> Result<Option<ExpansionInputs>> {
    if spec.maximum.is_some_and(|maximum| spec.minimum > maximum) {
        return Ok(None);
    }
    let traversal = spec.traversal;
    let edges = load_edges(connection, spec, budget)?;
    let seeds = match &spec.start_domain {
        Some(domain) => load_domain(connection, domain, traversal, budget)?,
        None if spec.minimum == 0 => load_nodes(connection, traversal, budget)?,
        None => edge_sources(&edges, spec.direction, traversal, budget)?,
    };
    let end_domain = spec
        .end_domain
        .as_ref()
        .map(|domain| load_domain(connection, domain, traversal, budget).map(BTreeSet::from_iter))
        .transpose()?;
    Ok(Some(ExpansionInputs {
        seeds,
        edges,
        end_domain,
    }))
}

fn load_nodes(
    connection: &Connection,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<Vec<String>> {
    let mut statement = connection.prepare("SELECT id FROM nodes ORDER BY id")?;
    debug_assert!(statement.readonly());
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    collect_retained_ids(rows, traversal, budget)
}

fn load_domain(
    connection: &Connection,
    domain: &ResolvedNodeDomain,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<Vec<String>> {
    let mut statement = connection.prepare_cached(&domain.sql)?;
    debug_assert!(statement.readonly());
    let rows = statement.query_map(params_from_iter(domain.bindings.iter()), |row| {
        row.get::<_, String>(0)
    })?;
    collect_retained_ids(rows, traversal, budget)
}

// Retain each streamed ID only after the budget admits it, so the failure
// point does not depend on how many rows SQLite has already produced.
fn collect_retained_ids(
    rows: impl Iterator<Item = rusqlite::Result<String>>,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for row in rows {
        let id = row?;
        budget.retain(traversal, id_cost(&id))?;
        ids.push(id);
    }
    Ok(ids)
}

// The distinct nodes that begin at least one eligible edge, in ID order. This
// matches the order `load_nodes` would produce for the same nodes because
// SQLite's default BINARY collation and Rust's `String` ordering both compare
// UTF-8 bytes.
fn edge_sources(
    edges: &[StoredEdge],
    direction: TrailDirection,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<Vec<String>> {
    let mut sources = BTreeSet::new();
    let mut retain = |sources: &mut BTreeSet<String>, id: &str| -> Result<()> {
        if !sources.contains(id) {
            budget.retain(traversal, id_cost(id))?;
            sources.insert(id.to_owned());
        }
        Ok(())
    };
    for edge in edges {
        match direction {
            TrailDirection::Right => retain(&mut sources, &edge.from)?,
            TrailDirection::Left => retain(&mut sources, &edge.to)?,
            TrailDirection::Undirected => {
                retain(&mut sources, &edge.from)?;
                retain(&mut sources, &edge.to)?;
            }
        }
    }
    Ok(sources.into_iter().collect())
}

fn admits_end(end_domain: Option<&BTreeSet<String>>, node: &str) -> bool {
    end_domain.is_none_or(|domain| domain.contains(node))
}

fn load_edges(
    connection: &Connection,
    spec: &ResolvedTrailExpansion,
    budget: &mut TrailBudget,
) -> Result<Vec<StoredEdge>> {
    let mut statement = connection.prepare_cached(&spec.edge_sql)?;
    debug_assert!(statement.readonly());
    let rows = statement.query_map(params_from_iter(spec.edge_bindings.iter()), |row| {
        Ok(StoredEdge {
            id: row.get(0)?,
            from: row.get(1)?,
            to: row.get(2)?,
        })
    })?;
    let mut edges = Vec::new();
    for row in rows {
        let edge = row?;
        budget.retain(spec.traversal, edge_cost(&edge))?;
        edges.push(edge);
    }
    Ok(edges)
}

fn enumerate_trails(
    graph: ExpansionGraph<'_>,
    direction: TrailDirection,
    minimum: usize,
    maximum: Option<usize>,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<String> {
    let ExpansionGraph {
        seeds: nodes,
        edges,
        end_domain,
    } = graph;
    let mut rows = EncodedRows::new(traversal, budget)?;
    let effective_maximum = maximum.unwrap_or(edges.len()).min(edges.len());
    if minimum > effective_maximum {
        return rows.finish(traversal, budget);
    }

    let checkpoint = budget.memory_checkpoint();
    let adjacency = adjacency(edges, direction, traversal, budget)?;
    for start in nodes {
        let seed = TrailState {
            end_node: start.clone(),
            edge_ids: Vec::new(),
            node_ids: vec![start.clone()],
        };
        if minimum == 0 {
            budget.admit(traversal)?;
            if admits_end(end_domain, start) {
                rows.push(start, &seed, traversal, budget)?;
            }
        }

        budget.retain(traversal, state_cost(&seed))?;
        let mut pending = vec![seed];
        while let Some(state) = pending.pop() {
            // The popped state stays charged while its successors are built
            // from it and is released once they are on the frontier.
            let state_bytes = state_cost(&state);
            'expand: {
                if state.edge_ids.len() >= effective_maximum {
                    break 'expand;
                }
                let Some(steps) = adjacency.get(&state.end_node) else {
                    break 'expand;
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
                    if next.edge_ids.len() >= minimum && admits_end(end_domain, &next.end_node) {
                        rows.push(start, &next, traversal, budget)?;
                    }
                    budget.retain(traversal, state_cost(&next))?;
                    pending.push(next);
                }
            }
            budget.release(state_bytes);
        }
    }
    drop(adjacency);
    budget.release_to(checkpoint);
    rows.finish(traversal, budget)
}

// Run one BFS per stored start node. The first depth recorded for an end node
// is its minimum; equal-depth states are retained only for allShortestPaths.
// Eligible edges arrive in ID order, making shortestPath's unspecified tied
// choice stable for an unchanged snapshot.
fn enumerate_shortest_paths(
    graph: ExpansionGraph<'_>,
    direction: TrailDirection,
    bounds: (usize, Option<usize>),
    mode: ShortestMode,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<String> {
    let ExpansionGraph {
        seeds: nodes,
        edges,
        end_domain,
    } = graph;
    let mut rows = EncodedRows::new(traversal, budget)?;
    let (minimum, maximum) = bounds;
    let effective_maximum = maximum.unwrap_or(edges.len()).min(edges.len());
    if minimum > effective_maximum {
        return rows.finish(traversal, budget);
    }

    let checkpoint = budget.memory_checkpoint();
    let adjacency = adjacency(edges, direction, traversal, budget)?;
    let include_all = mode == ShortestMode::All;
    for start in nodes {
        let start_checkpoint = budget.memory_checkpoint();
        let seed = TrailState {
            end_node: start.clone(),
            edge_ids: Vec::new(),
            node_ids: vec![start.clone()],
        };
        if minimum == 0 {
            budget.admit(traversal)?;
            if admits_end(end_domain, start) {
                rows.push(start, &seed, traversal, budget)?;
            }
        }

        budget.retain(traversal, depth_entry_cost(start))?;
        let mut best_depth = BTreeMap::from([(start.clone(), 0_usize)]);
        budget.retain(traversal, state_cost(&seed))?;
        let mut pending = VecDeque::from([seed]);
        while let Some(state) = pending.pop_front() {
            let state_bytes = state_cost(&state);
            'expand: {
                if state.edge_ids.len() >= effective_maximum {
                    break 'expand;
                }
                let Some(steps) = adjacency.get(&state.end_node) else {
                    break 'expand;
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
                            budget.retain(traversal, depth_entry_cost(&step.next_node))?;
                            best_depth.insert(step.next_node.clone(), depth);
                        }
                        Some(_) => {}
                    }

                    let mut next = state.clone();
                    next.end_node.clone_from(&step.next_node);
                    next.edge_ids.push(step.edge_id.clone());
                    next.node_ids.push(step.next_node.clone());
                    if depth >= minimum && admits_end(end_domain, &next.end_node) {
                        rows.push(start, &next, traversal, budget)?;
                    }
                    budget.retain(traversal, state_cost(&next))?;
                    pending.push_back(next);
                }
            }
            budget.release(state_bytes);
        }
        // A positive cycle back to `start` needs its own search: a longer
        // arrival at an intermediate node can preserve the edge that closes
        // the cycle, so ordinary node-distance pruning is not sound for it.
        if minimum > 0 && admits_end(end_domain, start) {
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
        // The visited map and any retained cycles belong to this start only.
        drop(best_depth);
        budget.release_to(start_checkpoint);
    }
    drop(adjacency);
    budget.release_to(checkpoint);
    rows.finish(traversal, budget)
}

fn depth_entry_cost(node: &str) -> usize {
    id_cost(node) + RETAINED_DEPTH_OVERHEAD
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
            retain_shortest_cycle(
                &mut cycles,
                &mut best_cycle_depth,
                first_state,
                include_all,
                traversal,
                budget,
            )?;
            continue;
        }
        if maximum <= 1 || best_cycle_depth.is_some_and(|best| best <= 1) {
            continue;
        }

        // Each first step runs its own search. Its frontier and visited map
        // are tracked separately from the retained cycles, which outlive the
        // search, and are released before the next first step starts.
        let mut search = RetainedBytes::default();
        search.retain(budget, traversal, depth_entry_cost(&first.next_node))?;
        let mut best_depth = BTreeMap::from([(first.next_node.clone(), 1_usize)]);
        search.retain(budget, traversal, state_cost(&first_state))?;
        let mut pending = VecDeque::from([first_state]);
        while let Some(state) = pending.pop_front() {
            let state_bytes = state_cost(&state);
            'expand: {
                let depth = state.edge_ids.len();
                if depth >= maximum || best_cycle_depth.is_some_and(|best| depth >= best) {
                    break 'expand;
                }
                let Some(steps) = adjacency.get(&state.end_node) else {
                    break 'expand;
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
                        retain_shortest_cycle(
                            &mut cycles,
                            &mut best_cycle_depth,
                            next,
                            include_all,
                            traversal,
                            budget,
                        )?;
                        continue;
                    }

                    match best_depth.get(&next.end_node) {
                        Some(best) if next_depth > *best => continue,
                        Some(best) if next_depth == *best && !include_all => continue,
                        None => {
                            search.retain(budget, traversal, depth_entry_cost(&next.end_node))?;
                            best_depth.insert(next.end_node.clone(), next_depth);
                        }
                        Some(_) => {}
                    }
                    search.retain(budget, traversal, state_cost(&next))?;
                    pending.push_back(next);
                }
            }
            search.release(budget, state_bytes);
        }
        drop(best_depth);
        search.release_all(budget);
    }
    Ok(cycles)
}

/// Retained bytes owned by one scope that must be released independently of
/// other charges interleaved on the same budget.
#[derive(Default)]
struct RetainedBytes {
    bytes: usize,
}

impl RetainedBytes {
    fn retain(&mut self, budget: &mut TrailBudget, traversal: usize, bytes: usize) -> Result<()> {
        budget.retain(traversal, bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    fn release(&mut self, budget: &mut TrailBudget, bytes: usize) {
        debug_assert!(bytes <= self.bytes);
        self.bytes -= bytes;
        budget.release(bytes);
    }

    fn release_all(self, budget: &mut TrailBudget) {
        budget.release(self.bytes);
    }
}

// Retained cycles are charged while they are kept as the current best and
// released when a shorter cycle replaces them.
fn retain_shortest_cycle(
    cycles: &mut Vec<TrailState>,
    best_depth: &mut Option<usize>,
    candidate: TrailState,
    include_all: bool,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<()> {
    let depth = candidate.edge_ids.len();
    match *best_depth {
        Some(best) if depth > best => {}
        Some(best) if depth == best => {
            if include_all {
                budget.retain(traversal, state_cost(&candidate))?;
                cycles.push(candidate);
            }
        }
        Some(_) | None => {
            budget.release(cycles.iter().map(state_cost).sum());
            cycles.clear();
            budget.retain(traversal, state_cost(&candidate))?;
            cycles.push(candidate);
            *best_depth = Some(depth);
        }
    }
    Ok(())
}

// Every adjacency key and step is charged as it is inserted, so the map is
// bounded before it is fully built.
fn adjacency(
    edges: &[StoredEdge],
    direction: TrailDirection,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<BTreeMap<String, Vec<TrailStep>>> {
    let mut adjacency = BTreeMap::<String, Vec<TrailStep>>::new();
    for edge in edges {
        match direction {
            TrailDirection::Right => {
                add_step(
                    &mut adjacency,
                    &edge.from,
                    &edge.to,
                    &edge.id,
                    traversal,
                    budget,
                )?;
            }
            TrailDirection::Left => {
                add_step(
                    &mut adjacency,
                    &edge.to,
                    &edge.from,
                    &edge.id,
                    traversal,
                    budget,
                )?;
            }
            TrailDirection::Undirected => {
                add_step(
                    &mut adjacency,
                    &edge.from,
                    &edge.to,
                    &edge.id,
                    traversal,
                    budget,
                )?;
                if edge.from != edge.to {
                    add_step(
                        &mut adjacency,
                        &edge.to,
                        &edge.from,
                        &edge.id,
                        traversal,
                        budget,
                    )?;
                }
            }
        }
    }
    Ok(adjacency)
}

fn add_step(
    adjacency: &mut BTreeMap<String, Vec<TrailStep>>,
    from: &str,
    to: &str,
    edge_id: &str,
    traversal: usize,
    budget: &mut TrailBudget,
) -> Result<()> {
    let step = TrailStep {
        edge_id: edge_id.to_owned(),
        next_node: to.to_owned(),
    };
    let mut bytes = step_cost(&step);
    if !adjacency.contains_key(from) {
        bytes += id_cost(from);
    }
    budget.retain(traversal, bytes)?;
    adjacency.entry(from.to_owned()).or_default().push(step);
    Ok(())
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

    fn graph<'a>(seeds: &'a [String], edges: &'a [StoredEdge]) -> ExpansionGraph<'a> {
        ExpansionGraph {
            seeds,
            edges,
            end_domain: None,
        }
    }

    #[test]
    fn undirected_self_loop_is_emitted_once_per_depth() {
        let nodes = vec!["a".into()];
        let edges = vec![edge("loop", "a", "a")];
        let mut budget = TrailBudget::new(10);
        let encoded = enumerate_trails(
            graph(&nodes, &edges),
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
            graph(&nodes, &edges),
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
            graph(&nodes, &edges),
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
            graph(&nodes, &edges),
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
            graph(&nodes, &edges),
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
            graph(&nodes, &edges),
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
        let rows = enumerate_trails(
            graph(&[], &[]),
            TrailDirection::Right,
            1,
            None,
            13,
            &mut exact,
        )
        .unwrap();
        assert_eq!(rows, "[]");

        let mut short = TrailBudget::with_limits(0, 1);
        let error = enumerate_trails(
            graph(&[], &[]),
            TrailDirection::Right,
            1,
            None,
            13,
            &mut short,
        )
        .unwrap_err();
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
            graph(&nodes, &edges),
            TrailDirection::Right,
            1,
            None,
            14,
            &mut budget,
        )
        .unwrap();
        assert_eq!(first, expected);

        let error = enumerate_trails(
            graph(&nodes, &edges),
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
            graph(&nodes, &edges),
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

    #[test]
    fn unconstrained_positive_traversals_seed_only_edge_sources() {
        let edges = vec![edge("ab", "a", "b"), edge("cb", "c", "b")];
        let mut budget = TrailBudget::new(0);

        assert_eq!(
            edge_sources(&edges, TrailDirection::Right, 1, &mut budget).unwrap(),
            ["a", "c"]
        );
        assert_eq!(
            edge_sources(&edges, TrailDirection::Left, 1, &mut budget).unwrap(),
            ["b"]
        );
        assert_eq!(
            edge_sources(&edges, TrailDirection::Undirected, 1, &mut budget).unwrap(),
            ["a", "b", "c"]
        );
        // Each distinct seed is charged once per call: 2 + 1 + 3 one-byte IDs.
        assert_eq!(budget.memory_used(), 6 * id_cost("a"));
    }

    #[test]
    fn retained_memory_accounting_is_deterministic_and_released_on_completion() {
        // One edge a->b expanded from `a` with `*1..`:
        //   adjacency: key "a" + step ("ab", "b")
        //   seed state: end "a" + node_ids ["a"]
        //   successor:  end "b" + edge_ids ["ab"] + node_ids ["a", "b"]
        // The seed stays charged while its successor is built, so the peak
        // holds all three at once.
        let nodes = vec!["a".into()];
        let edges = vec![edge("ab", "a", "b")];
        let adjacency_bytes = id_cost("a") + id_cost("ab") + id_cost("b");
        let seed_bytes = 2 * id_cost("a");
        let successor_bytes = id_cost("b") + id_cost("ab") + id_cost("a") + id_cost("b");
        let expected_peak = adjacency_bytes + seed_bytes + successor_bytes;
        assert_eq!(expected_peak, 25 + 26 + 25 + 50 + 25 + 26 + 25 + 25);

        let mut budget = TrailBudget::new(10);
        enumerate_trails(
            graph(&nodes, &edges),
            TrailDirection::Right,
            1,
            None,
            3,
            &mut budget,
        )
        .unwrap();
        assert_eq!(budget.memory_peak(), expected_peak);
        assert_eq!(budget.memory_used(), 0);

        let mut exact = TrailBudget::new(10).with_memory_limit(expected_peak);
        enumerate_trails(
            graph(&nodes, &edges),
            TrailDirection::Right,
            1,
            None,
            3,
            &mut exact,
        )
        .unwrap();

        let mut short = TrailBudget::new(10).with_memory_limit(expected_peak - 1);
        let error = enumerate_trails(
            graph(&nodes, &edges),
            TrailDirection::Right,
            1,
            None,
            3,
            &mut short,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            Error::ResourceLimit(message)
                if message.contains("~3")
                    && message.contains(&format!(
                        "{}-byte retained-memory budget",
                        expected_peak - 1
                    ))
        ));
    }

    #[test]
    fn depth_first_frontier_is_released_as_states_pop() {
        // A chain a->b->c->d keeps at most one pending successor per depth,
        // so the depth-first peak is far below the sum of every state built.
        let nodes = vec!["a".into()];
        let edges = vec![
            edge("ab", "a", "b"),
            edge("bc", "b", "c"),
            edge("cd", "c", "d"),
        ];
        let mut budget = TrailBudget::new(20);
        enumerate_trails(
            graph(&nodes, &edges),
            TrailDirection::Right,
            1,
            None,
            4,
            &mut budget,
        )
        .unwrap();
        let adjacency_bytes = 3 * (id_cost("a") + id_cost("ab") + id_cost("b"));
        // Deepest moment: the depth-2 state (a,b,c) is charged while its
        // depth-3 successor (a,b,c,d) is retained on the frontier.
        let depth_two = id_cost("c") + 2 * id_cost("ab") + 3 * id_cost("a");
        let depth_three = id_cost("d") + 3 * id_cost("ab") + 4 * id_cost("a");
        assert_eq!(
            budget.memory_peak(),
            adjacency_bytes + depth_two + depth_three
        );
        assert_eq!(budget.memory_used(), 0);
    }

    #[test]
    fn shortest_search_releases_per_start_structures_and_retained_cycles() {
        // Two starts over a triangle with a self-loop. Every visited map,
        // frontier, and retained cycle must be released between starts and
        // after the search, and a zero memory budget must fail closed.
        let nodes = vec!["a".into(), "b".into()];
        let edges = vec![
            edge("ab", "a", "b"),
            edge("bc", "b", "c"),
            edge("ca", "c", "a"),
            edge("bb", "b", "b"),
        ];
        for mode in [ShortestMode::One, ShortestMode::All] {
            let mut budget = TrailBudget::new(200);
            enumerate_shortest_paths(
                graph(&nodes, &edges),
                TrailDirection::Right,
                (1, None),
                mode,
                4,
                &mut budget,
            )
            .unwrap();
            assert!(budget.memory_peak() > 0);
            assert_eq!(budget.memory_used(), 0);

            let mut zero = TrailBudget::new(200).with_memory_limit(0);
            let error = enumerate_shortest_paths(
                graph(&nodes, &edges),
                TrailDirection::Right,
                (1, None),
                mode,
                4,
                &mut zero,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                Error::ResourceLimit(message)
                    if message.contains("~4") && message.contains("0-byte retained-memory budget")
            ));
        }
    }

    #[test]
    fn end_domain_filters_emitted_rows_without_pruning_states() {
        let nodes = vec!["a".into()];
        let edges = vec![edge("ab", "a", "b"), edge("bc", "b", "c")];
        let end_domain = BTreeSet::from(["c".to_owned()]);

        let mut unfiltered_budget = TrailBudget::new(10);
        let unfiltered = enumerate_trails(
            graph(&nodes, &edges),
            TrailDirection::Right,
            0,
            None,
            0,
            &mut unfiltered_budget,
        )
        .unwrap();
        let mut filtered_budget = TrailBudget::new(10);
        let filtered = enumerate_trails(
            ExpansionGraph {
                end_domain: Some(&end_domain),
                ..graph(&nodes, &edges)
            },
            TrailDirection::Right,
            0,
            None,
            0,
            &mut filtered_budget,
        )
        .unwrap();

        assert_eq!(decode_rows(&unfiltered).len(), 3);
        let rows = decode_rows(&filtered);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["end_id"], "c");
        assert_eq!(rows[0]["depth"], 2);
        assert_eq!(unfiltered_budget.state_used, filtered_budget.state_used);
        assert!(filtered_budget.payload_used() < unfiltered_budget.payload_used());
    }

    #[test]
    fn shortest_end_domain_filters_identity_and_cycle_rows() {
        let nodes = vec!["a".into()];
        let edges = vec![edge("ab", "a", "b"), edge("ba", "b", "a")];
        let only_b = BTreeSet::from(["b".to_owned()]);

        let mut budget = TrailBudget::new(20);
        let zero = enumerate_shortest_paths(
            ExpansionGraph {
                end_domain: Some(&only_b),
                ..graph(&nodes, &edges)
            },
            TrailDirection::Right,
            (0, None),
            ShortestMode::All,
            0,
            &mut budget,
        )
        .unwrap();
        let rows = decode_rows(&zero);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["end_id"], "b");

        let mut budget = TrailBudget::new(20);
        let positive = enumerate_shortest_paths(
            ExpansionGraph {
                end_domain: Some(&only_b),
                ..graph(&nodes, &edges)
            },
            TrailDirection::Right,
            (1, None),
            ShortestMode::All,
            0,
            &mut budget,
        )
        .unwrap();
        let rows = decode_rows(&positive);
        assert_eq!(rows.len(), 1, "the a->b->a cycle ends outside the domain");
        assert_eq!(rows[0]["end_id"], "b");
    }
}
