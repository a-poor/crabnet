# Historical testing notes

Retained from the testing guide during the 2026-10-13 cleanup. These notes
explain past discoveries and test evolution; feature availability and deviation
counts may be stale. Use [semantics](../semantics.md), [coverage](../coverage.md),
and the [test catalog](../test-catalog.md) for current behavior. In particular,
current writes remove top-level null properties, and graph-list operations have
expanded beyond the early milestone descriptions below.

Milestone 8 is complete for the supported read subset. Named paths, path
functions, relationship-list bindings, query-wide batch hydration,
deep/open-ended trail expansion, shortest-path BFS, and the explicit physical
operator boundary are green. `tests/physical_plan.rs` locks down pure SQL,
multiple runtime slots, shortest modes, branch-local shaping, unions, and one
root hydration operator. Planner/database unit tests additionally prove
whole-tree parameter preparation and one work budget across `UNION` branches;
the path and traversal integration suites remain the semantic oracle.

Application usability (milestone 10) is complete for the recorded
[Git Graph pilot](../git-graph-pilot.md). Production qualification (milestone 11)
is in progress: the [supported configuration and CI](../support.md) are defined,
with initial hosted validation complete and the enforcement, compatibility,
correctness, and failure-behavior gates still open;
its checklist and current working order live in [roadmap.md](../roadmap.md). Multi-query
transactions, `WITH DISTINCT`, entity collection/expansion, path-list `UNWIND`,
aggregate expressions, and application node uniqueness constraints are complete.
Resource controls, the initial incremental cursor API, and the persistent
application lifecycle are complete. Broader entity-list operations and cursor
operator extensions remain follow-up work. Milestone 9 continues as
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
`src/database/tests.rs`.
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
