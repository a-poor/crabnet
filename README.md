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
- anonymous legacy bounded traversal in mandatory `MATCH`: `*n`,
  `*min..max`, and `*..max`, with outgoing, incoming, and undirected direction
- named paths in mandatory read-only `MATCH`, across fixed, bounded, and mixed
  patterns, returned as `CypherValue::Path(PathValue)`
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

Bounded traversal returns endpoint rows and can contribute to a named path. Its
upper bound may be at most 32. An omitted lower bound in `*..max` means one,
while an explicit zero permits the identity path and still applies the endpoint
node constraints. A reversed range is valid but produces no rows. An optional
relationship type and inline property map filter every traversed edge. SQLite
evaluates the traversal as a recursive CTE that carries the ordered node and
edge IDs used by each candidate: nodes may be revisited, but a stored edge may
appear only once in one path. Distinct edge sequences retain their normal bag
multiplicity even when they reach the same endpoint.

`PathValue` exposes nodes and relationships in traversal order, always with one
more node than relationship. A zero-hop path therefore has one node and no
relationships. Relationship values keep their stored start/end direction even
when the path traversed them backwards. SQLite returns only compact ordered IDs;
after the cursor is materialized, Rust hydrates graph values with node and
relationship caches scoped to each SQL branch. Paths survive direct `WITH`
aliases and use ordered node/relationship identity for `RETURN DISTINCT` and
distinct `UNION`. Hydration currently precedes that deduplication; batching it
after identity-level dedup and sharing one deferred read snapshot across union
branches are milestone-8 follow-ups. Concurrent external writers are not a
current goal.

Unsupported syntax is parsed first and then reported as `Error::Unsupported`.
Malformed Cypher, semantic errors such as unknown variables, and missing
parameters have separate error variants. All parameters and stored property
values are resolved and validated before a write transaction begins. Match
materialization, every mutation, and post-write result evaluation then share
one immediate transaction; any runtime failure rolls the whole statement back.

Notable omissions include multiple or boolean labels/types, label mutations,
unbounded traversal, postfix graph quantifiers, named variable-length
relationship lists, path selectors and path functions/property access, named
paths in `OPTIONAL MATCH`, quantified `OPTIONAL MATCH`, traversal-fed writes,
writes fed by any named path, match modes and hints, parameter-map matching,
`RETURN *`, `MERGE`, exponentiation (`^`), concatenation (`||`), computed
right-hand sides for property updates, and computed projections or
`DISTINCT`/`ORDER BY`/`SKIP`/`OFFSET`/`LIMIT` after mutations. Paths also cannot
yet be scalar operands, aggregate inputs/grouping keys, or ordering keys.
`WITH DISTINCT`, `WITH *`, unaliased computed `WITH` expressions,
`collect(DISTINCT ...)`, nested/embedded aggregate projections, and writes in a
read-composition pipeline are explicitly rejected. For now, `DISTINCT` is
supported only by `count`; the other aggregate forms fail during planning.

## Planning and execution

The planner is deliberately small and layered:

- `planner/lower.rs` converts the openCypher AST into the backend-neutral,
  slot-based row plan in `planner/logical.rs`.
- `planner/sqlite.rs` compiles reads and mutation row sources to parameterized
  SQLite queries; each read-only `WITH` boundary becomes a CTE carrying the
  projected entity, path, and scalar slots, while bounded traversal becomes a
  recursive CTE carrying depth plus ordered per-path node/edge identity.
- `database.rs` executes SQL read fragments, hydrates compact path identities
  after cursor materialization, and combines union branches. For mutations, it
  materializes all input rows before the first change and applies ordered
  mutation operators to that stable row bag.

Path hydration, read-only `RETURN DISTINCT`, and distinct query unions are
deliberate hybrid boundaries. SQLite produces compact rows, then Rust hydrates
paths and performs recursive typed row deduplication. `RETURN DISTINCT` applies
its deferred pagination after deduplication. With that projection modifier,
`ORDER BY` is restricted to projected expressions or their aliases.

`crabnet::explain(cypher)` renders a deterministic logical plan for inspecting
slots, graph operators, clause stages, and `WITH` scopes without turning
incidental SQLite aliases into the debug contract.
`crabnet::explain_physical(cypher)` exposes the generated read SQL/CTEs, bind
positions, path-hydration boundary, and Rust distinct/pagination stages when
backend details matter.

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
`tests/unwind.rs`, `tests/union.rs`, and `tests/optional_match.rs`. Milestone 7
bounded traversal is covered in `tests/bounded_traversal.rs`.
