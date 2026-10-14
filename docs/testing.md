# Testing workflow

Crabnet uses a lightweight, low-noise test-driven workflow. The goal is to
keep the test suite useful as a semantic contract without accumulating a large
red or ignored backlog for features that are not being implemented yet.

## The development loop

Work on one small behavior slice at a time:

1. Add the smallest focused test that demonstrates the behavior currently
   being implemented.
2. Run that test and confirm that it fails for the intended reason. A parse
   error, unrelated planner error, or broken fixture is not the expected red
   state unless that is the behavior under development.
3. Implement the behavior.
4. Rerun the focused test until it passes.
5. Run the adjacent test group, then the full checks before completing the
   milestone or handing the work off.

A larger slice may need a small coherent group of tests, but it should not add
dozens of speculative failures. Planned behavior belongs in the coverage
roadmap until it becomes the active implementation slice.

Milestones and handoffs must be green. Do not use `#[ignore]`, conditional
skips, or commented-out assertions to stockpile future conformance cases. When
an external suite such as the openCypher TCK is introduced, unsupported cases
should live in an explicit coverage/exclusion manifest rather than appearing
as ignored Rust tests.

## Test layers

Use the [test catalog](test-catalog.md) to find the suite covering a behavior.
It maps public integration tests, internal regressions and fault-injection
fixtures to their contracts. This guide covers the development loop, commands,
bag comparisons, and measurement practices.

## Rows are bags

Cypher rows preserve bag (multiset) multiplicity until an explicit distinct
projection or an aggregation changes it. Unless a query specifies `ORDER BY`, their order is
unspecified. Tests for unordered results may canonicalize and sort rows for
comparison, but they must preserve duplicate rows. Never deduplicate the
result merely to make an assertion convenient; distinct tests must instead
exercise Crabnet's recursive typed equivalence directly.

For `RETURN DISTINCT`, assert that semantic deduplication happens before
`SKIP`/`OFFSET` and `LIMIT`, and keep `ORDER BY` within the projected expression
scope. This catches accidental SQLite JSON/text distinctness and incorrect
pre-dedup pagination. Path rows must compare ordered node and relationship
identities, not display properties or endpoint pairs alone.
For `WITH DISTINCT`, also check that later matches and writes consume the
deduplicated bag, while prior writes remain intact and later failures roll
back the whole statement. Modifiers must reuse materialized projected values.
For path-bearing results, also assert that branch-local distinctness and
pagination plus every union operation precede exactly one query-wide
`batch-hydrate-paths` stage in the physical explanation.

Use exact row order only when the query has a meaningful `ORDER BY`, or when
cardinality makes order irrelevant. Generated entity IDs should normally be
checked for shape, uniqueness, and referential consistency rather than against
a fixed value.

## Focused commands

During the red/green loop, run one test by its exact name:

```console
cargo test --test engine match_create_runs_once_per_input_row -- --exact
cargo test --test read_semantics property_to_property_equality_is_type_aware -- --exact
cargo test --test arithmetic_and_distinct dynamic_arithmetic_is_numeric_only_and_null_propagating -- --exact
cargo test --test membership_and_string_predicates in_preserves_typed_membership_and_three_valued_null_semantics -- --exact
cargo test --test predicate_truth_tables binary_predicates_match_the_finite_typed_oracle -- --exact
cargo test --test predicate_truth_tables unary_predicates_match_the_finite_typed_oracle -- --exact
cargo test --test write_semantics create_duplicate_property_keys_fail_before_any_mutation -- --exact
cargo test --test property_invariants unusual_property_keys_survive_match_set_read_and_remove -- --exact
cargo test --test property_invariants recursive_property_depth_is_preflighted_before_any_write -- --exact
cargo test --test matched_mutations set_equals_replaces_the_complete_property_map -- --exact
cargo test --test delete_mutations duplicate_bag_targets_are_deleted_once_but_keep_return_snapshots -- --exact
cargo test --test bounded_traversal cycles_may_revisit_nodes_but_may_not_reuse_an_edge -- --exact
cargo test --test unbounded_traversal unbounded_cycle_revisits_nodes_but_never_reuses_an_edge -- --exact
cargo test --test shortest_paths lower_bound_one_finds_shortest_positive_cycles_to_same_endpoint -- --exact
cargo test --test path_values fixed_multi_hop_match_returns_a_public_typed_path -- --exact
cargo test --test relationship_list_values exact_relationship_list_coexists_with_its_named_path -- --exact
cargo test --lib planner::sqlite::tests::relationship_joins_are_lowered_to_endpoint_predicates -- --exact
cargo test --test read_semantics relationship_property_matching_preserves_scalar_and_nested_json_types -- --exact
cargo test --lib planner::physical::tests
cargo test --lib database::tests::generated_read_fragments_prepare_with_exact_dense_bindings -- --exact
```

Then run the adjacent integration or unit group:

```console
cargo test --test engine match_create
cargo test --test arithmetic_and_distinct
cargo test --test membership_and_string_predicates
cargo test --test predicate_truth_tables
cargo test --test matched_mutations
cargo test --test delete_mutations
cargo test --test property_invariants
cargo test --test bounded_traversal
cargo test --test unbounded_traversal
cargo test --test shortest_paths
cargo test --test path_values
cargo test --lib planner::sqlite
```

Before a milestone or handoff, run the complete checks:

```console
bash scripts/check.sh
```

The [check script](../scripts/check.sh) uses the lockfile for all Cargo build/test
commands and runs all targets (including the vendored TCK and both benchmark
smoke sizes), doctests, formatting, Clippy, and whitespace checks. It rejects
`CRABNET_TCK_UPDATE`; update pins separately and review every change. The
[CI workflow](../.github/workflows/ci.yml) runs the same checks on Ubuntu 24.04
with Rust 1.99.0, retains logs, and offers larger lifecycle measurements through
manual dispatch with `workloads: true`. macOS, rolling-stable, and scheduled CI
are paused to conserve Actions usage; see the [current CI contract](support.md).
The first hosted matrix and workload runs passed at
`683b8e9`; [support.md](support.md#qualification-evidence) links their preserved
evidence. Required-check enforcement is unavailable under the current private
repository plan; successful checks alone do not complete production qualification.

For release packaging, follow the [package rehearsal](package-readiness.md#repeating-the-local-package-rehearsal).
It builds the Cargo archive and checks an independent consumer, README examples,
doctests, TCK, opening, backup and transaction tests from the unpacked sources. It is a local
release check, not another recurring CI job.

The full suite need not run after every edit; focused commands keep the inner
loop fast, while the final checks prevent changes in one planner layer from
breaking another.

## Benchmarks

`benches/queries.rs` is a Criterion suite that measures planning, reads,
traversal, writes, and `MERGE` through the public `Database` API. It is a measuring
instrument, not a correctness oracle: every query is executed once before it
is timed, and the chain traversal cases assert their expected counts, so an
unsupported or wrongly shaped query fails the run rather than producing a
meaningless number.

```console
cargo bench --bench queries
cargo bench --bench queries -- 'read/.*/1000'
CRABNET_BENCH_SIZES=1000 cargo bench --bench queries -- traversal
```

The fixtures are deterministic and built in memory at the start of each group:

- The social graph has `CRABNET_BENCH_SIZES` people (default `1000,10000`)
  in communities of 250, four intra-community `KNOWS` edges per person, and
  two bridge edges per community. Communities are created one statement at a
  time because planning a multi-pattern `CREATE` is quadratic in its pattern
  count.
- The chain graph is 1,000 `Step` nodes joined by `NEXT`, so deep,
  open-ended, and above-threshold traversals have exactly one trail per depth.
- The fan-out `MERGE` cases rebuild a relationship-free people graph for every
  iteration, because the merge itself changes the graph; `fanout_shared`
  collapses the rows onto fifty keys while `fanout_distinct` creates once per
  row.

`write/set_property_batch_1000` repeatedly changes one property on 1,000
uniquely keyed nodes, retaining their other indexed values. Each iteration
checks the changed-property count. The dependency application measures full
first/repeat imports at 1,000 and 10,000 packages; see the
[node-upsert profile and comparison](dependency-graph.md#bulk-node-upserts-2026-10-08-follow-up).

Group ids are `plan/*`, `read/<query>/<nodes>`, `traversal/*`, `write/*`,
and `merge/*`, and Criterion writes HTML reports under `target/criterion/`.
Compare a change against `main` by running the suite before and after; the
roadmap's rule still holds that optimizations follow a measured bottleneck
rather than precede one.

`read/cursor_scan_properties/<nodes>` measures full cursor consumption, and
`read/cursor_first_row/<nodes>` measures preparation, first-row delivery, and
early-drop cleanup. They share the eager scan's fixture and projection.
The corresponding `union_all_scan_properties`, `cursor_union_all_scan_properties`,
and `cursor_union_all_first_row` cases repeat that projection across two branches
and validate the full duplicate-preserving row count before timing.
For process memory and first-row timings on a larger result bag, see the
standalone [cursor measurement](cursors.md#measurements).
`cursor_distinct_city`, `cursor_distinct_first_row`, `cursor_union_distinct_city`,
and `cursor_union_distinct_first_row` add duplicate-heavy cursor cases to the
same Criterion group. The distinct example separately measures latency and
process memory as the number of unique output rows changes.
`hydrate_paths`, `hydrate_nodes`, their `cursor_` counterparts, and
`cursor_hydrate_paths_first_row` / `cursor_hydrate_nodes_first_row` measure eager,
full-cursor, and first-row hydration on the same social graphs. The standalone
hydration example measures large repeated outputs with sizeable entity payloads.

## Rust traversal cursors

`tests/read_cursor_traversal.rs` covers the first incremental Rust traversal
slice: one deep/open-ended relationship and its endpoints with filters and
projections. Fourteen tests cover typed/path/list bags, parallel edges and cycles,
directions, endpoint/edge predicates, empty and zero-hop results, DISTINCT,
pagination, union counters, zero-page preflight, skipped-projection guards,
volatile expressions, fallback boundaries, partial
errors, hydration read-ahead, resource limits, snapshots, panic/drop cleanup,
and transactional rollback. A unit oracle compares emitted trail order and
state work with eager enumeration, and verifies producer memory charges are
released on completion, early drop, and initialization failure.

Constrained cursor tests hold transfer and memory budgets below the cost of
disconnected eligible edges, then prove early exit avoids a high-degree unread
neighborhood. Typed parameter filters and physical source explanations are
covered in both directions and undirected traversal. A second unit oracle
compares exact trail order over 64 graph subsets, all directions, unconstrained
fallback, zero-only/bounded/open-ended/impossible/reversed depths, and endpoint
domains. Shared neighbor-cache tests check index selection, dense bindings,
transfer accounting, cancellation on cache hits, exact retained-memory bounds,
failed-probe cleanup, and release between producers.

`tests/read_cursor_traversal_with.rs` adds ten tests for following plain WITH
chains: aliases, typed paths/entities/nested values, shadowing and dropped
incoming variables in WHERE, empty/zero-hop inputs, predicates, directions,
volatile alias reuse, final DISTINCT/UNION/pages, and explicit fallback for
global stages. Resource tests prove filtered trails spend work, early LIMIT/drop
stop the suffix, final SKIP guards RETURN while WITH expressions still run,
parameters preflight on zero pages, and errors are terminal. Coverage includes
hydration with earlier transaction writes, rollback/reuse, and read-only file
snapshot pinning and cleanup on exhaustion/error/drop/panic.

`tests/read_cursor_traversal_unwind.rs` adds ten tests for one UNWIND, with
plain WITH stages on either side. They compare scalar/entity/path/nested bags,
including nulls and repeated identities; prove per-trail versus per-member
volatile evaluation; cover final DISTINCT/UNION/pages, empty/non-list inputs,
resumption within one trail, early exit and later-member errors; and check
transfer/result/traversal limits, preflight, hydration-batch rollback, read-only
snapshots, cancellation, reuse and cleanup. The parity matrix also guards JSON
null normalization for entity UNWIND in eager and cursor execution.

`tests/read_cursor_traversal_unwind_chain.rs` adds eleven tests for chained
expansions with dependent scalar/entity/nested lists and plain WITH stages.
Four levels exercise scope drops, shadowing, filters, empty lists and backtracking;
volatile aliases refresh only in their own scope. Tests cover input-list/WITH/
RETURN errors, skipped projection guards, final DISTINCT/UNION/pages, zero-page
preflight, cumulative work, exact input/consumer transfer accounting, byte limits,
hydration across parent-member boundaries, rollback versus early exit, and
cancellation/deadlines plus snapshot cleanup on error/drop/exhaustion/panic.
Intermediate global stages and additional joins retain explicit fallback tests.

`tests/read_cursor_traversal_pages.rs` adds eleven tests for intermediate
WITH SKIP/LIMIT, before, between, and after expansions. An independent sequence
model covers successive filtered pages, and typed bag comparisons cover paths,
entities, nulls, duplicates, aliases, scope drops, empty inputs, and final shaping.
Tests prove that limits stop ancestors after draining descendants, skipped
projections preserve error evaluation, volatile aliases remain frozen, and
zero pages preflight without execution. Exact transfer budgets, cumulative
union work, hydration rollback, prepared reuse, cancellation, and read-only
snapshot release on drop/exhaustion/error/panic are covered.

`tests/read_cursor_traversal_distinct.rs` adds twelve tests for intermediate
DISTINCT across parent lists, independent stage/branch seen sets, complete-row
typed keys and numeric representatives, entities/nested values, path and
path-list keys sharing storage, hidden incoming filter scope, empty scopes,
volatile aliases, unique-row pages, early exit, projection errors, zero-page
preflight, exact duplicate/skip transfer budgets, hydration rollback, and
read-only snapshot/control cleanup. Full bags compare with eager execution;
unordered path pages compare to the cursor's own complete stream.

The existing cursor tests retain buffered coverage with ordered WITH stages and
shortest paths; eligible transaction cursor cases now assert Incremental.
`examples/cursor_traversal.rs` checks the complete depth histogram on a parallel
edge chain in eager/full-cursor/first-row modes. The `cursor_traversal/*`
Criterion cases use the same fixture. An optional unrelated-edge argument adds
disconnected eligible self-loops while retaining the same output bag. The
`cursor_traversal_neighbors/*` group adds 1,000 and 10,000 such edges. See the
[measurement guide](cursors.md#rust-traversal-measurements) for fresh-process
first-row latency, full-consumption time, and peak-RSS comparisons.
The example's optional `with` shape and `cursor_traversal_with/*` Criterion
group run two WITH stages over the same fixture and validate the same histogram.
The `unwind-nodes` and `unwind` shapes add path-node or scalar-range fanout,
with WITH stages on both sides. Full consumption checks each depth's row count,
member sum and squared sum; first-row consumption checks one one-hop result.
The scalar range accepts a fanout argument, defaulting to 128. Criterion adds
`cursor_traversal_unwind_nodes/*` and `cursor_traversal_unwind/*`.
The `unwind-chain-nodes` shape follows each path node with `range(0,n.id)`;
`unwind-chain` expands two ranges, with inner bounds depending on the outer member.
Full consumption checks row multiplicity, both member sums/squared sums, and their
cross-products per depth. Matching Criterion groups exercise both chains; fresh
process comparisons report first-row latency, full consumption, and peak RSS.
The `page-trails`, `page-members`, and `page-all` shapes add intermediate pages
before expansion, between dependent expansions, and across all expanded rows.
Bounded pages validate scalar-member bags independently of which unordered
trails were selected; the pass-through page also checks the depth histogram.
Matching `cursor_traversal_page_{trails,members,all}/*` Criterion groups cover
eager, full cursor, and first-row consumption.
The `distinct-entities`, `distinct-unique`, and `distinct-page` shapes exercise
duplicate-heavy node keys, unique path keys, and a limited DISTINCT stage.
Counts, per-depth member sums/squared sums, and valid member bounds guard the
result bags without assuming unordered representative order. Matching
`cursor_traversal_distinct_{entities,unique,page}/*` Criterion cases cover all
three consumption modes; measurements report the retained-key cost explicitly.

## Persistent application workload

`examples/dependency_graph.rs` imports real Cargo metadata and supplies a
deterministic disk-backed lifecycle workload through the public API. Its six
tests cover package/edge import, identity and annotation preservation, changed
and empty snapshots, hostile bound IDs, traversal results, invalid metadata,
resource-failure rollback, deletion, reopening, and refusing existing demo
files or unrelated graphs.

```sh
cargo test --example dependency_graph
cargo build --release --example dependency_graph
workload_dir=$(mktemp -d)
python3 scripts/measure_dependency_graph.py target/release/examples/dependency_graph demo "$workload_dir/graph.sqlite" 1000 20 128
```

The wrapper uses Python 3.11+ and macOS/Linux child peak RSS. Compile separately
and run measurements sequentially, so compiler memory and concurrent workloads
do not distort the results. The demo checks expected graph bags, identities,
query results, rollback, deletion, and reopening before reporting success.
See [the application guide](dependency-graph.md) for the fixture, timing and
memory boundaries, larger-scale commands, and recorded results.

`cargo test --all-targets` includes these example tests. For an example-only
change, `CRABNET_BENCH_SIZES=1000 cargo test --all-targets` still exercises all
Criterion query shapes at the smaller social-graph scale; report that override
explicitly. The default runs both 1,000 and 10,000-node social graphs.

## Adding broader test techniques

Broader techniques should arrive in an order that keeps their maintenance cost
proportional to their value.

1. **Semantic result tests first.** Continue growing the focused integration
   targets because executed results and graph state are the primary correctness
   oracle. Split by capability when a file becomes difficult to navigate.
2. **Focused plan and SQL tests next.** Once Crabnet has an intentionally stable
   plan or `EXPLAIN` rendering, snapshot a small representative set of logical
   plans and normalized SQL plus ordered bind descriptors. Do not snapshot
   every query or treat incidental whitespace and alias choices as API.
3. **Property tests for invariants.** Add generated cases for type and `null`
   truth tables, property round trips, unusual identifiers, parameterization,
   valid slot references, placeholder/binding agreement, and SQL that SQLite
   can prepare. Keep seeds reproducible and turn minimized failures into named
   regression tests.
4. **A pinned openCypher TCK run.** `tests/tck.rs` executes the whole
   vendored openCypher 2024.3 suite in normal CI. `tests/tck/allowlist.txt`
   names the scenarios that must pass and `tests/tck/exclusions.txt` names
   every other scenario with its observed, generated reason. The test fails
   on a regression, a newly passing scenario, or a changed reason, so the
   exclusion list is an explicit, reviewed backlog rather than a skip list.
   See `tests/tck/README.md` for the step semantics and update workflow.
5. **Differential testing.** After the supported grammar and result
   normalization are stable, generate small valid queries and graphs and
   compare them with a pinned reference implementation. Preserve bag
   multiplicity, compare ordering only when requested, and normalize
   implementation-specific IDs. Random reference-backed runs can begin as a
   nightly job; every useful mismatch should become a deterministic local
   regression test.

Snapshots, generated tests, TCK scenarios, and differential runs are supporting
oracles. A feature is complete only when its direct semantic tests are green
and its behavior is documented as supported.

## Current coverage and historical evidence

Use the [test catalog](test-catalog.md) for suite ownership and
[coverage](coverage.md) for the supported surface. The [roadmap](roadmap.md) and
[first-release checklist](first-release.md) track current priorities; this guide
does not maintain a second milestone backlog.

[Historical testing notes](history/testing-notes.md) retain the discoveries that
motivated existing regressions, including float transport and generated-SQL
size defects. Their historical feature restrictions are not today's contract.
