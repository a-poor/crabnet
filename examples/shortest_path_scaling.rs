//! Measure a fixed shortest path while disconnected eligible edges grow.
//! Build with `cargo build --release --example shortest_path_scaling`, then
//! run the binary with `<unrelated-nodes> <samples>` (for example `10000 100`).
#[path = "shortest_path_scaling/fixture.rs"]
mod fixture;

use std::{error::Error, time::Instant};

use crabnet::CypherValue;
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        return Err("usage: shortest_path_scaling <unrelated-nodes> <samples>".into());
    }
    let unrelated = args[0].parse()?;
    let samples = args[1].parse()?;
    if unrelated > 100_000 || !(1..=10_000).contains(&samples) {
        return Err("unrelated nodes must be <= 100000; samples must be 1..=10000".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&measure(unrelated, samples)?)?
    );
    Ok(())
}

fn measure(unrelated: usize, samples: usize) -> crabnet::Result<Value> {
    let setup = Instant::now();
    let mut database = fixture::graph(unrelated)?;
    let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
    let mut times = Vec::with_capacity(samples + 1);
    for _ in 0..=samples {
        let started = Instant::now();
        let result = database.execute(fixture::QUERY)?;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(result.rows, [vec![CypherValue::Integer(32)]]);
    }
    // Check actual path identities and every hop outside the timed length query.
    let result = database.execute(&fixture::QUERY.replace("length(p)", "p"))?;
    let CypherValue::Path(path) = &result.rows[0][0] else {
        panic!("expected path");
    };
    assert_eq!(path.relationships.len(), 32);
    assert_eq!(path.nodes.len(), 33);
    assert_eq!(path.nodes[0].properties["i"], CypherValue::Integer(0));
    assert_eq!(path.nodes[32].properties["i"], CypherValue::Integer(63));
    for (pair, edge) in path.nodes.windows(2).zip(&path.relationships) {
        let (CypherValue::Integer(from), CypherValue::Integer(to)) =
            (&pair[0].properties["i"], &pair[1].properties["i"])
        else {
            panic!("expected integer node indices");
        };
        assert!((1..=2).contains(&(to - from)));
        assert_eq!(edge.start_node_element_id, pair[0].element_id);
        assert_eq!(edge.end_node_element_id, pair[1].element_id);
    }
    let first_ms = times.remove(0);
    let ordered = {
        let mut ordered = times.clone();
        ordered.sort_by(f64::total_cmp);
        ordered
    };
    Ok(json!({
        "unrelated_nodes": unrelated,
        "total_nodes": unrelated + 64,
        "total_edges": unrelated + 125,
        "reachable_nodes": 64,
        "hops": 32,
        "setup_ms": setup_ms,
        "first_ms": first_ms,
        "p50_ms": ordered[(samples * 50).div_ceil(100) - 1],
        "p95_ms": ordered[(samples * 95).div_ceil(100) - 1],
        "samples_ms": times,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_path_survives_unrelated_growth() {
        for unrelated in [0, 10] {
            let report = measure(unrelated, 2).unwrap();
            assert_eq!(report["hops"], 32);
            assert_eq!(report["total_edges"], unrelated + 125);
        }
    }
}
