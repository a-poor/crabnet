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
- Unit tests beside `planner/lower.rs` should cover binding, scope, semantic
  validation, and logical-plan shape when those details need focused
  diagnosis.
- Unit tests beside `planner/sqlite.rs` cover SQL lowering, internal aliases,
  bind placement, and graph-to-relational joins. They complement rather than
  replace execution tests.
- Unit tests in `database.rs` cover schema invariants and failure modes that
  need direct SQLite access, especially late transaction failures and
  rollback.

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
pre-dedup pagination.

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
cargo test --lib planner::sqlite::tests::relationship_joins_are_lowered_to_endpoint_predicates -- --exact
```

Then run the adjacent integration or unit group:

```console
cargo test --test engine match_create
cargo test --test arithmetic_and_distinct
cargo test --test membership_and_string_predicates
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

The next active test slice is milestone 4: `WITH` clause boundaries, aliases,
scope loss, filtering, ordering, and pagination between pipeline stages.
