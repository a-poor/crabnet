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

The default batch size is 128, configurable from 1 through 1024. Both node and
edge queries project lookup keys into scalar slots before matching; this lets
the current planner use its property index. In particular, `MERGE` on a direct
`row.id` map access currently takes a much slower scan path.

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
needed. Supporting index probes for additional deterministic computed keys is
a concrete planner follow-up; this result does not establish that plan caching
is the principal bottleneck.

Two performance questions now have evidence behind them. Node upserts still
take about 77% of initial import time at 10,000 packages, so profile matcher,
mutation, and property-index maintenance costs before adding plan reuse.
Shortest-path latency grows with the surrounding graph despite fixed endpoints
and hop count; inspect eligible-edge loading and adjacency construction next.
The eight-result distinct neighborhood queries stay nearly flat, giving no
reason to prioritize semantic deduplication for this workload.

Opening options and reader/writer contention are now covered by the library's
file-operation tests. The next application-usability slice is interrupted-process
recovery and backup/restore. This example verifies ordinary close/reopen and
transactional rollback; it does not establish a crash-recovery or backup
contract. Incremental result consumption remains separate work; the eager
correctness snapshots contribute to the reported process RSS.
