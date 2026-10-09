use crabnet::{Database, Parameters};
use serde_json::json;

pub const QUERY: &str =
    "MATCH p = shortestPath((:Step {i: 0})-[:NEXT*]->(:Step {i: 63})) RETURN length(p)";

/// A fixed 64-node component with edges to the next one and two nodes,
/// surrounded by disconnected nodes with one eligible self-loop each.
pub fn graph(unrelated: usize) -> crabnet::Result<Database> {
    let mut database = Database::in_memory()?;
    let mut patterns = (0..64)
        .map(|i| format!("(n{i}:Step {{i: {i}}})"))
        .collect::<Vec<_>>();
    for i in 0..64 {
        for j in i + 1..=(i + 2).min(63) {
            patterns.push(format!("(n{i})-[:NEXT]->(n{j})"));
        }
    }
    database.execute(&format!("CREATE {}", patterns.join(", ")))?;
    if unrelated > 0 {
        database.execute_with(
            "UNWIND range(1, $unrelated) AS i CREATE (n:Unrelated)-[:NEXT]->(n)",
            &Parameters::from([("unrelated".into(), json!(unrelated))]),
        )?;
    }
    Ok(database)
}
