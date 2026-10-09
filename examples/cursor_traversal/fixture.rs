use crabnet::{Database, Parameters, QueryOptions, TraversalLimits};
use serde_json::json;

pub const QUERY: &str = "MATCH p=(:Start)-[:NEXT*]->(b) RETURN length(p), b.id";

// Two parallel relationships per hop produce 2^d distinct trails at depth d.
// Graph input stays small while the potential runtime relation grows.
pub fn fixture(depth: usize) -> crabnet::Result<Database> {
    fixture_with_unrelated(depth, 0)
}

// Each disconnected node has one eligible self-loop. The reachable component
// and its output bag stay fixed while the whole eligible graph grows.
pub fn fixture_with_unrelated(depth: usize, unrelated: usize) -> crabnet::Result<Database> {
    let mut db = Database::in_memory()?;
    for id in 0..=depth {
        db.execute_with(
            "CREATE (:Step {id:$id})",
            &Parameters::from([("id".into(), json!(id))]),
        )?;
    }
    db.execute("MATCH (n:Step {id:0}) SET n:Start")?;
    for from in 0..depth {
        db.execute_with(
            "MATCH (a:Step {id:$from}),(b:Step {id:$to})
             CREATE (a)-[:NEXT]->(b),(a)-[:NEXT]->(b)",
            &Parameters::from([("from".into(), json!(from)), ("to".into(), json!(from + 1))]),
        )?;
    }
    if unrelated > 0 {
        db.execute_with(
            "UNWIND range(1, $unrelated) AS i CREATE (n:Unrelated)-[:NEXT]->(n)",
            &Parameters::from([("unrelated".into(), json!(unrelated))]),
        )?;
    }
    Ok(db)
}

pub fn potential_rows(depth: usize) -> usize {
    (1 << (depth + 1)) - 2
}

pub fn options() -> QueryOptions {
    QueryOptions {
        traversal: TraversalLimits {
            max_states: 1_000_000,
            max_payload_bytes: 1024 * 1024 * 1024,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    }
}
