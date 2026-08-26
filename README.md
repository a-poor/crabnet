# crabnet

Crabnet is an experiment in lowering a small, useful subset of openCypher into
a logical row plan backed by SQLite. Read-only `MATCH`/`WITH` pipelines compile
to parameterized SQL and CTEs; `CREATE` mutations are applied to matched rows
inside one SQLite transaction. It is intentionally not a production database
or a stable library API yet.

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
represents whole nodes and relationships without disguising them as maps.

## Current query slice

The accepted statement shapes are:

- `RETURN ...`
- `MATCH ... RETURN ...`
- consecutive read-only `MATCH ... MATCH ... RETURN ...` stages
- standalone or chained `WITH ... RETURN ...`, optionally interleaved with
  `MATCH` clauses
- `CREATE ... [RETURN ...]`
- `MATCH ... CREATE ... [RETURN ...]`

Within those shapes, the first iteration supports:

- `CREATE` for nodes and fixed, directed relationship chains, including
  relationships between matched and fresh nodes
- `MATCH` for nodes and fixed-length outgoing, incoming, or undirected patterns
- at most one node label; zero or one relationship type in `MATCH`, and exactly
  one type for each created relationship
- literal maps and named `$parameters` for properties
- inline property matching and `WHERE` typed comparisons, boolean expressions,
  `IN`, `STARTS WITH`, `CONTAINS`, and `ENDS WITH`
- scalar, computed scalar, whole-node, and whole-relationship read projections,
  including numeric `+`, `-`, `*`, `/`, `%`, unary `+`/`-`, aliases, and
  `elementId`, `labels`, and `type`
- read-only `WITH` stages carrying aliased entities and scalars, with scope
  loss, `WHERE`, `ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` before later clauses
- `RETURN DISTINCT`, `ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` on read-only
  returns
- typed `CypherValue` result cells for null, booleans, integers, floats,
  strings, lists, maps, nodes, and relationships

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

Unsupported syntax is parsed first and then reported as `Error::Unsupported`.
Malformed Cypher, semantic errors such as unknown variables, and missing
parameters have separate error variants. All parameters and stored property
values are resolved and validated before a write transaction begins. Match
materialization, every mutation, and post-write result evaluation then share
one immediate transaction; any runtime failure rolls the whole statement back.

Notable omissions include multiple or boolean labels/types, variable-length
traversal, path bindings and path result values, `OPTIONAL MATCH`, parameter-map
matching, `RETURN *`, aggregation, `SET`, `DELETE`, `MERGE`, `UNION`, `UNWIND`,
exponentiation (`^`), concatenation (`||`), and computed projections or
`DISTINCT`/`ORDER BY`/`SKIP`/`OFFSET`/`LIMIT` after `CREATE`. Within a `WITH`
pipeline, `WITH DISTINCT`, `WITH *`, unaliased computed expressions,
aggregation, and writes are explicitly rejected.

## Planning and execution

The planner is deliberately small and layered:

- `planner/lower.rs` converts the openCypher AST into the backend-neutral,
  slot-based row plan in `planner/logical.rs`.
- `planner/sqlite.rs` compiles reads and mutation row sources to parameterized
  SQLite queries; each read-only `WITH` boundary becomes a CTE carrying the
  projected entity and scalar slots.
- `database.rs` executes reads directly. For mutations, it materializes all
  input rows before the first insert and applies the ordered `CREATE` operations
  once per row.

Read-only `RETURN DISTINCT` is a deliberate hybrid boundary: SQLite produces
and orders candidate rows, Crabnet decodes them to `CypherValue` and performs
semantic row deduplication in Rust, then applies pagination. With `DISTINCT`,
`ORDER BY` is restricted to projected expressions or their aliases.

`crabnet::explain(cypher)` renders a deterministic logical plan for inspecting
slots, graph operators, clause stages, and `WITH` scopes without turning
incidental SQLite aliases into the debug contract.
`crabnet::explain_physical(cypher)` exposes the generated read SQL/CTEs, bind
positions, and any Rust distinct/pagination stage when backend details matter.

Materializing the input is intentionally naive, but gives this iteration clear
snapshot, multiplicity, and rollback behavior. A streaming or staged executor
can replace it later without changing the logical plan.

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
contracts are covered in `tests/with_pipeline.rs`.
