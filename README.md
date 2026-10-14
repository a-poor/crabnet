# `crabnet`

An (experimental) embedded graph database for Rust, backed by SQLite.

`crabnet` lets you store connected data and query it with Cypher, directly from
your application. Model nodes and relationships, follow paths, and get typed
Rust values back—without running a separate database server.

It’s a good fit for local developer tools, dependency explorers, and rebuildable
graph indexes.

**Status:** Experimental. `crabnet` supports a documented subset of openCypher,
and its API is still evolving. See [supported queries](docs/coverage.md) and
[compatibility](docs/compatibility.md) before adopting it.

## What you get

- File-backed or in-memory storage with bundled SQLite.
- Cypher queries for graph matching, traversal, and updates.
- Typed results for nodes, relationships, paths, and ordinary values.
- Transactions, uniqueness constraints, and backup/restore.

## Getting started

### Installation

`crabnet` requires Rust 1.99 or later. Add it to your `Cargo.toml`:

```toml
[dependencies]
crabnet = "0.1"
serde_json = "1"
```

The example below uses `serde_json` to supply query parameters. SQLite is bundled;
you don’t need to install or run a database server.

### Your first graph

Create two people and a relationship, then find Ada’s friends:

```rust
use crabnet::{Database, Parameters};
use serde_json::json;

fn main() -> crabnet::Result<()> {
    let mut graph = Database::in_memory()?;
    graph.execute(
        "CREATE (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Grace'})",
    )?;

    let parameters = Parameters::from([("name".into(), json!("Ada"))]);
    let result = graph.execute_with(
        "MATCH (:Person {name: $name})-[:KNOWS]->(friend:Person)
         RETURN friend.name AS name",
        &parameters,
    )?;

    assert_eq!(result.rows[0][0].as_str(), Some("Grace"));
    Ok(())
}
```

## Working with `crabnet`

### Opening a database

Use `Database::in_memory()` for a temporary graph or
`Database::open("graph.sqlite")` to open or create a database file.
`Database::open_with_options` also supports read-only access, requiring an
existing file, and configuring the lock timeout.

The supported setup uses a local database file with one writer at a time.
See [opening and contention](docs/semantics.md#file-opening-and-contention)
for details.

### Queries and results

Use `execute` for a query or `execute_with` to pass named `$parameters`.
Parameters and stored properties use JSON-compatible values. Results contain
column names and rows of `CypherValue` cells, preserving integers, floats,
lists, maps, and graph entities as distinct types.

Returning `friend` instead of `friend.name` in the example gives you a typed
node with its identity, labels, and properties. Relationships and paths also
have their own result types.

For repeated or larger reads, explore [reusable compiled reads](docs/compiled-reads.md),
[incremental cursors](docs/cursors.md), and
[query resource controls](docs/semantics.md#query-resource-controls).

### Transactions

Each write query is atomic. Use `Database::transaction()` to group several
queries, then call `commit()` to keep the changes. Dropping an uncommitted
transaction rolls it back; a query error aborts the whole transaction.

See [application transactions](docs/semantics.md#application-transactions)
for an example and the [commit failure guidance](docs/semantics.md#commit-io-failures)
before adding automatic retries: a commit I/O error can occur after writes
have persisted.

## Cypher support

`crabnet` supports graph matching and updates, variable-length traversal,
selected shortest-path queries, filtering, aggregation, and list/map expressions.
Queries can combine stages with `WITH`, expand lists with `UNWIND`, and combine
read results with `UNION`.

It does not implement all of openCypher. Some combinations—such as
variable-length `OPTIONAL MATCH` and traversal-fed writes—are unsupported.
See the [feature matrix](docs/coverage.md) for supported forms and limitations,
and [query semantics](docs/semantics.md) for exact behavior.

## Examples

- **[Cargo dependency explorer](docs/dependency-graph.md):** Import a dependency
  graph, find dependencies and dependents, and query shortest paths. Run
  `cargo run --release --example dependency_graph -- --help` to get started.
- **[Git Graph pilot](docs/git-graph-pilot.md):** See how a downstream application
  uses `crabnet` to index and explore Git history, along with its recorded
  acceptance results.

## Documentation

- [Documentation guide](docs/README.md) — the full index.
- [Query semantics](docs/semantics.md) — values, transactions, constraints,
  resource limits, and errors.
- [Backups and recovery](docs/semantics.md#backups-and-process-recovery) — saving
  snapshots and recovering a database.
- [Support](docs/support.md) — tested platforms and operating assumptions.
- [Compatibility](docs/compatibility.md) — API changes, storage upgrades, and rollback.
- [Roadmap](docs/roadmap.md) — planned work and project direction.

## Development

Build the library and run the complete repository checks:

```sh
cargo build
bash scripts/check.sh
```

See the [testing workflow](docs/testing.md) for focused commands and benchmarks,
and the [architecture guide](docs/architecture.md) for code organization and
maintenance guidance.

## License

`crabnet` is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. See
[third-party materials](THIRD_PARTY.md) for dependency and vendored licenses.
