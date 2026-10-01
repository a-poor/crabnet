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

Use the narrowest layer that can prove the behavior, while retaining an
end-to-end result test for user-visible semantics.

- `tests/engine.rs` is the broad end-to-end acceptance layer. It exercises
  Cypher text through the public `Database` API, including planner errors,
  matched writes, rollback, and resulting graph state.
- `tests/transactions.rs` exercises the public multi-query transaction API:
  read-your-writes, parameterized application decisions, commit and rollback,
  drop/panic cleanup, query errors aborting all prior work, graph and index
  restoration, per-query summaries and traversal budgets, hybrid hydration,
  write-lock release, and persistence after reopening. Transaction unit tests
  cover cleanup after SQLite itself has already rolled back and after a
  failed commit.
- `tests/read_semantics.rs` holds focused read behavior matrices: nulls and
  types, directions, multiplicity, repeated bindings, comparisons,
  pagination, and parameters.
- `tests/arithmetic_and_distinct.rs` locks down numeric expression typing,
  precedence, invalid-operand nulls, typed ordering, and post-decode semantic
  `DISTINCT`, including pagination and ordering-scope boundaries.
- `tests/with_distinct.rs` covers typed row deduplication at intermediate
  boundaries: nulls and recursive numeric equality, complete rows and
  wildcards, entity/path/graph-list identity, filtering and ordering scope,
  pagination, aggregation, stable projected random values, hybrid traversal,
  later bag expansion, write multiplicity, and rollback.
- `tests/membership_and_string_predicates.rs` covers typed `IN` membership and
  the three string predicates across parameters, structured values, empty
  collections/strings, Unicode, null, and wrong-kind operands.
- `tests/predicate_truth_tables.rs` compares every supported comparison,
  boolean, membership, string, and null predicate with an independent finite
  Rust oracle across 28 typed operand states, including missing properties and
  mixed numeric precision boundaries.
- `tests/property_invariants.rs` covers exact decoded identifiers and named
  parameters, SQL-looking labels/types/keys, case sensitivity, duplicate
  decoded output names, empty relationship types, recursive node/relationship
  property transport through every mutation form and path hydration, signed
  zero normalization, fail-closed 100-container value-depth boundaries, and
  the independent 128-step expression-syntax guard.
- `tests/property_index.rs` checks that the trigger-maintained
  `node_properties` index equals a recomputation from `nodes` after every
  kind of write and after a rolled-back statement, that indexed lookups keep
  Cypher equality across numeric, boolean, text, list, map, and null values,
  and which query shapes are planned through the index.
- `tests/entity_values.rs` covers whole node and relationship result values,
  recursive properties, identity, and stored-direction endpoints.
- `tests/write_semantics.rs` holds focused write result/state contracts that do
  not need the longer scenarios in `engine.rs`.
- `tests/matched_mutations.rs` covers matched node/relationship `SET` and
  `REMOVE`, whole-map replace/merge semantics, counters, aliases, parameter
  preflight, no-match behavior, and rollback.
- `tests/delete_mutations.rs` covers node/relationship deletion, detach edge
  cleanup, input-bag target deduplication, pre-delete return snapshots,
  counters, constraint failures, and rollback.
- `tests/aggregation.rs` and `tests/aggregation_edges.rs` cover implicit
  grouping, typed aggregate behavior, empty inputs, aggregate scope, aliases,
  and structured rejection boundaries.
- `tests/collect.rs` covers node/relationship collections and typed distinct
  collection: identity, duplicates, null/empty inputs, order, aliases, grouping,
  typed `UNWIND`, later matches and writes, property/label refreshes, delete
  snapshots, rollback, hybrid traversal, and unions.
- `tests/unwind.rs`, `tests/union.rs`, and `tests/optional_match.rs` cover the
  row-expanding and null/deduplication composition operators, including bag
  multiplicity and clause-local scope.
- `tests/path_unwind.rs` covers expansion of `nodes(path)`,
  `relationships(path)`, and legacy relationship lists into entity bindings:
  aliases, scope, segment junctions, zero hops, cycles, self-loops, parallel
  edges, stored direction, bounded/runtime/shortest producers, later matches,
  collection, distinctness/pagination, unions, hydration, parameters, and
  retained write rejections without side effects.
- `tests/bounded_traversal.rs` covers exact/ranged/zero bounds, outgoing,
  incoming, and undirected traversal, per-hop type/property filters, cycles,
  self-loops, diamonds, parallel edges, dead ends, edge-trail uniqueness,
  composition, the depth-32 SQLite strategy threshold, seeded versus unseeded
  start constraints, and structured rejection boundaries.
- `tests/unbounded_traversal.rs` covers explicit bounds above that threshold,
  open-ended lower-bound forms, Rust trail enumeration, paths and relationship
  lists, mixed SQL/Rust composition, clause-wide relationship uniqueness,
  unions, explain-stage order, and retained rejection boundaries.
- `tests/shortest_paths.rs` covers legacy one/all shortest selection per endpoint
  pair, deterministic ties, diamonds and parallel edges, same-endpoint identity
  versus positive cycles, directions, bounds, type/property/parameter filters,
  paths and relationship lists, path functions, prior bags, `WITH`, `UNION`,
  Rust BFS stage order and structured planning rejections. Traversal unit
  tests cover shortest-specific use of the shared fail-closed work guard.
- `tests/traversal_limits.rs` crosses the public default 100,000-state limit
  while emitting no runtime rows, then proves the connection remains usable.
  Traversal and executor unit tests separately pin exact compact-JSON byte
  accounting, empty relations, shortest-path parity, and query-wide sharing
  across `UNION` branches.
- `tests/path_values.rs` covers public named-path results for fixed, bounded,
  mixed, and zero-hop patterns; traversal order and stored relationship
  direction; identity across cycles, diamonds, and parallel edges; direct
  `WITH` aliases; and the current rejection boundary. It also verifies that
  branch-local distinctness/pagination and left-to-right unions shape pending
  path identities before one query-wide batch hydration stage.
- `tests/relationship_list_values.rs` covers fresh bounded legacy
  relationship-list bindings, zero-hop empty lists, traversal order versus
  stored endpoint direction, named-path coexistence, direct and `WITH` aliases,
  semantic distinctness/pagination, unions, query-wide hydration, scope, exact
  re-matching of a bound list in the same clause and after `WITH` across both
  producers, and the explicit unsupported contexts.
- Unit tests beside `planner/lower.rs` should cover binding, scope, semantic
  validation, and logical-plan shape when those details need focused
  diagnosis.
- Unit tests beside `planner/sqlite.rs` cover SQL lowering, internal aliases,
  bind placement, and graph-to-relational joins. They complement rather than
  replace execution tests.
- Unit tests beside `planner/physical.rs` exhaust valid runtime-slot producer
  and consumer permutations with static arguments present in every possible
  gap, and reject duplicate, missing, unreferenced, or reused slots.
- Unit tests in `database.rs` cover schema invariants, pending typed-cell
  equivalence, query-wide path-ID collection, direct-entity cache seeding, and
  failure modes requiring direct SQLite access. Its generated-fragment corpus
  asks initialized SQLite to prepare main and traversal-edge statements and
  validates read-only behavior, dense exact bindings, and result shape.
  Integration tests retain semantic and rollback coverage.

For a new query feature, prefer a vertical set of tests: one successful
end-to-end example, relevant zero-row or `null` behavior, dynamic wrong-kind
operands without coercion, multiplicity and direction boundaries,
parameterized values, a nearby semantic or unsupported case, and rollback plus
final graph state when the feature writes.

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
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --doc
git diff --check
```

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

Group ids are `plan/*`, `read/<query>/<nodes>`, `traversal/*`, `write/*`,
and `merge/*`, and Criterion writes HTML reports under `target/criterion/`.
Compare a change against `main` by running the suite before and after; the
roadmap's rule still holds that optimizations follow a measured bottleneck
rather than precede one.

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

Milestone 8 is complete for the supported read subset. Named paths, path
functions, relationship-list bindings, query-wide batch hydration,
deep/open-ended trail expansion, shortest-path BFS, and the explicit physical
operator boundary are green. `tests/physical_plan.rs` locks down pure SQL,
multiple runtime slots, shortest modes, branch-local shaping, unions, and one
root hydration operator. Planner/database unit tests additionally prove
whole-tree parameter preparation and one work budget across `UNION` branches;
the path and traversal integration suites remain the semantic oracle.

Application usability (milestone 10) is the next main milestone; its checklist
and current working order live in [roadmap.md](roadmap.md). Multi-query
transactions, `WITH DISTINCT`, entity collection/expansion, and path-list
`UNWIND` are complete. Milestone 9 continues as
supporting conformance work. Its deterministic invariants now exhaust
runtime-slot layouts, prepare a representative generated-SQL corpus directly
with SQLite, cover typed relationship properties in fixed and hybrid paths,
and compare 9,492 predicate outcomes with a finite independent Rust oracle.
The local property-invariant slice is also green: escaped and hostile names run
through fixed and hybrid routes, eleven unusual property keys survive the full
mutation lifecycle, recursive maps/lists survive entity and path hydration,
and the 100-container input boundary is preflighted atomically. Labeled nodes
after an earlier node-local `WHERE` and some bare contextual-keyword labels
still await an upstream `open-cypher` parser release; `0.2.1` was
checked and still reproduces them.

Public default state-budget exhaustion and the 128 MiB serialized-payload guard
are now covered. Selective traversal seeding is covered at every layer: the
compiler emits bound node-domain queries, the generated-SQL corpus prepares
them, `tests/physical_plan.rs` shows them or `unrestricted` in explanations,
producer unit tests prove end domains filter rows without pruning states, and
`tests/traversal_limits.rs` shows a constrained start staying inside the state
budget on a graph whose unconstrained seeding exhausts it. Retained-memory
accounting is covered by producer unit tests that pin hand-computed peaks,
exact boundaries, and zero residual charge after depth-first and shortest-path
searches, plus executor tests showing the charge propagates as
`Error::ResourceLimit` and is released between `UNION` branches.

The openCypher TCK now runs as a pinned conformance oracle. Its first run found
a real defect: the three-valued boolean type gate repeated its operand, so each
`AND`/`OR` multiplied the generated SQL tenfold and an eleven-term chain
exhausted memory. `boolean_connective_chains_compile_to_linear_sql` in
`tests/physical_plan.rs` is that mismatch minimized into a permanent local
regression. The exclusion reasons also make several documented deviations
visible as concrete scenarios: explicit-null properties are stored rather than
dropped. It also exposed a
second silent defect: floats lost digits beyond fifteen significant figures
twice on the way out, once when SQLite's `json()` rendered a REAL and once when
`serde_json` parsed the text back without its `float_roundtrip` feature. Reals
now leave SQL through a Rust shortest-round-trip formatter and
`floats_round_trip_through_sql_without_losing_significant_digits` in
`tests/arithmetic_and_distinct.rs` pins literals, parameters, stored
properties, arithmetic, and aggregates bit-for-bit. The Precedence1 scenarios
then exposed the same shape of defect as the boolean gate one level down:
every operator mentioned each operand several times and a derived type
repeated its value again, so nested comparisons and arithmetic grew
exponentially (`a = (a IN c)` was 150 KB of SQL, two five-operator arithmetic
terms 25 MB) and the whole TCK run took half a minute. Non-trivial operands
are now computed once in a correlated scalar-subquery scope;
`nested_operands_compile_to_linear_sql` in `tests/physical_plan.rs` pins the
linear growth, checks that leaf-only filters still open no scope, and
re-verifies results through the scoped shape. Because the exclusion
reason is the engine's own message, rejections must name the feature: the
lowering names every unsupported expression kind and operator rather than
falling back to a generic `expression` bucket, which is what lets
`cut -f2 tests/tck/exclusions.txt | sort | uniq -c | sort -rn` serve as the
ranked backlog. Concurrent external-writer behavior remains outside the test
goal.

`tests/pattern_predicates.rs` covers pattern predicates in every position
they may appear, their rejections, and identity comparison of entities.
`tests/labels.rs` covers the label list: creation order and repeats, label
expressions in patterns and `WHERE`, encoded-row label tests after `WITH`,
`SET`/`REMOVE` labels with their summary counters, `MERGE` label subsets and
actions, and `XOR`; the version 1 to 2 schema migration is a unit test in
`src/database.rs`.
`tests/write_pipelines.rs` covers reading and updating clauses alternating in
one statement: scalar values feeding writes, aliases keeping entity identity,
`MATCH` after a write, modifiers and aggregation after a write, multi-segment
statements (with the logical explanation's `segments:` section), `MERGE`
after `UNWIND`/`WITH`, wildcard projections, and chained comparisons.
`tests/case_expressions.rs` and `tests/functions.rs` are the direct semantic
suites for `CASE` and the scalar function library. Two of their checks guard
design decisions rather than results: nested `CASE` results must keep the
generated SQL linear, and `rand()` must produce one draw per call site per
row whose value and type agree, which is why it compiles to a joined
`json_each(json_array(random()))` source instead of an inline expression.
