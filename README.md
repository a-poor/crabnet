# crabnet

Crabnet is an experiment in lowering a small, useful subset of openCypher into
a logical row plan backed by SQLite. Read-only `MATCH`/`WITH` pipelines compile
to parameterized SQL and CTEs; core `CREATE`, matched property, and matched
deletion mutations run inside one SQLite transaction. It is intentionally not
a production database or a stable library API yet.

```rust
use crabnet::{CypherValue, Database, Parameters};
use serde_json::json;

fn main() -> crabnet::Result<()> {
    let mut graph = Database::in_memory()?;
    let mut parameters = Parameters::new();
    parameters.insert("name".into(), json!("Ada"));

    graph.execute_with(
        "CREATE (:Person {name: $name})",
        &parameters,
    )?;

    let result = graph.execute("MATCH (person:Person) RETURN person")?;
    let CypherValue::Node(person) = &result.rows[0][0] else {
        panic!("expected a node")
    };
    assert_eq!(person.labels, ["Person"]);
    assert_eq!(
        person.properties.get("name"),
        Some(&CypherValue::String("Ada".into())),
    );
    Ok(())
}
```

Named parameters and stored properties use JSON-compatible values. Returned
cells use `CypherValue`, which keeps scalar, list, and map kinds distinct and
represents whole nodes, relationships, and paths without disguising them as
maps.

## Current query slice

The accepted statement shapes are:

- `RETURN ...`
- `MATCH ... RETURN ...`
- consecutive read-only `MATCH ... MATCH ... RETURN ...` stages
- standalone or chained `WITH ... RETURN ...`, optionally interleaved with
  `MATCH`, `OPTIONAL MATCH`, and `UNWIND` clauses
- read-only branches joined by `UNION`, `UNION DISTINCT`, or `UNION ALL`
- `CREATE ... [RETURN ...]`
- `MATCH ... CREATE ... [RETURN ...]`
- `MATCH ... SET/REMOVE ... [RETURN ...]`
- `MATCH ... DELETE/DETACH DELETE ... [RETURN ...]`

Within those shapes, the first iteration supports:

- `CREATE` for nodes and fixed, directed relationship chains, including
  relationships between matched and fresh nodes
- matched `SET entity.key = value` and `REMOVE entity.key` for node and
  relationship properties
- matched whole-property-map replacement (`SET entity = map`) and shallow
  merge (`SET entity += map`) when the map is a literal or named parameter
- matched node and relationship `DELETE`, plus `DETACH DELETE` for nodes with
  incident relationships
- `MATCH` for nodes and fixed-length outgoing, incoming, or undirected patterns
- anonymous legacy variable-length traversal in mandatory `MATCH`: `*`, `*n`,
  `*min..max`, `*min..`, `*..max`, and `*..`, with outgoing, incoming, and
  undirected direction
- legacy `shortestPath` and `allShortestPaths` factors in mandatory read-only
  `MATCH`, over exactly one variable-length relationship whose lower bound is
  zero or one, with unweighted minimum-trail selection for each matched
  endpoint pair
- fresh legacy relationship-list bindings in mandatory read-only `MATCH`, such
  as `[rels:TYPE*min..]`, returned as traversal-ordered typed relationships
- named paths in mandatory read-only `MATCH`, across fixed, bounded,
  open-ended, and mixed patterns, returned as `CypherValue::Path(PathValue)`
- `length(path)` as an integer scalar in filters, comparisons, arithmetic,
  ordering, grouping, aggregate arguments, and direct or aliased projections
- `nodes(path)` and `relationships(path)` as traversal-ordered typed entity
  lists in direct projections and computed `WITH` aliases, with semantic
  `DISTINCT`/`UNION` equality before hydration
- fixed-length `OPTIONAL MATCH`, including whole-pattern null extension and
  optional-local `WHERE` predicates
- at most one node label; zero or one relationship type in `MATCH`, and exactly
  one type for each created relationship
- literal maps and named `$parameters` for properties
- inline property matching and `WHERE` typed comparisons, boolean expressions,
  `IN`, `STARTS WITH`, `CONTAINS`, and `ENDS WITH`
- scalar, computed scalar, whole-node, and whole-relationship read projections,
  including numeric `+`, `-`, `*`, `/`, `%`, unary `+`/`-`, aliases, and
  `elementId`, `labels`, and `type`
- read-only `WITH` stages carrying aliased entities, paths, and scalars, with
  scope loss, `WHERE`, `ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` before later
  clauses
- implicit grouping with top-level `count`, `sum`, `avg`, `min`, `max`, and
  `collect` projections in `RETURN` or `WITH`
- list expansion with `UNWIND`, plus typed semantic `UNION` deduplication and
  bag-preserving `UNION ALL`
- `RETURN DISTINCT`, `ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` on read-only
  returns
- typed `CypherValue` result cells for null, booleans, integers, floats,
  strings, lists, maps, nodes, relationships, and paths

For example, a matched write can connect two existing nodes:

```cypher
MATCH (alice:Person {name: $from}), (bob:Person {name: $to})
CREATE (alice)-[knows:KNOWS {since: $since}]->(bob)
RETURN alice.name AS from, type(knows) AS kind, bob.name AS to
```

A read pipeline can carry a parameter and an entity across clause boundaries:

```cypher
WITH $minimum AS minimum
MATCH (person:Person) WHERE person.score >= minimum
WITH person AS candidate ORDER BY candidate.score DESC LIMIT 5
RETURN candidate.name AS name ORDER BY name
```

Each `WITH` keeps only the names it projects. It preserves bag multiplicity,
and a later `MATCH` expands each carried row independently. `WITH ALL` is
accepted as the default behavior. A `WHERE` attached to `WITH` may inspect both
projected aliases and incoming variables, but an incoming variable that was
not projected is unavailable after that filter and clause boundary.

`MATCH ... CREATE` follows row semantics. Crabnet materializes the complete
match result before writing, then applies the `CREATE` template once per input
row. A zero-row match is a no-op; duplicate and Cartesian-product rows each
produce their own mutations. Fresh entities are fresh per row, and a matched
node may be reused in `CREATE` only as a bare `(variable)` reference. When a
`RETURN` is present, it emits one post-mutation row per input row and may refer
to both matched and created entities.

Matched property updates operate on the same materialized input bag. Direct
assignment to `null` removes a property, as does a `null` entry in a `+=` map;
whole-map `=` replacement retains explicit JSON-null entries. The latter is a
documented Crabnet extension. `properties_set` counts top-level keys whose
stored value actually changed or was removed, so assigning the same value or
removing an absent key contributes zero.

Deletion targets are deduplicated across the complete matched bag. Explicitly
targeted relationships are removed before nodes, independent of their spelling
order in `DELETE`. Plain deletion of a connected node fails SQLite's foreign-key
check and rolls the statement back; `DETACH DELETE` first removes every incident
relationship. The deletion counters report actual stored rows removed. A
post-delete `RETURN` reads the materialized pre-delete entity snapshots and
retains the input bag's row multiplicity. In the current bounded grammar,
`DELETE` must be the final mutation clause before an optional `RETURN`.

Legacy variable-length traversal returns endpoint rows and can contribute to a
named path. A bare `*` or `*..` has minimum depth one, while `*0..` explicitly
includes the zero-hop identity and still applies the endpoint node constraints.
A reversed bounded range is valid but produces no rows. An optional
relationship type and inline property map filter every traversed edge. Explicit
upper bounds through 32 use a recursive SQLite CTE. A bound above 32 or an
omitted upper bound instead uses Rust to enumerate relationship-unique trails,
materializes their ordered identities as JSON, and feeds that relation back to
SQLite for the surrounding joins and row pipeline. In either route, nodes may
be revisited, but a stored edge may appear only once across all constituent
patterns in one `MATCH` result; a later `MATCH` starts a new uniqueness scope.
Distinct edge sequences retain their normal bag multiplicity even when they
reach the same endpoint.

Legacy shortest-path factors use Rust breadth-first search over one
variable-length relationship. Direction, the optional relationship type, its
inline property map, parameters, and the requested bounds restrict candidates
before selection. For each matched start/end pair, `shortestPath` returns one
minimum-hop relationship-unique trail, while `allShortestPaths` returns every
tied minimum trail, including distinct parallel edges. The one-trail tie choice
is deterministic for a fixed stored graph but deliberately unspecified.
`*0..` permits the zero-hop identity; the only supported lower bounds are zero
and one.
For the same endpoint, lower bound one instead selects the shortest positive
relationship-unique cycle (one deterministic tie or all ties).

A shortest result may bind a `PathValue` and a fresh relationship list, feed
`length`, `nodes`, or `relationships`, cross a `WITH` boundary, preserve prior
bag rows, and compose through `UNION`. The legacy calls are supported only as a
single path factor in mandatory read-only `MATCH`. Modern shortest selectors,
expression-position calls, `OPTIONAL MATCH`, a `WHERE` on that same `MATCH`,
additional comma-separated parts, lower bounds above one, and writes are
planning-time rejections.

Deep/open-ended enumeration and shortest-path BFS share a 100,000-trail-state
budget across the top-level query, including `UNION` branches and multiple
hybrid expansions. If the query exhausts that budget, execution fails with
`Error::ResourceLimit`; Crabnet never silently truncates the result. This is a
fixed explored-work guard, not a memory bound: the current naive Rust stages
fully materialize eligible graph inputs and their JSON result relations before
SQLite applies selective endpoint constraints.

`PathValue` exposes nodes and relationships in traversal order, always with one
more node than relationship. A zero-hop path therefore has one node and no
relationships. Relationship values keep their stored start/end direction even
when the path traversed them backwards. SQLite returns compact ordered
identities. `length(path)` is the relationship count and remains an ordinary
SQL-backed integer across filters, arithmetic, ordering, implicit grouping,
and aggregate arguments. `nodes(path)` and `relationships(path)` return
`CypherValue::List` values whose members are typed `NodeValue` and
`RelationshipValue` cells in traversal order; zero-hop relationships are an
empty list. These graph lists may be returned directly or carried through a
computed `WITH` alias, but they are not yet accepted as predicates, arithmetic
operands, ordering/grouping keys, aggregate inputs, or `UNWIND` inputs.

A fresh variable on a legacy relationship pattern, for example
`[rels:NEXT*0..]`, binds the same public relationship-list value shape. Its
members follow traversal order while each relationship keeps its stored
start/end direction, and zero hops bind `[]`. The list may be returned directly,
carried through a `WITH` alias, shaped by `RETURN DISTINCT` and pagination, and
combined through `UNION` or `UNION ALL`; it can also coexist with a named path
for the same pattern. Exact matching against a pre-bound relationship sequence
and the other graph-list expression contexts above remain unsupported.

Rust keeps path and graph-list cells pending while branch-local
`RETURN DISTINCT` and deferred pagination, followed by left-to-right
`UNION`/`UNION ALL`, shape complete typed rows. It then collects only
surviving IDs query-wide, seeds hydration from directly projected node and
relationship values, and loads the remainder in batches—at most one node query
and one edge query. Every branch and hydration read shares one deferred SQLite
snapshot. Concurrent external writers are not a current goal.

Unsupported syntax is parsed first and then reported as `Error::Unsupported`.
Malformed Cypher, semantic errors such as unknown variables, and missing
parameters have separate error variants. All parameters and stored property
values are resolved and validated before a write transaction begins. Match
materialization, every mutation, and post-write result evaluation then share
one immediate transaction; any runtime failure rolls the whole statement back.

Notable omissions include multiple or boolean labels/types, label mutations,
postfix graph quantifiers, exact matching against a pre-bound relationship
sequence, modern path selectors, expression-position shortest-path calls, and
path property access. Legacy shortest-path factors are also rejected in
`OPTIONAL MATCH`, with a same-clause `WHERE` or another comma-separated pattern
part, with a lower bound above one, or in a write query. Named paths or
relationship-list bindings in `OPTIONAL MATCH`, quantified `OPTIONAL MATCH`,
traversal-fed writes (including writes fed by a relationship list), writes fed
by any named path, match modes and hints, parameter-map matching,
`RETURN *`, `MERGE`, exponentiation (`^`), concatenation (`||`), computed
right-hand sides for property updates, and computed projections or
`DISTINCT`/`ORDER BY`/`SKIP`/`OFFSET`/`LIMIT` after mutations. Paths also cannot
themselves be scalar operands, aggregate inputs/grouping keys, or ordering
keys; use `length(path)` where an integer scalar is required. Typed graph lists
from `nodes` and `relationships` retain the context restrictions above.
`WITH DISTINCT`, `WITH *`, unaliased computed `WITH` expressions,
`collect(DISTINCT ...)`, nested/embedded aggregate projections, and writes in a
read-composition pipeline are explicitly rejected. For now, `DISTINCT` is
supported only by `count`; the other aggregate forms fail during planning.

## Planning and execution

The planner is deliberately small and layered:

- `planner/lower.rs` converts the openCypher AST into the slot-based logical
  row plan in `planner/logical.rs`.
- `planner/sqlite.rs` generates parameterized `SqlFragment` values. Each
  read-only `WITH` boundary becomes a CTE carrying projected entity, path,
  and scalar slots. Explicit traversal bounds through 32 become recursive CTEs;
  deep, open-ended, and shortest-path traversals instead declare typed runtime
  inputs consumed through branch-local runtime slots.
- `planner/physical.rs` connects SQLite fragments to explicit
  `trail-expand`, `shortest-expand`, semantic-distinct, pagination, union,
  and batch-hydration operators. Static parameters and runtime-slot arguments
  retain their exact SQLite placeholder order.
  Each supported read branch currently has one consuming SQL fragment; a
  future SQL-to-Rust-row-to-SQL construct will require a multi-fragment
  extension.
- `database.rs` prepares the complete physical tree before executing it, then
  evaluates its SQL and Rust operators into pending typed cells under one
  deferred snapshot and one query-wide traversal budget. Mutations continue to
  materialize their input rows before the first change and apply ordered
  mutation operators to that stable row bag.

Trail and shortest-path expansion, pending-row shaping, and path hydration are
deliberate hybrid boundaries. Whole-tree preparation resolves every static
argument, deferred page value, and traversal filter across all `UNION`
branches before any runtime producer or SQLite fragment runs. At execution,
Rust materializes deep/open-ended trails or shortest-path BFS results for
SQLite to consume, applies semantic row operators, and performs one final
query-wide hydration pass. Direct entities seed that pass; remaining IDs use
at most one node query and one edge query. With `RETURN DISTINCT`, `ORDER BY`
is restricted to projected expressions or their aliases.

`crabnet::explain(cypher)` renders a deterministic logical plan for inspecting
slots, graph operators, clause stages, and `WITH` scopes without turning
incidental SQLite aliases into the debug contract.
`crabnet::explain_physical(cypher)` renders the numbered physical
operator pipeline: generated SQL/CTEs and ordered bind descriptors, explicit
`trail-expand` or `shortest-expand` runtime-slot producers, semantic
distinct/pagination and union operators, and one final shared batch-hydration
operator when graph identities require it.

Materializing mutation input is intentionally naive, but gives this iteration
clear snapshot, multiplicity, and rollback behavior. A streaming or more
general staged executor can replace it later without changing the logical plan.

## Project notes

- [Roadmap](docs/roadmap.md) records the architectural direction and milestone
  gates.
- [Coverage](docs/coverage.md) is the living supported/partial/rejected feature
  matrix and next-test backlog.
- [Semantics](docs/semantics.md) records the behavior we have deliberately
  chosen so far.
- [Testing workflow](docs/testing.md) describes the focused TDD loop and the
  broader conformance strategy.

This codebase is still disposable in the useful sense: APIs, modules, and the
fresh SQLite schema may be reshaped as experiments demand. There is no deployed
database format to migrate or preserve.

## SQLite layout

The experiment stays close to the original two-table sketch:

```sql
CREATE TABLE nodes (
  id TEXT PRIMARY KEY NOT NULL,
  label TEXT,
  props TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(props) AND json_type(props) = 'object')
) STRICT;

CREATE TABLE edges (
  id TEXT PRIMARY KEY NOT NULL,
  edge_type TEXT NOT NULL,
  from_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
  to_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
  props TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(props) AND json_type(props) = 'object')
) STRICT;
```

For now, each `edges` row represents one relationship rather than a computed
multi-edge Cypher path. `edge_type` is relationship data, while both endpoints
are foreign keys. Foreign-key enforcement is enabled per connection, and
indexes cover label and endpoint/type lookups.

Run the full acceptance suite with:

```console
cargo test --all-targets
```

The milestone 3 expression contracts have focused coverage in
`tests/arithmetic_and_distinct.rs` and
`tests/membership_and_string_predicates.rs`. Milestone 4 clause and scope
contracts are covered in `tests/with_pipeline.rs`. Milestone 5 property and
deletion contracts are covered in `tests/matched_mutations.rs` and
`tests/delete_mutations.rs`. Milestone 6 query-composition contracts are
covered in `tests/aggregation.rs`, `tests/aggregation_edges.rs`,
`tests/unwind.rs`, `tests/union.rs`, and `tests/optional_match.rs`. Milestone 7's
SQLite-bounded traversal is covered in `tests/bounded_traversal.rs`. Milestone
8's path/list transport and hybrid deep/open-ended traversal slices are covered
in `tests/path_values.rs`, `tests/relationship_list_values.rs`, and
`tests/unbounded_traversal.rs`; its legacy shortest-path slice is covered in
`tests/shortest_paths.rs`.
