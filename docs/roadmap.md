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
- read-only queries can cross standalone or chained `WITH` boundaries, carrying
  explicitly projected entity, path, and scalar bindings into later clauses
  while enforcing scope loss;
- aggregation/grouping, list expansion, query unions, and fixed-length optional
  matching compose through that read pipeline with explicit bag, type, and
  null-extension semantics;
- anonymous legacy bounded traversal compiles to recursive SQLite CTEs with an
  explicit maximum depth, endpoint results, and per-path edge uniqueness;
- mandatory read-only fixed, bounded, and mixed patterns can bind public typed
  paths, including zero-hop values; SQLite carries ordered IDs into pending
  cells, which Rust shapes before one query-wide batch hydration pass on a
  shared deferred snapshot;
- fresh bounded legacy relationship variables bind traversal-ordered typed
  relationship lists, including zero-hop `[]`, through the same query-wide
  identity shaping and hydration;
- `CREATE` and `MATCH ... CREATE` run transactionally, preserve input-row
  multiplicity, and roll back atomically;
- matched node and relationship properties support direct `SET`/`REMOVE` plus
  literal/parameter whole-map replacement and merge;
- matched `DELETE` and `DETACH DELETE` deduplicate targets across the input bag,
  report actual storage-row counters, and roll back atomically; and
- unsupported, semantic, parse, data, and missing-parameter failures are
  distinguished.

The current executor is already a hybrid: SQLite materializes match rows and
compact path identities, while Rust applies semantic row shaping, batch-hydrates
surviving paths, and executes mutations. We should grow that design instead of
restarting from a read-only architecture.

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
values were deliberately deferred here and entered their first supported slice
in milestone 8.

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

### 4. Model clauses and scopes as a pipeline — complete

- Read-only `MATCH` and `WITH` clauses are composable logical stages rather
  than one hard-coded read shape; standalone and chained `WITH` queries are
  supported.
- `WITH` carries explicitly projected bindings, supports aliases and scope
  loss, and can feed a subsequent `MATCH` or final `RETURN`. This milestone
  established entity/scalar transport; direct path aliases joined it in
  milestone 8.
- A `WITH`-local `WHERE` can use projected aliases and incoming variables; only
  the projected bindings survive into the next clause. `ORDER BY`,
  `SKIP`/`OFFSET`, and `LIMIT` are applied at that boundary.
- Each `WITH` stage lowers to a parameterized SQLite CTE. Normalized logical
  explanations show stages and scopes, while the physical explanation shows
  the generated SQLite fragment and binding positions.
- The pipeline retains bag semantics. `WITH ALL` is accepted; `WITH DISTINCT`,
  `WITH *`, unaliased computed expressions, and writes in a `WITH` pipeline
  remain explicit planning-time rejections. Aggregation was added by milestone
  6 without changing this clause-boundary model.

Gate complete: `tests/with_pipeline.rs` covers standalone and chained stages,
entity/scalar aliases, scope loss, filtering, intermediate pagination, later
matching, consecutive `MATCH` stages, parameters, empty input, multiplicity,
and structured rejection. Direct path aliases are covered separately in
`tests/path_values.rs`.
Multi-stage queries are inspectable through `crabnet::explain` and
`crabnet::explain_physical`.

### 5. Add core matched mutations — complete

- The existing `CREATE` executor now composes with a bounded matched-mutation
  path over a stable, materialized input bag.
- `SET entity.key = value` and `REMOVE entity.key` update node or relationship
  properties. Whole-map `=` replacement and shallow `+=` merge accept literal
  or parameter maps; label mutation and computed right-hand sides remain
  rejected.
- `DELETE` removes matched relationships or unconnected nodes. Explicit
  relationships are scheduled before nodes and targets are deduplicated across
  duplicate match rows. `DETACH DELETE` additionally removes every incident
  relationship before its node. In this bounded slice, deletion terminates the
  mutation sequence before an optional `RETURN`.
- Mutation results expose actual changed-property and deleted-storage-row
  counters. A post-delete `RETURN` uses the materialized pre-delete snapshots,
  while any runtime failure rolls back the entire statement.
- Generated IDs remain opaque; changing their representation is a separate
  contract decision.

Gate complete: `tests/matched_mutations.rs` and `tests/delete_mutations.rs`
cover node and relationship state, zero matches, multi-row and duplicate bags,
return snapshots, counters, preflight rejection, foreign-key failures, and late
transaction rollback.

### 6. Add query composition — complete

- Top-level `count`, `sum`, `avg`, `min`, `max`, and `collect` projections use
  implicit grouping and can cross `WITH` boundaries. Typed grouping and
  `count(DISTINCT ...)` share recursive Cypher equivalence; aggregate aliases,
  empty inputs, HAVING-style filters, ordering, and pagination are covered.
- `UNWIND` expands typed list values without losing duplicates and composes
  with matches, prior scalar scopes, and collected values.
- Read-only `UNION`/`UNION DISTINCT` perform semantic row deduplication in
  Rust, while `UNION ALL` appends bags. Branch-local scopes and modifiers are
  preserved, and output columns are validated before execution.
- Fixed-length `OPTIONAL MATCH` lowers to a null-extending SQLite stage. The
  complete introduced pattern becomes null on failure, and its attached
  predicate stays in the join condition instead of erasing the outer row.
- Existing `elementId`, `labels`, and `type` functions now propagate null from
  optional entities. Additional scalar functions remain incremental work, not
  an implied openCypher compatibility claim.

Gate complete: `tests/aggregation.rs`, `tests/aggregation_edges.rs`,
`tests/unwind.rs`, `tests/union.rs`, and `tests/optional_match.rs` cover typed
grouping, duplicate policies, empty input, composition, whole-pattern null
propagation, and structured rejection boundaries.

### 7. Add bounded variable-length traversal — complete

- Anonymous legacy `*n`, `*min..max`, and `*..max` relationship forms lower to
  recursive SQLite CTEs for mandatory read-only `MATCH`; the omitted lower
  bound is one, explicit zero-hop is supported, reversed ranges are empty, and
  the maximum upper bound is 32.
- Outgoing, incoming, and undirected traversal may use zero or one relationship
  type plus an inline property map applied at every hop.
- Each recursive candidate carries used edge IDs. Nodes may be revisited, but a
  relationship cannot be reused in one complete path, including across mixed
  fixed and bounded segments.
- Endpoint rows retain one result per relationship sequence across cycles,
  self-loops, diamonds, parallel edges, and dead ends, and compose with the
  existing read pipeline and aggregation.
- Unbounded traversal, postfix graph quantifiers, path selectors, quantified
  `OPTIONAL MATCH`, and traversal-fed writes remain structured rejections.
  Named path values and named relationship lists were deferred at this gate and
  now have their first supported subsets in milestone 8.

Gate complete: `tests/bounded_traversal.rs` covers bounds, directions,
properties, identity paths, adversarial graph shapes, edge-trail uniqueness,
composition, explicit resource limits, and rejection boundaries.

### 8. Generalize hybrid execution — in progress

- Generalize the matched-write row executor into SQL fragments connected by
  streaming or materialized Rust operators.
- The first slice is green: mandatory read-only fixed, bounded, and mixed
  patterns bind public `PathValue` results, including zero-hop paths. Direct
  `WITH` aliases, `RETURN DISTINCT`, and `UNION` preserve ordered node and
  relationship identity.
- The path-function slice is green. `length(path)` remains a SQL integer across
  filters, arithmetic, ordering, grouping, aggregate arguments, and aliases.
  `nodes(path)` and `relationships(path)` return traversal-ordered typed entity
  lists and survive computed `WITH` aliases.
- SQLite emits compact ordered IDs into pending typed cells after cursor
  materialization. Branch-local semantic `RETURN DISTINCT` and deferred
  pagination plus left-to-right unions operate before path and graph-list
  hydration, including cross-transport zero-hop empty-list equality.
- Only surviving IDs of the required entity kinds are deduplicated query-wide.
  Directly projected graph entities seed the hydration maps, and the remainder
  uses at most one node query and one edge query.
- Every branch SQL fragment and hydration query shares one deferred read
  snapshot. Concurrent external-writer semantics beyond it remain a non-goal.
- The relationship-list slice is green. A fresh bounded legacy binding such as
  `[rels:TYPE*min..max]` yields traversal-ordered hydrated relationships, with
  zero hops represented by `[]` and stored endpoint direction preserved. It
  survives direct returns, `WITH` aliases, semantic distinctness/pagination,
  and unions, and may coexist with a named path through the same query-wide
  identity shaping and hydration.
- Add deep/unbounded traversal, `shortestPath`, and `allShortestPaths` where
  Rust graph algorithms are clearer than SQL. Exact matching against a
  pre-bound relationship sequence, optional-bound paths or relationship lists,
  selectors, and writes fed by a named path or relationship list remain
  explicit rejections meanwhile.
- Implement `MERGE` last as transactional whole-pattern match/create behavior,
  not as a misleading single-row SQLite upsert.
- Render SQL/Rust boundaries in the physical plan.

Gate: a query can mix pushable and Rust-executed operators without changing
scope, ordering, null, or bag semantics. This gate is not complete. Concurrent
external-writer semantics remain a non-goal.

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

With the gates from milestones 2–7 complete, milestone 8 hybrid execution is
now active. Named path values, path functions, fresh bounded legacy
relationship-list bindings, and query-wide shared batch hydration are green;
deep or unbounded traversal remains. Small contract gaps may still be tightened
when a focused test exposes them. The day-to-day loop is documented in
[testing.md](testing.md); planned-but-inactive cases stay in the coverage
backlog instead of accumulating as ignored tests.
