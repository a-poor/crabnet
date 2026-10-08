//! Measure DISTINCT and UNION with controlled input and unique-row counts.
//! Run each mode in a fresh process via scripts/measure_dependency_graph.py.
use std::{error::Error, hint::black_box, time::Instant};

use crabnet::{CypherValue, Database, Parameters, ReadMode};
use serde_json::json;

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 5
        || !matches!(args[0].as_str(), "eager" | "cursor" | "first")
        || !matches!(args[1].as_str(), "distinct" | "union")
    {
        return Err("usage: cursor_distinct <eager|cursor|first> <distinct|union> <input-rows> <unique-rows> <payload-bytes>".into());
    }
    let rows: usize = args[2].parse()?;
    let unique: usize = args[3].parse()?;
    let bytes: usize = args[4].parse()?;
    if !(2..=1_000_000).contains(&rows) || !(1..=rows).contains(&unique) || bytes > 65_536 {
        return Err("input rows must be 2..=1000000, unique rows 1..=input rows, and payload bytes <= 65536".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&measure(&args[0], &args[1], rows, unique, bytes)?)?
    );
    Ok(())
}

fn measure(
    mode: &str,
    operator: &str,
    rows: usize,
    unique: usize,
    payload_bytes: usize,
) -> crabnet::Result<serde_json::Value> {
    let mut database = Database::in_memory()?;
    let query = match operator {
        "distinct" => {
            "UNWIND range(0,$last) AS i RETURN DISTINCT $payload AS payload,i % $unique AS x"
        }
        "union" => {
            "UNWIND range(0,$split-1) AS i RETURN $payload AS payload,i % $unique AS x UNION UNWIND range($split,$last) AS i RETURN $payload AS payload,i % $unique AS x"
        }
        _ => unreachable!("validated operator"),
    };
    let parameters = Parameters::from([
        ("last".into(), json!(rows - 1)),
        ("split".into(), json!(rows / 2)),
        ("unique".into(), json!(unique)),
        ("payload".into(), json!("x".repeat(payload_bytes))),
    ]);
    let started = Instant::now();
    let mut first_row_ms = None;
    let mut consumed = 0;
    let mut consume = |row: Vec<CypherValue>| {
        first_row_ms.get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
        let [CypherValue::String(payload), CypherValue::Integer(x)] = row.as_slice() else {
            panic!("unexpected result: {row:?}");
        };
        assert_eq!(payload.len(), payload_bytes);
        // First occurrences run from 0 through unique-1, in that order.
        assert_eq!(*x as usize, consumed);
        consumed += 1;
        black_box(row);
    };
    if mode == "eager" {
        for row in database.execute_with(query, &parameters)?.rows {
            consume(row);
        }
    } else {
        let mut read = database.prepare_read_with(query, &parameters)?;
        assert_eq!(read.mode(), ReadMode::Incremental);
        let mut cursor = read.cursor()?;
        if mode == "first" {
            consume(cursor.next().expect("nonempty input")?);
        } else {
            for row in cursor {
                consume(row?);
            }
        }
    }
    let query_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(consumed, if mode == "first" { 1 } else { unique });
    Ok(
        json!({"mode":mode,"operator":operator,"input_rows":rows,"unique_rows":unique,
        "payload_bytes":payload_bytes,"consumed_rows":consumed,"first_row_ms":first_row_ms,"query_ms":query_ms}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_operators_validate_duplicate_heavy_and_mostly_unique_results() {
        for operator in ["distinct", "union"] {
            for unique in [1, 3, 15, 16] {
                for mode in ["eager", "cursor", "first"] {
                    let report = measure(mode, operator, 16, unique, 64).unwrap();
                    assert_eq!(
                        report["consumed_rows"],
                        if mode == "first" { 1 } else { unique }
                    );
                }
            }
        }
    }
}
