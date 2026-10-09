use crabnet::{Database, Parameters, QueryOptions, TraversalLimits};
use serde_json::json;

pub const QUERY: &str = "MATCH p=(:Start)-[:NEXT*]->(b) RETURN length(p), b.id";

// Two parallel relationships per hop produce 2^d distinct trails at depth d.
// Graph input stays small while the potential runtime relation grows.
pub fn fixture(depth: usize) -> crabnet::Result<Database> {
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
