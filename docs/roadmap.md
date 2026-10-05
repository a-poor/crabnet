# Crabnet roadmap

This is a living plan for an experimental embedded property-graph database. It
is intentionally a direction of travel rather than a compatibility promise.
Update it when implementation work teaches us something better.

The next main milestone is **usable inside a real embedded Rust application**
(milestone 10). Its checklist below is the active task list. Conformance and
optimization continue to support that work; broad temporal compatibility is
deferred until an application needs it.

## Project posture

- Crabnet is used only in this repository today. We may freely reshape its
  modules, public API, logical plan, SQLite schema, and on-disk representation.
- Schema compatibility remains experimental. Version 4 preserves the upgrades
  from main's versions 1, 2, and 3; migrations and index backfills run atomically.
  Unreleased branch-only formats may require recreating the database.
- Git provides recovery. Code should be clear enough to support the next
  experiment, not preserved merely because it already exists.
- Correct results or a structured plan-time rejection are both acceptable.
  Plausible but incorrect results are not.

## Where we are

Crabnet already has more than a greenfield skeleton:

- openCypher text is parsed by the in-house parser, bound, and lowered into a
  slot-based logical row plan;
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
- legacy variable-length traversal uses recursive SQLite CTEs for explicit
  maxima through 32 and Rust relationship-unique trail enumeration materialized
  as JSON for larger or omitted maxima;
- mandatory read-only fixed, variable-length, and mixed patterns can bind public
  typed paths, including zero-hop values; SQLite carries ordered IDs into
  pending cells, which Rust shapes before one query-wide batch hydration pass
  on a shared deferred snapshot;
- fresh legacy relationship variables, including deep/open-ended forms, bind
  traversal-ordered typed relationship lists with zero-hop `[]` through the same
  query-wide identity shaping and hydration;
- legacy `shortestPath` and `allShortestPaths` factors use Rust BFS over one
  variable-length relationship to select one deterministic but unspecified
  minimum trail or all tied unweighted minimum trails per endpoint pair. Lower
  bound zero includes identity; lower bound one permits shortest positive
  same-endpoint cycles. Results reuse the path/list composition and hydration
  boundary;
- relationship uniqueness spans every constituent path pattern in one `MATCH`,
  while a later `MATCH` starts an independent scope;
- `CREATE` and `MATCH ... CREATE` run transactionally, preserve input-row
  multiplicity, and roll back atomically;
- matched node and relationship properties support direct `SET`/`REMOVE` plus
  literal/parameter whole-map replacement and merge;
- matched `DELETE` and `DETACH DELETE` deduplicate targets across the input bag,
  report actual storage-row counters, and roll back atomically; and
- unsupported, semantic, parse, data, and missing-parameter failures are
  distinguished.

The current executor is already a hybrid: SQLite materializes match rows and
compact path identities, while Rust enumerates deep/open-ended trails or runs
shortest-path BFS and feeds fully materialized JSON relations back into SQLite,
applies semantic row shaping, batch-hydrates surviving graph values, and
executes mutations. We should grow that design instead of restarting from a
read-only architecture.

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
- Relationship uniqueness spans all constituent fixed patterns in one `MATCH`;
  a later `MATCH` starts an independent scope. Parallel-edge and path
  multiplicity remain intact for non-distinct queries.

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
- The pipeline retains bag semantics. `WITH ALL` and `WITH *` are accepted;
  `WITH DISTINCT` and unaliased computed expressions remain explicit
  planning-time rejections. Aggregation was added by milestone 6 without
  changing this clause-boundary model, and updating clauses were later
  admitted anywhere in it (see the TCK notes below).

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
  or parameter maps. Computed right-hand sides and label mutation arrived
  later (see the TCK notes below).
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

- Anonymous legacy `*n`, `*min..max`, and `*..max` relationship forms
  established the recursive SQLite CTE path for explicit upper bounds through
  32. The omitted lower bound is one, explicit zero-hop is supported, and
  reversed ranges are empty. Depth 32 is now a physical-strategy threshold,
  not an overall query limit.
- Outgoing, incoming, and undirected traversal may use zero or one relationship
  type plus an inline property map applied at every hop.
- Each recursive candidate carries used edge IDs. Nodes may be revisited, but a
  relationship cannot be reused anywhere in one `MATCH`, including across
  fixed, variable-length, and comma-separated constituent patterns.
- Endpoint rows retain one result per relationship sequence across cycles,
  self-loops, diamonds, parallel edges, and dead ends, and compose with the
  existing read pipeline and aggregation.
- Deep/open-ended traversal, named path values, and named relationship lists
  were deferred at this gate and now have supported subsets in milestone 8.
  Postfix graph quantifiers, path selectors, quantified `OPTIONAL MATCH`, and
  traversal-fed writes remain structured rejections.

Gate complete: `tests/bounded_traversal.rs` covers bounds, directions,
properties, identity paths, adversarial graph shapes, edge-trail uniqueness,
composition, the SQLite-depth threshold, and rejection boundaries.

### 8. Generalize hybrid execution — complete

- Read plans now form explicit physical operator trees connecting parameterized
  SQLite fragments, Rust value producers, pending-row transforms,
  left-associated unions, and one query-wide hydration operator when graph
  identities require it.
- The path-value slice is green: mandatory read-only fixed, variable-length,
  and mixed patterns bind public `PathValue` results, including zero-hop paths.
  Direct `WITH` aliases, `RETURN DISTINCT`, and `UNION` preserve ordered node and
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
- Every runtime traversal producer, branch SQL fragment, union, and hydration
  query shares one deferred read snapshot. External-writer semantics remain a
  non-goal.
- The relationship-list slice is green. A fresh legacy binding such as
  `[rels:TYPE*min..]` yields traversal-ordered hydrated relationships across
  bounded, deep, and open-ended forms, with zero hops represented by `[]` and
  stored endpoint direction preserved. It survives aliases, semantic row
  shaping, and unions, and may coexist with a named path through the same
  query-wide identity shaping and hydration.
- The deep/open-ended slice is green. Explicit maxima above 32 and omitted
  maxima use Rust trail enumeration; a bare `*` or `*..` starts at one, while
  `*0..` includes identity. Explicit maxima through 32 keep the SQLite CTE fast
  path.
- The legacy shortest-path slice is green. In mandatory read-only `MATCH`,
  `shortestPath` and `allShortestPaths` run Rust BFS over one variable-length
  relationship with lower bound zero or one and select one deterministic but
  unspecified minimum trail or all tied unweighted minimum trails per endpoint
  pair. Direction, bounds, edge type and properties, parameters,
  identity and positive-cycle behavior, path and
  relationship-list values, path functions, prior bags, `WITH`, and `UNION` are
  covered. Modern selectors, expression-position calls, optional/same-clause
  `WHERE`/multi-part forms, lower bounds above one, and writes remain structured
  rejections.
- Rust preserves relationship uniqueness across every constituent pattern in
  one `MATCH`, materializes ordered trail identities as JSON, and exposes each
  relation through a branch-local runtime slot consumed by SQLite. Numbered
  physical explanations show each producer before its consuming fragment and
  keep SQL plus static/runtime placeholder descriptors beneath that fragment.
- Whole-tree preparation resolves deferred pagination, ordinary parameters, and
  traversal-edge filters in every `UNION` branch before the first producer or
  SQL fragment executes.
- Fixed query-wide guards cap hybrid traversal at 100,000 explored trail states
  and 128 MiB of exact compact JSON runtime payload. Both fail closed with
  `Error::ResourceLimit`, and the payload encoder builds its final relation
  incrementally instead of retaining a second whole serialization. Selective
  seeding and a deterministic 256 MiB retained-memory budget for eligible
  edges, adjacency maps, and frontiers landed in milestone 9.
- The explicit read boundary is complete for the supported operator set, with
  one SQL fragment per branch. Future non-pushable reads can add a value or row
  operator without reviving traversal-specific SQL bindings; a construct that
  requires SQL-to-Rust-row-to-SQL composition will first need a multi-fragment
  extension. Exact pre-bound relationship-sequence matching compiles to a
  position-wise SQL comparison between a traversal's ordered edge IDs and the
  bound list's identity, so it needed no new operator kind. Optional-bound
  paths or relationship lists, modern selectors, deferred shortest forms, and
  path-fed writes remain explicit rejections.
- `MERGE` is transactional whole-pattern match/create behavior, not a
  single-row SQLite upsert: the executor matches the pattern from every row
  with one read over the materialized rows, then settles the unmatched rows
  one at a time, re-matching each alone before creating, so rows observe each
  other's creations in order. The first version re-read every pending row
  after each create, which the benchmarks showed to be cubic in the row
  count: a merge creating one node per row took 3.6 s for 250 rows and now
  takes 71 ms.

Gate complete: supported queries mix pushable and Rust-executed operators
without changing scope, ordering, null, bag, snapshot, or parameter-preflight
semantics. `tests/physical_plan.rs` locks down the operator boundary.
Concurrent external-writer semantics remain a non-goal.

### 9. Conformance and optimization — in progress

- The first deterministic invariant slice is green: physical-plan tests exhaust
  19,046 runtime producer/consumer layouts, while a seven-family compiler
  corpus asks SQLite to prepare every main and traversal-edge fragment and
  verifies dense bind positions plus result shape.
- Fixed and Rust-backed open relationship-property filters now share a typed
  scalar/nested JSON matrix.
- A finite independent Rust oracle now exhausts 9,408 binary and 84 unary
  predicate outcomes across 28 missing/null/boolean/numeric/text/list/map
  operands. It covers mixed integer/float precision boundaries and explicitly
  locks the current stored-JSON equality rule for structured values.
- The property-invariant slice is green. Escaped, case-sensitive, and SQL-
  looking label/type filters remain bound across fixed, SQLite-recursive, and
  Rust-backed reads. Contextual variables, aliases, parameters, and unusual
  node/edge keys survive their applicable read, mutation, and removal routes;
  recursive values survive whole-entity and path hydration. Duplicate decoded
  output columns, empty created relationship types, and values beyond the
  documented 100-container boundary now fail before execution. Remaining bare
  contextual keyword labels and labeled-node-after-local-`WHERE` cases are isolated to
  the parser; `open-cypher 0.2.1` still reproduces them, so they await
  a later upstream release.
- The selective-seeding slice is green. Start-endpoint label and
  inline-property constraints compile to a bound node-domain query that seeds
  the hybrid producer, an unconstrained positive-hop start seeds only nodes
  that begin an eligible edge, and end-endpoint constraints filter emitted rows
  before serialization. Both domains are supersets of the SQLite join, so the
  physical-plan tests, generated-SQL corpus, and traversal suites lock in
  unchanged results with fewer explored states and bytes. The first benchmark
  run showed the SQLite recursive-CTE route still seeded every stored node, so
  a `*1..3` count from one person cost as much as enumerating every depth-3
  trail in the graph; the CTE seed now repeats the same start constraints,
  which cut that case from 1.75 s to 12 ms on 10,000 nodes.
- The node property index slice is green. Benchmarks showed every property
  lookup scanning its whole label, so a point lookup cost 5.7 ms on 10,000
  nodes and every `MERGE` row paid it again. A `node_properties` table now
  holds each top-level non-null node property as `json_extract` returns it,
  kept in step by triggers on `nodes`. When a scanned node has an inline
  property equality or a top-level `WHERE` conjunct `n.key = expr`, where
  `expr` is a literal, parameter, carried scalar, or another entity's
  property, the compiler adds an `id IN (...)` probe of the index beside the
  exact comparison. With multiple labels, each candidate checks label
  membership through an `EXISTS` probe of `(label, node_id)` in `node_labels`. Variable-length seeds and hybrid node domains
  get the same probe. Because SQL equality on the stored value is necessary
  for Cypher equality, the probe can only drop rows the exact comparison
  would reject. Measurements on the original single-label branch with
  10,000 nodes: the point lookup went from 5.7 ms to
  35 µs, a filtered three-hop count from 5.8 ms to 0.11 ms, a `*1..3` count
  from one person from 11.9 ms to 0.4 ms, a matched `MERGE` from 0.69 ms to
  0.13 ms, and a 1,000-row distinct `MERGE` fan-out from 786 ms to 140 ms.
  The cost is on writes: creating one node went from 26 µs to 34 µs and a
  100-node batch got about 9% slower. Relationship properties, range
  predicates, and equalities inside `OR` are not indexed yet.
- The wrong-answer TCK slice is green apart from NaN. Of the 43
  `result-mismatch` and `side-effects-mismatch` rows, 36 now pass. List and
  map equality is decided in Rust (`crabnet_equals`) so nested nulls make it
  null and `[1] = [1.0]` holds; lists order lexicographically under `<`
  (`crabnet_compare`); `ORDER BY`, `min`, and `max` use a byte sort key
  (`crabnet_order_key`) that encodes Cypher's global orderability across
  kinds; `IN` is null when a member's comparison is null; and `CREATE` and
  `SET n = {...}` no longer store null-valued properties. The index probe
  falls back to every node with the key when the expected value is a list or
  map, whose JSON text can differ from an equal stored value. The TCK
  harness now counts `+labels` as label names new to the graph. The seven
  remaining rows compare NaN, which SQLite turns into NULL, so they need a
  NaN representation of their own. Main read, row-source, and traversal
  statements now go through rusqlite's prepared-statement cache, which took a
  point lookup on 10,000 nodes from 35 µs to 14 µs. The temporary rows table
  that `MERGE` and matched writes read through is now emptied rather than
  dropped and recreated, since every schema change expired the whole cache;
  a 1,000-row distinct `MERGE` fan-out went from 149 ms to 50 ms and a
  matched relationship `MERGE` from 0.40 ms to 0.10 ms.
- The type-error slice is green. Of the 188 `expected-error-not-raised` rows,
  184 now pass, and 185 scenarios moved to the allowlist in all. A planning
  pass (`planner/typecheck.rs`) infers the kind of literals and of `WITH` and
  `UNWIND` variables bound to them, and rejects `NOT 1`, `1 AND 'a'`,
  `1 IN 'a'`, `5.key`, `properties(1)`, and arithmetic or quantifier
  predicates over literal non-numbers with `Error::Semantic`. Generated SQL
  raises the new `Error::Type` (through `crabnet_type_error`) when the kind is
  only known at runtime: boolean contexts, `-`, `*`, `/`, `%` and unary signs,
  indexing, map key access, `IN`, `keys()`, `properties()`, and the
  conversion functions. A non-integer `range()` argument raises
  `Error::Argument`. Mixing `UNION` and `UNION ALL` and a literal null
  `MERGE` property are now semantic errors, and deleting a connected node
  fails with the new `Error::Constraint` instead of a raw SQLite error. Every
  raise sits in a `CASE` branch that only a value of the wrong kind reaches,
  in both a value's SQL and its type SQL, since result encoding can read
  the type without the value. Still open: the remaining scalar functions,
  `size`, slices, string predicates, and quantifiers over a non-list return
  null for a wrong kind; reading a deleted entity does not fail; and storing
  a list of maps stays allowed as a Crabnet extension.
- The pre-bound relationship-list slice is green. A bound list reused on a
  later variable-length relationship, in the same `MATCH` or after `WITH`,
  matches only the traversal whose relationship sequence equals it position by
  position; multi-segment `relationships(path)` aliases compare as one
  sequence, same-clause re-matches stay out of relationship uniqueness against
  their own binding, and both SQLite-recursive and Rust-backed producers accept
  the filter.
- The retained-memory slice is green. Eligible edges, seed and end-domain
  lists, adjacency maps, search frontiers, visited maps, and retained cycles
  are charged to a third fail-closed counter as they are loaded or queued,
  released as frontiers pop and producers finish, and costed by a fixed
  per-ID formula so the failure point is platform-independent. Unit tests pin
  hand-computed peaks and exact boundaries; executor tests show the charge
  released between `UNION` branches.
- The openCypher TCK slice is green. The complete openCypher 2024.3 suite is
  vendored and runs in normal CI against pinned `allowlist.txt` and
  `exclusions.txt` files that must agree with observed behavior in both
  directions. More than half of the expanded scenarios pass; every other
  scenario carries a generated reason, dominated by unsupported functions,
  temporal types, expression forms, and multi-label setup clauses. The first
  run surfaced and fixed exponential SQL growth in boolean connective chains.
- Add deterministic differential tests against a pinned reference engine;
  minimize every mismatch into a permanent local regression.
- Optimize only after benchmarks identify a real bottleneck.

Gate: the allowlist is entirely green, exclusions are explicit, randomized
failures are reproducible, and optimizations preserve plan/result contracts.

### 10. Application usability — in progress

The goal is one real Rust application that can import a graph, query it,
update it atomically, and reopen it with predictable behavior. Keep the
existing semantic tests and pinned TCK outcomes green throughout this work.

- [x] **Transactions spanning multiple queries — complete.**
  `Database::transaction()` exposes parameterized execution, read-your-writes,
  explicit commit and rollback, and rollback on drop. Any query error aborts
  and rolls back the entire transaction; an aborted handle cannot execute or
  commit. `tests/transactions.rs` covers Rust read/decide/write workflows,
  later-statement and post-write result failures, graph/index restoration,
  hybrid reads, per-query budgets, file-backed visibility and reopening, and
  connection reuse. Unit tests cover SQLite-initiated rollback and failed
  commits. Full tests, pinned TCK, benchmark smoke checks, doctests, formatting,
  and Clippy pass.
- [x] **Everyday query composition — complete.** `WITH DISTINCT` is complete, including
  typed recursive equality, entity/path/list identity, scope and modifier
  rules, hybrid reads, and deduplication before later writes with rollback.
  Ten focused integration tests and eight newly passing TCK scenarios cover
  the slice. Entity collection and expansion are also complete:
  `collect(n)`, `collect(DISTINCT n)`, relationship collections, and `UNWIND`
  preserve typed identity, ordering, null behavior, and write multiplicity.
  Nine integration tests cover aliases, grouping, graph reads, property/label
  refreshes, delete snapshots, and rollback; seven more TCK scenarios pass.
  Scalar `collect(DISTINCT ...)` uses recursive typed equality.
  Path-list `UNWIND` is complete for `nodes(path)`, `relationships(path)`,
  their aliases, and legacy relationship-list bindings. Eight integration
  tests cover segment junctions, repeated identities, zero hops, all supported
  path producers, entity reads and matches, collection, distinctness, unions,
  and hydration. Path-fed writes remain unsupported.
  Aggregate expressions are complete in `RETURN` and `WITH`: arithmetic,
  scalar functions, `CASE`, maps, scalar collection operations, and
  `size(collect(n))`. Aggregates are materialized once per group before scalar
  evaluation, keeping empty-input cardinality and SQL growth bounded. Twelve
  integration tests cover grouping scope, types/nulls/parameters, modifiers,
  distinctness and pagination, entity identity, runtime traversal, and write
  multiplicity/rollback. Twenty-five additional TCK scenarios pass, including
  proper rejection of ambiguous compound grouping expressions. Broader
  entity-list indexing, comprehensions, and nesting remain follow-up work.
- [x] **Application uniqueness constraints.** `Database` and `Transaction`
  expose create/list/drop methods for a node label/property pair such as
  `:Person.email`. Installation rejects conflicting existing data atomically;
  missing/null values are exempt, and other values use recursive typed
  `DISTINCT` equivalence. Trigger-maintained keys enforce immediate checks
  across `CREATE`, `MERGE`, property and label changes, and deletion. Twelve
  integration tests cover typed precision, bags, independent/overlapping keys,
  hostile names, rollback/abort, and persistence/connection visibility.
  Schema version 4 upgrades versions 1–3 atomically, with migration and
  failed-upgrade tests. Composite keys, relationship constraints, and Cypher
  schema DDL remain follow-up work.
- [x] **Caller-controlled query resources.** `QueryOptions` on both execution
  APIs configures traversal budgets, SQLite instruction work, cooperative
  deadlines/cancellation, cumulative materialized rows/bytes, and final
  result rows/bytes. One context spans branches, mutation stages, and hydration.
  Failures preserve rollback and connection reuse; tests cover cancellation
  after writes, cross-thread interruption, cleanup on panic, exact accounting
  boundaries, and per-query reset. Accounting boundaries and cooperative
  timing limits are explicit in the semantics document.
- [x] **Incremental result consumption — initial slice.** `prepare_read*`
  returns a reusable `PreparedRead` with explicit `ReadMode`; its borrowing
  cursor decodes single SQLite fragments without runtime producers one row
  at a time. Rust operators retain explicit buffered execution. Eleven tests
  cover typed bags, snapshot pinning, early drop, partial errors, resource
  accounting across advances, cancellation, read-only files, lock failures,
  and reuse. The lifetime contract and [scan measurements](cursors.md) are
  documented. Incremental UNION, hydration, and Rust traversal consumption,
  plus cursors inside application transactions, remain follow-up extensions.
- [x] **File opening and lock contention.** `OpenOptions` selects create-or-open,
  existing read/write, or read-only access and a connection-local busy timeout.
  Read-only handles reject mutations and migration; `Error::Busy` preserves
  SQLite contention details. Concurrent initialization/migration rechecks the
  schema after acquiring the writer. Twelve integration tests cover modes,
  invalid/foreign/future files, read-only hybrid queries, migrations, concurrent
  openers, visibility, competing writers, blocked reads/open/commit, rollback,
  and connection reuse. The dependency example uses these modes. The supported
  local-file rollback-journal contract and compatibility rules are documented.
- [x] **Process recovery and backup.** `backup_to` publishes a validated snapshot
  at a new path without overwrite; `restore_from` validates a current-version
  source and atomically replaces the destination graph and constraints. Nine
  integration tests cover identity, bags, memory/read-only files, replacement,
  visibility, invalid sources, lock failures, and cleanup. A subprocess test
  kills writers after dirty-page spill and after commit, then verifies journal
  recovery, exact rollback, retained commits, indexes/constraints, and reuse.
  The initial local-file recovery contract is documented. Power-loss/I/O fault
  injection, corruption repair, broader filesystems, and journal tuning remain
  separate work.
- [x] **A realistic application and workload.** The runnable
  `dependency_graph` example imports Cargo metadata, preserves identities and
  review annotations on repeat import, reconciles stale data, queries forward
  and reverse dependencies and shortest paths, updates/deletes, verifies
  resource-failure rollback, and reopens a file. Six tests cover the lifecycle.
  [Recorded release measurements](dependency-graph.md#measurements-and-next-work)
  cover 1,000 and 10,000 packages with three fresh-file trials each, query
  latency distributions, and child-process peak RSS. A scalar key projection
  removed an import scan bottleneck using the existing property index.
  Remaining node-upsert costs and shortest-path scaling deserve profiling
  before plan reuse or semantic-deduplication changes.

Gate: the example completes its lifecycle using the public API, documented
failure paths preserve graph state, persistent data survives reopening, and
the workload has reproducible performance and resource measurements.

## Near-term working order

Multi-query transactions, `WITH DISTINCT`, and entity collection/expansion
(`collect(n)`, `collect(DISTINCT n)`, and `UNWIND`) are complete, as is `UNWIND`
for path-function and legacy relationship lists. Aggregate expressions such
as `count(*) + 1` and application node uniqueness constraints are also complete.
Caller-controlled query resources and the dependency-graph application/workload
are complete for eager execution. Opening options and reader/writer contention
are complete for local files with rollback journaling, as are process-termination
recovery and backup/restore. The first incremental read slice is complete for
single SQLite fragments, with explicit buffering and cursor/snapshot rules.
Keep the example, cursor, and persistence tests as application regression coverage.
Next address broader entity-list operations (indexing, comprehensions, and nested
collections), or extend cursors through measured buffering bottlenecks.
Performance work should start with the measured node-upsert cost and
shortest-path scaling. Track completion in milestone 10 rather than a separate
TODO file.
Fix correctness regressions as they appear and keep reviewing TCK changes;
temporal support and broad differential testing remain follow-up conformance
work rather than prerequisites for this usability milestone.

## Milestone 9 implementation notes

Milestone 9 is in progress. Runtime slots are exhaustively checked,
representative generated SQL is prepared directly by SQLite,
relationship-property kind coverage spans fixed and hybrid paths, and the
finite predicate oracle exhausts the documented type/null contract. Unusual
identifier, property-key, recursive-value, depth-preflight, and transport
normalization invariants are now green as well. Consume the upstream parser fix
when released; it no longer blocks local hardening.

Public default state-budget exhaustion, exact query-wide serialized-payload
accounting, selective traversal seeding, deterministic retained-memory
accounting, and exact matching against a pre-bound relationship list are green,
the pinned openCypher TCK run is in place, and its seven remaining
`result-mismatch` rows need NaN support. The checked-in manifests currently
record 2,571 passing and 1,311 excluded scenarios, including 13
`expected-error-not-raised` cases. Temporal features dominate the unsupported
categories. Every unsupported expression kind and operator is now named in
its rejection, so the exclusions file exposes those compatibility gaps.
List quantifiers, map key access, list comprehensions, `size`,
computed list and map literals, indexing, slicing, `range`, string and list
concatenation with `+`, `CASE`, `rand`, and a scalar function library
(`coalesce`, `keys`, `properties`, conversions, string and math functions)
are in, and so are consecutive `CREATE` clauses after any number of `MATCH`
clauses, which unblocked setup for about a hundred scenarios. The
post-mutation result stage no longer runs in Rust: the executor writes the
materialized rows to a `_crabnet_rows` temporary table and the `RETURN` is
compiled as an ordinary read over it, which gave computed projections,
aggregates, and projection modifiers after a mutation in one step; computed
`CREATE`/`SET` values reuse the same read to evaluate one value per row. The
ranking now reads qualified function calls (all temporal), the temporal
functions themselves (which now also account for every remaining setup
failure), expected errors that are not raised (mostly wrong-kind arguments
that read as null where Cypher raises a type error, a deliberate choice that
a future strict-types pass could revisit in one place), and result
mismatches. `MERGE` is in. Writes in a clause pipeline followed: the
lowering now accepts any order of reading and updating clauses and splits
the statement into the input read, its mutations, and then segments, each a
run of reading clauses compiled as a read over the `_crabnet_rows` table
(which carries scalar columns as well as entities) followed by that run's
mutations; the executor re-materializes the rows between segments. That
closed the "writes in a read pipeline" bucket and the remaining `MERGE`
forms, and brought `WITH *`, `RETURN *`, chained comparisons
(`1 < n.num < 3`), and null-skipping updates after `OPTIONAL MATCH` along
with it, since the newly reachable scenarios needed them. Labels came next:
schema version 2 stores a node's labels as a JSON list and keeps a
trigger-maintained `node_labels (label, node_id)` table for indexed label
lookups (version 1 files migrate on open), which gave multiple labels, label
expressions in patterns and predicates, `SET`/`REMOVE` labels with summary
counters, and `XOR` in one slice. Pattern predicates followed: the pattern
is lowered by a scoped copy of the lowerer (so the anonymous entities never
join the row) and compiled as a correlated `EXISTS` subquery inside the
enclosing query, and `a = b` on entities became an element-id comparison at
lowering time, which together cleared the triadic-selection use cases.
`rand()` is
compiled as a one-row `json_each(json_array(random()))`
source joined after the row sources rather than as an expression, because
SQLite evaluates an uncorrelated scalar subquery once per statement; that
gives one draw per call site per row that the value and type SQL share.
One upstream parser gap is worth knowing: open-cypher 0.2.1 fails to parse a
second function-call projection item after a `CASE` earlier in the statement
(`RETURN CASE WHEN true THEN 1 END AS a, collect(1) AS c`), and the tests
here avoid that shape. The generated-SQL blowup that made
nested operators grow exponentially (a five-operator arithmetic expression
reached 3 MB) is fixed by computing non-trivial operands once in a
scalar-subquery scope and composing result types from operand types, so
nesting now grows at most quadratically; `sum`, `min`, and `max` still
repeat their argument once per aggregate call they issue, which is the
remaining constant-factor SQL-size cost. The working loop is
documented in [testing.md](testing.md).
