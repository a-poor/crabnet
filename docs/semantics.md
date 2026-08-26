# Current Cypher semantics

This document records Crabnet's current, implemented semantics. It is a
contract for the supported query slice, not a statement of full openCypher
conformance or a roadmap for unsupported features.

## Rows, multiplicity, and ordering

Query results use bag semantics. Matching the same bindings through multiple
input rows produces multiple result rows; Crabnet does not implicitly
deduplicate them. Comma-separated, disconnected patterns therefore produce a
Cartesian product. `RETURN DISTINCT` is not supported.

Without `ORDER BY`, result row order is unspecified. With `ORDER BY`, rows are
ordered by the supported expressions and directions in that clause, but the
relative order of equal sort keys is unspecified. `SKIP`/`OFFSET` and `LIMIT`
are applied after ordering when an order is present. Projection modifiers,
including ordering and pagination, are not currently supported after
`CREATE`.

## Values and properties

Parameters, stored properties, and result cells use `serde_json::Value`. The
currently supported mapping is:

| Cypher value | JSON representation | SQLite predicate representation |
| --- | --- | --- |
| null | `null` | SQL `NULL` |
| boolean | `true` or `false` | integer payload plus JSON type information |
| integer | JSON number in the signed 64-bit range | SQLite integer |
| float | finite JSON number | SQLite real |
| string | JSON string | SQLite text |
| list | JSON array | JSON text plus JSON type information |
| map | JSON object | JSON text plus JSON type information |

Node and relationship `props` columns must contain JSON objects. Lists and maps
may be nested property values, and their JSON types are preserved when they are
returned. Integers outside SQLite's signed 64-bit range and non-finite floats
are rejected.

A missing property and a property whose stored value is explicit JSON `null`
both evaluate to Cypher `null`. Returning either produces JSON `null`, and `IS
NULL` is true for either. Crabnet does not currently expose a supported query
operation that distinguishes the two cases.

Graph entities and paths are not values in the current result model. A whole
node, relationship, or path cannot be returned; callers can currently return
properties and supported metadata functions instead.

## Predicates and null

Supported predicates use three-valued logic: true, false, or null. A `WHERE`
clause retains only rows for which its predicate is true. False and null both
remove the row. Comparisons involving null, including `null = null`, evaluate
to null. `IS NULL` and `IS NOT NULL` are the supported way to test for null.
Supported `AND`, `OR`, and `NOT` expressions propagate null according to
SQLite's three-valued boolean rules.

Equality uses JSON type information to avoid SQLite's broad coercions:

- integers and floats are compatible numeric types, so values such as `1` and
  `1.0` compare equal;
- booleans compare only with booleans and are not equal to numeric `0` or `1`;
- strings compare only with strings;
- lists compare only with lists, and maps compare only with maps;
- values of different non-numeric JSON kinds are not equal.

For the current implementation, same-kind list and map equality ultimately
uses their stored JSON representation. Full Cypher structural equality rules,
including whether map key order must be ignored in every storage situation,
remain unresolved. The semantics of ordering comparisons across unlike JSON
types are also unresolved and should not be relied upon.

## Labels and relationship types

A node has zero or one label. An unlabeled node stores a null label;
`labels(node)` returns an empty list for it and a singleton list for a labeled
node. Multiple labels and boolean label expressions are unsupported.

A stored relationship has exactly one nonempty type. A `MATCH` relationship
pattern may omit its type or specify one type. A created relationship must
specify exactly one type. Multiple and boolean relationship-type expressions
are unsupported. `type(relationship)` returns the relationship's single type.

## Fixed-length paths and direction

Every stored edge is directed from `from_node` to `to_node`. A right-arrow
pattern matches that direction, a left-arrow pattern matches the reverse
spelling, and an undirected `MATCH` pattern accepts either orientation. An
undirected `CREATE` is unsupported; left- and right-arrow creates are stored in
the direction indicated by the arrow.

Within one fixed-length path pattern, one stored relationship cannot occupy two
relationship positions. Nodes may repeat, so cycles and self-relationships are
allowed as long as each relationship position uses a distinct relationship.
Relationship uniqueness is per path pattern, not global across disconnected
comma-separated pattern parts. Variable-length paths and path bindings are not
supported.

## Entity identifiers

Node and relationship identifiers are opaque text values assigned when an
entity is created and constrained to be unique by the storage schema.
`elementId(entity)` exposes the identifier. Callers must not depend on its
current textual format, ordering, or generation mechanism. The legacy-style
`id()` function is unsupported.

## Writes and transactions

`CREATE` without a `MATCH` has one implicit input row, so its mutation template
runs once. For `MATCH ... CREATE`, Crabnet first materializes the complete
matched row bag, then applies the complete `CREATE` template once to every
materialized row. Consequently:

- a zero-row match performs no mutations;
- duplicate and Cartesian-product rows each cause their own mutations;
- fresh entities are fresh for each input row, while reuse of a fresh variable
  within one row refers to that row's one entity;
- entities created by the statement cannot become additional input matches for
  that same statement.

All parameters and storable property values for a write are resolved and
validated before the write transaction starts. Match materialization, all
mutations, and post-write result evaluation then occur in one immediate SQLite
transaction. Any execution failure rolls back every mutation performed by the
statement. A `RETURN` after `CREATE` emits one row per mutation input row and
may project supported properties or metadata from matched and created
entities.

## Unsupported constructs and errors

Crabnet parses a statement before deciding whether its constructs are in the
supported slice. A syntactically valid but unsupported construct fails during
planning with `Error::Unsupported`; it is never silently approximated.
Malformed input uses `Error::Parse`, and supported syntax with invalid names or
entity kinds uses `Error::Semantic`. Planning completes before mutation
execution, so an unsupported write statement cannot partially modify the
graph.

Semantics for unsupported constructs—including whole-entity and path values,
variable-length traversal, optional matching, aggregation, query composition,
and mutations other than `CREATE`—remain unresolved until those constructs are
designed and implemented.
