# Reusable compiled reads

`compile_read(cypher)` returns an owned `CompiledRead`. It parses, validates,
lowers, and generates the physical/SQL plan once. `columns()` exposes output
names without connecting to a database or binding parameters.

Both `Database` and `Transaction` provide `execute_compiled`,
`execute_compiled_with`, and `execute_compiled_with_options`. These are eager:
each call resolves fresh parameters throughout the complete plan, then executes
with fresh resource counters and the supplied options. Resolved bindings and
execution state belong only to that call. Binding still allocates resolved SQL
and values; SQLite still prepares or retrieves its connection-local cached
statements. This API does not cache query results.

Standalone execution uses one new deferred read transaction across SQL/UNION
branches, Rust traversal, and hydration. It releases the snapshot on return,
including failure, with no partial result. Transactional execution sees earlier
writes in the parent snapshot; any execution-call error, including invalid
parameters or cancellation, aborts and rolls back the entire parent transaction.
The compiled plan remains reusable on another active connection or transaction.

`compile_read_with_options` checks cancellation and deadlines before and after
successful planning. It does not preempt parsing or individual planner work.
Other budgets cover execution work and do not cap compiler memory or time.
Compilation retains no options, parameters, connection, snapshot, or SQLite
statement. Cancelling a compilation token later does not cancel execution unless
the caller also supplies that token to the execution call. Free-standing compile
errors cannot abort an application transaction.

The plan owns its query literals and generated SQL. It can execute on any
database opened by the same Crabnet build, including read-only connections and
files migrated by the normal opening workflow. Compilation does not inspect
stored data, uniqueness declarations, or connection-specific schema statistics;
supported writes, constraint changes, and restore do not require recompilation.
Raw external schema modification remains unsupported. Plans have no serialized
format or compatibility promise across engine versions. Cloning copies the
owned plan; callers can use `Arc<CompiledRead>` to share it across independent
connections without copying it.

`PreparedRead` retains its existing bound-parameter cursor contract. Compiled
reads initially support eager execution; cursor binding can be a later slice.

## Validation and measurement protocol

`tests/compiled_reads.rs` covers changing scalar/list/map/null bindings, owned
source lifetime, static/mutation rejection, whole-plan preflight, pagination,
bags and typed graph values, hybrid/shortest traversal, union/distinct/hydration,
fresh controls, read-only/reopened/restored/migrated files, independent connections,
transaction rollback, constraint restoration, and immediate writer lock release.
Unit tests attempt competing writes during path production in both UNION
branches before hydration, and cancel execution after a previous branch has
produced rows. No partial eager result may escape. The example verifies the
complete rows from both strategies plus explicit fixture oracles.

Declared before measurement: build `examples/repeated_reads.rs` in release mode
with the locked dependencies. The four-node Git Graph-shaped fixture has four
parent edges, including a two-parent merge stored out of position order. Each
iteration cycles through root/left/right/merge and performs one commit read and
one parent read with the same limits and options. Both modes warm up four
iterations, then measure 10,000 iterations / 20,000 public queries, including
per-result oracle checks. Construction and one-time compilation are outside the
execution timer in both modes. Each run must verify exactly 20,000 returned rows,
parameter-dependent parent lists, labels, metadata, and duplicate-sensitive
position bags. A successful run prints `all_rows_verified: true`.

Run three fresh processes per mode, interleaved ordinary/compiled,
compiled/ordinary, ordinary/compiled. Keep every raw stdout, stderr, process
exit, process CPU/RSS observation, and host-load sample in fresh output. Record
the source and binary hashes, toolchain, and revision. Other agents continue
working. Report all samples and medians without dropping slow runs. These are
diagnostic measurements without a latency acceptance gate; they neither replace
Git Graph's full protocol nor establish that compiled reads solve its remaining
shared-host latency failure.

```sh
cargo build --release --locked --example repeated_reads
target/release/examples/repeated_reads ordinary 10000
target/release/examples/repeated_reads compiled 10000
cargo bench --bench queries -- compiled_reads
```

The Criterion group separately measures compilation and changing-OID execution
for both modes. It shares the verified fixture and contributes four smoke checks
to the ordinary all-targets validation.

## Recorded results (2026-10-13 UTC)

Measured on macOS 26.5.1 arm64 with the locked release build, on top of `fabc8c4`
and the compiled-read changes. Three fresh processes per mode:

| Execution | Samples, ms | Median, ms | Peak process RSS, MiB |
| --- | --- | --- | --- |
| Ordinary, changing OIDs | 1,052.51 / 960.11 / 1,032.67 | 1,032.67 | 7.20 |
| Compiled, changing OIDs | 236.98 / 231.50 / 254.94 | 236.98 | 6.47 |

Every run verified 20,000 queries and 20,000 returned rows. The median compiled
time was about 77% lower (4.4 times faster) in this four-node workload. Recorded
process user CPU ranged from 0.93–0.98 seconds for ordinary execution and
0.23–0.25 seconds for compiled execution. These observations demonstrate useful
planning amortization for this query pair; they do not predict full merge-base
latency, large-history RSS, or behavior under arbitrary host load.

The six-process window recorded load averages of 7.27 / 9.35 / 10.60; other
agents were not paused. All raw probe stdout, macOS `time -l` stderr, exits,
CPU/RSS observations, host samples, commands, toolchain, binary hash, and source
hashes are retained in the report. A source archive and runner were also retained
under the report's local output paths. The earlier sandbox attempt verified its
application results but `time` failed on a denied `sysctl kern.clockrate`; its
output is retained separately and is not one of the six measurement samples.

`bash scripts/check.sh` passed on the implementation: 1,003 tests/doctests,
147 benchmark smoke checks at the default 1,000/10,000 sizes, formatting, Clippy
with warnings denied, and whitespace checks. The pinned TCK manifests are
unchanged. The API is experimental; Git Graph must validate an explicit upgrade
and rerun its own complete result/work/resource and latency protocol.
