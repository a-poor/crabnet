# Current Cypher semantics

This document records Crabnet's current, implemented semantics. It is a
contract for the supported query slice, not a statement of full openCypher
conformance or a roadmap for unsupported features.

## Rows, multiplicity, and ordering

Query results use bag semantics. Matching the same bindings through multiple
input rows produces multiple result rows; Crabnet does not implicitly
deduplicate them. Comma-separated, disconnected patterns therefore produce a
Cartesian product unless a read query explicitly uses `RETURN DISTINCT`.

Read-only `RETURN DISTINCT` deduplicates the complete projected row after its
cells have been decoded to `CypherValue`. This is a Rust execution stage, not
SQLite text or JSON equality. Its semantic equivalence rules are:

- all null cells are one equivalence class, including missing and explicit-null
  properties after evaluation;
- integers and exactly equivalent integral floats share an equivalence class,
  while booleans remain distinct from numeric `0` and `1`;
- list and map comparison applies these rules recursively; and
- nodes and relationships compare by their respective opaque element IDs.

For a distinct query, `SKIP`/`OFFSET` and `LIMIT` are applied after semantic
deduplication. `ORDER BY` may refer only to a projected expression or its
alias; ordering by an unprojected expression is a semantic error.

Without `ORDER BY`, result row order is unspecified. With `ORDER BY`, rows are
ordered by the supported expressions and directions in that clause, but the
relative order of equal sort keys is unspecified. `SKIP`/`OFFSET` and `LIMIT`
are applied after ordering when an order is present. Projection modifiers,
including ordering and pagination, are not currently supported after a
mutation; neither is `RETURN DISTINCT` after a mutation.

## `WITH` clause boundaries

`WITH` is supported in read-only pipelines. It may begin a query, appear more
than once, or separate `MATCH` clauses. Each `WITH` consumes the current row
bag and emits one row for every retained input row; it does not implicitly
deduplicate. A later `MATCH` therefore expands every carried row independently,
preserving duplicate and Cartesian-product multiplicity.

A `WITH` projection can carry whole node or relationship bindings and typed
scalar values. A bare variable keeps its name, while `AS` may give either kind
of value a new name. After the boundary, only projected names remain in scope:
an entity or scalar that was not projected cannot be referenced by a later
`MATCH`, `WITH`, or `RETURN`.

`WHERE` attached to a `WITH` is evaluated at that boundary. It may refer to the
new projected aliases and to variables from the incoming row, with a projected
name shadowing the same incoming name. This extra incoming visibility belongs
only to that filter; using a variable there does not carry it into the next
clause.

`ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` on `WITH` are applied before any
subsequent clause. Intermediate ordering determines which rows pagination
retains and is carried into the immediately following clause across the SQLite
CTE boundary. As with Cypher generally, use `ORDER BY` on the final `RETURN`
when presentation order after additional clauses is part of the caller's
contract. Named pagination parameters follow the same validation rules as
`RETURN` modifiers.

Omitting a quantifier and spelling `WITH ALL` have the same bag-preserving
behavior. `WITH DISTINCT` is not yet implemented. `WITH *`, computed
expressions without an alias, aggregate expressions, and mutation clauses in a
query containing `WITH` are also explicit planning-time rejections. The
implemented acceptance and rejection cases live in `tests/with_pipeline.rs`.

## Values and properties

Named parameters and stored properties use `serde_json::Value`, while every
public result cell uses Crabnet's recursive `CypherValue`. Graph entities are
therefore distinct from ordinary property maps at the API boundary.

| Cypher value | Parameter/property representation | Result representation |
| --- | --- | --- |
| null | JSON `null` / SQL `NULL` | `CypherValue::Null` |
| boolean | JSON boolean; SQLite integer payload plus JSON type | `CypherValue::Boolean` |
| integer | JSON number in the signed 64-bit range / SQLite integer | `CypherValue::Integer` |
| float | finite JSON number / SQLite real | `CypherValue::Float` |
| string | JSON string / SQLite text | `CypherValue::String` |
| list | JSON array | `CypherValue::List` |
| map | JSON object | `CypherValue::Map` |
| node | not accepted as a parameter or property | `CypherValue::Node(NodeValue)` |
| relationship | not accepted as a parameter or property | `CypherValue::Relationship(RelationshipValue)` |
| path | not supported | not supported |

Node and relationship `props` columns must contain JSON objects. Lists and maps
may be nested property values; conversion to result cells recursively preserves
their Cypher value kinds. Integers outside SQLite's signed 64-bit range and
non-finite floats are rejected.

A missing property and a property whose stored value is explicit JSON `null`
both evaluate to Cypher `null`. Returning either property produces
`CypherValue::Null`, and `IS NULL` is true for either. A returned whole entity
does preserve whether its property map contains an explicit-null key.

`NodeValue` contains an opaque `element_id`, a `labels` vector, and a recursive
`properties` map. The current storage model produces zero or one label, while
the vector leaves the result shape ready for multi-label nodes later.

`RelationshipValue` contains an opaque `element_id`, `relationship_type`,
`start_node_element_id`, `end_node_element_id`, and a recursive `properties`
map. Start and end IDs always describe the stored relationship direction,
regardless of whether a query matched it with a right arrow, left arrow, or
undirected pattern.

Whole nodes and relationships may be returned by reads and by `RETURN` after
supported mutations. Entity decoding follows the planned expression kind, so
an ordinary map remains `CypherValue::Map` even if it contains keys resembling
the public entity fields. Paths are not result values yet, and path bindings
remain unsupported.

## Predicates and null

Supported predicates use three-valued logic: true, false, or null. A `WHERE`
clause retains only rows for which its predicate is true. False and null both
remove the row. Comparisons involving null, including `null = null`, evaluate
to null. `IS NULL` and `IS NOT NULL` are the supported way to test for null.
Boolean contexts accept only JSON booleans: null, missing, numeric, string,
list, and map values become null rather than inheriting SQLite truthiness.
Supported `AND`, `OR`, and `NOT` expressions then propagate those values with
three-valued boolean logic.

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
remain unresolved.

Ordering comparisons are deliberately narrower than SQLite's storage-class
ordering. Integers and floats are mutually comparable as numbers; strings are
comparable only with strings; and booleans are comparable only with booleans.
A null or missing operand, or any incompatible or unsupported pair of value
kinds, produces null. Crabnet does not coerce text to numbers or booleans to
numbers for either equality or ordering.

## Membership and string predicates

`IN` uses the same typed equality rules for each list member. It does not
coerce element kinds or expand duplicate members into duplicate result rows.
An empty right-hand list produces false even when the left side is null. For a
nonempty list, a matching member produces true; otherwise a null/missing left
operand or a null member makes the result null, and a definite miss produces
false. A null, missing, or non-list right-hand operand produces null rather
than being treated as an empty or singleton list.

`STARTS WITH`, `CONTAINS`, and `ENDS WITH` require two strings and return a
boolean. They are case-sensitive, treat `%` and `_` literally, and support
empty and Unicode strings. If either operand is null, missing, or not a string,
the result is null; there is no string coercion. These predicates and `IN` may
appear in `WHERE` or as computed read projections, where their result is a
typed boolean or null cell.

## Numeric arithmetic and computed projections

Read predicates and projections support binary `+`, `-`, `*`, `/`, and `%`,
plus unary `+` and `-`, with the parser's normal precedence and left
associativity. Integer-only operations retain integer results, including
truncating integer division, while an operation with a float operand produces
a float. Null, missing, boolean, string, list, or map operands produce null;
Crabnet never dynamically coerces them to numbers.

Exponentiation (`^`) and concatenation (`||`) remain explicit planning-time
rejections. Computed scalar projections are currently read-only: the
post-mutation result stage does not yet execute them.

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

Matched `SET` and `REMOVE` currently accept direct property targets on nodes or
relationships. `SET entity.key = value` assigns a literal or parameter value,
and `REMOVE entity.key` deletes that key. `SET entity = map` replaces the
complete property map, while `SET entity += map` performs a shallow merge; the
map may be a literal or named parameter. Row-dependent or otherwise computed
right-hand sides and label mutation are explicit planning-time rejections, as
are writes in a query containing `WITH`.

Direct assignment of `null` removes the targeted key. A null-valued entry in a
`+=` map also removes that key. Whole-map replacement is deliberately
different: an explicit null entry in `SET entity = {key: null}` remains stored
in the replacement map. Retaining that key is a Crabnet extension to Cypher's
usual property-null behavior and is observable when returning the whole
entity. Map merge is shallow; nested maps replace their existing value rather
than recursively merging it.

`QuerySummary::properties_set` counts top-level property keys whose observable
stored value changed or was removed. Reassigning the same value and removing an
absent key contribute zero. A whole-map replacement counts additions,
removals, and value changes rather than counting the clause as one operation.

Matched `DELETE` accepts node and relationship variables. Targets are gathered
and deduplicated across the complete materialized match bag, then explicitly
targeted relationships are deleted before nodes regardless of target spelling
order. Plain deletion of a node that still has an incident relationship fails
the storage foreign-key constraint and rolls the entire statement back.
`DETACH DELETE` first removes all incoming, outgoing, and self-loop
relationships incident to its node targets; relationships shared by multiple
targets or also named explicitly are deleted and counted once. `DELETE` or
`DETACH DELETE` must currently be the final mutation clause before an optional
`RETURN`; a later mutation clause is rejected during planning.

`QuerySummary::nodes_deleted` and `relationships_deleted` count actual stored
rows removed, not matched-row occurrences. A post-delete `RETURN` still emits
one result per materialized input row and evaluates against its pre-delete
entity snapshots. This intentionally allows the returned node or relationship
value to describe an entity that no longer exists after commit.

All parameters and storable property values for a write are resolved and
validated before the write transaction starts. Match materialization, all
mutations, and post-write result evaluation then occur in one immediate SQLite
transaction. Any execution failure rolls back every mutation performed by the
statement. An optional `RETURN` emits one row per mutation input row and may
project whole matched or created entities as well as supported properties and
metadata; for deleted entities it follows the snapshot rule above.

## Unsupported constructs and errors

Crabnet parses a statement before deciding whether its constructs are in the
supported slice. A syntactically valid but unsupported construct fails during
planning with `Error::Unsupported`; it is never silently approximated.
Malformed input uses `Error::Parse`, and supported syntax with invalid names or
entity kinds uses `Error::Semantic`. Planning completes before mutation
execution, so an unsupported write statement cannot partially modify the
graph.

Semantics for unsupported constructs—including path values, variable-length
traversal, optional matching, aggregation, `UNWIND`, `UNION`, `MERGE`, label
mutation, and computed mutation right-hand sides—remain unresolved until those
constructs are designed and implemented.
