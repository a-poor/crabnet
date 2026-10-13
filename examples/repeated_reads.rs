//! Run `cargo run --release --example repeated_reads -- ordinary 10000` or
//! `... -- compiled 10000`. Each iteration changes OID and performs two reads.
#[path = "repeated_reads/fixture.rs"]
mod fixture;

use fixture::Fixture;
use std::{env, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let mode = arguments.first().map(String::as_str).unwrap_or("compiled");
    let compiled = match mode {
        "ordinary" => false,
        "compiled" => true,
        _ => return Err("mode must be ordinary or compiled".into()),
    };
    let iterations = arguments
        .get(1)
        .map(|v| v.parse::<usize>())
        .transpose()?
        .unwrap_or(10_000);
    if arguments.len() > 2 || !(4..=100_000).contains(&iterations) {
        return Err("usage: repeated_reads [ordinary|compiled] [iterations:4..100000]".into());
    }
    // Construction and compilation are outside execution timing in both modes.
    let mut fixture = Fixture::new()?;
    for i in 0..4 {
        Fixture::check(i, &fixture.read(compiled, i)?);
    }
    let start = Instant::now();
    let mut rows = 0;
    for i in 0..iterations {
        rows += Fixture::check(i, &fixture.read(compiled, i)?);
    }
    println!(
        "{}",
        serde_json::json!({"mode":mode, "iterations":iterations, "query_calls":iterations*2,
        "rows":rows, "elapsed_ms":start.elapsed().as_secs_f64()*1000.0, "all_rows_verified":true,
        "note":"four-node changing-OID microbenchmark; not merge-base performance qualification"})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_modes_match_complete_rows_and_fixture_oracles() {
        let mut fixture = Fixture::new().unwrap();
        for i in 0..12 {
            let ordinary = fixture.read(false, i).unwrap();
            let compiled = fixture.read(true, i).unwrap();
            Fixture::check(i, &ordinary);
            Fixture::check(i, &compiled);
            assert_eq!(ordinary, compiled);
        }
    }
}
