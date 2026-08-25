# crabnet

Crabnet is an experiment in translating a small, useful subset of openCypher
into parameterized SQL and executing it as an embedded SQLite property graph.
It is intentionally not a production database or a stable library API yet.

```rust
use crabnet::{Database, Parameters};
use serde_json::json;

fn main() -> crabnet::Result<()> {
    let mut graph = Database::in_memory()?;
    let mut parameters = Parameters::new();
    parameters.insert("name".into(), json!("Ada"));

    graph.execute_with(
        "CREATE (:Person {name: $name})",
        &parameters,
    )?;

    let result = graph.execute(
        "MATCH (person:Person) RETURN person.name AS name",
    )?;
    assert_eq!(result.rows, [vec![json!("Ada")]]);
    Ok(())
}
```

## Current query slice

The first iteration supports:

- `CREATE` for nodes and fixed, directed relationship chains
- `MATCH` for nodes and fixed-length outgoing, incoming, or undirected patterns
- one optional node label and one relationship type
- literal maps and named `$parameters` for properties
- inline property matching and `WHERE` scalar comparisons/boolean expressions
- scalar-property `RETURN`, aliases, `ORDER BY`, `SKIP`, and `LIMIT`
- `elementId`, `labels`, and `type` projections
- JSON null, booleans, integers, floats, strings, lists, and maps with their types preserved

Unsupported syntax is parsed first and then reported as `Error::Unsupported`.
Malformed Cypher, semantic errors such as unknown variables, and missing
parameters have separate error variants. Planning and parameter resolution
happen before a write transaction starts.

Notable omissions include multiple labels, variable-length traversal,
`OPTIONAL MATCH`, whole-node/relationship results, `MATCH ... CREATE`, `SET`,
`DELETE`, `MERGE`, `WITH`, `UNION`, and `RETURN DISTINCT`.

## SQLite layout

The experiment stays close to the original two-table sketch:

```sql
CREATE TABLE nodes (
  id TEXT PRIMARY KEY NOT NULL,
  label TEXT,
  props TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(props) AND json_type(props) = 'object')
) STRICT;

CREATE TABLE paths (
  id TEXT PRIMARY KEY NOT NULL,
  path_type TEXT NOT NULL,
  from_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
  to_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
  props TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(props) AND json_type(props) = 'object')
) STRICT;
```

For now, each `paths` row represents one relationship rather than a computed
multi-edge Cypher path. `path_type` is relationship data, while both endpoints
are foreign keys. Foreign-key enforcement is enabled per connection, and
indexes cover label and endpoint/type lookups.

Run the full acceptance suite with:

```console
cargo test --all-targets
```
