//! Compare eager collection, cursor draining, and early drop in separate processes.
//! See docs/cursors.md for measurement commands and interpretation.

use std::{error::Error, hint::black_box, time::Instant};

use crabnet::{CypherValue, Database, Parameters, ReadMode};
use serde_json::json;

const SCAN: &str = "MATCH (a:Item), (b:Item) RETURN a.payload, b.x";

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 3 || !matches!(args[0].as_str(), "eager" | "cursor" | "first") {
        return Err("usage: cursor_scan <eager|cursor|first> <nodes> <payload-bytes>".into());
    }
    let nodes: usize = args[1].parse()?;
    let payload_bytes: usize = args[2].parse()?;
    if !(1..=10_000).contains(&nodes) || payload_bytes > 65_536 {
        return Err("nodes must be 1..=10000 and payload bytes <= 65536".into());
    }
    let report = measure(&args[0], nodes, payload_bytes)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn measure(mode: &str, nodes: usize, payload_bytes: usize) -> crabnet::Result<serde_json::Value> {
    let mut database = Database::in_memory()?;
    database.execute_with(
        "UNWIND range(0, $last) AS x CREATE (:Item {x: x, payload: $payload})",
        &Parameters::from([
            ("last".into(), json!(nodes - 1)),
            ("payload".into(), json!("x".repeat(payload_bytes))),
        ]),
    )?;
    // The small fixture is the same in each process; n nodes produce n² rows.
    // Timings include planning and cleanup, but exclude database creation.
    let started = Instant::now();
    let mut first_row_ms = None;
    let mut rows = 0_usize;
    let mut checksum = 0_u64;
    let mut consume = |row: Vec<CypherValue>| {
        first_row_ms.get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
        let [CypherValue::String(payload), CypherValue::Integer(x)] = row.as_slice() else {
            panic!("unexpected scan result: {row:?}");
        };
        assert_eq!(payload.len(), payload_bytes);
        assert!(*x >= 0 && (*x as usize) < nodes);
        rows += 1;
        checksum += *x as u64;
        black_box(row);
    };
    if mode == "eager" {
        for row in database.execute(SCAN)?.rows {
            consume(row);
        }
    } else {
        let mut read = database.prepare_read(SCAN)?;
        assert_eq!(read.mode(), ReadMode::Incremental);
        let mut cursor = read.cursor()?;
        if mode == "first" {
            consume(cursor.next().expect("fixture is nonempty")?);
        } else {
            for row in cursor {
                consume(row?);
            }
        }
    }
    let query_ms = started.elapsed().as_secs_f64() * 1000.0;
    if mode == "first" {
        assert_eq!(rows, 1);
    } else {
        assert_eq!(rows, nodes * nodes);
        assert_eq!(
            checksum,
            nodes as u64 * (nodes as u64 * (nodes as u64 - 1) / 2)
        );
    }
    Ok(json!({
        "mode": mode, "nodes": nodes, "payload_bytes": payload_bytes,
        "potential_rows": nodes * nodes, "consumed_rows": rows, "checksum": checksum,
        "first_row_ms": first_row_ms, "query_ms": query_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_check_full_result_counts_and_early_exit() {
        for mode in ["eager", "cursor", "first"] {
            let report = measure(mode, 4, 64).unwrap();
            assert_eq!(
                report["consumed_rows"],
                if mode == "first" { 1 } else { 16 }
            );
        }
    }
}
