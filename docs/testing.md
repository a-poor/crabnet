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
- `tests/read_semantics.rs` holds focused read behavior matrices: nulls and
  types, directions, multiplicity, repeated bindings, comparisons,
  pagination, and parameters.
- `tests/arithmetic_and_distinct.rs` locks down numeric expression typing,
  precedence, invalid-operand nulls, typed ordering, and post-decode semantic
  `DISTINCT`, including pagination and ordering-scope boundaries.
- `tests/membership_and_string_predicates.rs` covers typed `IN` membership and
  the three string predicates across parameters, structured values, empty
  collections/strings, Unicode, null, and wrong-kind operands.
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
- `tests/unwind.rs`, `tests/union.rs`, and `tests/optional_match.rs` cover the
  row-expanding and null/deduplication composition operators, including bag
  multiplicity and clause-local scope.
- `tests/bounded_traversal.rs` covers exact/ranged/zero bounds, outgoing,
  incoming, and undirected traversal, per-hop type/property filters, cycles,
  self-loops, diamonds, parallel edges, dead ends, edge-trail uniqueness,
  composition, the depth-32 SQLite strategy threshold, and structured rejection
  boundaries.
- `tests/unbounded_traversal.rs` covers explicit bounds above that threshold,
  open-ended lower-bound forms, Rust trail enumeration, paths and relationship
  lists, mixed SQL/Rust composition, clause-wide relationship uniqueness,
  unions, explain-stage order, and retained rejection boundaries.
- `tests/path_values.rs` covers public named-path results for fixed, bounded,
  mixed, and zero-hop patterns; traversal order and stored relationship
  direction; identity across cycles, diamonds, and parallel edges; direct
  `WITH` aliases; and the current rejection boundary. It also verifies that
  branch-local distinctness/pagination and left-to-right unions shape pending
  path identities before one query-wide batch hydration stage.
- `tests/relationship_list_values.rs` covers fresh bounded legacy
  relationship-list bindings, zero-hop empty lists, traversal order versus
  stored endpoint direction, named-path coexistence, direct and `WITH` aliases,
  semantic distinctness/pagination, unions, query-wide hydration, scope, and
  the explicit unsupported contexts.
- Unit tests beside `planner/lower.rs` should cover binding, scope, semantic
  validation, and logical-plan shape when those details need focused
  diagnosis.
- Unit tests beside `planner/sqlite.rs` cover SQL lowering, internal aliases,
  bind placement, and graph-to-relational joins. They complement rather than
  replace execution tests.
- Unit tests in `database.rs` cover schema invariants, pending typed-cell
  equivalence, query-wide path-ID collection, direct-entity cache seeding, and
  failure modes that need direct SQLite access. Integration tests cover the
  staged hydration boundary; direct database tests retain late transaction
  failures and rollback.

For a new query feature, prefer a vertical set of tests: one successful
end-to-end example, relevant zero-row or `null` behavior, dynamic wrong-kind
operands without coercion, multiplicity and direction boundaries,
parameterized values, a nearby semantic or unsupported case, and rollback plus
final graph state when the feature writes.

## Rows are bags

Unless a read query specifies `RETURN DISTINCT`, Cypher result rows preserve
bag (multiset) multiplicity. Unless it specifies `ORDER BY`, their order is
unspecified. Tests for unordered results may canonicalize and sort rows for
comparison, but they must preserve duplicate rows. Never deduplicate the
result merely to make an assertion convenient; distinct tests must instead
exercise Crabnet's recursive typed equivalence directly.

For `RETURN DISTINCT`, assert that semantic deduplication happens before
`SKIP`/`OFFSET` and `LIMIT`, and keep `ORDER BY` within the projected expression
scope. This catches accidental SQLite JSON/text distinctness and incorrect
pre-dedup pagination. Path rows must compare ordered node and relationship
identities, not display properties or endpoint pairs alone.
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
cargo test --test write_semantics create_duplicate_property_keys_fail_before_any_mutation -- --exact
cargo test --test matched_mutations set_equals_replaces_the_complete_property_map -- --exact
cargo test --test delete_mutations duplicate_bag_targets_are_deleted_once_but_keep_return_snapshots -- --exact
cargo test --test bounded_traversal cycles_may_revisit_nodes_but_may_not_reuse_an_edge -- --exact
cargo test --test unbounded_traversal unbounded_cycle_revisits_nodes_but_never_reuses_an_edge -- --exact
cargo test --test path_values fixed_multi_hop_match_returns_a_public_typed_path -- --exact
cargo test --test relationship_list_values exact_relationship_list_coexists_with_its_named_path -- --exact
cargo test --lib planner::sqlite::tests::relationship_joins_are_lowered_to_endpoint_predicates -- --exact
```

Then run the adjacent integration or unit group:

```console
cargo test --test engine match_create
cargo test --test arithmetic_and_distinct
cargo test --test membership_and_string_predicates
cargo test --test matched_mutations
cargo test --test delete_mutations
cargo test --test bounded_traversal
cargo test --test unbounded_traversal
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
4. **A curated openCypher TCK subset.** Add this after the result model can
   represent the graph values required by the chosen scenarios. Pin the TCK
   revision, maintain an explicit scenario allowlist and exclusion reasons,
   and require every allowlisted scenario to pass. Run the small deterministic
   subset in normal CI rather than importing a mostly skipped suite.
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

Milestone 8 is in progress: named paths, path functions, legacy
relationship-list bindings, query-wide batch hydration, and deep/open-ended
trail expansion are green. `tests/path_values.rs`,
`tests/relationship_list_values.rs`, `tests/unbounded_traversal.rs`, and the
focused planner/database unit tests lock down pending-row shaping, ordered typed
lists, semantic distinct/union equality, clause-wide trail uniqueness,
parameter preflight, aliases, and hydration. The next active TDD slice is
shortest-path execution or a more general non-pushable physical boundary;
payload accounting and selective trail seeding remain explicit hardening work.
Concurrent external-writer behavior remains outside the test goal.
