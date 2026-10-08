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

Inspect `mode()` before opening a cursor. `Incremental` means SQLite fragments
without Rust producers, composed through `UNION ALL`, `UNION`, `RETURN DISTINCT`,
pagination, and path/list hydration. Each advance yields at most one row, but can
consume several duplicate or skipped input rows. SQLite may still buffer sorts,
aggregates, or intermediate relations internally.
`Buffered` means the existing executor completes the result when `cursor()`
is called. Rust traversal producers use that fallback. A union with any buffered
branch uses the fallback for the whole query.

An incremental `UNION ALL` visits branches from left to right, keeping duplicate
rows and each branch's output types. All statements are prepared and parameters
resolved up front, but later branches do not execute until reached. One snapshot
and cumulative resource budget span every branch. Dropping early skips execution
and runtime validation beyond any hydration read-ahead. Prepared statements and
bindings still occupy memory proportional to the query's branches and parameters.

`RETURN DISTINCT` and distinct `UNION` emit the first representative of each
typed row, retaining seen values until their operator is released. Equal numeric
values and graph identities use the same rules as eager execution. Memory grows
with the number and size of unique rows; streaming distinct is not a bounded-memory
algorithm. Branch-level distinct and pagination finish before applying the
union's duplicate policy. `SKIP` and `LIMIT` count unique rows after distinct;
reaching a cursor's limit stops fetching that input, including runtime validation
of its unread suffix. Whole-query parameter checks still happen at preparation.

SQLite-produced paths, `nodes(p)`, `relationships(p)`, and legacy relationship
lists hydrate incrementally after distinct, pagination, and union processing.
The first batch contains one surviving row; later batches contain up to 64.
Each batch deduplicates required IDs, reuses directly projected entities, and
loads the remainder with at most one node query and one relationship query.
Only that batch's pending rows and entity cache are retained. Large individual
paths and DISTINCT seen sets can still use substantial memory.

Hydration reads ahead: requesting the second row can fetch up to 64 more
surviving rows, including rows from later union branches. A runtime or resource
error while filling or loading a batch discards that batch and ends iteration;
previously yielded rows remain valid. Drop after the first row avoids further
batch reads. Entity IDs repeated in later batches are fetched and charged again,
so full consumption can use more transfer/work budget than eager execution.
Final result limits apply one row at a time, before cloning hydrated values.

A cursor yields owned rows and can report an error after earlier rows were
delivered. An error ends iteration. In an application transaction it also rolls
back the transaction's writes, though it cannot undo external application work
done with previous rows. To require the entire read to succeed before consuming its
results, use `execute` or collect into `crabnet::Result<Vec<_>>` first.

Each new cursor gets fresh resource counters, using the same parameters and
options. Only one cursor may borrow the prepared read at a time.
A standalone incremental cursor pins a fresh snapshot at creation, even before
its first advance. Drop idle cursors promptly: they can block other connections' commits.
See the [complete contract](semantics.md#read-cursors) for buffering boundaries,
partial errors, cancellation, and resource accounting.

## Inside an application transaction

`Transaction::prepare_read`, `prepare_read_with`, and `prepare_read_with_options`
return the same `PreparedRead` and `ReadCursor` types. They reuse the existing
transaction and see earlier writes, including paths and graph lists:

```rust
use crabnet::Database;

let mut database = Database::in_memory()?;
let mut tx = database.transaction()?;
tx.execute("CREATE (:Person {name:'Ada'})-[:KNOWS]->(:Person {name:'Grace'})")?;
let mut read = tx.prepare_read("MATCH p=(:Person)-[:KNOWS]->() RETURN p")?;
for row in read.cursor()? {
    let row = row?;
    println!("Provisional path: {:?}", row[0]);
}
drop(read);
tx.execute("MATCH (p:Person) SET p.checked = true")?;
tx.commit()?;
# Ok::<(), crabnet::Error>(())
```

Drop the prepared read before querying, mutating, committing, or rolling back
through the parent handle. Rust borrowing enforces this even after a cursor is
exhausted. Repeated cursors reset all per-query counters and operator state;
they use the same parent snapshot and retain the options' absolute deadline
and cancellation token. Incremental and buffered execution have the same
operator boundaries as standalone reads.

Exhaustion, early drop, and dropping an unused prepared read leave the parent
transaction open, preserving its writes and write lock. Any error in preparation,
cursor creation, or iteration—including a rejected mutation, cancellation,
deadline, or resource failure—immediately rolls back all earlier writes. The
original error is returned unless rollback fails. A failing iterator emits that
error once, then stays exhausted. Its prepared read and parent subsequently
reject execution with `Error::TransactionAborted`; dropping them is not required
to release the failed transaction's lock.

Rows are owned snapshots and remain readable after rollback, but their graph
changes are provisional until commit. Closing a successful cursor does not
release the parent's locks; finish the transaction to do that. A consumer panic
that only drops the cursor/prepared read acts like early drop. If unwinding also
drops the transaction, its writes roll back. Early exit does not validate the
unread suffix beyond any hydration read-ahead.

## Measurements

`examples/cursor_scan.rs` creates a small in-memory fixture with `n` nodes,
then reads an `n × n` Cartesian bag with a 1,024-byte string and an integer in
every result row. Modes are eager collection, full cursor consumption with
rows discarded immediately, and first-row consumption followed by early drop.
Full scans validate their row count and integer checksum; every consumed row
validates its types and payload length. An optional fourth argument repeats
the scan in that many `UNION ALL` branches (default one, maximum 100), producing
`branches × n × n` potential rows. A test exercises all three modes with one,
two, and four branches.

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
The `union_all_scan_properties`, `cursor_union_all_scan_properties`, and
`cursor_union_all_first_row` cases compare eager, full-cursor, and first-row
consumption of a two-branch scan in the same `read` group.
Those cases use the existing social graph at 1,000 and 10,000 nodes and include
preparation and cursor cleanup on each iteration.

### UNION ALL measurements

The optional fourth argument selects two identical scan branches. Run each
mode in a fresh process after building, and repeat with `100` nodes to compare
20,000 potential rows with the 245,000-row case:

```sh
cargo build --release --example cursor_scan
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_scan eager 350 1024 2
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_scan cursor 350 1024 2
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_scan first 350 1024 2
```

Recorded on 2026-10-07, macOS 26.5.1 arm64, release build, three fresh
processes per cell, run sequentially. These are medians; every row has a
1,024-byte payload. Full consumption verifies both branches' row counts and
checksums. Timings include planning, preparation of both statements, snapshot
setup, decoding, and cleanup; fixture creation is excluded.

| Potential rows | Mode | Consumed rows | First row (ms) | Complete query (ms) | Peak RSS (MiB) |
| ---: | --- | ---: | ---: | ---: | ---: |
| 20,000 | Eager | 20,000 | 41.419 | 43.368 | 38.156 |
| 20,000 | Cursor | 20,000 | 0.195 | 39.385 | 7.922 |
| 20,000 | First row | 1 | 0.196 | 0.204 | 7.891 |
| 245,000 | Eager | 245,000 | 509.380 | 541.361 | 384.891 |
| 245,000 | Cursor | 245,000 | 0.271 | 486.169 | 12.766 |
| 245,000 | First row | 1 | 0.270 | 0.281 | 12.797 |

At 245,000 rows, eager peak RSS ranged from 384.734–384.953 MiB and full-cursor
RSS from 12.672–12.781 MiB. Cursor first-row latency ranged from 0.265–0.282 ms;
eager first-row latency was 505.947–519.513 ms. Full-query times were
534.581–553.419 ms eager and 485.429–488.777 ms cursor. These trials show the
benefit of avoiding the output buffer for this query; they do not measure a
general throughput improvement or extend to buffered branches. RSS includes
the fixture, SQLite, and prepared statements. Early exit consumes one row from
the first branch and never executes the second branch.

Measured executable SHA-256:
`3f5fc365d32009583fb267210315a6d3ae9b5f92d0dc65af541c5cb04e5e7728`.

### DISTINCT and distinct UNION measurements

`examples/cursor_distinct.rs` generates a controlled number of rows using
`UNWIND range(...)`. Every row contains a 1,024-byte payload and an integer
cycling through the requested number of unique values. The `distinct` case
uses one RETURN DISTINCT; `union` divides the same input sequence between two
UNION branches. Full consumption validates every first representative in order
and the exact unique-row count. The example's test covers both operators,
all three modes, and several unique/input ratios.

Build once, then run each mode in a fresh process:

```sh
cargo build --release --example cursor_distinct
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_distinct eager distinct 10000 64 1024
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_distinct cursor distinct 10000 64 1024
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_distinct first distinct 10000 64 1024
```

Repeat with `union` and with `9900` unique rows to compare duplicate-heavy
and mostly-unique inputs. Recorded on 2026-10-07, macOS 26.5.1 arm64, release
build, three fresh processes per cell run sequentially. All cases have 10,000
input rows. Values are medians; timings include planning, parameter binding,
snapshot setup, SQLite range generation, decoding, deduplication, and cleanup.
`First row` consumes one result and drops the cursor; other modes drain the
complete unique result.

| Operator | Unique rows | Mode | First row (ms) | Complete query (ms) | Peak RSS (MiB) |
| --- | ---: | --- | ---: | ---: | ---: |
| DISTINCT | 64 | Eager | 32.174 | 32.178 | 21.172 |
| DISTINCT | 64 | Cursor | 5.648 | 25.223 | 6.281 |
| DISTINCT | 64 | First row | 5.667 | 5.691 | 6.125 |
| UNION | 64 | Eager | 34.437 | 34.441 | 21.766 |
| UNION | 64 | Cursor | 4.122 | 28.720 | 6.703 |
| UNION | 64 | First row | 4.124 | 4.155 | 6.297 |
| DISTINCT | 9,900 | Eager | 1,885.899 | 1,886.300 | 21.391 |
| DISTINCT | 9,900 | Cursor | 5.722 | 29.964 | 20.344 |
| DISTINCT | 9,900 | First row | 5.675 | 5.701 | 6.141 |
| UNION | 9,900 | Eager | 1,787.676 | 1,788.078 | 21.781 |
| UNION | 9,900 | Cursor | 4.098 | 33.400 | 20.656 |
| UNION | 9,900 | First row | 4.123 | 4.156 | 6.328 |

For DISTINCT, cursor peak RSS ranged from 6.250–6.297 MiB with 64 unique rows
and 20.297–20.359 MiB with 9,900 unique rows. This shows the retained-state
cost: streaming avoids the complete input buffer, but stores each unique
representative. At 9,900 unique rows, DISTINCT cursor first-row latency ranged
from 5.599–8.065 ms, versus 1,836.521–2,270.294 ms eager. The UNION cursor
range was 4.080–4.139 ms, versus 1,699.089–1,839.908 ms eager.

The eager executor still uses pairwise duplicate comparisons, while cursors
use hash buckets followed by the same equality check. Full-query timing
differences therefore include that algorithm difference, not just streaming.
RSS covers the entire process and SQLite's internal work. These cases do not
establish bounds for arbitrary values, sorting, traversal, or hydration.
Criterion's `cursor_distinct_city`, `cursor_distinct_first_row`,
`cursor_union_distinct_city`, and `cursor_union_distinct_first_row` cases also
exercise duplicate-heavy reads on the standard graph fixtures.

Measured executable SHA-256:
`2a9a860ab6e9a8734d6058a8ed0c0762f0356c5c5ed1ee967870dfe9f1bb84f6`.

### Path and graph-list hydration measurements

`examples/cursor_hydration.rs` creates one directed relationship between two
nodes, with a configurable payload on all three entities. It repeats that path
using `UNWIND range(1,$last)` and returns either the path (`path`) or its node
list (`nodes`), plus the range index. Every consumed row validates graph types,
entity payload lengths, and its index; path results also validate cardinality
and endpoints. Full consumption checks the complete row count. The example test
covers both projections and all modes across batch boundaries.

Build separately, then measure each mode in a fresh process:

```sh
cargo build --release --example cursor_hydration
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_hydration eager path 20000 1024
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_hydration cursor path 20000 1024
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_hydration first path 20000 1024
```

Repeat with `nodes` and with `1000` rows to compare sizes. The wrapper records
the binary hash, peak process RSS, and platform. Query times exclude fixture
creation but include planning, snapshot setup, SQLite range processing, hydration,
validation, and cleanup. Full-cursor mode discards each delivered row immediately;
first-row mode consumes one row and drops the cursor. RSS includes SQLite and
the entire process, rather than measuring retained Rust allocations alone.

Recorded on 2026-10-08, macOS 26.5.1 arm64, release build, three fresh
processes per cell run sequentially after all test/build workloads finished.
Values are medians; each entity carries a 1,024-byte payload.

| Projection | Potential rows | Mode | First row (ms) | Complete query (ms) | Peak RSS (MiB) |
| --- | ---: | --- | ---: | ---: | ---: |
| Path | 1,000 | Eager | 9.753 | 10.172 | 18.000 |
| Path | 1,000 | Cursor | 6.155 | 10.324 | 8.531 |
| Path | 1,000 | First row | 6.345 | 7.122 | 8.391 |
| Path | 20,000 | Eager | 215.280 | 236.348 | 200.281 |
| Path | 20,000 | Cursor | 149.441 | 216.172 | 9.484 |
| Path | 20,000 | First row | 142.920 | 144.875 | 9.141 |
| Node list | 1,000 | Eager | 9.650 | 9.906 | 15.281 |
| Node list | 1,000 | Cursor | 5.964 | 10.199 | 8.562 |
| Node list | 1,000 | First row | 5.978 | 6.920 | 8.391 |
| Node list | 20,000 | Eager | 199.030 | 211.853 | 145.203 |
| Node list | 20,000 | Cursor | 142.493 | 200.839 | 9.469 |
| Node list | 20,000 | First row | 139.996 | 146.006 | 9.141 |

At 20,000 paths, full-cursor RSS ranged from 9.453–9.531 MiB, versus
200.281–200.297 MiB eager. Cursor first-row latency ranged from 145.593–154.088 ms,
versus 208.755–216.481 ms eager. First-row latency still grows with the range
size: incremental hydration does not make SQLite's internal work incremental.
Full-query timings include repeated hydration queries and do not establish a
general throughput improvement; at 1,000 rows cursor medians were slightly slower.

A pre-change baseline of this same example on `c3d4734` reported `Buffered`.
At 20,000 paths its full-cursor medians were 209.288 ms to first row,
225.595 ms total, and 199.172 MiB RSS. For node lists they were 195.921 ms,
208.960 ms, and 144.078 MiB. Early drop still materialized the whole result:
199.156 MiB for paths and 144.094 MiB for node lists. These baseline trials also
used three sequential fresh processes per cell on the same date and platform.

The 64-row continuation batch limits pending rows and entity-cache growth while
amortizing lookups. This repeated-path case loads three entities per batch:
942 hydration records across 314 batches for 20,000 paths, versus three eager
records. Node lists load 628 rather than two. Transfer and SQLite work limits
charge those extra reads, even though result limits count the same output.
These measurements do not bound the memory of large individual paths, DISTINCT
state, sorting, or Rust traversal. Criterion's hydration cases also exercise
many different paths on the standard social-graph fixtures.

Measured executable SHA-256:
`26f8bbebe86459c3df6bf4e0aac2e6165fdfdf64208a202b54619e43168be716`.
Pre-change baseline SHA-256:
`ac56a22e34ce1a1035ecca1c37ea5115613038077e40c8a9d42cee36437b8d49`.
