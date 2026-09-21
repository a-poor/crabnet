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
- Implement `MERGE` last as transactional whole-pattern match/create behavior,
  not as a misleading single-row SQLite upsert.

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
  the parser; `open-cypher 0.2.0-alpha.2` still reproduces them, so they await
  a later upstream release.
- The selective-seeding slice is green. Start-endpoint label and
  inline-property constraints compile to a bound node-domain query that seeds
  the hybrid producer, an unconstrained positive-hop start seeds only nodes
  that begin an eligible edge, and end-endpoint constraints filter emitted rows
  before serialization. Both domains are supersets of the SQLite join, so the
  physical-plan tests, generated-SQL corpus, and traversal suites lock in
  unchanged results with fewer explored states and bytes.
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
  directions. Roughly a quarter of the expanded scenarios pass; every other
  scenario carries a generated reason, dominated by unsupported functions,
  temporal types, expression forms, and multi-label setup clauses. The first
  run surfaced and fixed exponential SQL growth in boolean connective chains.
- Add deterministic differential tests against a pinned reference engine;
  minimize every mismatch into a permanent local regression.
- Optimize only after benchmarks identify a real bottleneck.

Gate: the allowlist is entirely green, exclusions are explicit, randomized
failures are reproducible, and optimizations preserve plan/result contracts.

## Near-term working order

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
the pinned openCypher TCK run is in place, and its first two wrong-answer rows
(float transport precision) are fixed. The working order from here is
to burn down `tests/tck/exclusions.txt` by category, starting with the
`result-mismatch` and `side-effects-mismatch` rows because they are wrong
answers rather than rejections, then the largest `unsupported` buckets, before
adding deterministic differential cases. Every unsupported expression kind and
operator is now named in its rejection, so the exclusions file ranks the gaps
directly. List quantifiers, map key access, list comprehensions, `size`,
computed list and map literals, indexing, slicing, `range`, string and list
concatenation with `+`, `CASE`, `rand`, and a scalar function library
(`coalesce`, `keys`, `properties`, conversions, string and math functions)
are in; the ranking now reads qualified function calls (all temporal), the
`CREATE` clause sequences that gate setup for other features, expected errors
that are not raised (mostly wrong-kind arguments that read as null where
Cypher raises a type error, a deliberate choice that a future strict-types
pass could revisit in one place), the temporal functions, the stored-value
expressions that gate setup for the `WITH ... ORDER BY` scenarios, and
`MERGE`. `rand()` is compiled as a one-row `json_each(json_array(random()))`
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
