# Reading with cursors

Prepared reads let applications process rows without collecting the complete
result in Rust. The `execute` APIs keep their existing eager behavior.

```rust
use crabnet::{Database, Parameters, QueryOptions, ReadMode};
use serde_json::json;

let mut database = Database::in_memory()?;
database.execute("CREATE (:Person {name: 'Ada', active: true})")?;
let parameters = Parameters::from([("active".into(), json!(true))]);
let options = QueryOptions {
    max_result_rows: Some(10_000),
    ..QueryOptions::default()
};
let mut read = database.prepare_read_with_options(
    "MATCH (p:Person {active: $active}) RETURN p.name",
    &parameters,
    &options,
)?;
assert_eq!(read.mode(), ReadMode::Incremental);
assert_eq!(read.columns(), &["p.name"]);
{
    let mut cursor = read.cursor()?;
    if let Some(row) = cursor.next() {
        println!("First row: {:?}", row?);
    }
} // early drop resets the statement and releases its snapshot
drop(read); // releases the exclusive database borrow
# Ok::<(), crabnet::Error>(())
```

Inspect `mode()` before opening a cursor. `Incremental` means a single SQLite
fragment without Rust producers; Rust decodes one row per advance. SQLite may
still buffer sorts, aggregates, or intermediate relations internally.
`Buffered` means the existing executor completes the result when `cursor()`
is called. UNION, Rust traversal, semantic result shaping, and graph-value
hydration currently use that fallback.

A cursor yields owned rows and can report an error after earlier rows were
delivered. An error ends iteration; it does not undo work the application did
with previous rows. To require the entire read to succeed before consuming its
results, use `execute` or collect into `crabnet::Result<Vec<_>>` first.

Each new cursor gets a fresh snapshot and resource counters, using the same
parameters and options. Only one cursor may borrow the prepared read at a time.
An incremental cursor pins its snapshot at creation, even before its first
advance. Drop idle cursors promptly: they can block other connections' commits.
See the [complete contract](semantics.md#read-cursors) for buffering boundaries,
partial errors, cancellation, and resource accounting.

## Measurements

`examples/cursor_scan.rs` creates a small in-memory fixture with `n` nodes,
then reads an `n × n` Cartesian bag with a 1,024-byte string and an integer in
every result row. Modes are eager collection, full cursor consumption with
rows discarded immediately, and first-row consumption followed by early drop.
Full scans validate their row count and integer checksum; every consumed row
validates its types and payload length. A test exercises all three modes.

Build separately and run each mode in a fresh process, sequentially:

```sh
cargo build --release --example cursor_scan
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_scan eager 500 1024
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_scan cursor 500 1024
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_scan first 500 1024
```

The existing measurement wrapper accepts any JSON-reporting example. It adds
child-process peak RSS, platform, command, and binary hash. Query timing begins
after fixture creation and includes planning, snapshot setup, decoding,
validation, and cleanup. First-row latency is measured at the first row visible
to the caller. RSS includes the whole process and fixture; it is not a retained
heap measurement or a guarantee for other operators. Run `100 1024` as well to
compare 10,000 result rows with the 250,000-row case.

Recorded on 2026-10-04, macOS 26.5.1 arm64, release build, three fresh
processes per cell. Values below are medians; payload size is 1,024 bytes.

| Potential rows | Mode | Consumed rows | First row (ms) | Complete query (ms) | Peak RSS (MiB) |
| ---: | --- | ---: | ---: | ---: | ---: |
| 10,000 | Eager | 10,000 | 41.608 | 45.484 | 22.469 |
| 10,000 | Cursor | 10,000 | 0.178 | 32.295 | 7.703 |
| 10,000 | First row | 1 | 0.190 | 0.198 | 7.703 |
| 250,000 | Eager | 250,000 | 1,738.078 | 2,186.329 | 385.328 |
| 250,000 | Cursor | 250,000 | 0.366 | 1,386.799 | 15.312 |
| 250,000 | First row | 1 | 0.406 | 0.419 | 15.406 |

For the 250,000-row case, eager RSS ranged from 385.109–385.406 MiB and
cursor RSS from 15.266–15.344 MiB. Cursor first-row latency ranged from
0.361–0.789 ms. Full-query times varied substantially: 1,924–2,258 ms eager
and 1,379–2,354 ms cursor, so these trials support the memory and first-row
benefits without establishing a general throughput improvement. The larger
fixture itself accounts for some RSS growth in all modes. Sorted, aggregated,
hybrid, and buffered plans need their own measurements.

Measured executable SHA-256:
`8f8fd4bbffea7662dd0bf0a2826ee67f56f1f88f34686461e270028d9dad625d`.

Criterion also includes `read/cursor_scan_properties/<nodes>` and
`read/cursor_first_row/<nodes>` alongside the equivalent eager property scan.
Those cases use the existing social graph at 1,000 and 10,000 nodes and include
preparation and cursor cleanup on each iteration.
