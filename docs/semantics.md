# Current Cypher semantics

This document records Crabnet's current, implemented semantics. It is a
contract for the supported query slice, not a statement of full openCypher
conformance or a roadmap for unsupported features.

## Rows, multiplicity, and ordering

Query results use bag semantics. Matching the same bindings through multiple
input rows produces multiple result rows; Crabnet does not implicitly
deduplicate them. Comma-separated, disconnected patterns therefore produce a
Cartesian product unless a read query explicitly uses `RETURN DISTINCT`.

Read-only `RETURN DISTINCT` deduplicates the complete projected pending row
after SQL cursor materialization and before graph-value hydration. Scalar,
ordinary collection, and directly projected entity cells already have typed
`CypherValue` representations. Whole paths, path-function entity lists, and
bounded relationship-list pattern bindings remain compact ordered identities.
This is a Rust execution stage, not SQLite text or JSON equality. Its semantic
equivalence rules are:

- all null cells are one equivalence class, including missing and explicit-null
  properties after evaluation;
- integers and exactly equivalent integral floats share an equivalence class,
  while booleans remain distinct from numeric `0` and `1`;
- list and map comparison applies these rules recursively;
- nodes and relationships compare by their respective opaque element IDs; and
- paths compare by their ordered node-ID and relationship-ID sequences;
- `nodes(path)` compares by ordered node IDs, independently of relationship
  IDs, while `relationships(path)` compares by ordered relationship IDs; and
- pending path-derived lists compare with materialized lists by graph entity
  identity. A zero-hop `relationships(path)` therefore equals a literal empty
  list from another union branch.

For a distinct query, `SKIP`/`OFFSET` and `LIMIT` are applied after semantic
deduplication and before graph-value hydration. `ORDER BY` may refer only to a
projected expression or its alias; ordering by an unprojected expression is a
semantic error. A raw path or path-derived typed list cannot itself be an
ordering key yet; `length(path)` is an ordinary integer ordering expression.

Without `ORDER BY`, result row order is unspecified. With `ORDER BY`, rows are
ordered by the supported expressions and directions in that clause, but the
relative order of equal sort keys is unspecified. Ascending order places nulls
last; descending order places nulls first. `SKIP`/`OFFSET` and `LIMIT` are
applied after ordering when an order is present. Projection modifiers,
including ordering and pagination, are not currently supported after a
mutation; neither is `RETURN DISTINCT` after a mutation.

## `WITH` clause boundaries

`WITH` is supported in read-only pipelines. It may begin a query, appear more
than once, or separate `MATCH` clauses. Each `WITH` consumes the current row
bag and emits one row for every retained input row; it does not implicitly
deduplicate. A later `MATCH` therefore expands every carried row independently,
preserving duplicate and Cartesian-product multiplicity.

A `WITH` projection can carry whole node, relationship, or path bindings and
typed scalar values. A bare variable keeps its name, while `AS` may give any
of these values a new name. A path may cross directly or feed an aliased
`length(path)`, `nodes(path)`, or `relationships(path)` projection. Length uses
a normal scalar slot. The two entity-list results retain ordered path identity
and element kind across later direct aliases and projections, but cannot yet be
used as predicates, scalar operands, ordering/grouping keys, aggregate inputs,
or `UNWIND` inputs. After the boundary, only projected names remain in scope:
a binding that was not projected cannot be referenced by a later `MATCH`,
`WITH`, or `RETURN`.

`WHERE` attached to a `WITH` is evaluated at that boundary. It may refer to the
new projected aliases and to variables from the incoming row, with a projected
name shadowing the same incoming name. This extra incoming visibility belongs
only to that filter; using a variable there does not carry it into the next
clause.

An aggregating `WITH` narrows that rule: its `WHERE` and `ORDER BY` may use
projected aggregate aliases and grouping values, but not an ungrouped incoming
value that the projection drops. Violations fail during semantic planning
rather than becoming accidental SQLite grouping behavior.

`ORDER BY`, `SKIP`/`OFFSET`, and `LIMIT` on `WITH` are applied before any
subsequent clause. Intermediate ordering determines which rows pagination
retains and is carried into the immediately following clause across the SQLite
CTE boundary. As with Cypher generally, use `ORDER BY` on the final `RETURN`
when presentation order after additional clauses is part of the caller's
contract. Named pagination parameters follow the same validation rules as
`RETURN` modifiers.

Omitting a quantifier and spelling `WITH ALL` have the same bag-preserving
behavior. `WITH DISTINCT` is not yet implemented. `WITH *`, computed
expressions without an alias, and mutation clauses in a query containing
`WITH` are explicit planning-time rejections. Aggregating `WITH` projections
follow the rules below. The implemented pipeline cases live in
`tests/with_pipeline.rs`, `tests/aggregation.rs`, `tests/unwind.rs`, and
`tests/path_values.rs`.

## Aggregation and grouping

Read-only `RETURN` and `WITH` support top-level `count`, `sum`, `avg`, `min`,
`max`, and `collect` calls. A projection containing an aggregate is an
aggregating projection. Every non-aggregate, row-dependent projection item is
then an implicit grouping key; constants do not create grouping keys. A global
aggregate over empty input emits one row, while an aggregate with grouping keys
emits no rows for empty input.

Grouping and `count(DISTINCT expression)` use typed Cypher equivalence rather
than raw SQLite JSON text. Integers and equal integral floats share a group,
booleans remain distinct from numbers, lists and maps apply the rule
recursively, and whole graph entities group by element ID. Null values form one
group, while `count(expression)` and `count(DISTINCT expression)` omit them.
`count(*)` retains the complete input bag, and `count(entity)` omits a
null-extended entity.

`sum` and `avg` ignore nulls and accept numeric inputs only. `sum` of an empty
numeric bag is integer zero; `avg` of one is null. Mixed integer/float inputs
use SQLite's numeric promotion. If any non-null input has another kind, the
result is null instead of being coerced. `min` and `max` support one compatible
family at a time—numbers, strings, or booleans—and likewise return null for an
incompatible or structured non-null value. `collect` omits nulls, preserves
duplicates, and returns an empty list for empty input. Ordering established by
an immediately preceding `WITH ... ORDER BY` controls collection order.

Explicit `ALL` has the same bag behavior as an omitted aggregate quantifier.
`count(DISTINCT expression)` is supported. `DISTINCT` on the other aggregates,
collecting whole entities, nested aggregate calls, aggregate calls embedded in
a larger projection expression, aggregate-only ordering/filtering without an
aggregating projection, and aggregation after a mutation are structured
planning-time rejections. Aggregates are not allowed in `MATCH` predicates or
directly in `UNWIND`; a `WITH` boundary must materialize them first.
Whole path values and path-derived typed lists are not accepted as aggregate
inputs or grouping keys. `length(path)` is an ordinary integer and may be a
grouping key or aggregate argument.

## `UNWIND`

`UNWIND` expands a list into one output row per member and retains duplicates
and explicit null members. It composes with prior `MATCH`, `WITH`, aggregation,
and `UNWIND` stages while carrying the current entity and scalar scope. Empty
lists, null, and dynamic non-list values produce zero rows; Crabnet does not
coerce a scalar or map to a singleton list. The new alias must not replace a
name already in scope. Unless a later clause orders the result, row order after
`UNWIND` is unspecified. The typed graph lists produced by `nodes(path)` and
`relationships(path)` are not yet accepted as `UNWIND` inputs.

## Query unions

`UNION ALL` appends the pending row bags produced by independently planned
read-only branches. Bare `UNION` and explicit `UNION DISTINCT` semantically
deduplicate the accumulated pending rows using the same recursive typed
equivalence as `RETURN DISTINCT`, including ordered node/relationship identity
for paths and the relevant ordered entity identity for path-derived lists.
Mixed duplicate policies apply from left to right. Branch-local
matching, `WITH`, filters, ordering, semantic distinctness, and deferred
pagination finish before that branch enters the union. Path identities that
survive the complete union chain are hydrated afterward. Cross-transport
comparison handles a path-derived list on one branch and a materialized list on
another, including zero-hop relationship lists versus literal `[]`.

Every branch must end in `RETURN` and expose the same column names in the same
positions; value kinds may differ from row to row. Branch scopes and SQL bind
positions are independent. Writes in a union are rejected before execution,
and final union row order is unspecified.

## `OPTIONAL MATCH`

Fixed-length `OPTIONAL MATCH` preserves every incoming row. If the complete
optional pattern has no candidate, every node and relationship introduced by
that clause becomes null; a partial path candidate never leaks one of its
entities. A matching pattern retains its normal one-to-many and parallel-edge
bag multiplicity. An optional match over the implicit unit input therefore
emits one all-null row when its pattern has no match.

A `WHERE` syntactically attached to an optional match filters its candidate
pattern inside the left join. It does not become an outer filter and cannot
erase the incoming row. Chained optional clauses can consume nullable entity
bindings, and properties plus `elementId`, `labels`, and `type` evaluate to
null for a null-extended entity. Fixed-pattern direction, type, property,
relationship-uniqueness, and variable-kind rules are otherwise the same as
mandatory `MATCH`. Named paths in `OPTIONAL MATCH`, quantified optional
patterns, path selectors, match modes, and hints remain rejected.

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
| path | not accepted as a parameter or property | `CypherValue::Path(PathValue)` |

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
the public entity fields. A named path may be returned by a supported mandatory
read, but path-fed writes and post-mutation path results remain unsupported.

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

## Paths, direction, and variable-length traversal

Every stored edge is directed from `from_node` to `to_node`. A right-arrow
pattern matches that direction, a left-arrow pattern matches the reverse
spelling, and an undirected `MATCH` pattern accepts either orientation. An
undirected `CREATE` is unsupported; left- and right-arrow creates are stored in
the direction indicated by the arrow.

Within one `MATCH` clause, one stored relationship cannot occupy two
relationship positions across any of its constituent path patterns, including
comma-separated parts. Nodes may repeat, so cycles and self-relationships are
allowed as long as every relationship position uses a distinct relationship.
A later `MATCH` clause starts an independent relationship-uniqueness scope.

Mandatory read-only `MATCH` supports anonymous legacy variable-length
forms. A bare `*` or `*..` means one or more hops. `*n` selects exactly `n`
hops, `*min..max` selects every inclusive depth in that range, `*min..` omits
the upper bound, and `*..max` defaults its omitted lower bound to one. Bounds
are non-negative integer literals. Zero-hop identity is available only through
an explicit zero such as `*0`, `*0..2`, or `*0..`; the endpoint is then the
starting node and must still satisfy its node label, property, and binding
constraints. A finite range whose lower bound exceeds its upper bound is valid
and emits no rows.

Variable-length traversal supports right-arrow, left-arrow, and undirected
spelling. It may omit the relationship type or specify one type, and an inline
property map must match every traversed relationship. The zero-hop candidate
traverses no relationship, so relationship type and property conditions are
vacuously satisfied at depth zero.

An explicit upper bound through 32 uses a recursive SQLite CTE carrying the
start node, current endpoint, depth, and ordered node and edge IDs for each
candidate. An explicit upper bound above 32 or any omitted upper bound uses
Rust to enumerate the same relationship-unique trails, materializes their
ordered identities as a JSON relation, and feeds that relation back to SQLite
for the surrounding joins and row pipeline. In both routes, a node may be
revisited through cycles and self-loops, but a stored edge cannot be reused
within one complete path. The uniqueness rule also spans fixed and
variable-length segments in the same path. Each distinct valid edge sequence
remains a separate result row, so diamonds and parallel edges can produce
duplicate endpoint projections. The current naive physical plan seeds every
stored node before applying selective endpoint constraints; start-domain
pushdown remains future optimization work.

An omitted upper bound has no hidden hop cutoff: relationship uniqueness limits
one trail to at most the number of eligible stored edges, although the number of
distinct trails can still grow rapidly. Deep and open-ended expansions share a
100,000-trail-state budget across one top-level query, including `UNION`
branches and multiple hybrid expansions. Exhausting it fails the query with
`Error::ResourceLimit` rather than returning a truncated or partial result.
The counter is a fixed explored-work guard, not a memory bound. The current
stage fully materializes eligible graph inputs and its JSON trail relation;
selective seeding and payload accounting remain future hardening.

Mandatory read-only `MATCH` may bind a fixed, variable-length, or mixed pattern
with `p = (...)`. Its public `PathValue` contains nodes and relationships in
traversal order and always has exactly one more node than relationship. A
zero-hop value contains its one endpoint node and no relationships. Each
`RelationshipValue` still reports the stored start and end IDs when an incoming
or undirected match traverses it backwards; its position in the path records
the traversal orientation.

A named path can be returned directly, carried through a direct `WITH` alias,
and compared for `RETURN DISTINCT` or distinct `UNION` by its ordered node and
relationship identities. `length(path)` counts relationships and is a
SQL-backed integer in filters, comparisons, arithmetic, ordering, implicit
grouping, aggregate arguments, and direct or aliased projections.
`nodes(path)` returns traversal-ordered nodes; `relationships(path)` returns
traversal-ordered relationships while each relationship retains its stored
start/end direction. Both are public `CypherValue::List` values containing
typed graph entities. They may be returned directly or carried through
computed `WITH` aliases, and their compact identities participate in semantic
distinctness, pagination, and unions before hydration.

Each function requires exactly one in-scope path argument; bad arity, unknown
variables, and wrong variable kinds fail during planning. Property access and
selectors on a path, plus raw path use as a scalar, aggregate, grouping, or
ordering expression, remain planning-time rejections. The typed lists from
`nodes` and `relationships` retain the expression-context restrictions above.
A fresh legacy relationship variable in `[rels:TYPE*min..]` is a
relationship-list binding, not a stored relationship or a first-class path. It
returns a public `CypherValue::List` of typed relationships in traversal order;
zero hops return `[]`, and each member retains its stored endpoint direction.
The list can be returned directly, carried through `WITH`, shaped by semantic
distinctness and pagination, combined through unions, and bound alongside the
named path for the same pattern.

Exact matching against a pre-bound relationship sequence and use in the scalar,
aggregate, grouping, ordering, or `UNWIND` contexts above remain unsupported.
Named paths or relationship-list bindings in `OPTIONAL MATCH`, postfix graph
quantifiers, shortest-path forms, and writes fed by variable-length traversal,
a named path, or a relationship list also remain unsupported.

SQLite carries a compact encoding of the ordered node and relationship IDs.
Rust drains each branch cursor into pending typed rows before another branch or
hydration query runs. Branch-local semantic `RETURN DISTINCT` and deferred
pagination run on those pending cells, followed by left-to-right
`UNION`/`UNION ALL` operations. Only IDs required by surviving whole paths or
graph lists are collected and deduplicated query-wide. A
nodes-only result does not load relationships, and a relationships-only result
does not load nodes. Directly projected graph entities seed the hydration maps
from their already-materialized snapshots; Rust then loads every remaining ID
in batches, using at most one node query and one edge query before constructing
public paths and typed lists in traversal order.

The complete read—hybrid trail-input queries, every branch SQL fragment, and
both possible hydration queries—shares one deferred SQLite read snapshot.
Concurrent external-writer semantics beyond that snapshot remain a non-goal.

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
are writes in a query containing `WITH`, fed by variable-length traversal, or
fed by any named path.

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

Supported deep or open-ended traversal can instead fail during execution with
`Error::ResourceLimit` when the query-wide 100,000-trail-state budget is
exhausted. This is an explicit resource failure; Crabnet does not return a
truncated result.

Semantics for unsupported constructs—including path selectors and other path
expressions beyond the three path functions above, exact matching against
pre-bound relationship-list values, optional-bound paths or relationship lists,
postfix graph quantifiers, shortest-path forms, quantified optional matching,
`MERGE`, label mutation, and computed mutation right-hand sides—remain
unresolved until those constructs are designed and implemented.
