# Crabnet roadmap

This is a living plan for an experimental embedded property-graph database. It
is intentionally a direction of travel rather than a compatibility promise.
Update it when implementation work teaches us something better.

The core engine and application foundations are implemented for a substantial,
documented subset. The next main milestone is **a validated downstream Rust
application pilot** (milestone 10), followed by **production qualification for
a defined supported configuration** (milestone 11). Close the current
implementation/validation cycle, then let the pilot's requirements drive
further engine work. Conformance and measured optimization support these gates;
expanding the query language is a separate backlog.

## Project posture

- Crabnet is used only in this repository today. We may freely reshape its
  modules, public API, logical plan, SQLite schema, and on-disk representation.
- Schema compatibility remains experimental. Version 5 preserves the upgrades
  from main's versions 1–4; migrations and index backfills run atomically.
  Unreleased branch-only formats may require recreating the database.
- A pilot pins its Crabnet version and records its API and storage assumptions.
  Changes affecting an adopted pilot need an explicit upgrade or rebuild path.
  Completing the pilot does not itself establish a stable public API or storage
  compatibility promise; milestone 11 defines those commitments.
- Use repository history to recover earlier code and the documented backup and
  recovery workflow for database files. Keep code clear enough to support the
  next experiment.
- Correct results or a structured plan-time rejection are both acceptable.
  Plausible but incorrect results are not.

## Where we are

The implemented baseline includes:

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
  maxima through 32 and Rust relationship-unique trail enumeration for larger
  or omitted maxima. Eligible cursors stream encoded singleton trails; eager
  execution and buffered cursor shapes materialize runtime JSON relations;
- mandatory read-only fixed, variable-length, and mixed patterns can bind public
  typed paths, including zero-hop values; SQLite carries ordered IDs into
  pending cells, which Rust shapes before query-wide hydration in eager reads
  or bounded row batches in eligible cursors, on a shared snapshot;
- fresh legacy relationship variables, including deep/open-ended forms, bind
  traversal-ordered typed relationship lists with zero-hop `[]` through the same
  identity shaping and hydration pipeline;
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
  report actual storage-row counters, and roll back atomically;
- `MERGE`, application-managed node uniqueness constraints, and transactions
  spanning multiple queries support import/update workflows with read-your-writes
  and rollback on query failure;
- prepared reads expose incremental or buffered execution before starting.
  Eligible single-trail cursors compose through `WITH` projections, filters,
  DISTINCT and pagination, chained `UNWIND`, final DISTINCT/UNION, and batched
  hydration. Constrained starts load neighbors on demand. The corresponding
  implementation entries below record each slice and its validation;
- query options bound traversal work and retained traversal state, SQL work,
  transfers, and results, with cooperative cancellation and deadlines. These
  counters do not constitute a whole-query memory cap;
- file opening modes, lock contention, atomic migrations, validated backup and
  restore, and process-termination recovery have local-file coverage; and
- structured errors distinguish planning, type/data, constraint, resource,
  cancellation, and contention failures.

The executor combines parameterized SQLite fragments with Rust traversal,
semantic row shaping, and hydration. Incremental reads retain their snapshot
until exhaustion, error, or drop; eager and ineligible cursor shapes buffer.
Global traversal sorting/aggregation, additional joins, multiple producers,
and shortest-path output still have buffering boundaries. SQLite can also
materialize internally. The supported concurrency contract is independent,
serially used connections on local files with DELETE rollback journaling and
one writer at a time. Long-lived cursors can block commits.

The dependency-graph example exercises a full persistent lifecycle and has
recorded measurements at 1,000 and 10,000 packages. It supplies regression and
benchmark evidence for a downstream pilot. API/schema stability, a qualified
platform matrix, and power-loss/disk-I/O failure behavior remain explicit work.
See [coverage.md](coverage.md), [semantics.md](semantics.md), and
[cursors.md](cursors.md) for the current supported surface and limits.

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

Logical and physical explanations already exist and provide a useful test and
diagnostic boundary. Their text format remains experimental; stabilize it only
as part of an explicit application or public compatibility requirement.

## Milestones

Each milestone should leave the supported subset documented and the full suite
green. The order is intentional, but discoveries may move individual features.
Completed milestones and checked implementation entries record the behavior at
the time each slice landed; later entries may extend their original boundaries.
Use the current-state summary and coverage/semantics documents for today's
contract. Unchecked pilot and qualification items below are the remaining gates.

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
  vendored and runs through `cargo test --test tck` and the normal all-targets
  test command against pinned `allowlist.txt` and
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
update it atomically, and reopen it with predictable behavior. The implemented
foundations below support that goal; the remaining gate is a downstream pilot
with recorded acceptance criteria. Keep the existing semantic tests and pinned
TCK outcomes green throughout this work.

#### Pilot completion checklist

- [ ] **Record a validated pilot baseline.** Finish the current implementation
  and validation cycle, including streaming WITH DISTINCT, and pin the revision
  with its required test/check results and relevant measurements.
- [ ] **Select and define one downstream application.** Name an actual Rust
  consumer of the library and record its required queries, data sizes and
  distributions, mutation mix, concurrency, and file setup. Set latency,
  peak-memory, and sustained-run duration or operation-count targets before
  the acceptance run. Reuse the dependency-graph example's fixtures and checks
  where useful.
- [ ] **Exercise the public application lifecycle.** Integrate import,
  parameterized queries, updates, transactions, constraints, resource limits,
  reopening, and backup/restore as required by the application. Record which
  reads buffer and how the application handles busy, cancelled, and failed
  operations. The repository example alone does not complete this pilot.
- [ ] **Run and record the sustained workload.** Verify expected graph/results
  throughout repeated import/update/read cycles; exercise cancellation, failed
  writes, process interruption, reopening, and restoration of a real backup.
  Record tail latency, peak RSS, and contention under the declared workload.
  Turn every correctness or operational failure into a reproducible regression.
- [ ] **Review the pilot report.** Keep a versioned report in `docs/`, linked
  here, with the application/version, commands, workload, acceptance results,
  and remaining restrictions. Resolve defects in the required contract and
  demonstrate the declared targets before closing this milestone.

#### Implemented foundations

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
- [x] **Read cursors inside application transactions.** `Transaction::prepare_read*`
  reuses `PreparedRead`/`ReadCursor` and both execution modes on the parent
  transaction, including earlier writes. Exhaustion and early drop keep the
  parent active. Errors in preparation, cursor creation, or iteration abort all
  earlier writes immediately and make both parent and prepared read unusable.
  Each cursor gets fresh counters without a new snapshot. Fourteen integration
  tests cover value/operator parity, hydration, limits, cleanup, provisional
  values, rollback of graph/index/constraint changes, retained write locks on
  success and immediate release on failure, and panic scopes. Two compile-fail
  doctests enforce the transaction borrow against writes and commit.
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
  proper rejection of ambiguous compound grouping expressions.
  Entity-list indexing and slicing are complete for collected nodes and
  relationships, path-function lists, and legacy relationship-list bindings.
  Indexed entities retain graph behavior through property/function access,
  comparisons, aggregation, and WITH rebinding; slices preserve ordering,
  duplicates, graph types, and nulls across aliases, UNWIND, distinctness,
  grouping, unions, and write boundaries. Tests cover all path transports,
  extreme bounds, parameters, errors/rollback, and eager/cursor parity. One
  additional TCK scenario passes for `last()` over legacy relationship lists.
  Entity-list comprehensions are complete for the same inputs. They preserve
  entity types or project scalar values, with scoped property/function access,
  comparisons, and fixed-length pattern predicates. Tests cover nullable
  members, order/duplicates, aliases, nested scalar projections, grouping,
  distinctness/unions, mutation refresh, deleted snapshots, rollback, and
  eager/cursor parity including partial errors. Three additional TCK scenarios
  pass. Entity-list quantifiers (`all`, `any`, `none`, and `single`) are also
  complete for these inputs, with typed member access, scoped patterns,
  null/empty truth tables, duplicate-sensitive `single`, nested bindings,
  parameters, aggregate composition, stored results, mutation refresh/deletion
  snapshots, rollback, and eager/cursor parity. Eight additional TCK scenarios
  pass. Nested graph collections are complete: lists/maps can mix entities,
  paths, ordinary values, and other collections, with typed selection,
  comprehensions/quantifiers, UNWIND, count/collect, recursive identity,
  grouping/distinctness/unions, and mutation refresh/deletion snapshots.
  Eleven integration tests cover these contracts, forged-record isolation,
  depth limits, errors/rollback, and cursor parity. Six additional TCK scenarios
  pass. Graph-list concatenation with `+` is also complete: entity, path, and
  mixed lists can be joined, and individual values appended or prepended,
  with typed selection/rebinding, order, duplicates, nulls, aggregation,
  recursive identity, and mutation refresh/deletion snapshots preserved.
  Ten integration tests cover these contracts across every traversal
  transport, including parameters, writes/rollback, cursor parity/partial
  errors, scalar addition parity, depth limits, and bounded SQL growth.
  One additional TCK scenario passes. Expression targets for `DELETE` and
  `DETACH DELETE` are complete: indexes, nested/dynamic selections, and mixed
  node/relationship values feed the existing deduplicated deletion lifecycle.
  Eleven integration tests cover nulls, parameter preflight, target evaluation
  before deletion, refreshed snapshots, counters, forged-record isolation,
  errors and rollback, application transactions, and result budgets. Six
  additional TCK scenarios pass. Graph-aware `head`, `last`, `tail`, and
  `reverse` are complete, including exact tuple positions after transformation.
  Ten integration tests cover entity, path, and mixed/nested lists across all
  transports, nulls/empty lists, order/duplicates, identity and rebinding,
  parameters, scalar reversal parity, aggregation, mutation snapshots,
  write bags/rollback, cursor errors, depth limits, and bounded SQL growth.
  Graph-aware `coalesce` and both forms of `CASE` are complete. Twelve
  integration tests cover typed selections and rebinding, identity-aware
  selectors, all path transports, nested/mixed results, lazy branches and
  conditions, random value/type consistency, nulls, parameters, aggregation,
  mutations and rollback, cursor parity/errors, depth limits, and bounded
  SQL growth. One additional TCK scenario passes. Direct path and path-list
  aggregation/grouping are complete: count/collect and DISTINCT variants,
  typed grouped values, ordered identity and direction, duplicate/null/empty
  semantics, collection composition, and all traversal transports. Eleven
  integration tests cover these contracts along with aliases/modifiers,
  later matches, unions, parameters, cursor errors, resource/depth guards,
  bounded SQL growth, and retained write restrictions. Seven additional TCK
  scenarios pass; one exclusion now reaches the whole-path DELETE restriction.
  Direct path and graph-list predicates are complete: equality/inequality,
  membership, and null tests share typed identity across deferred and materialized
  values. Eleven integration tests cover all traversal transports, aliases,
  chained comparisons, ordered identity/direction/parallel edges, nulls/zero hops,
  clause composition, parameters, cursor parity/errors, predicate writes and
  rollback, resource limits, SQL growth, and retained restrictions. One additional
  TCK scenario passes; another exclusion now reaches the optional-path restriction.
  Expression targets for property `SET` and `REMOVE` are complete. Indexed,
  nested/dynamic, function, and conditional selections update nodes and
  relationships directly. Twelve integration tests cover per-item evaluation,
  duplicate/null behavior, counters and refreshed copies, CREATE/MERGE and
  deletion composition, parameter isolation/preflight, invalid targets/values,
  uniqueness/property indexes, transaction rollback, and result budgets.
  Two additional TCK scenarios pass. Direct `size` on path-function and legacy
  relationship lists is complete, including aliases. Eight integration tests
  cover all traversal transports, zero hops, repeated positions and segment
  junctions, nulls, transformed lists, clause/aggregate composition, parameters,
  scope/write boundaries and rollback, eager/cursor parity, and avoiding entity
  records and hydration when compact path identities can supply the count.
  Other general scalar functions and ordering
  on graph values, and expression targets for whole-map/label mutations remain
  follow-ups.
- [x] **Application uniqueness constraints.** `Database` and `Transaction`
  expose create/list/drop methods for a node label/property pair such as
  `:Person.email`. Installation rejects conflicting existing data atomically;
  missing/null values are exempt, and other values use recursive typed
  `DISTINCT` equivalence. Trigger-maintained keys enforce immediate checks
  across `CREATE`, `MERGE`, property and label changes, and deletion. Twelve
  integration tests cover typed precision, bags, independent/overlapping keys,
  hostile names, rollback/abort, and persistence/connection visibility.
  Schema version 5 upgrades versions 1–4 atomically, with migration and
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
  documented. Incremental `UNION ALL` is also complete when every branch is
  an eligible SQLite fragment. Eleven additional tests cover branch-specific
  types/parameters, duplicates/empty branches, SQL modifiers, fallback boundaries,
  early exit, partial errors, cumulative budgets, cancellation/deadlines, shared
  snapshots, cleanup/reuse, and read-only/busy behavior. The scan example and
  Criterion benchmarks cover eager, full-cursor, and first-row union consumption;
  release measurements record latency and memory. Semantic DISTINCT, distinct
  UNION, and pagination now stream over eligible fragments too. Twelve additional
  integration tests cover typed first representatives, exact numeric and graph
  identity, branch scopes/pages, long unions, input versus output accounting,
  early completion and errors, resource controls, snapshots, cleanup, and reuse.
  Two unit tests verify hash-bucket filtering against the existing equality and
  force collisions. The distinct example measures duplicate-heavy and mostly-unique
  inputs: retained state grows with unique rows, while early exit avoids draining
  the suffix. SQLite-produced path and graph-list hydration now streams after
  pending-row shaping: one surviving row initially, then up to 64 per batch.
  Caches deduplicate IDs within each batch and reuse direct entities; repeated
  reads in later batches count again. Eleven integration tests cover typed
  bags, compact/materialized equality, batch boundaries and budgets, read-ahead
  errors, control limits, snapshot cleanup/reuse, and read-only/busy behavior.
  The hydration example and Criterion cases measure eager/full/early consumption.
  Transaction-scoped cursors reuse this pipeline with parent-owned snapshots
  and rollback on errors. The first Rust traversal extension is described below.
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
  Node-upsert maintenance and shortest-path input loading are addressed below.
- [x] **Computed property-index keys.** Small deterministic expressions such
  as `row.id`, nested/dynamic indexing, arithmetic, selected scalar functions,
  and CASE can now drive node-property probes in MERGE matchers.
  Computed probe values and types are materialized inside the candidate-ID
  relation so nested scalar subqueries do not run for every index entry.
  The exact typed comparison and remaining pattern constraints stay in place;
  volatile, candidate-dependent, iterating, and large operands keep their
  existing plans. Read filters retain their original plans to preserve errors when
  no candidate nodes exist. Two planner and six additional integration tests
  cover eligibility, typed bags, nulls and guarded errors, work-budget evidence,
  mutation re-matching, whole
  patterns, and rollback. The dependency-graph node import uses direct map keys;
  Criterion compares direct and projected keys for first and repeat imports.
  [Release measurements](dependency-graph.md#computed-merge-keys-2026-10-08-follow-up)
  show 3.4× faster initial and 9.3× faster repeat direct-key imports at 1,000
  packages; the record also compares the alias workaround and 10,000-package
  imports, including trial variability.
- [x] **Constrained shortest-path input loading.** Same-clause start domains
  use indexed neighbor queries and cache only expanded nodes' filtered lists.
  Outgoing, incoming, and undirected searches preserve tie order, parallel
  edges, same-endpoint cycles, and hop bounds. Cache memory remains charged
  across seeds while frontier/visited memory is released per search. Transfer
  and SQLite work budgets and cancellation remain query-wide. Small-graph
  differential/oracle tests and public resource regressions cover the source
  change. The scaling example and Criterion group hold a 64-node reachable
  component fixed while adding disconnected eligible edges. Unconstrained
  starts still load eagerly; the runtime output relation remains buffered.
- [x] **Bulk node-upsert maintenance.** Profiling identified property/uniqueness
  trigger writes and repeated statement preparation as substantial import
  costs. Property updates retain unchanged index entries, uniqueness writes
  are skipped when canonical keys and labels are unchanged, and node/edge
  inserts and property reads/writes reuse prepared statements. Sequential
  mutation semantics, bags, immediate constraints, summaries, and rollback
  remain unchanged. Schema version 5 atomically replaces the maintenance
  triggers on writable opens from versions 1–4. Focused audit/migration/type
  tests and the application lifecycle cover the change; a 1,000-node Criterion
  case measures repeated single-property updates. See the
  [profile and release comparison](dependency-graph.md#bulk-node-upserts-2026-10-08-follow-up).
- [x] **Incremental Rust trail cursors — initial slice.** A single deep/open-ended
  relationship between endpoint nodes streams through filters and projections,
  DISTINCT, final pagination, UNION/UNION ALL, and batched path/list hydration.
  The resumable enumerator preserves relationship-unique trails and duplicate
  paths, with graph/frontier memory charged while paused and released on drop.
  Each trail uses a cached consuming SQL projection; eager SQL remains intact.
  Global traversal sorts/aggregates, extra joins/stages, multiple producers,
  and shortest paths retain explicit buffering. Twelve integration tests and a
  traversal oracle cover parity, early exit, resource controls, snapshots,
  partial errors, and transaction rollback. The example and Criterion cases
  compare eager, full-cursor, and first-row consumption; see the
  [measurement guide](cursors.md#rust-traversal-measurements).
- [x] **Constrained trail cursor neighbor loading.** Same-clause start labels
  and inline properties enable direction-aware indexed neighbor probes. The
  active producer caches expanded neighborhoods, preserving trail order,
  parallel edges, cycles, zero hops, typed filters, and resource controls.
  Disconnected eligible edges and unread neighborhoods no longer need loading.
  Endpoint domains remain eager; unconstrained starts and buffered/eager trail
  execution keep the whole-edge source. Budget regressions, a 64-graph order
  oracle, cache accounting/cleanup, and index checks cover the boundary. The
  parallel-edge example and Criterion fixtures now vary disconnected graph
  size; see [scaling measurements](cursors.md#constrained-cursor-neighbor-measurements).
- [x] **Plain WITH stages in traversal cursors.** A single trail-producing
  MATCH can stream through chains of WITH aliases, computed projections, and
  filters. Typed paths/entities/lists, volatile aliases, final DISTINCT/UNION/
  pagination, hydration, and constrained neighbor probes compose through these
  stages. WITH DISTINCT, aggregation, ordering, intermediate pagination, and
  additional MATCH/OPTIONAL MATCH stages retain buffering; the following slice
  adds UNWIND. Ten integration tests cover bags/scopes, early exit, error
  evaluation, resource controls, snapshots and transaction rollback. The shared example and Criterion group
  measure the same traversal through two WITH stages; see the
  [comparison](cursors.md#traversal-with-measurements).
- [x] **One UNWIND in Rust traversal cursors.** A single trail producer streams
  through one list expansion and plain WITH stages on either side. The cursor
  freezes one input row/list per trail, then projects members individually;
  typed values, duplicates, nulls, volatile scope, final DISTINCT/UNION/pages,
  partial errors, hydration, budgets, snapshots, and rollback are covered by
  ten integration tests. The following slice adds multiple UNWINDs.
  List construction still materializes one complete value, charged with its
  input row to transfer limits. Node-list and configurable scalar-fanout
  examples/benchmarks measure full and partial consumption; see the
  [comparison](cursors.md#traversal-unwind-measurements).
- [x] **Chained UNWIND in Rust traversal cursors.** Dependent expansions and plain
  WITH stages compose after one trail producer. Iterative resumption retains
  each active stage's frozen scope and list position; empty/filtered inner inputs
  return to the parent, and exhausted stages release their carried values.
  Eleven tests cover four levels, types/bags, scope and volatile evaluation,
  partial errors, early limits, final shaping, budgets, hydration/rollback, and
  snapshot cleanup. Node-derived and scalar-range fanouts have example/benchmark
  coverage; see the [comparison](cursors.md#chained-unwind-measurements).
- [x] **Intermediate pagination in Rust traversal cursors.** WITH SKIP/LIMIT
  composes before, between, and after dependent expansions. Per-stage counters
  span incoming rows; skipped projections are guarded, admitted bindings are
  frozen, and downstream expansions finish before a page stops its ancestors.
  Eleven tests cover a sequence oracle, typed bags, filter/error/volatile scope,
  zero-page preflight, transfer budgets, hydration rollback, and snapshot cleanup.
  The shared example and Criterion groups cover caps before/between expansions
  and full consumption through a pass-through page; see the
  [comparison](cursors.md#intermediate-pagination-measurements).
- [x] **Intermediate DISTINCT in Rust traversal cursors.** WITH DISTINCT
  compares projected typed keys across all incoming rows, then applies its
  SKIP/LIMIT and resumes downstream expansions. Independent stage seen sets
  preserve identity and numeric/recursive equivalence; projected aliases and
  WHERE scope match eager lowering. Twelve tests cover bags, shared path-key
  storage, empty scopes, volatile/error behavior, budgets, hydration rollback,
  preflight, snapshots, and cleanup. Duplicate-heavy, unique-key, and early-page
  fixtures measure retained state and consumption; see the
  [comparison](cursors.md#intermediate-distinct-measurements).

Gate: the named downstream pilot meets its recorded correctness, recovery,
resource, and performance criteria through the public API, with reproducible
evidence and no unresolved defects in its required contract. Additional Cypher
features are required only when the pilot depends on them. The experimental
designation remains until the release commitments below are qualified.

### 11. Production qualification — planned

Qualify a release for an explicit query subset and operating configuration.
The pilot supplies application evidence; this milestone establishes repeatable
release checks and compatibility commitments. Basic automation can begin while
the pilot runs.

- [ ] **Define the supported configuration.** Record supported operating systems,
  Rust toolchains, SQLite build/configuration, local-file assumptions, and
  concurrency limits. Start with the tested DELETE-journal contract. WAL,
  network filesystems, and other configurations require their own evidence if
  added to the support matrix.
- [ ] **Set API and storage compatibility policy.** Define versioning,
  deprecations, the supported schema-upgrade range, and recovery or rebuild
  procedures when an upgrade cannot proceed. Test release-to-release upgrades
  and backup/restore using retained fixtures from supported versions. Document
  downgrade limitations and the application's rollback procedure.
- [ ] **Verify automated release checks.** No checked-in CI configuration was
  present in the 2026-10-09 roadmap audit; existing references to CI are not
  evidence of an automated platform matrix. Verify any external pipeline or
  add repository-owned configuration. Run the required tests, pinned TCK,
  doctests, formatting, and Clippy on the declared platforms; give expensive
  workloads an explicit scheduled or release gate. Preserve commands/results.
- [ ] **Qualify failure behavior and durability claims.** Add reproducible
  disk-full and I/O fault tests for writes, commit, migration, backup, and
  restore. Exercise corruption detection and failed-open/restore behavior.
  Use a documented fault model to test power-loss/torn-write recovery before
  claiming it; current process-termination tests cover a narrower failure model.
  Keep unqualified storage configurations outside the supported contract.
- [ ] **Close correctness gaps in the declared surface.** Resolve known
  wrong-answer cases, including NaN where supported, and document or reject
  unsupported inputs. Review error-semantics deviations explicitly. Add
  deterministic differential tests against a pinned reference for supported
  queries, preserving bags and minimizing failures into local regressions.
  Broad temporal or procedure compatibility is not a prerequisite unless it
  belongs to the chosen surface.
- [ ] **Publish a reproducible qualification record.** Repeat sustained workload,
  contention, resource-limit, upgrade, and restore checks for the release
  candidate. Review package metadata/licensing and dependencies, record known
  limitations and measured regressions, and provide release/upgrade notes.
  Link the evidence before making a stable or production-support claim.

Gate: a named release meets all declared support and compatibility commitments,
with repeatable automated checks, failure/recovery evidence, and a reviewed
qualification report. Qualification applies to that recorded subset and
configuration; unsupported features remain explicit.

## Near-term working order

1. Close the current intermediate WITH DISTINCT implementation/validation
   cycle and record the pilot baseline. Preserve eager semantics and explicit
   retained-state/resource boundaries.
2. Select and validate the milestone 10 downstream pilot. Prioritize defects,
   unsupported required queries, and measured performance problems it exposes.
   Keep the example, cursor, persistence, and pinned TCK tests as regressions.
3. Qualify the supported release under milestone 11. Establish automation early;
   make compatibility and durability claims only when their gates are met.

Correctness regressions remain immediate work throughout. Track pilot and
qualification completion in their checklists instead of adding every new engine
feature to milestone 10.

### Feature follow-ups

These extend capability and are not automatic pilot or release blockers. Promote
an item into a gate when a concrete application requirement or measurement
justifies it:

- Broader traversal cursor shapes: sorting/aggregation, additional joins,
  multiple producers, shortest-path output, and unconstrained graph-input
  loading. Retain explicit buffering where streaming is not implemented.
- Further graph-value scalar functions and ordering, and expression targets
  for whole-map and label mutations.
- Temporal types/functions, procedures, and broader Cypher compatibility.
  Continue reviewing pinned TCK outcomes as each supported slice changes.
- Composite/relationship constraints, Cypher schema DDL, and additional storage
  configurations where an application needs them.
- Further performance work supported by a new profile, such as imports that
  change constrained keys or reachable-region shortest search. Plan caching
  remains unproven by the current workload.

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
record 2,607 passing and 1,275 excluded scenarios, including 13
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
