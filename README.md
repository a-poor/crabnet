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

## Opening a database file

`Database::open("graph.sqlite")` opens or creates a writable graph. Use
`OpenOptions` when an application must require an existing graph, open
read-only, or choose how long to wait for a lock:

```rust
use crabnet::{Database, OpenMode, OpenOptions};
use std::time::Duration;

fn main() -> crabnet::Result<()> {
    let mut graph = Database::open_with_options("graph.sqlite", &OpenOptions {
        mode: OpenMode::ReadOnly,
        busy_timeout: Duration::from_millis(250),
    })?;
    let nodes = graph.execute("MATCH (n) RETURN n")?;
    Ok(())
}
```

`OpenMode::ReadWrite` requires an existing graph; the default
`OpenMode::OpenOrCreate` can initialize one. Read-only handles reject mutations
and require the current schema. Lock contention returns `Error::Busy`, with
the underlying SQLite error preserved. The default lock timeout is five
seconds. See [file opening and contention](docs/semantics.md#file-opening-and-contention)
for migration, concurrency, and retry behavior.

## Backups and recovery

`backup_to` saves a committed snapshot to a **new** path. `restore_from`
validates that snapshot and atomically replaces the destination graph,
including its identities and uniqueness constraints:

```rust
use crabnet::Database;

fn main() -> crabnet::Result<()> {
    let mut graph = Database::open("graph.sqlite")?;
    graph.backup_to("graph-backup.sqlite")?;
    let mut restored = Database::open("restored.sqlite")?;
    restored.restore_from("graph-backup.sqlite")?;
    Ok(())
}
```

Backups never overwrite existing paths. Both methods support in-memory graphs;
read-only handles can create backups. These synchronous operations use the
connection's lock timeout and hold a source read snapshot while checking and
copying data.

After a process is interrupted during a write, reopen the original file with
`OpenMode::ReadWrite` so SQLite can recover its journal. Preserve the journal
alongside the database until recovery finishes. Read-only opening may fail
while that recovery is needed. See [backup and recovery semantics](docs/semantics.md#backups-and-process-recovery)
for publication, compatibility, and durability limits.

## Transactions across queries

Use `Database::transaction()` when a Rust operation needs several queries to
commit together. Queries see earlier writes in the same transaction:

```rust
use crabnet::Database;

fn main() -> crabnet::Result<()> {
    let mut graph = Database::in_memory()?;
    let mut transaction = graph.transaction()?;
    transaction.execute("CREATE (:Person {name: 'Ada'})")?;
    let people = transaction.execute("MATCH (p:Person) RETURN p")?;
    if people.rows.len() == 1 {
        transaction.execute("MATCH (p:Person) SET p.verified = true")?;
    }
    transaction.commit()?;
    Ok(())
}
```

The handle also supports `execute_with` and explicit `rollback`. Dropping it
without committing rolls back all its writes. Any query error, including
invalid syntax or missing parameters, immediately rolls back the whole
transaction; later execution or commit returns `Error::TransactionAborted`.
Results and mutation summaries are per query and provisional until commit.
The transaction reserves SQLite's writer immediately and borrows the database
until consumed or dropped. Nested transactions and savepoints are not exposed.

## Query resource controls

`Database::execute_with_options` and `Transaction::execute_with_options` accept
per-query budgets, a cooperative deadline, and a shared cancellation token:

```rust
use crabnet::{CancellationToken, Database, Parameters, QueryOptions};
use std::time::{Duration, Instant};

let mut database = Database::in_memory()?;
let cancellation = CancellationToken::new();
let options = QueryOptions {
    cancellation: Some(cancellation.clone()),
    deadline: Some(Instant::now() + Duration::from_secs(5)),
    max_result_rows: Some(1_000),
    max_result_bytes: Some(8 * 1024 * 1024),
    max_materialized_bytes: Some(32 * 1024 * 1024),
    ..QueryOptions::default()
};
// Another thread may call cancellation.cancel(). Tokens cannot be reset.
let result = database.execute_with_options(
    "MATCH (n) RETURN n LIMIT 1000", &Parameters::new(), &options,
)?;
# Ok::<(), crabnet::Error>(())
```

`options.traversal` configures the existing state, payload, and retained-memory
budgets. `max_sqlite_steps` optionally bounds SQLite progress callbacks across
the query. Every counter spans all branches and write stages, then resets for
the next call. Existing execution methods retain their defaults.

Exhaustion returns `Error::ResourceLimit`; cancellation and deadline expiry
return `Error::Cancelled` and `Error::DeadlineExceeded`. Failures roll back the
statement, or the whole explicit transaction, and leave the database reusable.
The `execute` methods remain eager. Byte limits use documented accounting rather than total
process memory, and deadlines do not preempt parsing, an individual scalar or
aggregate callback, or lock/I/O waits. See [resource semantics](docs/semantics.md#query-resource-controls)
for accounting boundaries and cleanup behavior.

## Read cursors

Use a prepared read to consume rows incrementally:

```rust
use crabnet::{Database, ReadMode};

let mut database = Database::in_memory()?;
database.execute("CREATE (:Person {name: 'Ada'})")?;
let mut read = database.prepare_read("MATCH (p:Person) RETURN p.name")?;
assert_eq!(read.mode(), ReadMode::Incremental);
for row in read.cursor()? {
    let row = row?;
    println!("{row:?}");
}
drop(read); // releases the database borrow
# Ok::<(), crabnet::Error>(())
```

`prepare_read_with` binds parameters; `prepare_read_with_options` also accepts
`QueryOptions`. `columns()` and `mode()` are available before execution.
Single SQLite fragments without Rust traversal inputs decode one row per
advance. Other plans use `ReadMode::Buffered`, computing the complete result
when the cursor starts. SQLite may still buffer internally for sorts or aggregates.

Incremental cursors retain a read snapshot. Drop them promptly after early exit
to release locks. A later error can follow rows already yielded; the error ends iteration.
Mutations are rejected. See [cursor usage and measurements](docs/cursors.md) and
the [lifetime and resource contract](docs/semantics.md#read-cursors).

## Current query slice

The accepted statement shapes are:

- `RETURN ...`
- `MATCH ... RETURN ...`
- consecutive read-only `MATCH ... MATCH ... RETURN ...` stages
- standalone or chained `WITH ... RETURN ...`, optionally interleaved with
  `MATCH`, `OPTIONAL MATCH`, and `UNWIND` clauses
- read-only branches joined by `UNION`, `UNION DISTINCT`, or `UNION ALL`
- `CREATE ... [CREATE ...] [RETURN ...]`, with later clauses reusing earlier
  bindings
- `MATCH ... [MATCH ...] CREATE ... [CREATE ...] [RETURN ...]`
- `MATCH ... SET/REMOVE ... [RETURN ...]`
- `MATCH ... DELETE/DETACH DELETE ... [RETURN ...]`
- `MERGE ... [ON CREATE SET ...] [ON MATCH SET ...]`, alone, after `MATCH`,
  or mixed with `CREATE`, `SET`, `REMOVE`, and `DELETE`
- any sequence of those reading and updating clauses, such as
  `UNWIND ... CREATE ...`, `MATCH ... WITH ... MERGE ...`, or
  `CREATE ... WITH ... MATCH ... SET ... RETURN ...`, ending with an updating
  clause or `RETURN`

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
- legacy relationship-list bindings in mandatory read-only `MATCH`, such as
  `[rels:TYPE*min..]`, returned as traversal-ordered typed relationships; a
  later occurrence of a bound list matches that exact relationship sequence
- named paths in mandatory read-only `MATCH`, across fixed, bounded,
  open-ended, and mixed patterns, returned as `CypherValue::Path(PathValue)`
- `length(path)` as an integer scalar in filters, comparisons, arithmetic,
  ordering, grouping, aggregate arguments, and direct or aliased projections
- `nodes(path)` and `relationships(path)` as traversal-ordered typed entity
  lists in direct projections and computed `WITH` aliases, with semantic
  `DISTINCT`/`UNION` equality before hydration
- fixed-length `OPTIONAL MATCH`, including whole-pattern null extension and
  optional-local `WHERE` predicates
- any number of node labels: `:A:B` and `:A&B` conjunctions in patterns and
  writes, `|`, `!`, and parentheses in `MATCH` patterns, and `n:Label`
  predicates in `WHERE`; zero or one relationship type in `MATCH` (with
  `r:T|U` as a predicate), and exactly one type for each created relationship
- `SET n:Label` and `REMOVE n:Label`
- literal maps and named `$parameters` for properties
- exact, case-sensitive escaped identifiers and named parameters, including
  whitespace, punctuation, Unicode, embedded backticks, and contextual words
- recursive JSON list/map inputs through 100 nested containers, with deeper
  values rejected during preflight before they can become unreadable stored
  properties
- pattern predicates such as `WHERE (n)-[:KNOWS]->(:Person)` or
  `NOT (a)-->(c)`, and `exists(pattern)`, over fixed-length patterns whose
  variables are already bound
- identity comparison of nodes and relationships with `=` and `<>`
- inline property matching and `WHERE` typed comparisons, boolean expressions
  (`AND`, `OR`, `XOR`, `NOT`),
  `IN`, `STARTS WITH`, `CONTAINS`, `ENDS WITH`, and the list quantifiers
  `all`, `any`, `none`, and `single`
- `.key` access on map values: projected maps, parameters, nested properties,
  and quantified list elements
- list comprehensions `[x IN list WHERE predicate | projection]`, `size()` of
  lists and strings, and list and map literals with computed elements such as
  `[n.a, n.b + 1]` or `{key: n.a}`
- `list[i]`, `map[k]`, `entity[k]`, `list[a..b]`, `range()`, and string and
  list concatenation with `+`, including entity lists and mixed graph collections
- `CASE` expressions in the simple and generic forms
- scalar functions: `coalesce`, `keys`, `properties`, `head`, `last`, `tail`,
  `reverse`, `toInteger`, `toFloat`, `toString`, `toBoolean`, `toLower`,
  `toUpper`, `trim`, `ltrim`, `rtrim`, `substring`, `left`, `right`,
  `replace`, `split`, `abs`, `sign`, `ceil`, `floor`, `round`, `sqrt`, `exp`,
  `log`, `log10`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `pi`, `e`, and
  `rand`
- scalar, computed scalar, whole-node, and whole-relationship read projections,
  including numeric `+`, `-`, `*`, `/`, `%`, unary `+`/`-`, aliases, and
  `elementId`, `labels`, and `type`
- `WITH` and `WITH DISTINCT` stages carrying aliased entities, paths, and scalars, with scope
  loss, `WHERE`, `ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` before later
  clauses, plus `WITH *` and `RETURN *`
- implicit grouping with `count`, `sum`, `avg`, `min`, `max`, and `collect`
  in `RETURN` or `WITH`, including expressions such as `count(*) + 1`,
  `coalesce(avg(n.score), 0)`, and `size(collect(DISTINCT n))`; entity
  collections preserve identity and scalar collections use typed deduplication
- list expansion with `UNWIND`, including collected entities that can feed
  later matches and writes, and path-function or legacy relationship lists
  that can feed later reads, plus typed semantic `UNION` deduplication and
  bag-preserving `UNION ALL`
- `RETURN DISTINCT`, `ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` on read-only
  returns
- nested lists and maps containing nodes, relationships, paths, ordinary values,
  and other collections, including `collect({node:n})`, typed selection,
  comprehensions, `UNWIND`, and recursive identity-based distinctness
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
accepted as the default behavior. `WITH DISTINCT` removes duplicate projected
rows using typed equality before pagination, later matches, or writes:

```cypher
MATCH (person:Person)-[:KNOWS]->()
WITH DISTINCT person
SET person.connected = true
RETURN person.name AS name
```

A `WHERE` attached to `WITH` may inspect both
projected aliases and incoming variables, but an incoming variable that was
not projected is unavailable after that filter and clause boundary.

`MATCH ... CREATE` follows row semantics. Crabnet materializes the complete
match result before writing, then applies the `CREATE` template once per input
row. A zero-row match is a no-op; duplicate and Cartesian-product rows each
produce their own mutations. Fresh entities are fresh per row, and a matched
node may be reused in `CREATE` only as a bare `(variable)` reference. When a
`RETURN` is present, it runs as an ordinary read projection over the
post-mutation rows, so it may refer to matched and created entities, compute
expressions, aggregate, and use `DISTINCT`, `ORDER BY`, `SKIP`, and `LIMIT`.
Property values in `CREATE` and `SET` may likewise be computed from the row,
including from entities created or updated earlier in the same statement.

Matched property updates operate on the same materialized input bag. Direct
assignment to `null` removes a property, as does a `null` entry in a `+=` map;
whole-map `=` replacement retains explicit JSON-null entries. The latter is a
documented Crabnet extension. `properties_set` counts top-level keys whose
stored value actually changed or was removed, so assigning the same value or
removing an absent key contributes zero.

`DELETE` and `DETACH DELETE` accept expressions that select a node or
relationship, including `people[0]` and `box.node`; null targets are skipped.
For example:

```cypher
MATCH (n:Person)
WITH n ORDER BY n.name
WITH collect(n) AS people
DETACH DELETE people[0]
```

Targets are evaluated before deletion and deduplicated across the complete
matched bag. Explicitly targeted relationships are removed before nodes,
independent of their spelling order in `DELETE`. Plain deletion of a connected
node fails SQLite's foreign-key
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

Deep/open-ended enumeration and shortest-path BFS share two fail-closed limits
across the top-level query, including `UNION` branches and multiple hybrid
expansions: 100,000 explored trail states and 128 MiB of compact serialized JSON
runtime relations. The JSON encoder admits exact UTF-8 bytes incrementally,
including array punctuation and escaped IDs, instead of retaining a second
whole-relation serialization. Exhausting either limit returns
`Error::ResourceLimit`; Crabnet never silently truncates the result. Label and
inline-property constraints on a traversal's start node seed only the matching
nodes, an unconstrained positive-hop start seeds only nodes with an eligible
edge, and end-node constraints filter emitted rows before serialization. A
third fail-closed limit, 256 MiB of deterministically counted retained graph
input, bounds the eligible edges, seed lists, adjacency maps, and search
frontiers a producer holds while it runs; it is charged as inputs are loaded
and released when each producer finishes. None of the limits is a total heap
bound: allocator overhead, SQLite's memory, and the final result set stay
outside them.

`PathValue` exposes nodes and relationships in traversal order, always with one
more node than relationship. A zero-hop path therefore has one node and no
relationships. Relationship values keep their stored start/end direction even
when the path traversed them backwards. SQLite returns compact ordered
identities. `length(path)` is the relationship count and remains an ordinary
SQL-backed integer across filters, arithmetic, ordering, implicit grouping,
and aggregate arguments. `nodes(path)` and `relationships(path)` return
`CypherValue::List` values whose members are typed `NodeValue` and
`RelationshipValue` cells in traversal order; zero-hop relationships are an
empty list. These graph lists may be returned directly, carried through a
computed `WITH` alias, indexed or sliced, or expanded with `UNWIND` into entity
bindings for later reads and matches. Segment junctions appear once in `nodes(path)`; repeated
nodes in cycles and self-loops retain their multiplicity. Bare path lists
are not yet accepted as predicates, arithmetic operands, ordering/grouping
keys, or aggregate inputs. Path-fed writes remain unsupported.

A fresh variable on a legacy relationship pattern, for example
`[rels:NEXT*0..]`, binds the same public relationship-list value shape. Its
members follow traversal order while each relationship keeps its stored
start/end direction, and zero hops bind `[]`. The list may be returned directly,
carried through a `WITH` alias, expanded with `UNWIND`, shaped by
`RETURN DISTINCT` and pagination, and combined through `UNION` or `UNION ALL`;
it can also coexist with a named path
for the same pattern. Reusing a bound relationship-list variable in a later
variable-length pattern, in the same `MATCH` or after `WITH`, matches only
traversals whose relationship sequence equals the bound list position by
position, while the new pattern keeps its own direction, bounds, type, and
property filters. The other graph-list expression contexts above remain
unsupported.

Rust keeps path and graph-list cells pending while branch-local
`RETURN DISTINCT` and deferred pagination, followed by left-to-right
`UNION`/`UNION ALL`, shape complete typed rows. It then collects only
surviving IDs query-wide, seeds hydration from directly projected node and
relationship values, and loads the remainder in batches—at most one node query
and one edge query. Every branch and hydration read shares one deferred SQLite
snapshot. Concurrent external writers are not a current goal.

Unsupported syntax is parsed first and then reported as `Error::Unsupported`.
Malformed Cypher, semantic errors such as unknown variables or `NOT 1`, and
missing parameters have separate error variants. At runtime, a value of the
wrong kind (`NOT n.name` for a string) fails with `Error::Type`, a function
argument outside its domain with `Error::Argument`, and a write that would
break the graph with `Error::Constraint`. For standalone queries, parameters
and stored property values are resolved and validated before a write
transaction begins, including integer range, map shape, and recursive depth.
Match materialization, every
mutation, and post-write result evaluation then share one immediate transaction;
any runtime failure rolls the whole statement back.

Notable omissions include multiple or boolean relationship types in patterns,
the label wildcard `%`, postfix graph quantifiers, modern path selectors, expression-position
shortest-path calls, and path property access. Legacy shortest-path factors are also rejected in
`OPTIONAL MATCH`, with a same-clause `WHERE` or another comma-separated pattern
part, with a lower bound above one, or in a write query. Named paths or
relationship-list bindings in `OPTIONAL MATCH`, quantified `OPTIONAL MATCH`,
traversal-fed writes (including writes fed by a relationship list), writes fed
by any named path, match modes and hints, parameter-map matching,
exponentiation (`^`), and concatenation (`||`). Paths also cannot
themselves be scalar operands, aggregate inputs/grouping keys, or ordering
keys; use `length(path)` where an integer scalar is required. Typed graph lists
from `nodes` and `relationships` retain the context restrictions above.
Collected entity lists support direct projection, aliases, grouping,
`DISTINCT`, `size()`, `UNWIND`, indexing, slicing, comprehensions, list
quantifiers, concatenation with `+`, and `head`, `last`, `tail`, and `reverse`.
For example,
`people[0].name` reads the first collected node's name and `people[-2..]`
returns the last two entities. Indexing and slicing also work on `nodes(path)`
and `relationships(path)`, preserving graph types, order, and duplicates.
Concatenation supports `[person] + friends`, `friends + person`, and joins
between typed lists; a null operand produces null, while `[null]` retains a
null member. Paths may also be appended or prepended to lists.
The four list functions also accept path-function lists and mixed nested lists.
`head(reverse(people))` retains the last node's identity; `tail(people)` keeps
the remaining members in order. Reversal changes only the outer list order.
Comprehensions such as `[n IN people WHERE n.active]`,
`[n IN nodes(path) | n.name]`, and `[r IN relationships(path) | r.weight]`
filter typed entities or project ordinary values while keeping list order,
duplicates, and null members. Quantifiers (`all`, `any`, `none`, and `single`)
accept these same typed lists, as in `any(n IN people WHERE n.age >= 18)` or
`all(r IN relationships(path) WHERE r.weight > 0)`. Nested graph collections
such as `{person:n, friends:collect(friend)}` preserve graph types through
selection, iteration, grouping, and writes. Graph values cannot be stored as
properties; general scalar operations and ordering on graph collections
remain unsupported. See [nested collection semantics](docs/semantics.md#nested-graph-collections).
Unaliased computed `WITH` expressions and aggregates nested inside aggregates
are explicitly rejected, as is an updating clause after `DELETE` in the same
statement. Among aggregates, `DISTINCT` is supported by `count` and `collect`;
using it on another aggregate fails during planning.

## Node uniqueness constraints

Applications can require a property to be unique among nodes with a label:

```rust
use crabnet::Database;

fn main() -> crabnet::Result<()> {
    let mut database = Database::in_memory()?;
    database.create_unique_constraint("Person", "email")?;
    database.execute("MERGE (:Person {email: 'ada@example.org'})")?;
    assert!(database.execute("CREATE (:Person {email: 'ada@example.org'})").is_err());

    let constraints = database.unique_constraints()?;
    assert_eq!(constraints[0].property, "email");
    database.drop_unique_constraint("Person", "email")?;
    Ok(())
}
```

Create/drop return whether the declaration changed; repeating either is a
no-op. Installation checks existing data atomically. A conflict returns
`Error::Constraint` identifying the label/property pair, and rolls back the
entire query or application transaction. All three methods are also available
on `Transaction`, with schema changes committed or rolled back alongside data.
Constraints persist when the database is reopened.

Missing and null properties are exempt. Other values use `DISTINCT` equality:
`1` conflicts with `1.0`, but not `true` or `'1'`; lists and maps compare
recursively, including null members. Checks apply to each node mutation, so a
key must be released before another node can take it. Names are exact strings
without Cypher quoting. This API supports one node label/property pair per
constraint; composite keys, relationship constraints, and Cypher schema DDL
remain outside this slice. See [semantics](docs/semantics.md#node-uniqueness-constraints).

## Planning and execution

The planner is deliberately small and layered:

- `planner/lower.rs` converts the openCypher AST into the slot-based logical
  row plan in `planner/logical.rs`.
- `planner/sqlite.rs` generates parameterized `SqlFragment` values. Each
  read-only `WITH` boundary becomes a CTE carrying projected entity, path,
  and scalar slots. `WITH DISTINCT` materializes projections and ranks rows by
  typed semantic keys before pagination. Explicit traversal bounds through 32 become recursive CTEs;
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
  deferred snapshot and one query-wide traversal resource budget. Mutations
  continue to materialize their input rows before the first change and apply
  ordered mutation operators to that stable row bag.

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

## Dependency graph application

The runnable Cargo dependency explorer imports a complete resolved graph into a
dedicated database. Repeat imports preserve package and relationship identities,
update package metadata, and remove stale dependencies atomically.

```sh
cargo metadata --format-version 1 --locked > /tmp/crabnet-metadata.json
cargo run --release --example dependency_graph -- import /tmp/crabnet-dependencies.sqlite /tmp/crabnet-metadata.json
cargo run --release --example dependency_graph -- --help
```

It supports dependency/dependent queries, shortest paths, review annotations,
and deletion. Its generated workload verifies import, repeat import, queries,
updates, resource-failure rollback, deletion, and reopening:

```sh
workload_dir=$(mktemp -d)
cargo run --release --example dependency_graph -- demo "$workload_dir/graph.sqlite" 1000
```

See [the application guide](docs/dependency-graph.md) for query commands, import
semantics, reproducible disk-backed timings, and peak-memory measurements.

## Project notes

- [Roadmap](docs/roadmap.md) records the architectural direction and milestone
  gates.
- [Coverage](docs/coverage.md) is the living supported/partial/rejected feature
  matrix and next-test backlog.
- [Semantics](docs/semantics.md) records the behavior we have deliberately
  chosen so far.
- [Testing workflow](docs/testing.md) describes the focused TDD loop and the
  broader conformance strategy.
- [openCypher TCK run](tests/tck/README.md) explains the vendored suite, its
  pinned allowlist and exclusion reasons, and how to update them.

This codebase is still disposable in the useful sense: APIs, modules, and the
fresh SQLite schema may be reshaped as experiments demand. There is no deployed
database format to migrate or preserve.

## SQLite layout

The experiment stays close to the original two-table sketch:

```sql
CREATE TABLE nodes (
  id TEXT PRIMARY KEY NOT NULL,
  labels TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(labels) AND json_type(labels) = 'array'),
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
are foreign keys. Foreign-key enforcement is enabled per connection. A
trigger-maintained `node_labels (label, node_id)` table indexes label lookups,
and indexes cover endpoint/type lookups. Schema version 4 adds persistent node
uniqueness declarations and typed value keys maintained by triggers. Files from
main's versions 1, 2, and 3 are upgraded atomically on writable open, including
property-index backfill. The experimental single-label version 2 from the former benchmark
branch is rejected; recreate those experimental databases.

A fourth table indexes node properties. It holds one row per top-level
non-null property of each node, with the value `json_extract` returns, and
triggers on `nodes` keep it in step with every write:

```sql
CREATE TABLE node_properties (
  node_id TEXT NOT NULL,
  key TEXT NOT NULL,
  value ANY NOT NULL,
  PRIMARY KEY (node_id, key)
) STRICT, WITHOUT ROWID;

CREATE INDEX node_properties_by_value ON node_properties(key, value);
```

When a node scanned by a `MATCH` or `MERGE` has an equality on one of its
properties, as an inline map like `{id: $id}` or a top-level `WHERE`
conjunct like `n.id = $id` or `c.name = p.city`, the compiler adds a probe
of this index beside the exact, type-aware comparison. The probe only narrows
the candidate nodes, so results are unchanged. For those candidates, label
membership is checked with `EXISTS` against `(label, node_id)` in `node_labels`,
avoiding materialization of every node with each label. One probe per node is enough; its other equalities filter
the probed nodes. A list or map value can equal a stored value with different
JSON text (`[1]` and `[1.0]`), so for those the probe returns every node that
has the key. Relationship properties are not indexed yet.

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
`tests/shortest_paths.rs`. Milestone 9's deterministic planner, generated-SQL,
relationship-property, finite predicate, unusual-identifier, and recursive
property invariants are covered by planner and database unit tests plus
`tests/read_semantics.rs`, `tests/predicate_truth_tables.rs`, and
`tests/property_invariants.rs`.
