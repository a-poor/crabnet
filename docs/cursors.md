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

Inspect `mode()` before opening a cursor. `Incremental` includes SQLite fragments
and eligible single Rust trail expansions, composed through `UNION ALL`,
`UNION`, `RETURN DISTINCT`, pagination, and path/list hydration. Each advance yields at most one row, but can
consume several duplicate or skipped input rows. SQLite may still buffer sorts,
aggregates, or intermediate relations internally.
`Buffered` means the existing executor completes the result when `cursor()`
is called. Traversal shapes outside the slice below use that fallback. A union
with any buffered branch uses the fallback for the whole query.

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

Paths from eligible SQLite and Rust producers, `nodes(p)`, `relationships(p)`,
and legacy relationship lists hydrate incrementally after distinct, pagination,
and union processing.
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

## Incremental Rust traversal

A single `MATCH` with one deep/open-ended variable-length relationship and only
its endpoint nodes can stream. Filters, scalar/entity/path projections, legacy
relationship lists, chained `UNWIND`, `WITH` projections/filters/DISTINCT/pagination,
final DISTINCT, and final pagination are supported. For example:

```cypher
MATCH p=(:Start)-[:NEXT*]->(end)
WITH p, end WHERE end.active = true
RETURN p, end.id
LIMIT 10
```

The producer loads endpoint domains on first demand, then resumes the trail walk
between rows. When the same MATCH constrains the start with a label or inline
property, each expanded node's eligible neighbors are fetched through SQLite's
outgoing/incoming indexes. Ordered neighbor lists, including empty lists, are
cached across repeated visits and seeds. Unrelated edges and neighborhoods past
the hop bound or early-exit point are not fetched. Undirected probes include
self-loops once and preserve parallel relationships. Starts without these
constraints retain whole-edge loading; a WHERE predicate alone does not enable
indexed probes. Eager execution and buffered cursor shapes keep their old source.

Endpoint domains, cached neighbors, and the search frontier stay in memory while
paused. Expanding a high-degree node loads its entire eligible neighbor list.
One encoded trail passes through a cached SQLite projection at a time. Cycles
remain relationship-unique; parallel relationships and duplicate result values
keep their multiplicity. This avoids retaining the complete runtime-result
relation, while graph inputs and the traversal frontier still consume memory.

Each following `WITH` may rename bindings, compute expressions, carry typed
entities/paths/lists, and filter rows. Without UNWIND, these stages produce zero
or one final row per admitted trail. With chained UNWINDs, each list expression
and its preceding WITH stages run once per incoming row. Inner lists can depend
on outer members. Following stages run once per member; the cursor exhausts an
inner list before advancing its parent. A projected random value is reused by
its aliases in that scope, including while resuming an inner expansion. Filters
do not refund traversal work or payload.

Intermediate `WITH SKIP/LIMIT` counts matching incoming rows across trails and
parent lists, with independent counters for each page. Its WHERE filter runs
before pagination. Without DISTINCT, SKIP suppresses that WITH's projection on
discarded rows; earlier WITH/UNWIND computations and the page's filter still run
and can fail.
Each admitted row freezes its projected bindings once, preserving volatile
aliases while downstream expansions run. At the limit, those expansions finish
before upstream production stops. Empty or filtered downstream results do not
refund an admitted row. LIMIT 0 anywhere in this eligible pipeline starts no
producer; all parameters and pages are still validated at preparation.

For example, this expands nodes from at most ten admitted trails:

```cypher
MATCH p=(:Start)-[:NEXT*]->(end)
WITH p, end SKIP 2 LIMIT 10
UNWIND nodes(p) AS member
RETURN end, member
```

Page stages retain one admitted row and their counters. Each matching input,
including skipped placeholders, counts as a SQL transfer; filtered inputs do
not. Downstream UNWIND inputs and final projections are charged separately.
Without ORDER BY, pagination selects an unspecified subset and introduces no
ordering guarantee between eager and cursor execution.

`WITH DISTINCT` evaluates its projection once, applies WHERE using the projected
aliases and incoming scope, then deduplicates only the projected columns.
Each stage retains its own seen set across all trails and parent lists, keeping
the first representative encountered by that cursor. Keys use the same typed
equivalence as final DISTINCT: exact numeric equality, recursive lists/maps,
entity identity, and ordered path/list identities. A path and its node or
relationship list can share storage while contributing different key values.
Compact path keys do not require hydration. Ordinary maps remain distinct from
graph records with similar fields.

SKIP/LIMIT on WITH DISTINCT count unique rows. Skipped unique keys remain in
the seen set so their later duplicates cannot reappear. DISTINCT projections
are evaluated even for skipped rows; duplicate and skipped inputs count toward
SQL transfer limits before key decoding/copying. Filtered inputs do not transfer
a row. Aliases remain frozen while downstream stages run; a reached limit
drains admitted expansions before stopping ancestors. LIMIT 0 still avoids
producer execution after preflight. For example:

```cypher
MATCH p=(:Start)-[:NEXT*]->()
UNWIND nodes(p) AS n
WITH DISTINCT n LIMIT 10
UNWIND n.tags AS tag
RETURN n, tag
```

Seen-set memory grows with the number and size of unique projected rows,
including skipped keys. This retained state is outside the traversal-memory
counter; cumulative transfer limits still apply. Stage/branch completion,
errors, and cursor drop release it. Eager window ranking may emit another
unordered order and therefore choose another paginated subset.

For example, `MATCH p=(:Start)-[:NEXT*]->() UNWIND nodes(p) AS n RETURN n`
streams nodes while retaining repeated identities and parallel-trail multiplicity.
Scalar, entity, and nested graph lists retain their types and null members;
empty, null, and non-list inputs yield no members. The cursor retains one
encoded list and the live bindings from its input row for each active expansion,
then decodes members as needed. Returning to an outer stage preserves its frozen
values and list position. Exhausted stages release their list and carried bindings;
retention grows with active nesting and value sizes, rather than the complete
expanded result. Bare `nodes(p)`/`relationships(p)` carry identities and fetch each
member's entity record on demand. List construction itself still runs in full:
`range(...)`, comprehensions, and computed entity lists can allocate a large
single value before the first member is returned. This is not a whole-query
memory cap. Each frozen input row, including the list, is charged to SQL
transfer row/byte limits before copying; consuming rows are charged separately.
Traversal memory limits continue to cover graph inputs and the frontier.

Dependent expansions can stop at an early final limit, for example:

```cypher
MATCH p=(:Start)-[:NEXT*]->()
UNWIND nodes(p) AS n
WITH n WHERE n.active = true
UNWIND n.tags AS tag
RETURN n, tag
LIMIT 10
```

Global ORDER BY or aggregation over Rust trails, additional pattern joins,
multiple traversal producers, OPTIONAL MATCH, and shortest paths still report
`Buffered`. A `WITH` containing aggregation or ordering also forces
buffering, including when combined with pagination. Stages preceding the
initial MATCH remain outside this streaming shape. Small bounded traversals
executed entirely by SQLite retain their existing behavior. Inspect `mode()`.

Early drop and a reached LIMIT stop further enumeration beyond hydration's
current batch. LIMIT 0 starts no producer, but all parameters and page values
are still validated at preparation. Final ordinary SKIP suppresses projection
expressions in RETURN on discarded rows; prior WITH and UNWIND input computations
still run and can fail. DISTINCT pagination evaluates projected values before
deduplication. Skipped placeholder rows still count as
SQL transfers. Errors can follow earlier rows and are
terminal; transaction-scoped failures roll back earlier writes. The standalone
snapshot is pinned at cursor creation, before graph loading begins.

A following WITH or RETURN expression error in an unread member/trail suffix is
not encountered after early drop or a reached page limit. Eager execution can encounter
that error while materializing the complete stage. Once a cursor observes an error it becomes terminal, even
if earlier rows have been delivered.

State and payload counters span all cursor advances and union branches. Payload
accounting counts each singleton JSON array's framing, so full consumption can
use more payload/work budget than one eager relation. Graph inputs and the
frontier stay charged to retained traversal memory while paused; exhausted or
dropped branches release that charge before another branch begins. Per-row SQL
execution can cost more than a single batched query. The measurements below
report both full consumption and early exit.

Fetched edge rows count against SQL transfer limits; cache keys and steps count
against retained traversal memory. An undirected edge can be fetched once at
each endpoint. State limits still count explored trails, including work that
finds no qualifying endpoint. No global edge count is loaded to rule out an
impossible minimum depth, so such a cursor can spend state budget where eager
execution can return empty immediately. `explain_physical` shows the eager
`edge-source` and, when available, the alternative `cursor-neighbor-source`
with its final current-node binding.

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

### Rust traversal measurements

`examples/cursor_traversal.rs` builds a short chain with two parallel `NEXT`
relationships per hop. At depth d there are exactly 2^d relationship-unique
paths. Depth 10 produces 2,046 potential rows from 11 nodes / 20 edges; depth
14 produces 32,766 from 15 nodes / 28 edges. The query returns path length and
endpoint ID. Full consumption verifies the exact histogram at every depth;
early exit verifies one one-hop row. This isolates growth of the result relation
from graph-input growth. It does not measure shortest paths or hydrated paths.

```sh
cargo build --release --example cursor_traversal
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal eager 14
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 14
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 14
cargo bench --bench queries -- cursor_traversal
```

Repeat the example with depth 10. Each measurement uses a fresh process and
in-memory graph; build first, then run sequentially. The example explicitly
allows 1,000,000 explored states and 1 GiB of cumulative serialized payload,
with the default retained-memory budget. These are workload options, not new
library defaults. Query and first-row timings include planning, snapshot setup,
decoding and validation; query timing also includes cleanup. Fixture setup is
reported separately. Process RSS includes the fixture and SQLite, not just
cursor state. The Criterion group uses depth 10 for eager, full-cursor, and
first-row cases and checks each consumed row count.

Measured on 2026-10-08 with Rust 1.99.0, release builds, and macOS 26.5.1
arm64. The baseline is commit `3932cfd`, with only the identical example and
fixture copied into an isolated archive/target. Three fresh-process trials per
engine, depth, and consumption mode alternated engine order and rotated mode
order. Builds and tests finished before measurement. All 36 runs passed their
row-count, endpoint, and depth-histogram checks. Baseline cursor reads reported
`Buffered`; updated cursor reads reported `Incremental`.

The table gives medians across trials, with before → after values. “First only”
drops the cursor after one row; “Full cursor” drains it without collecting rows.

| Potential rows | Consumption | First row (ms) | Complete query (ms) | Peak RSS (MiB) |
| ---: | --- | ---: | ---: | ---: |
| 2,046 | Full cursor | 37.723 → 0.458 | 37.803 → 43.583 | 14.094 → 6.766 |
| 2,046 | First only | 36.452 → 0.421 | 36.515 → 0.431 | 14.047 → 6.625 |
| 32,766 | Full cursor | 754.895 → 0.349 | 762.008 → 852.291 | 157.594 → 6.938 |
| 32,766 | First only | 732.381 → 0.439 | 738.382 → 0.452 | 157.531 → 6.578 |

Full-consumption medians increased about 15% at the smaller size and 12% at
the larger size. Large-case ranges overlap: 752.9–951.9 ms before and
836.1–1,032.4 ms after. Eager controls retained similar RSS (about 14.1 and
157.6 MiB); their query medians were 38.1 → 37.7 ms and 747.7 → 802.7 ms,
with substantial variation in the larger case. These observations support
lower first-row latency and memory retention, without claiming a full-scan
throughput improvement. Per-trail SQL execution has a cost.

At 32,766 rows, first-only RSS ranged from 157.52–157.58 MiB before and
6.56–6.66 MiB after; first-row latency ranged from 730.49–774.40 ms before
and 0.37–0.78 ms after. These are local observations on a small graph with many
paths, not a whole-query memory guarantee. This comparison predates indexed
cursor neighbor loading: graph inputs loaded up front. Individual paths can
still be large, and DISTINCT/hydration retain their
own state. The [measurement record](measurements/cursor-traversal-2026-10-08.json)
contains all trial reports, medians/ranges, binary and source hashes, and
whole-process measurements.

### Constrained cursor neighbor measurements

The same example accepts an optional third argument: the number of disconnected
nodes with one eligible `NEXT` self-loop each. At depth 10, the reachable
component remains 11 nodes / 20 edges with exactly 2,046 results. Varying the
disconnected edges measures graph-input scaling independently of result size.

```sh
cargo build --release --example cursor_traversal
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 10 10000
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 10 10000
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal eager 10 10000
cargo bench --bench queries -- cursor_traversal
```

Repeat with 0 and 1,000 disconnected edges. The existing Criterion group covers
zero unrelated edges; `cursor_traversal_neighbors/*` adds 1,000 and 10,000 for
all three consumption modes. Every full run verifies the exact depth histogram
and endpoints, and first-only runs validate one one-hop row.

Measured on 2026-10-08 with Rust 1.99.0, release builds, and macOS 26.5.1
arm64. The baseline is commit `851d30d` in an isolated archive/target, with only
the identical extended example and fixture copied in. Three fresh-process
trials per engine, graph size, and mode alternated engine order and rotated
mode order. Builds and tests finished before measurement. All 54 runs passed
their result checks; both versions report `Incremental` for cursor modes.

These are medians, before → after. Timings include planning, snapshot setup,
decoding, and validation; complete query time also includes cleanup. Fixture
setup is excluded from query timing and reported separately in the record.
Peak RSS covers the entire process, including graph construction and SQLite.

| Disconnected edges | Consumption | First row (ms) | Complete query (ms) | Peak RSS (MiB) |
| ---: | --- | ---: | ---: | ---: |
| 0 | Full cursor | 0.298 → 0.288 | 34.374 → 34.509 | 6.656 → 6.656 |
| 0 | First only | 0.286 → 0.282 | 0.291 → 0.287 | 6.484 → 6.500 |
| 1,000 | Full cursor | 0.860 → 0.302 | 35.524 → 35.196 | 8.656 → 8.328 |
| 1,000 | First only | 0.853 → 0.301 | 0.950 → 0.307 | 8.609 → 8.078 |
| 10,000 | Full cursor | 6.772 → 0.350 | 42.986 → 35.493 | 24.969 → 21.188 |
| 10,000 | First only | 6.874 → 0.348 | 8.034 → 0.355 | 25.000 → 20.969 |

At 10,000 disconnected edges, first-only latency improved about 20×, with trial
ranges of 6.780–6.876 ms before and 0.341–0.364 ms after. Full consumption
improved about 17% (42.967–43.479 ms before, 35.166–35.612 ms after). The
zero-unrelated-edge full-cursor median changed by less than 1%, with overlapping
ranges. Eager controls at 10,000 edges remained about 37.6–37.7 ms and 30.7 MiB.

These results support avoiding disconnected graph input for constrained
cursors. Process RSS still grows with the stored graph and setup allocations;
the first-only reduction at 10,000 edges is about 16%. Endpoint domains and
whole neighbor lists still load eagerly when needed. Broad start domains,
high-degree reachable nodes, hydration, and unconstrained starts require
separate measurements before extending this strategy. The
[measurement record](measurements/cursor-neighbors-2026-10-08.json) contains
all trial reports, medians/ranges, source and binary hashes, and platform data.

### Traversal WITH measurements

The example's optional fourth argument selects `direct` (the default) or `with`.
The latter uses the same parallel-edge fixture and returns the same length/ID
rows through two WITH stages:

```cypher
MATCH p=(:Start)-[:NEXT*]->(b)
WITH p AS trail, b WHERE b.id >= 1
WITH length(trail) AS hops, b.id AS endpoint
RETURN hops, endpoint
```

All fixture paths survive the filter. Full consumption checks the exact depth
histogram and endpoints; first-only consumption checks one one-hop row.

```sh
cargo build --release --example cursor_traversal
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 14 0 with
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 14 0 with
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal eager 14 0 with
cargo bench --bench queries -- cursor_traversal_with
```

Repeat with depth 10, then depth 10 and 10,000 unrelated edges. Criterion uses
depth 10 with zero unrelated edges for all three consumption modes.

Measured on 2026-10-08 with Rust 1.99.0, release builds, and macOS 26.5.1
arm64. The baseline is commit `1428d35` in an isolated archive/target with only
the identical extended example/fixture copied in. Three fresh-process trials
per engine, fixture, and mode alternated engine order and rotated mode order,
after all builds and tests finished. All 54 runs passed their row-count,
endpoint, and depth-histogram checks. Baseline WITH cursors reported `Buffered`;
updated WITH cursors reported `Incremental`.

The table gives medians, before → after. First-row and complete-query timings
include planning, snapshot setup, decoding, and validation; complete-query time
also includes cleanup. Setup time is reported separately. Peak RSS covers the
whole process, including fixture construction and SQLite.

| Potential rows | Unrelated edges | Consumption | First row (ms) | Complete query (ms) | Peak RSS (MiB) |
| ---: | ---: | --- | ---: | ---: | ---: |
| 2,046 | 0 | Full cursor | 24.946 → 0.514 | 24.997 → 33.807 | 16.625 → 6.859 |
| 2,046 | 0 | First only | 24.853 → 0.497 | 24.887 → 0.502 | 16.625 → 6.719 |
| 32,766 | 0 | Full cursor | 514.277 → 0.506 | 517.341 → 589.019 | 160.828 → 7.016 |
| 32,766 | 0 | First only | 513.321 → 0.515 | 516.391 → 0.520 | 160.781 → 6.734 |
| 2,046 | 10,000 | Full cursor | 32.750 → 0.610 | 32.800 → 34.240 | 32.578 → 21.266 |
| 2,046 | 10,000 | First only | 32.594 → 0.577 | 32.640 → 0.584 | 32.688 → 21.016 |

At 32,766 paths, first-only latency ranged from 505.446–520.920 ms before and
0.493–0.598 ms after. Peak RSS ranged from 160.766–160.859 MiB before and
6.703–6.734 MiB after. Full-consumption medians increased about 35% at 2,046
paths and 14% at 32,766; the large-case ranges were 509.336–518.872 ms before
and 585.380–589.592 ms after. The case with 10,000 unrelated edges increased
about 4%. Eager controls changed by less than 2% in all three fixtures; their
large-case query medians were 510.611 → 516.255 ms with roughly 160.8 MiB RSS.

Executing materialized WITH stages separately for every trail costs throughput
when consuming everything, while avoiding full-result retention and permitting
early exit. Indexed neighbor loading also remains available through plain WITH
stages. These observations cover scalar final outputs and the documented
fixture, rather than a whole-query memory guarantee or general throughput gain.
The [measurement record](measurements/cursor-with-2026-10-08.json) includes all
trial reports, medians/ranges, source/binary hashes, and platform information.

## Traversal UNWIND measurements

A 2026-10-09 release comparison uses baseline `701b8e8` and this implementation.
Both engines use identical example/fixture sources; the baseline is an isolated
archive with its own target directory. Three sequential fresh-process trials
per engine, fixture, and mode alternate engine order and rotate mode order.
All builds and tests finished first. The
[54 raw trials](measurements/cursor-unwind-2026-10-09.json) include ranges,
source/binary hashes, commands, compiler, and platform details.

The parallel-edge chain expands either `nodes(trail)` at depth 10 (2,046 trails,
20,480 members), a range of 128 values at depth 7 (254 trails, 32,512 members),
or a range of 32,768 values at depth 1 (two trails, 65,536 members). Plain WITH
stages precede and follow UNWIND. Full consumption checks per-depth multiplicity
and member sums/squared sums; first-row mode checks one one-hop result. Final
outputs are scalars, so these trials do not measure path hydration.

Medians below show **before → after**. Total query time includes planning,
snapshot setup, decoding, consumer checks, and cleanup; first-row timing stops
when the first owned row reaches the consumer. Both exclude graph setup.
Peak RSS covers the entire process, including setup.

| Expansion | Mode | First row ms | Total ms | Peak RSS MiB |
| --- | --- | ---: | ---: | ---: |
| Nodes, depth 10 | eager | 296.062 → 318.951 | 296.673 → 319.538 | 30.125 → 29.156 |
| Nodes, depth 10 | cursor | 305.999 → 0.943 | 306.736 → 282.033 | 28.938 → 7.125 |
| Nodes, depth 10 | first | 300.221 → 1.229 | 300.699 → 1.247 | 29.766 → 6.953 |
| Range 128, depth 7 | eager | 201.128 → 180.580 | 203.054 → 184.900 | 25.672 → 25.734 |
| Range 128, depth 7 | cursor | 184.515 → 0.996 | 186.599 → 198.497 | 25.656 → 6.938 |
| Range 128, depth 7 | first | 186.373 → 0.984 | 189.921 → 0.993 | 25.625 → 6.766 |
| Range 32,768, depth 1 | eager | 390.880 → 407.171 | 394.113 → 410.896 | 44.500 → 44.812 |
| Range 32,768, depth 1 | cursor | 387.302 → 14.815 | 394.222 → 360.783 | 44.516 → 7.766 |
| Range 32,768, depth 1 | first | 423.369 → 14.551 | 429.126 → 14.570 | 44.906 → 7.422 |

Both cursor modes were Buffered before this change and are Incremental after it.
First-row consumption drops from about 300 ms to 1.2 ms for node expansion and
from 186 ms to 1.0 ms for range-128 expansion. The range-32,768 first row still
costs about 14.6 ms because the complete range value must be built first, down
from 423 ms. First-row process RSS falls from 26–45 MiB to about 7 MiB.

Full-cursor medians change from 307 to 282 ms for nodes, 187 to 198 ms for
range-128, and 394 to 361 ms for range-32,768. The range-128 case is about 6%
slower; ranges overlap and eager timings also vary, so these small samples do
not establish a general full-consumption speedup. Per-member SQLite execution
has a cost. The strong changes here are earlier delivery and avoiding a buffer
of all projected rows. One input list and its live bindings still stay resident.

Reproduce the shapes with the shared example and process runner:

```sh
cargo build --release --example cursor_traversal
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 10 0 unwind-nodes
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 7 0 unwind 128
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 1 0 unwind 32768
```

Use `eager`, `cursor`, and `first` for all-row eager execution, all-row cursor
consumption, and one-row consumption followed by drop. Criterion adds
`cursor_traversal_unwind_nodes/{eager,cursor,first}` and
`cursor_traversal_unwind/{eager,cursor,first}`, using depth 10 and range fanout 128.

## Chained UNWIND measurements

A 2026-10-09 release comparison uses baseline `d6bbf6f` and this implementation.
The baseline is an isolated archive with its own target directory; both engines
use identical example/fixture sources. Three sequential fresh-process trials
per engine, fixture, and mode alternate engine order and rotate mode order.
All builds and tests finished before measurement. The
[54 raw trials](measurements/cursor-unwind-chain-2026-10-09.json) record commands,
source/binary hashes, compiler/platform, medians, and ranges.

The parallel-edge fixture expands two dependent lists with plain WITH stages
before, between, and after them:

- Node-dependent ranges at depth 10: `nodes(p)`, then `range(0,n.id)`, producing
  114,686 rows from 2,046 trails.
- Scalar ranges at depth 8, fanout 8: `range(1,$fanout)`, then
  `range(source,source+$fanout-1)`, producing 32,640 rows from 510 trails.
- The same scalar ranges at depth 1, fanout 256: 131,072 rows from two trails.

Full consumption checks each depth's row count, both members' sums and squared
sums, and their cross-products. First-row mode checks one one-hop result, then
drops the cursor. Final outputs are scalars, so these cases exclude path hydration.

Medians show **before → after**. Total query time includes planning, snapshot
setup, decoding, consumer checks, and cleanup. First-row timing stops when the
first owned row reaches the consumer. Both exclude graph setup; peak RSS covers
the entire process, including setup.

| Expansion | Mode | First row ms | Total ms | Peak RSS MiB |
| --- | --- | ---: | ---: | ---: |
| Node ranges, depth 10 | eager | 1355.255 → 1301.452 | 1363.192 → 1309.314 | 83.906 → 83.859 |
| Node ranges, depth 10 | cursor | 1409.527 → 1.414 | 1419.852 → 1206.404 | 83.875 → 7.156 |
| Node ranges, depth 10 | first | 1372.881 → 1.315 | 1379.970 → 1.328 | 83.906 → 6.969 |
| Ranges 8×8, depth 8 | eager | 355.812 → 317.882 | 359.779 → 319.895 | 27.422 → 27.500 |
| Ranges 8×8, depth 8 | cursor | 335.294 → 2.180 | 337.629 → 338.398 | 27.453 → 7.422 |
| Ranges 8×8, depth 8 | first | 328.069 → 2.193 | 330.303 → 2.211 | 27.469 → 7.234 |
| Ranges 256×256, depth 1 | eager | 1200.018 → 1173.387 | 1208.063 → 1186.494 | 81.125 → 81.188 |
| Ranges 256×256, depth 1 | cursor | 1148.688 → 2.287 | 1158.673 → 977.253 | 81.250 → 7.016 |
| Ranges 256×256, depth 1 | first | 1192.204 → 2.365 | 1200.887 → 2.377 | 81.328 → 7.156 |

Both cursor modes were Buffered before this change and are Incremental after it.
For node-dependent ranges, first-row consumption falls from about 1.37 seconds
to 1.3 ms, with process RSS falling from about 84 MiB to 7 MiB. The 256×256 case
falls from about 1.19 seconds to 2.4 ms and from 81 MiB to 7 MiB. The 8×8 case
falls from 328 ms to 2.2 ms and from 27 MiB to 7 MiB.

Full-cursor medians fall about 15% for node ranges and 16% for the 256×256 case;
the 8×8 case is essentially unchanged (338 ms). Its trial ranges overlap and
vary substantially; eager controls vary too. These samples do not establish a
general throughput gain.
Per-member SQL execution remains a cost. Each active stage still constructs and
retains its complete input list and live bindings; nesting and large values can
raise memory use. These results demonstrate avoiding the complete expanded
result buffer, rather than a whole-query memory cap.

Reproduce with the shared example and process runner:

```sh
cargo build --release --example cursor_traversal
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 10 0 unwind-chain-nodes
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 8 0 unwind-chain 8
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 1 0 unwind-chain 256
```

Use `eager`, `cursor`, and `first` for all-row eager execution, full cursor
consumption, and one-row consumption followed by drop. Criterion adds
`cursor_traversal_unwind_chain_nodes/{eager,cursor,first}` at depth 10 and
`cursor_traversal_unwind_chain/{eager,cursor,first}` at depth 6 with fanout 8.

## Intermediate pagination measurements

A 2026-10-09 release comparison uses baseline `2937091` and the intermediate
WITH pagination implementation. The baseline is an isolated archive with its
own target directory and identical example/fixture sources. Three sequential
fresh-process trials per engine, fixture, and mode alternate engine order and
rotate mode order. All builds and tests finished before measurement. The
[54 raw trials](measurements/cursor-pages-2026-10-09.json) include commands,
source/binary hashes, compiler/platform, medians, and ranges. Source hashes
were checked again after the trials.

The parallel-edge chain supports three shapes:

- `page-trails`: depth 14, with 32,766 potential trails. WITH SKIP 2 LIMIT 10
  precedes a range of 128 members, producing 1,280 output rows.
- `page-members`: depth 10, with 2,046 potential trails. A range of 128 members
  precedes WITH SKIP 64 LIMIT 128, spanning two parents; a dependent range of
  128 members follows it, producing 16,384 output rows.
- `page-all`: depth 10, with a range of 32 members per trail and then WITH
  SKIP 0. All 65,472 rows pass through, so pagination avoids no upstream work.

Full consumption checks member multiplicities, dependent member sums/squared
sums, and, for the pass-through page, the depth histogram. Every consumed row
checks hop count against endpoint and validates member bounds. Bounded pages
validate the scalar-member bags independently of which unordered trails were
selected; no cross-mode ordering guarantee is implied. Scalar outputs exclude
path hydration. First-row mode validates one row, then drops the cursor.

Medians show **before → after**. Query timings include planning, snapshot setup,
decoding, per-row validation, and cleanup, excluding graph setup. First-row time
ends when the consumer receives its first owned row. Peak RSS covers the whole
process, including graph setup.

| Page | Mode | First row ms | Total ms | Peak RSS MiB |
| --- | --- | ---: | ---: | ---: |
| Before expansion | eager | 150.113 → 196.061 | 150.197 → 196.157 | 142.328 → 142.531 |
| Before expansion | cursor | 215.693 → 0.837 | 215.751 → 3.480 | 142.422 → 6.828 |
| Before expansion | first | 154.061 → 0.877 | 154.091 → 0.885 | 142.453 → 6.875 |
| Between expansions | eager | 2532.251 → 2634.311 | 2533.629 → 2634.916 | 19.078 → 19.109 |
| Between expansions | cursor | 2534.686 → 2.037 | 2535.439 → 59.590 | 18.594 → 7.109 |
| Between expansions | first | 2550.284 → 2.193 | 2550.693 → 2.204 | 25.328 → 7.047 |
| All rows, SKIP 0 | eager | 447.012 → 467.337 | 451.927 → 470.369 | 50.219 → 49.781 |
| All rows, SKIP 0 | cursor | 422.922 → 1.035 | 428.341 → 510.328 | 49.578 → 7.078 |
| All rows, SKIP 0 | first | 448.041 → 0.962 | 453.035 → 0.970 | 49.594 → 6.891 |

Both cursor modes were Buffered before and are Incremental after the change.
For the page before expansion, full consumption falls from about 216 ms to
3.5 ms and process RSS from 142 MiB to 6.8 MiB. For the page between expansions,
first-row consumption falls from 2.55 seconds to 2.2 ms; full consumption falls
from 2.54 seconds to 60 ms. These improvements come from stopping upstream work
once the page is satisfied and avoiding complete result materialization.

The pass-through page exposes a throughput cost: its full-cursor median rises
about 19%, from 428 ms to 510 ms. Trial ranges are 399–463 ms before and
473–546 ms after. It still delivers the first row in about 1 ms and reduces
process RSS from about 50 MiB to 7 MiB. Eager controls vary, especially for the
smaller page-before-expansion case, so these trials do not establish a universal
speedup or an exact slowdown for other workloads. Each page adds a statement
execution and scope transfer per matching input row. Active UNWIND stages still
retain complete input lists; page frames retain one admitted row and counters.
This is not a whole-query memory cap.

Reproduce with the shared example and process runner:

```sh
cargo build --release --example cursor_traversal
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 14 0 page-trails 128
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 10 0 page-members 128
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 10 0 page-all 32
```

Use `eager`, `cursor`, and `first` for all-row eager execution, full cursor
consumption, and one-row consumption followed by drop. Criterion adds
`cursor_traversal_page_{trails,members,all}/{eager,cursor,first}`, all at depth 10;
fanout is 128 for the bounded pages and 32 for the pass-through page.

## Intermediate DISTINCT measurements

A 2026-10-09 release comparison uses baseline `0b57317` and the intermediate
WITH DISTINCT implementation. The baseline is an isolated archive with its own
target directory and identical example/fixture sources. Three sequential
fresh-process trials per engine, fixture, and mode alternate engine order and
rotate mode order. All builds and checks finished before measurement. The
[54 raw trials](measurements/cursor-with-distinct-2026-10-09.json) include commands,
source/binary hashes, compiler/platform, medians, and ranges. Baseline sources
were checked against the commit, fixture copies were compared byte-for-byte,
and source hashes were checked again after the trials.

The parallel-edge chain supports three DISTINCT shapes:

- `distinct-entities`: depth 10, with 2,046 trails and 20,480 path-node members,
  including start nodes filtered out by WHERE. WITH DISTINCT n retains ten
  node keys, then a range of 32 members produces 320 output rows.
- `distinct-unique`: depth 12, with 8,190 unique path/endpoint keys. WITH
  DISTINCT p,b precedes a range of four members, producing 32,760 output rows.
- `distinct-page`: depth 12, with 8,190 potential trails and 98,304 path-node
  members before filtering. WITH DISTINCT n SKIP 2 LIMIT 10 precedes a range
  of 128 members, producing 1,280 rows from ten admitted keys. The cursor stops
  upstream traversal when this page is filled and its expansions finish.

Full consumption checks per-depth counts, member sums/squared sums, and member
bounds. Unordered pages check total cardinality and each selected key's member
bag without requiring the same node IDs across modes. First-row consumption
checks one result without assuming its depth. Final outputs are scalars, so
these cases exclude hydration; path keys stay compact in the seen set.

Medians show **before → after**. Query timings include planning, snapshot setup,
decoding, per-row validation, and cleanup, excluding graph setup. First-row time
ends when the consumer receives its first owned row. Peak RSS covers the whole
process, including graph setup.

| DISTINCT keys | Mode | First row ms | Total ms | Peak RSS MiB |
| --- | --- | ---: | ---: | ---: |
| Duplicate-heavy nodes | eager | 1451.391 → 1306.336 | 1451.406 → 1306.347 | 25.547 → 25.484 |
| Duplicate-heavy nodes | cursor | 1137.514 → 0.837 | 1137.532 → 1049.813 | 25.391 → 7.141 |
| Duplicate-heavy nodes | first | 1194.418 → 1.151 | 1194.435 → 1.163 | 25.453 → 6.812 |
| Unique paths/endpoints | eager | 2659.886 → 2901.607 | 2668.966 → 2904.695 | 67.500 → 65.766 |
| Unique paths/endpoints | cursor | 3224.444 → 1.427 | 3227.710 → 3552.494 | 63.094 → 35.672 |
| Unique paths/endpoints | first | 2543.344 → 0.726 | 2546.054 → 0.736 | 67.562 → 6.672 |
| Node page, SKIP 2 LIMIT 10 | eager | 8143.577 → 8343.231 | 8143.609 → 8343.265 | 52.484 → 52.547 |
| Node page, SKIP 2 LIMIT 10 | cursor | 8068.792 → 1.049 | 8069.049 → 6.468 | 52.500 → 6.891 |
| Node page, SKIP 2 LIMIT 10 | first | 6942.929 → 1.086 | 6942.959 → 1.098 | 53.719 → 6.828 |

Both cursor modes were Buffered before and are Incremental after the change.
First-row consumption falls from about 1.19 seconds to 1.2 ms for node keys,
2.54 seconds to 0.7 ms for unique paths, and 6.94 seconds to 1.1 ms for the page.
The limited page's full-cursor median falls from 8.07 seconds to 6.5 ms, with
process RSS falling from 52.5 MiB to 6.9 MiB. It stops upstream work once the
unique-row limit is satisfied and avoids buffering the complete result.

Full consumption still has costs. The duplicate-heavy cursor median falls
about 8%, from 1,138 to 1,050 ms, while the unique-key median rises about 10%,
from 3,228 to 3,552 ms. Unique-key trial ranges are 2,239–4,041 ms before and
3,341–3,660 ms after. Ranges overlap substantially and eager controls vary too;
these small samples do not establish a general throughput gain or an exact
slowdown for other workloads. Per-input SQL execution and scope transfer remain
costs even when most keys are duplicates.

Seen sets retain every unique key, including skipped ones. Full consumption
with 8,190 path/endpoint keys uses about 36 MiB, compared with 6.7 MiB when
consuming one row. This is still below the buffered cursor's 63 MiB median,
but demonstrates growth with retained keys. Each active UNWIND also retains its
complete input list. These results do not imply a whole-query memory cap.

Reproduce with the shared example and process runner:

```sh
cargo build --release --example cursor_traversal
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal first 10 0 distinct-entities 32
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 12 0 distinct-unique 4
python3 scripts/measure_dependency_graph.py target/release/examples/cursor_traversal cursor 12 0 distinct-page 128
```

Use `eager`, `cursor`, and `first` for all-row eager execution, full cursor
consumption, and one-row consumption followed by drop. Criterion adds
`cursor_traversal_distinct_{entities,unique,page}/{eager,cursor,first}`, using
the same depth and fanout values as these measurements.
