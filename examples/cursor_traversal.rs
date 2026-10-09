//! Compare buffered and incremental Rust traversal consumption in fresh processes.
//! See docs/cursors.md for the fixture, commands, and resource boundaries.
use std::{error::Error, hint::black_box, time::Instant};

use crabnet::{CypherValue, Parameters};
use serde_json::json;

#[path = "cursor_traversal/fixture.rs"]
mod fixture;

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2 || !matches!(args[0].as_str(), "eager" | "cursor" | "first") {
        return Err("usage: cursor_traversal <eager|cursor|first> <depth:1..16>".into());
    }
    let depth: usize = args[1].parse()?;
    if !(1..=16).contains(&depth) {
        return Err("depth must be 1..16".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&measure(&args[0], depth)?)?
    );
    Ok(())
}

fn measure(mode: &str, depth: usize) -> crabnet::Result<serde_json::Value> {
    let setup = Instant::now();
    let mut db = fixture::fixture(depth)?;
    let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
    let options = fixture::options();
    let started = Instant::now();
    let mut first_row_ms = None;
    let mut histogram = vec![0usize; depth + 1];
    let mut consume = |row: Vec<CypherValue>| {
        first_row_ms.get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
        let [CypherValue::Integer(hops), CypherValue::Integer(endpoint)] = row.as_slice() else {
            panic!("unexpected row: {row:?}")
        };
        assert_eq!(hops, endpoint);
        assert!((1..=depth as i64).contains(hops));
        histogram[*hops as usize] += 1;
        black_box(row);
    };
    let mut read_mode = None;
    if mode == "eager" {
        for row in db
            .execute_with_options(fixture::QUERY, &Parameters::new(), &options)?
            .rows
        {
            consume(row);
        }
    } else {
        let mut read =
            db.prepare_read_with_options(fixture::QUERY, &Parameters::new(), &options)?;
        read_mode = Some(format!("{:?}", read.mode()));
        let mut cursor = read.cursor()?;
        if mode == "first" {
            consume(cursor.next().expect("nonempty fixture")?);
        } else {
            for row in cursor {
                consume(row?);
            }
        }
    }
    let query_ms = started.elapsed().as_secs_f64() * 1000.0;
    let consumed_rows: usize = histogram.iter().sum();
    if mode == "first" {
        assert_eq!(consumed_rows, 1);
        assert_eq!(histogram[1], 1);
    } else {
        assert_eq!(consumed_rows, fixture::potential_rows(depth));
        for (hops, count) in histogram.iter().enumerate().skip(1) {
            assert_eq!(*count, 1 << hops);
        }
    }
    Ok(
        json!({"mode":mode,"read_mode":read_mode,"depth":depth,"nodes":depth+1,"edges":depth*2,
        "potential_rows":fixture::potential_rows(depth),"consumed_rows":consumed_rows,"depth_histogram":histogram,
        "setup_ms":setup_ms,"first_row_ms":first_row_ms,"query_ms":query_ms}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_mode_validates_trail_multiplicity_and_early_exit() {
        for depth in [1, 4, 7] {
            for mode in ["eager", "cursor", "first"] {
                let report = measure(mode, depth).unwrap();
                assert_eq!(
                    report["consumed_rows"],
                    if mode == "first" {
                        1
                    } else {
                        fixture::potential_rows(depth)
                    }
                );
                if mode != "eager" {
                    assert_eq!(report["read_mode"], "Incremental");
                }
            }
        }
    }
}
