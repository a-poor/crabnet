# Cargo dependency graph application

[`examples/dependency_graph.rs`](../examples/dependency_graph.rs) is a runnable
Rust application using only Crabnet's public API. It imports Cargo metadata,
answers dependency questions, preserves local review annotations on reimport,
and exercises a complete persistent graph lifecycle. Every command prints JSON;
errors go to stderr with a nonzero exit status. Query commands open read-only;
review/delete require an existing writable graph. Import can create a graph.
All use the default five-second lock timeout; see the
[opening contract](semantics.md#file-opening-and-contention).

## Import a real workspace

Build once, then run the executable directly:

```sh
cargo build --release --example dependency_graph
workload_dir=$(mktemp -d)
cargo metadata --format-version 1 --locked > "$workload_dir/metadata.json"
target/release/examples/dependency_graph import "$workload_dir/cargo.sqlite" "$workload_dir/metadata.json"
```

Run `cargo metadata` in the workspace you want to explore, or pass Cargo its
`--manifest-path`. The importer uses that invocation's resolved graph: it does
not resolve features or targets itself. Metadata must include `resolve`; output
from `--no-deps` is rejected. Package IDs are opaque strings from Cargo, rather
than names, so different versions and sources remain distinct.

For example, select the first workspace member and query it:

```sh
package_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["workspace_members"][0])' "$workload_dir/metadata.json")
target/release/examples/dependency_graph dependencies "$workload_dir/cargo.sqlite" "$package_id"
target/release/examples/dependency_graph dependents "$workload_dir/cargo.sqlite" "$package_id"
target/release/examples/dependency_graph review "$workload_dir/cargo.sqlite" "$package_id"
target/release/examples/dependency_graph import "$workload_dir/cargo.sqlite" "$workload_dir/metadata.json"
```

The last command keeps the review annotation. `path DATABASE SOURCE_ID TARGET_ID`
returns one shortest directed path, including its package IDs and hop count.
`remove DATABASE PACKAGE_ID` detaches and deletes a package and reports the
number of nodes and relationships removed. A subsequent authoritative import
recreates a removed package if it still appears in the metadata, with a new
identity and an initially false review annotation.

`dependencies` and `dependents` return distinct packages reachable within **four
hops**, sorted by ID; they are not an unbounded transitive closure. Multiple
paths through a diamond produce one package in this view. `path` is unweighted
and permits the zero-hop identity path. Unreachable pairs return JSON `null`;
unknown endpoints are errors. Removing an unknown package returns zero counts.

## Storage and import contract

Use a dedicated database. The CLI accepts an empty graph or one carrying this
example's import marker, and refuses unrelated nonempty graphs. Read/update
commands require an existing initialized file. The marker is an application
convention, not an isolation boundary for arbitrary external writers.

| Entity | Stored properties | Identity |
| --- | --- | --- |
| `:Package` | `package_id`, `name`, `version`, `workspace`, `reviewed`, `generation` | Unique `package_id` |
| `:DEPENDS_ON` | `generation` | One directed relationship per source/target package pair |
| `:DependencyImport` | `key: 'cargo'`, `generation` | Unique `key` |

Normal, build, and development dependencies, and aliases resolving to the same
package, collapse into one edge. The example does not model their kinds,
feature selections, aliases, or target predicates separately.

Before writing, the importer validates IDs, field types, and dependency
endpoints. One application transaction then installs the uniqueness
constraints, advances the generation, upserts packages and dependencies, removes
stale relationships and packages, and commits. An empty snapshot removes the
entire package graph. Invalid input or a query failure cannot leave a partial
import. The importer uses bound parameters throughout, including for IDs
containing quotes or newlines.

Unchanged packages and relationships keep their opaque element IDs. Import
updates `name`, `version`, and `workspace`, and initializes `reviewed` only when
creating a package. Reimport is idempotent for graph identity and application
data, but advances generation bookkeeping and performs writes. Relationship
uniqueness comes from the application's `MERGE` and snapshot model; it is not
a database relationship constraint.

The default batch size is 128, configurable from 1 through 1024. Node imports
use `MERGE (p:Package {package_id: row.id})` directly: computed MERGE keys now
use the property index without a separate scalar projection. Edge imports
still project `row.source` and `row.target` into scalar aliases for indexed
read matching. Computed probes are limited to MERGE so read filters preserve
their error behavior when there are no candidate nodes.

Queries use a 60-second cooperative deadline, cumulative materialization
limits of 1,000,000 rows / 128 MiB, and final-result limits of 250,000 rows /
64 MiB, alongside the default traversal guards. An import shares one absolute
deadline across its queries; row and byte counters reset for each query. These
are example policy choices, not database defaults or a process-memory cap.
Metadata parsing, fixture construction, application-side verification, opening,
constraint installation, and commit are outside those query budgets. Results
remain eager. See [the resource contract](semantics.md#query-resource-controls).

## Reproduce the lifecycle workload

```sh
cargo build --release --example dependency_graph
workload_dir=$(mktemp -d)
python3 scripts/measure_dependency_graph.py target/release/examples/dependency_graph demo "$workload_dir/1000.sqlite" 1000 20 128 > "$workload_dir/1000.json"
python3 scripts/measure_dependency_graph.py target/release/examples/dependency_graph demo "$workload_dir/10000.sqlite" 10000 20 128 > "$workload_dir/10000.json"
```

The measurement wrapper requires Python 3.11+ on macOS or Linux. It runs exactly
one already-built executable and reports that child's peak RSS in bytes, wall
time, platform, architecture, command, and binary SHA-256. Build separately so
Cargo/compiler memory and build time do not contaminate the measurement. Run
trials sequentially and use a fresh database path for each; `demo` refuses to
overwrite an existing file. Without the wrapper, the executable still reports
all phase and query timings.

The deterministic fixture has components of up to 64 packages. Each package
depends on the next one and two packages within its component, producing
diamonds without exponential unbounded path enumeration. At 1,000 packages it
has 1,952 dependencies; at 10,000 it has 19,529. The fixed query endpoints are
packages 0 and 63, so the shortest path remains 32 hops at both scales while
the surrounding database grows. These are controlled scaling fixtures, not a
claim that real dependency graphs have this topology.

Each run:

1. Imports the complete graph and checks stored package and dependency bags.
2. Reimports and compares values and opaque node/relationship IDs.
3. Measures lookup, four-hop dependencies, four-hop dependents, and shortest
   path. Repeated results must agree; the fixture independently checks reachable
   IDs, the minimum hop count, and every edge of the returned path.
4. Marks a package reviewed. A later transaction changes it and creates a probe
   node, then deliberately exceeds a zero-row result budget. It verifies
   `Error::ResourceLimit`, an aborted commit, and complete rollback.
5. Deletes the final package, checking its incident-edge counts and remaining
   graph against the fixture.
6. Closes and reopens the file, comparing all remaining values and identities
   and checking that the review annotation survived.

The report separates import node/edge/prune/commit phases, repeat import, update,
rollback, deletion, reopening, and final database size. The first query call is
reported separately from 20 subsequent samples; p50 and p95 use nearest ranks.
Queries include planning, execution, decoding, endpoint-existence checks, and
application result shaping. They use a connection already warmed by import:
even `first_ms` is **not a cold-cache measurement**. Main-file size is measured
after closing the final connection and includes free pages, without vacuuming.

Peak RSS covers the whole application process, including the input fixture,
SQLite, eager results, and correctness snapshots. It is not an isolated query
allocation measurement. Total process wall time also includes correctness
checks and report generation; individual phase timers exclude those checks.

## Measurements and next work

Observed on 2026-10-04 with Rust 1.99.0, the release profile, bundled SQLite,
and macOS 26.5.1 arm64. Three sequential fresh-file trials per scale used the
same binary, batch size 128, and 20 repeated samples per query. The table shows
medians across trials; query entries are medians of each trial's p50 / p95.
These are local observations, not performance guarantees. The
[measurement record](measurements/dependency-graph-2026-10-04.json) contains all
trial timings, environment details, binary hashes, and the comparison below.

| Metric | 1,000 packages / 1,952 edges | 10,000 packages / 19,529 edges |
| --- | ---: | ---: |
| Initial import | 525.7 ms | 5,613.3 ms |
| Repeat import | 177.3 ms | 2,021.5 ms |
| Lookup p50 / p95 | 0.036 / 0.040 ms | 0.036 / 0.042 ms |
| Four-hop dependencies p50 / p95 | 0.240 / 0.251 ms | 0.244 / 0.257 ms |
| Four-hop dependents p50 / p95 | 0.238 / 0.244 ms | 0.242 / 0.253 ms |
| Shortest path p50 / p95 | 1.561 / 1.601 ms | 18.399 / 18.785 ms |
| Reopen (`reopen_ms`, opening only) | 0.321 ms | 0.437 ms |
| Complete process wall time | 0.807 s | 8.640 s |
| Peak RSS | 19.31 MiB | 82.83 MiB |
| Main database file after deletion and close | 2.01 MiB | 19.58 MiB |

All six runs passed the lifecycle checks. Across trials, peak RSS ranged from
18.84–19.83 MiB at 1,000 packages and 82.36–83.67 MiB at 10,000. The same CLI
also imported this repository's actual metadata: 91 packages and 143 dependency
pairs. A repeat import created no nodes or edges; forward/reverse queries and
the direct path from Crabnet to `serde_json` were verified.

The workload immediately exposed an application query-shape problem. The
original `MERGE (p:Package {package_id: row.id})` took a median 2,705.9 ms for
the 1,000-package first import and 2,506.6 ms for reimport. The 10,000-package
attempt reached the 60-second deadline, leaving zero nodes, edges, or
uniqueness declarations after rollback. Projecting `row.id` into a scalar
slot made the existing property-index probe applicable: first import improved
about 5.1× and reimport about 14.1× at 1,000 packages. No engine changes were
needed. The computed-key planner follow-up is now implemented, removing that
query-shaping requirement. This result does not establish that plan caching
is the principal bottleneck.

Two performance questions emerged from this run. Node upserts still
take about 77% of initial import time at 10,000 packages, so profile matcher,
mutation, and property-index maintenance costs before adding plan reuse.
Shortest-path latency grows with the surrounding graph despite fixed endpoints
and hop count; the neighbor-loading follow-up below addresses eligible-edge
loading and adjacency construction.
The eight-result distinct neighborhood queries stay nearly flat, giving no
reason to prioritize semantic deduplication for this workload.

Opening options, reader/writer contention, process-termination recovery, and
backup/restore are covered by the library's persistence tests; see the
[recovery contract](semantics.md#backups-and-process-recovery). This example's
workload verifies ordinary close/reopen and transactional rollback. The initial
[incremental cursor API](cursors.md) is available for single SQLite fragments;
this example still uses eager correctness snapshots, which contribute to the
reported process RSS.

### Computed MERGE keys: 2026-10-08 follow-up

Computed MERGE keys now use indexed candidate lookups, with the exact typed
comparison retained. This run compares the direct `row.id` query before and
after the planner change, and the scalar-alias workaround on the updated
engine. All variants retain the same edge-import aliases. The pre-change
engine is commit `f82a8bd` with only the node-import query changed to the direct
form; its 10,000-package case was not rerun.

Three sequential fresh-process, fresh-file trials per variant and scale used
the release profile, Rust 1.99.0, macOS 26.5.1 arm64, batch size 128, and 20
samples per read query. Variant order rotated between trials; builds and test
runs finished before measurement. All 15 runs passed the complete lifecycle
checks, and every repeat import created zero nodes and relationships. The
[measurement record](measurements/computed-merge-keys-2026-10-08.json) retains
each trial, binary hashes, import phases, whole-process wall time and peak RSS.

| Packages | Engine / node-import key | Initial import | Repeat import |
| ---: | --- | ---: | ---: |
| 1,000 | Before / direct `row.id` | 6,476.0 ms | 5,334.1 ms |
| 1,000 | After / direct `row.id` | 1,888.3 ms | 571.8 ms |
| 1,000 | After / projected scalar | 1,681.2 ms | 406.9 ms |
| 10,000 | After / direct `row.id` | 19,807.7 ms | 7,905.9 ms |
| 10,000 | After / projected scalar | 21,193.8 ms | 7,414.9 ms |

The direct form improved about 3.4× for initial imports and 9.3× for repeat
imports at 1,000 packages. Explicit projection still has lower medians at
that size, particularly on repeat imports. At 10,000 packages, the direct
and projected forms have overlapping trial ranges. Timings varied noticeably:
10,000-package initial imports ranged from 18.4–24.5 seconds for direct keys
and 20.3–24.8 seconds for projected keys. These are local observations;
compare variants within this run rather than against the earlier session.

The example now uses the direct node-import form. Computed read-filter
probes remain separate work because evaluating a fallible key before a scan
can introduce errors when no candidate nodes exist. Edge-import aliases
therefore remain in place. The `merge_keys` Criterion group also compares
direct and projected keys for first and repeat 1,000-row in-memory merges:
`cargo bench --bench queries -- merge_keys`.

### Shortest-path neighbor loading: 2026-10-08 follow-up

Constrained shortest-path queries now load filtered neighbors through SQLite's
endpoint indexes as BFS expands each node. The producer caches those lists
across tied paths, positive-cycle searches, and start nodes. Unrelated
components do not enter the cache. Same-clause start labels or inline
properties enable this strategy; an unrestricted start still uses the eager
source. The existing endpoint property constraints in the dependency-graph
application enable it without an application query change.

The isolated workload in
[`examples/shortest_path_scaling.rs`](../examples/shortest_path_scaling.rs)
holds a 64-node, 125-edge component fixed, with the same next-one/next-two
topology as the application fixture. It adds 0, 1,000, or 10,000 disconnected
nodes, each with one relationship of the eligible type. Every timed query
must return 32 hops; an untimed hydrated-path check verifies endpoints and
every relationship. Unlike the application workload, this fixture is in
memory and measures `RETURN length(p)` without endpoint-existence checks or
application shaping. Setup is reported separately.

```sh
cargo build --release --example shortest_path_scaling
python3 scripts/measure_dependency_graph.py target/release/examples/shortest_path_scaling 10000 100
cargo bench --bench queries -- shortest_scaling
```

The first query is separate from the repeated samples, but follows fixture
creation on the same connection and is not a cold-cache result. Process RSS
includes fixture construction and SQLite; it is not the traversal accounting
budget. The regression tests independently enforce transfer-row and exact
retained-memory boundaries.

An instrumented pre-change release run separated eligible-edge loading,
adjacency construction, and BFS plus runtime-row encoding. With 10,000
unrelated edges, their median times were 6.711 ms, 4.605 ms, and 0.167 ms,
respectively. With none, they were 0.046 ms, 0.023 ms, and 0.110 ms. The
search examined 125 candidate steps at every scale, while the eager source
loaded 125, 1,125, and 10,125 edges. Phase timers exclude endpoint-domain
queries, destruction, planning, and the consuming SQLite fragment.

The end-to-end comparison used uninstrumented release binaries, Rust 1.99.0,
and macOS 26.5.1 arm64. The baseline is commit `519db19` with the identical
benchmark example copied into an isolated checkout. Three fresh-process trials
per engine and scale each collected 100 repeated samples. Engine order
alternated between trials; builds and tests finished before measurement. All
18 runs passed the length and hydrated-path checks. The table gives medians
across trials of each run's nearest-rank p50 / p95.

| Unrelated edges | Before p50 / p95 | After p50 / p95 | Median speedup |
| ---: | ---: | ---: | ---: |
| 0 | 0.288 / 0.776 ms | 0.333 / 0.792 ms | 0.86× |
| 1,000 | 1.192 / 2.880 ms | 0.342 / 0.799 ms | 3.48× |
| 10,000 | 17.358 / 25.936 ms | 0.346 / 0.877 ms | 50.12× |

The small graph pays about 0.046 ms (16%) more for individual neighbor probes.
With unrelated data, median query time stays near the small-graph result.
This does not establish a speedup when most of a large graph is reachable:
search still explores that region, and neighbor probes have per-node overhead.
At 10,000 unrelated edges, trial p50s ranged from 16.326–17.852 ms before and
0.331–0.486 ms after. Whole-process peak RSS ranged from 25.56–26.19 MiB before
and 22.19–22.30 MiB after. These are local observations, not guarantees or
isolated query-memory measurements. The
[measurement record](measurements/shortest-neighbors-2026-10-08.json) contains
all samples, phase-profile medians, environment details, source and binary
hashes, and process measurements.
