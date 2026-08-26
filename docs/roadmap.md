# Crabnet roadmap

This is a living plan for an experimental embedded property-graph database. It
is intentionally a direction of travel rather than a compatibility promise.
Update it when implementation work teaches us something better.

## Project posture

- Crabnet is used only in this repository today. We may freely reshape its
  modules, public API, logical plan, SQLite schema, and on-disk representation.
- There is no schema-migration machinery while that remains true. When the
  schema changes, recreate experimental databases instead of carrying migration
  code for formats nobody relies on.
- Git provides recovery. Code should be clear enough to support the next
  experiment, not preserved merely because it already exists.
- Correct results or a structured plan-time rejection are both acceptable.
  Plausible but incorrect results are not.

## Where we are

Crabnet already has more than a greenfield skeleton:

- openCypher text is parsed by the in-house parser, bound, and lowered into a
  backend-neutral slot-based logical row plan;
- fixed-length node/relationship patterns compile to parameterized SQLite;
- the physical schema uses `nodes` and `edges`, while Cypher-facing code uses
  the word *relationship*;
- computed scalar projections, typed JSON properties, comparisons, membership,
  string predicates, arithmetic, semantic distinctness, ordering, pagination,
  and named parameters cover a useful read subset;
- `CREATE` and `MATCH ... CREATE` run transactionally, preserve input-row
  multiplicity, and roll back atomically; and
- unsupported, semantic, parse, data, and missing-parameter failures are
  distinguished.

The current executor is already a small hybrid: SQLite materializes match rows
and Rust applies mutations. We should grow that design instead of restarting
from a read-only architecture.

## Design principles

1. **Semantics first.** Cypher bag semantics, three-valued logic, type behavior,
   scope, and relationship uniqueness are part of the feature—not cleanup.
2. **Bind all user data.** Values, labels, relationship types, and JSON paths
   become SQLite parameters. Only compiler-owned SQL structure is emitted.
3. **Keep graph and storage vocabulary separate.** A Cypher relationship is
   stored as an `edges` row. A path is a query value, never a table row.
4. **Push down contextually.** The eventual physical planner should fuse the
   largest useful SQL fragments, but pushability depends on parents, ordering,
   null extension, and required columns; it is not a permanent boolean on an
   isolated operator.
5. **Preserve multiplicity.** Unless `DISTINCT` is requested, rows are a bag.
   Test normalization must never hide duplicate-path or Cartesian-product bugs.
6. **Introduce abstractions when a feature needs them.** A constrained SQL/CTE
   builder will be useful for composition. A large general SQL AST is not a
   prerequisite for improving today's compiler.
7. **Optimize from evidence.** Statistics, join reordering, generated property
   columns, and additional indexes follow query plans and benchmarks.

## Target pipeline

The existing modules can evolve toward this pipeline without a flag-day
rewrite:

```text
Cypher text
  -> parser AST
  -> binder/analyzer (names, scopes, graph/scalar kinds)
  -> logical row-operator plan
  -> contextual physical plan (SQLite fragments and Rust operators)
  -> execution in a consistent SQLite snapshot/transaction
  -> typed Cypher rows and mutation summary
```

Plans should eventually have a deliberate, stable text rendering. That gives
tests a useful boundary and can later power an `EXPLAIN` facility.

## Milestones

Each milestone should leave the supported subset documented and the full suite
green. The order is intentional, but discoveries may move individual features.

### 1. Make the current contract explicit

- Maintain the feature inventory in [coverage.md](coverage.md).
- Document value, null, comparison, ordering, ID, and multiplicity decisions in
  [semantics.md](semantics.md) as those decisions are locked down.
- Fill high-risk tests for current behavior: null truth tables, all directions,
  repeated variables, 3+ hops, structured properties, Cartesian products,
  parameters, rollback, and structured rejection.
- Add a normalized logical-plan renderer and a small set of reviewed plan/SQL
  fixtures; do not snapshot every generated SQL string.

Gate: every feature we call supported has representative positive, empty/null,
multiplicity, and boundary coverage where applicable.

### 2. Introduce real Cypher result values — complete

- The public row model uses recursive `CypherValue` variants for scalars,
  lists, maps, nodes, and relationships.
- `NodeValue` and `RelationshipValue` expose opaque element IDs, labels/types,
  properties, and directed relationship endpoints.
- Reads and post-`CREATE` projections return whole nodes and relationships
  across the planner/executor boundary.

Gate complete: `MATCH (n) RETURN n`, relationship equivalents, and post-create
returns round-trip without confusing graph entities with ordinary maps. Path
values remain deliberately deferred to the traversal and hybrid-execution
milestones.

### 3. Finish the fixed-length read core — complete

- Read predicates now include typed `IN`, `STARTS WITH`, `CONTAINS`, and
  `ENDS WITH`, with explicit three-valued behavior and no dynamic coercion.
- Read projections and predicates support numeric `+`, `-`, `*`, `/`, `%`, and
  unary `+`/`-`; exponentiation (`^`) and concatenation (`||`) remain explicit
  planning errors.
- Ordering comparisons are type-aware: integers and floats interoperate,
  strings compare with strings, booleans compare with booleans, and null or
  incompatible operands produce null.
- Read-only `RETURN DISTINCT` performs semantic, post-decode row deduplication
  in Rust. Pagination follows deduplication, and a distinct query may order only
  by projected expressions or aliases.
- Relationship uniqueness remains per fixed pattern, while parallel-edge and
  path multiplicity remain intact for non-distinct queries.

Gate complete: the fixed-pattern subset has a broad semantic corpus in
`tests/arithmetic_and_distinct.rs` and
`tests/membership_and_string_predicates.rs`, alongside the existing read and
entity suites, with dynamic invalid operands covered as null rather than
silently coerced.

### 4. Model clauses and scopes as a pipeline

- Separate binding/scoping from lowering enough to model clause boundaries.
- Turn the current flat read plan into composable row operators.
- Implement `WITH` first, including scope loss, aliases, filtering, ordering,
  skip, and limit between segments.
- Introduce a small SQL/CTE representation once composition makes direct string
  assembly hard to reason about.

Gate: multi-stage queries have inspectable logical and physical plans and obey
Cypher variable-scope rules.

### 5. Add core matched mutations

- Retain and extend the existing `CREATE` executor.
- Add `SET`, `REMOVE`, `DELETE`, and `DETACH DELETE` with explicit graph-state,
  zero-match, multi-row, and rollback tests.
- Keep generated IDs opaque; changing their representation is a separate
  contract decision.

Gate: each mutation composes after `MATCH`, returns the documented counters and
rows, and is atomic across all input rows.

### 6. Add query composition

- Aggregation and grouping (`count`, `sum`, `avg`, `min`, `max`, `collect`).
- `UNWIND`, `UNION`/`UNION ALL`, and `OPTIONAL MATCH`.
- For optional matches, test that predicates belonging to the optional pattern
  do not accidentally become outer filters and erase null-extended rows.
- Add commonly needed scalar and graph functions deliberately, with explicit
  null/type semantics.

Gate: grouping, duplicate handling, empty input, and optional null propagation
match the documented semantics.

### 7. Add bounded variable-length traversal

- Lower explicitly bounded relationship quantifiers to recursive CTEs.
- Track used **edge IDs** per candidate path; revisiting a node is legal, while
  reusing a relationship in the same path is not.
- Test cycles, self-loops, diamonds, parallel edges, dead ends, bounds, and row
  multiplicity.
- Start with endpoint results, then expose relationship lists/path values.
- Keep truly unbounded traversal rejected until it has complete semantics; do
  not silently substitute a hidden depth cap.

Gate: bounded traversal is correct on adversarial small graph shapes and its
resource limit is explicit.

### 8. Generalize hybrid execution

- Generalize the matched-write row executor into SQL fragments connected by
  streaming or materialized Rust operators.
- Add path values, deep/unbounded traversal, `shortestPath`, and
  `allShortestPaths` where Rust graph algorithms are clearer than SQL.
- Implement `MERGE` last as transactional whole-pattern match/create behavior,
  not as a misleading single-row SQLite upsert.
- Render SQL/Rust boundaries in the physical plan.

Gate: a query can mix pushable and Rust-executed operators without changing
scope, ordering, null, or bag semantics.

### 9. Conformance and optimization

- Add property tests for binding, type/null matrices, planner invariants, and
  generated SQL preparation.
- Adopt a pinned, curated openCypher TCK allowlist once typed graph values and
  setup clauses can represent its scenarios faithfully.
- Add deterministic differential tests against a pinned reference engine;
  minimize every mismatch into a permanent local regression.
- Optimize only after benchmarks identify a real bottleneck.

Gate: the allowlist is entirely green, exclusions are explicit, randomized
failures are reproducible, and optimizations preserve plan/result contracts.

## Near-term working order

With the typed-value gate from milestone 2 and the fixed-length read gate from
milestone 3 complete, the next active work is milestone 4: model clauses and
scopes as a pipeline, beginning with `WITH`. Small contract gaps may still be
tightened when a focused test exposes them. Path values remain planned for the
traversal and hybrid-executor milestones. The day-to-day test loop is
documented in [testing.md](testing.md); planned-but-inactive cases stay in the
coverage backlog instead of accumulating as ignored tests.
