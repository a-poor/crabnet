//! Compare buffered and incremental Rust traversal consumption in fresh processes.
//! See docs/cursors.md for the fixture, commands, and resource boundaries.
use std::{error::Error, hint::black_box, time::Instant};

use crabnet::{CypherValue, Parameters};
use serde_json::json;

#[path = "cursor_traversal/fixture.rs"]
mod fixture;

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if !(2..=5).contains(&args.len()) || !matches!(args[0].as_str(), "eager" | "cursor" | "first") {
        return Err(
            "usage: cursor_traversal <eager|cursor|first> <depth:1..16> [unrelated:0..100000] [direct|with|unwind-nodes|unwind|unwind-chain-nodes|unwind-chain] [fanout:1..100000]"
                .into(),
        );
    }
    let depth: usize = args[1].parse()?;
    let unrelated: usize = args
        .get(2)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(0);
    if !(1..=16).contains(&depth) || unrelated > 100_000 {
        return Err("depth must be 1..16; unrelated must be <= 100000".into());
    }
    let shape = args.get(3).map_or("direct", String::as_str);
    if !matches!(
        shape,
        "direct" | "with" | "unwind-nodes" | "unwind" | "unwind-chain-nodes" | "unwind-chain"
    ) {
        return Err(
            "shape must be direct, with, unwind-nodes, unwind, unwind-chain-nodes or unwind-chain"
                .into(),
        );
    }
    let fanout: usize = args
        .get(4)
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(128);
    if !(1..=100_000).contains(&fanout) {
        return Err("fanout must be 1..100000".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&measure(&args[0], depth, unrelated, shape, fanout)?)?
    );
    Ok(())
}

fn measure(
    mode: &str,
    depth: usize,
    unrelated: usize,
    shape: &str,
    fanout: usize,
) -> crabnet::Result<serde_json::Value> {
    let setup = Instant::now();
    let mut db = if unrelated == 0 {
        fixture::fixture(depth)?
    } else {
        fixture::fixture_with_unrelated(depth, unrelated)?
    };
    let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
    let options = fixture::options();
    let query = match shape {
        "with" => fixture::WITH_QUERY,
        "unwind-nodes" => fixture::UNWIND_NODES_QUERY,
        "unwind" => fixture::UNWIND_QUERY,
        "unwind-chain-nodes" => fixture::UNWIND_CHAIN_NODES_QUERY,
        "unwind-chain" => fixture::UNWIND_CHAIN_QUERY,
        _ => fixture::QUERY,
    };
    let parameters = Parameters::from([("fanout".into(), json!(fanout))]);
    let started = Instant::now();
    let mut first_row_ms = None;
    let mut histogram = vec![0usize; depth + 1];
    let mut member_checks = vec![(0u128, 0u128); depth + 1];
    let mut chain_checks = vec![[0u128; 5]; depth + 1];
    let mut consume = |row: Vec<CypherValue>| {
        first_row_ms.get_or_insert_with(|| started.elapsed().as_secs_f64() * 1000.0);
        let [CypherValue::Integer(hops), CypherValue::Integer(endpoint)] = &row[..2] else {
            panic!("unexpected row: {row:?}")
        };
        assert_eq!(hops, endpoint);
        if shape.starts_with("unwind-chain") {
            let [CypherValue::Integer(source), CypherValue::Integer(member)] = &row[2..] else {
                panic!("invalid chained members")
            };
            assert!(if shape == "unwind-chain-nodes" {
                (0..=*hops).contains(source) && (0..=*source).contains(member)
            } else {
                (1..=fanout as i64).contains(source)
                    && (*source..*source + fanout as i64).contains(member)
            });
            let (source, member) = (*source as u128, *member as u128);
            for (sum, value) in chain_checks[*hops as usize].iter_mut().zip([
                source,
                source * source,
                member,
                member * member,
                source * member,
            ]) {
                *sum += value;
            }
        } else if shape.starts_with("unwind") {
            let CypherValue::Integer(member) = row[2] else {
                panic!("invalid member")
            };
            assert!(if shape == "unwind-nodes" {
                (0..=*hops).contains(&member)
            } else {
                (1..=fanout as i64).contains(&member)
            });
            member_checks[*hops as usize].0 += member as u128;
            member_checks[*hops as usize].1 += (member as u128).pow(2);
        }
        assert!((1..=depth as i64).contains(hops));
        histogram[*hops as usize] += 1;
        black_box(row);
    };
    let mut read_mode = None;
    if mode == "eager" {
        for row in db.execute_with_options(query, &parameters, &options)?.rows {
            consume(row);
        }
    } else {
        let mut read = db.prepare_read_with_options(query, &parameters, &options)?;
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
        assert_eq!(consumed_rows, fixture::output_rows(depth, shape, fanout));
        for (hops, count) in histogram.iter().enumerate().skip(1) {
            let last = match shape {
                "unwind-nodes" => hops,
                "unwind" => fanout,
                _ => 0,
            } as u128;
            let multiplier = match shape {
                "unwind-nodes" => hops + 1,
                "unwind" => fanout,
                "unwind-chain-nodes" => (hops + 1) * (hops + 2) / 2,
                "unwind-chain" => fanout * fanout,
                _ => 1,
            };
            assert_eq!(*count, (1 << hops) * multiplier);
            if shape.starts_with("unwind-chain") {
                let mut expected = [0u128; 5];
                let sources = if shape == "unwind-chain-nodes" {
                    0..=hops
                } else {
                    1..=fanout
                };
                for source in sources {
                    let (start, end) = if shape == "unwind-chain-nodes" {
                        (0, source)
                    } else {
                        (source, source + fanout - 1)
                    };
                    let (source, start, end) = (source as u128, start as u128, end as u128);
                    let count = end - start + 1;
                    let sum = (start + end) * count / 2;
                    let squares = |n: u128| n * (n + 1) * (2 * n + 1) / 6;
                    for (total, value) in expected.iter_mut().zip([
                        source * count,
                        source * source * count,
                        sum,
                        squares(end) - squares(start.saturating_sub(1)),
                        source * sum,
                    ]) {
                        *total += value;
                    }
                }
                assert_eq!(
                    chain_checks[hops],
                    expected.map(|value| value * (1 << hops))
                );
            } else if shape.starts_with("unwind") {
                assert_eq!(
                    member_checks[hops],
                    (
                        (1 << hops) * last * (last + 1) / 2,
                        (1 << hops) * last * (last + 1) * (2 * last + 1) / 6
                    )
                );
            }
        }
    }
    Ok(
        json!({"mode":mode,"read_mode":read_mode,"shape":shape,"depth":depth,"nodes":depth+1+unrelated,"edges":depth*2+unrelated,
        "unrelated_edges":unrelated,"reachable_nodes":depth+1,"reachable_edges":depth*2,
        "potential_trails":fixture::potential_rows(depth),"fanout":fanout,"potential_rows":fixture::output_rows(depth,shape,fanout),"consumed_rows":consumed_rows,"depth_histogram":histogram,
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
                let report = measure(mode, depth, 0, "direct", 128).unwrap();
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

    #[test]
    fn unrelated_edges_preserve_the_reachable_bag() {
        for unrelated in [0, 10] {
            for mode in ["eager", "cursor", "first"] {
                let report = measure(mode, 4, unrelated, "direct", 128).unwrap();
                assert_eq!(report["edges"], 8 + unrelated);
                assert_eq!(report["nodes"], 5 + unrelated);
            }
        }
    }

    #[test]
    fn with_stages_preserve_the_depth_histogram() {
        for mode in ["eager", "cursor", "first"] {
            let report = measure(mode, 7, 10, "with", 128).unwrap();
            assert_eq!(
                report["consumed_rows"],
                if mode == "first" { 1 } else { 254 }
            );
        }
    }
    #[test]
    fn unwind_members_preserve_multiplicity_and_member_checksums() {
        for shape in ["unwind-nodes", "unwind"] {
            for mode in ["eager", "cursor", "first"] {
                let report = measure(mode, 5, 0, shape, 128).unwrap();
                assert_eq!(
                    report["consumed_rows"],
                    if mode == "first" {
                        1
                    } else {
                        fixture::output_rows(5, shape, 128)
                    }
                );
                if mode != "eager" {
                    assert_eq!(report["read_mode"], "Incremental");
                }
            }
        }
    }
    #[test]
    fn chained_members_preserve_parent_child_counts_and_correlations() {
        for shape in ["unwind-chain-nodes", "unwind-chain"] {
            for mode in ["eager", "cursor", "first"] {
                let report = measure(mode, 5, 0, shape, 8).unwrap();
                assert_eq!(
                    report["consumed_rows"],
                    if mode == "first" {
                        1
                    } else {
                        fixture::output_rows(5, shape, 8)
                    }
                );
                if mode != "eager" {
                    assert_eq!(report["read_mode"], "Incremental");
                }
            }
        }
    }
}
