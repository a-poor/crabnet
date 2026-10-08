//! Compare eager and cursor hydration using repeated paths or node lists.
//! Run each mode separately via scripts/measure_dependency_graph.py.
use std::{error::Error, hint::black_box, time::Instant};

use crabnet::{CypherValue, Database, Parameters};
use serde_json::json;

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 4
        || !matches!(args[0].as_str(), "eager" | "cursor" | "first")
        || !matches!(args[1].as_str(), "path" | "nodes")
    {
        return Err(
            "usage: cursor_hydration <eager|cursor|first> <path|nodes> <rows> <payload-bytes>"
                .into(),
        );
    }
    let rows: usize = args[2].parse()?;
    let bytes: usize = args[3].parse()?;
    if !(1..=1_000_000).contains(&rows) || bytes > 65_536 {
        return Err("rows must be 1..=1000000 and payload bytes <= 65536".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&measure(&args[0], &args[1], rows, bytes)?)?
    );
    Ok(())
}

fn measure(
    mode: &str,
    projection: &str,
    rows: usize,
    bytes: usize,
) -> crabnet::Result<serde_json::Value> {
    let mut db = Database::in_memory()?;
    let parameters = Parameters::from([
        ("last".into(), json!(rows)),
        ("payload".into(), json!("x".repeat(bytes))),
    ]);
    db.execute_with(
        "CREATE (:Start {payload:$payload})-[:LINK {payload:$payload}]->(:End {payload:$payload})",
        &parameters,
    )?;
    let query = format!(
        "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,$last) AS i RETURN {},i",
        if projection == "path" {
            "p"
        } else {
            "nodes(p)"
        }
    );
    let started = Instant::now();
    let mut first_row_ms = None;
    let mut consumed = 0;
    let mut consume = |row: Vec<CypherValue>| {
        first_row_ms.get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
        let check_payload = |props: &std::collections::BTreeMap<String, CypherValue>| {
            let CypherValue::String(payload) = &props["payload"] else {
                panic!("expected payload");
            };
            assert_eq!(payload.len(), bytes);
        };
        match &row[0] {
            CypherValue::Path(path) if projection == "path" => {
                assert_eq!(path.nodes.len(), 2);
                assert_eq!(path.relationships.len(), 1);
                for node in &path.nodes {
                    check_payload(&node.properties);
                }
                check_payload(&path.relationships[0].properties);
                assert_eq!(
                    path.relationships[0].start_node_element_id,
                    path.nodes[0].element_id
                );
                assert_eq!(
                    path.relationships[0].end_node_element_id,
                    path.nodes[1].element_id
                );
            }
            CypherValue::List(nodes) if projection == "nodes" => {
                assert_eq!(nodes.len(), 2);
                for node in nodes {
                    let CypherValue::Node(node) = node else {
                        panic!("expected node");
                    };
                    check_payload(&node.properties);
                }
            }
            other => panic!("unexpected graph value: {other:?}"),
        }
        consumed += 1;
        assert_eq!(row[1], CypherValue::Integer(consumed as i64));
        black_box(row);
    };
    let read_mode;
    if mode == "eager" {
        read_mode = "eager".to_owned();
        for row in db.execute_with(&query, &parameters)?.rows {
            consume(row);
        }
    } else {
        let mut read = db.prepare_read_with(&query, &parameters)?;
        read_mode = format!("{:?}", read.mode());
        let mut cursor = read.cursor()?;
        if mode == "first" {
            consume(cursor.next().expect("nonempty fixture")?);
        } else {
            for row in cursor {
                consume(row?);
            }
        }
    }
    assert_eq!(consumed, if mode == "first" { 1 } else { rows });
    Ok(
        json!({"mode":mode,"read_mode":read_mode,"projection":projection,"potential_rows":rows,"payload_bytes":bytes,"consumed_rows":consumed,"first_row_ms":first_row_ms,"query_ms":started.elapsed().as_secs_f64()*1000.0}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graph_values_and_counts_are_checked_across_batches() {
        for projection in ["path", "nodes"] {
            for mode in ["eager", "cursor", "first"] {
                for rows in [1, 2, 130] {
                    measure(mode, projection, rows, 32).unwrap();
                }
            }
        }
    }
}
