# Current Cypher semantics

This document records Crabnet's current, implemented semantics. It is a
contract for the supported query slice, not a statement of full openCypher
conformance or a roadmap for unsupported features.

## Rows, multiplicity, and ordering

Query results use bag semantics. Matching the same bindings through multiple
input rows produces multiple result rows; Crabnet does not implicitly
deduplicate them. Comma-separated, disconnected patterns therefore produce a
Cartesian product; `WITH DISTINCT` and `RETURN DISTINCT` explicitly remove
duplicates at their respective projection boundaries.

Read-only `RETURN DISTINCT` deduplicates the complete projected pending row
after SQL cursor materialization and before graph-value hydration. Scalar,
ordinary collection, and directly projected entity cells already have typed
`CypherValue` representations. Whole paths, bare path-function entity lists,
and bounded relationship-list pattern bindings remain compact ordered identities.
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

For `RETURN DISTINCT`, `SKIP`/`OFFSET` and `LIMIT` are applied after semantic
deduplication and before graph-value hydration. `ORDER BY` may refer only to a
projected expression or its alias; ordering by an unprojected expression is a
semantic error. A raw path or path-derived typed list cannot itself be an
ordering key yet; `length(path)` is an ordinary integer ordering expression.

Without `ORDER BY`, result row order is unspecified. With `ORDER BY`, rows are
ordered by the supported expressions and directions in that clause, but the
relative order of equal sort keys is unspecified. Sorting follows Cypher's
global orderability, so values of different kinds sort against each other:
maps, then lists, strings, booleans, numbers, and null last. Integers and
floats interleave by numeric value, `false` sorts before `true`, and lists
sort element by element with a shorter prefix first. Ascending order therefore
places nulls last; descending order reverses the whole order and places nulls
first. `SKIP`/`OFFSET` and `LIMIT` are
applied after ordering when an order is present. The same rules apply to a
`RETURN` after a mutation, which runs as a read over the post-mutation rows.

## `WITH` clause boundaries

`WITH` may begin a query, appear more than once, separate `MATCH` clauses, or
sit between reading and updating clauses. Each `WITH` consumes the current row
bag. Plain `WITH` emits one row for every retained input row; it does not
implicitly deduplicate. A later `MATCH` expands every carried row independently,
preserving duplicate and Cartesian-product multiplicity.

A `WITH` projection can carry whole node, relationship, or path bindings and
typed scalar values. A bare variable keeps its name, while `AS` may give any
of these values a new name. A path may cross directly or feed an aliased
`length(path)`, `nodes(path)`, or `relationships(path)` projection. Length uses
a normal scalar slot. The two entity-list results retain ordered path identity
and element kind across later direct aliases and projections. They support
`UNWIND`, indexing, slicing, comprehensions, quantifiers, `size`, `count`,
`collect`, grouping, and equality/membership/null predicates. Other general
scalar uses and whole-list ordering remain unsupported. After the boundary,
only projected names remain in scope:
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
behavior. `WITH DISTINCT` deduplicates the complete projected row using the
same typed equivalence as `RETURN DISTINCT`. The projection is evaluated once;
aliases in filters and ordering reuse its values, including `rand()` draws.
An attached `WHERE` filters incoming rows before deduplication and retains the
incoming-variable visibility described above. Aggregation, when present, runs
before distinctness. Deduplication precedes pagination and all later clauses,
including writes; writes before the boundary retain their side effects.

Distinctness uses only projected values, ignoring hidden ordering columns.
Paths compare ordered identities independently of segment boundaries, and
path-derived lists compare only their own node or relationship sequence.
`ORDER BY` at a distinct boundary may use projected values and expressions
derived from them, including properties of projected entities, but cannot
use a dropped value or an unprojected aggregate. Scope violations are semantic
errors. These keys and modifiers execute in SQLite over materialized CTEs;
direct path values remain compact identities until final hydration. Nested
graph collections carry materialized entity records.

`WITH DISTINCT *` deduplicates all names in scope. Over an empty scope it keeps
at most one input row; over empty input it produces no rows. Computed `WITH`
expressions without aliases remain unsupported. Aggregating `WITH` projections
follow the rules below. The implemented pipeline cases live in
`tests/with_pipeline.rs`, `tests/with_distinct.rs`, `tests/aggregation.rs`,
`tests/unwind.rs`, and `tests/path_values.rs`.

## Aggregation and grouping

`RETURN` and `WITH` support `count`, `sum`, `avg`, `min`, `max`, and `collect`
both directly and inside scalar expressions. Examples include `count(*) + 1`,
`coalesce(avg(n.score), 0)`, `CASE WHEN count(*) > 0 THEN max(n.score) END`,
`{total: sum(n.score)}`, and `size(collect(DISTINCT n))`.
A projection containing an aggregate is an
aggregating projection. Every non-aggregate, row-dependent projection item is
then an implicit grouping key; constants do not create grouping keys. A global
aggregate over empty input emits one row, while an aggregate with grouping keys
emits no rows for empty input.

Aggregate calls are evaluated over each input group before their surrounding
expressions. Scalar collection results support indexing, slicing, functions,
and use as the input list of comprehensions and quantifiers in the same
projection, for example `[x IN collect(n.score) WHERE x > 0 | x * 2]`.
An aggregate-containing expression may refer outside its aggregate arguments
to constants, parameters, and grouped variables or property accesses. It may
not introduce an ungrouped value or reuse a compound grouping expression:
bind that expression with an earlier `WITH` first. For example,
`WITH n.score % 2 AS parity RETURN parity, parity + count(*)` is valid.
Aggregate expressions and aliases also work in an aggregating projection's
`ORDER BY` and in `WITH ... WHERE`, subject to the usual scope and distinctness
rules. Filtering, ordering, distinctness, and pagination shape the resulting
group rows before later clauses consume them.

Grouping, `count(DISTINCT expression)`, and `collect(DISTINCT expression)`
use typed Cypher equivalence rather
than raw SQLite JSON text. Integers and equal integral floats share a group,
booleans remain distinct from numbers, lists and maps apply the rule
recursively, and whole graph entities group by element ID. Null values form one
group, while `count(expression)` and `count(DISTINCT expression)` omit them.
`count(*)` retains the complete input bag, and `count(entity)` omits a
null-extended entity.

`sum` and `avg` ignore nulls and accept numeric inputs only. `sum` of an empty
numeric bag is integer zero; `avg` of one is null. Mixed integer/float inputs
use SQLite's numeric promotion. A nonnumeric kind known from the query raises
`Error::Semantic` during planning, including unused branches and empty matches.
Any non-null input of another kind at runtime raises `Error::Type`, without
coercing strings or booleans. Parameters, properties, and values selected from
mixed graph collections follow the same rule. Aggregate inputs are checked
before conditional selection of the aggregate result, so an outer `CASE` or
`coalesce` cannot hide an invalid input; lazy branches within the input expression
still skip unselected values. Empty input does not evaluate dynamic arguments,
but missing parameters and static errors are always rejected before execution.
A non-finite numeric `sum` or `avg`
result raises `Error::Argument`, including an indeterminate accumulation that
SQLite reports as null despite non-null numeric input. `min` and `max` ignore
nulls and choose by the same global orderability as `ORDER BY`, so `max` over
numbers and strings is a number and `min` over a list and strings is the list.
`collect` omits nulls, preserves duplicates, and returns an empty list for empty
input. Ordering established by an immediately preceding `WITH ... ORDER BY`
controls collection order.

`collect(n)` and `collect(r)` return lists of typed nodes or relationships.
Entities with identical properties remain distinct when their element IDs
differ. `collect(DISTINCT ...)` omits nulls and keeps the first representative
of each typed value in collection order. Without an upstream `ORDER BY`, that
order is unspecified. Scalar values use recursive typed equality; entities
use identity. These lists survive aliases, wildcard projections, grouping,
`WITH DISTINCT`, `RETURN DISTINCT`, and unions. `size(list)` returns their
length, including directly as `size(collect(n))`. Their ordered entity
identities determine list equality, independently of properties.

Collected entities can feed `UNWIND`, property reads, later matches, and
writes. Property and label changes refresh all retained copies in collected
lists as well as direct entity bindings; deletion retains the same pre-delete
snapshots as direct entity returns. Lists do not become ordinary JSON maps
or storable properties. Indexing, slicing, and list comprehensions preserve
these graph types. Comprehensions can also project scalar properties or maps,
which produce ordinary lists. List quantifiers also accept these typed lists.
Lists and maps may contain entities, paths, and other graph collections.
`count` and `collect` also accept these containers, including entity lists
from an earlier `WITH`. `head`, `last`, `tail`, and `reverse` preserve graph
members. Other general scalar operations on graph collections remain
unsupported; see the nested collection contract below.

Explicit `ALL` on `count`, `sum`, `avg`, `min`, and `max` has the same bag
behavior as an omitted aggregate quantifier. The parser currently rejects
`collect(ALL ...)`; use `collect(...)` for bag collection.
`DISTINCT` on aggregates other than `count` and `collect`, nested aggregate
calls such as `sum(count(*))`, and aggregate-only ordering/filtering without
an aggregating projection are structured planning-time rejections. Aggregate
calls inside comprehension bodies or quantifier predicates are also rejected.
A `RETURN` after a mutation may aggregate over the post-mutation rows.
Aggregates are not allowed in `MATCH` predicates or
directly in `UNWIND`; a `WITH` boundary must materialize them first.

### Path aggregation and grouping

`count(p)`, `collect(p)`, `count(DISTINCT p)`, and `collect(DISTINCT p)` accept
whole paths directly. The same aggregates accept `nodes(p)`,
`relationships(p)`, and legacy relationship-list variables. Paths and these
lists can also be grouping keys in `WITH` and `RETURN`, including aliases
and wildcard projections. This works across fixed, bounded, deep/open-ended,
shortest-path, and all-shortest-path producers.

```cypher
MATCH p = (:Person)-[:KNOWS*1..3]->(:Person)
WITH p, count(*) AS copies
RETURN length(p) AS hops, copies, nodes(p) AS people
```

A path's ordered node and relationship identities determine its group and
distinctness, independently of properties and internal segment boundaries.
A reversed route is distinct when its identity sequence changes; parallel
relationships remain distinct even with identical properties. Path lists compare their
ordered entity identities. An empty relationship list is a non-null value:
zero-hop paths contribute to `count(relationships(p))`, and their empty lists
share one distinct value. Zero-hop paths themselves remain distinct by node
identity. Nullable path values follow the ordinary aggregate rules: nulls
form a group, `count` and `collect` omit them, and an ungrouped empty input
produces zero counts and empty collections. A grouped empty input has no rows.

Collected paths retain their type through aliases, indexing, list functions,
comprehensions, quantifiers, `UNWIND`, conditionals, distinctness, and unions.
They support `length`, `nodes`, and `relationships` after selection. Grouped
paths also support these functions in later clauses and in expressions over
the same group's aggregates. Collected path lists remain nested lists; they
are not flattened. Collection order follows an immediately preceding
`WITH ... ORDER BY`; `collect(DISTINCT ...)` keeps first occurrences.

Aggregate arguments and grouping keys materialize complete paths/lists using
the private tagged transport, including for `count`. Ordinary projections
retain deferred hydration. The existing query resource limits and 100-container
result-depth limit apply. Materialized relationship lists retain their entity
kind but do not become legacy pre-bound relationship-list pattern bindings.
Named-path/relationship-list `OPTIONAL MATCH`, writes fed by paths, whole-path
`DELETE`, numeric/extremum aggregates over paths or graph lists, and ordering
by these values remain unsupported. `length(path)` remains an ordinary integer
usable in numeric aggregates and ordering.

## `UNWIND`

`UNWIND` expands a list into one output row per member and retains duplicates
and explicit null members. It composes with prior `MATCH`, `WITH`, aggregation,
and `UNWIND` stages while carrying the current entity and scalar scope. Empty
lists, null, and dynamic non-list values produce zero rows; Crabnet does not
coerce a scalar or map to a singleton list. The new alias must not replace a
name already in scope. Unless a later clause orders the result, row order after
`UNWIND` is unspecified. A collected node or relationship list binds each
member as a graph entity with its original kind and identity, so the alias
supports graph functions, matches, and mutations. For example:

```cypher
MATCH (person:Person)
WITH collect(DISTINCT person) AS people
UNWIND people AS person
SET person.active = true
RETURN person
```

The typed graph lists produced by `nodes(path)` and `relationships(path)`,
including their `WITH` aliases and legacy relationship-list bindings, also
expand into entity bindings. Those bindings support property reads, entity
functions, later matches, and collection. This works for fixed, bounded,
deep/open-ended, and shortest paths. A shared segment endpoint occurs once in
the node list; cycles and self-loops still repeat a node for every visit.
Zero-hop paths expand to one node and no relationships. The resulting rows
preserve input-bag multiplicity, and relationship values retain their stored
endpoint direction. Path-fed writes and named paths in `OPTIONAL MATCH`
remain unsupported.

```cypher
MATCH route = (:Person {name: 'Ada'})-[:KNOWS*1..3]->()
UNWIND nodes(route) AS person
RETURN DISTINCT person
```

## Query unions

`UNION ALL` appends the pending row bags produced by independently planned
read-only branches. Bare `UNION` and explicit `UNION DISTINCT` semantically
deduplicate the accumulated pending rows using the same recursive typed
equivalence as `RETURN DISTINCT`, including ordered node/relationship identity
for paths and the relevant ordered entity identity for path-derived lists.
One query uses one duplicate policy: mixing `UNION` and `UNION ALL` is a
semantic error, as Cypher requires. Branch-local
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
non-finite floats are rejected. Floats keep their exact `f64` value across
every transport: SQLite's own JSON rendering of a REAL stops at fifteen
significant digits, so reals leave SQL through a registered shortest
round-trip formatter, and result decoding parses JSON floats exactly. Float
zero is canonicalized to positive zero at the public `CypherValue` boundary so
a value returned through a whole entity, direct property projection, or
hydrated path has the same representation.

A standalone literal or named JSON value may contain at most 100 nested
list/map containers. When an object supplies a complete stored property map,
its outer `props` envelope is not counted; each top-level property value
receives the same 100-container allowance whether it came from a parameter
or literal. Parentheses and unary signs do not consume that collection-depth
allowance. Output, scalar, literal/property-map, and pagination lowering
separately cap recursive expression syntax at 128 AST steps to bound planner
stack use.

Resolution checks JSON depth iteratively before cloning parameter data and, for
writes, before opening the transaction. A deeper input fails with
`Error::Semantic` instead of committing JSON that cannot later be decoded as a
whole entity or hydrated path.

A property whose value is null does not exist. `CREATE ({k: null})`,
`SET n = {k: null}`, `SET n += {k: null}`, and `SET n.k = null` all leave `k`
absent, and a returned whole entity has no such key. Reading an absent
property produces `CypherValue::Null`, and `IS NULL` is true for it. Nested
nulls inside list and map values are stored as given.

Early schema-1/2 writers stored explicit top-level null entries. Upgrading those
files preserves the entries, and whole-entity maps can still expose them until
they are removed. Property access, `keys()`, `properties()`, and uniqueness
constraints treat them as absent. See the [compatibility policy](compatibility.md)
and retained fixtures for this historical-file boundary.

`NodeValue` contains an opaque `element_id`, a `labels` vector, and a recursive
`properties` map. Nodes carry zero or more distinct labels in insertion order.

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
Boolean contexts (`WHERE`, `AND`, `OR`, `XOR`, `NOT`, generic `CASE` conditions, and
quantifier and comprehension predicates) accept only booleans and null; a
missing property is null. Any other kind is a type error, never SQLite
truthiness. `AND`, `OR`, and `NOT` propagate null with three-valued logic.
`XOR` is null when either operand is null and otherwise true when they differ.
`AND` and `OR` may stop at an operand that decides the result, so
`false AND 1` can be false rather than an error.

Kind errors are reported as early as the query text allows. When a literal,
or a `WITH` or `UNWIND` variable bound to one, has the wrong kind (`NOT 1`,
`1 IN 'a'`, `WITH 5 AS n RETURN n.key`, `none(x IN ['a'] WHERE x % 2 = 0)`),
planning fails with `Error::Semantic`. When the kind is only known at runtime,
as for parameters, properties, and computed values, execution fails with
`Error::Type`.

Comparisons chain: `1 < n.num < 3` means `1 < n.num AND n.num < 3`, and any
run of `=`, `<>`, `<`, `<=`, `>`, `>=` compares each adjacent pair. Parentheses
end a chain, so `(1 < n.num) < 3` compares a boolean with an integer and is
null.

Equality uses JSON type information to avoid SQLite's broad coercions:

- integers and floats are compatible numeric types, so values such as `1` and
  `1.0` compare equal;
- booleans compare only with booleans and are not equal to numeric `0` or `1`;
- strings compare only with strings;
- lists compare only with lists, and maps compare only with maps;
- values of different non-numeric JSON kinds are not equal.

Lists and maps compare element by element under these same rules, so
`[1] = [1.0]` and `{x: 1} = {x: 1.0}` are true and map key order never
matters. Lists of different lengths, and maps with different key sets, are
not equal. Otherwise any element pair that is definitely unequal makes the
result false, and failing that, any element pair involving null makes it null:
`[1, null] = [2, null]` is false, while `[null] = [null]` and
`{x: null} = {x: null}` are null. The finite predicate oracle in
`tests/predicate_truth_tables.rs` locks this contract.

Ordering comparisons are deliberately narrower than SQLite's storage-class
ordering. Integers and floats are mutually comparable as numbers; strings are
comparable only with strings; and booleans are comparable only with booleans.
Lists compare lexicographically: the first element pair that differs decides,
a list that is a prefix of the other is smaller, and the result is null as
soon as an element pair before that point is not comparable, so
`[1, null] > [1]` is true but `[1, 2] > [1, null]` is null. Maps are not
comparable. A null or missing operand, or any incompatible or unsupported pair
of value kinds, produces null. Crabnet does not coerce text to numbers or booleans to
numbers for either equality or ordering.

Node-property equalities may use the property index to obtain candidates
before the exact typed comparison. In a `MERGE` matcher, small deterministic
computed keys, including `row.id`, nested map/list access, arithmetic, scalar collections, `CASE`,
`coalesce`, and selected numeric/conversion/string-normalization functions,
can use this path without a separate `WITH` alias. The current planner limits
these operands to 32 expression nodes and requires their entity references
to be bound before the matcher. It excludes unmaterialized `rand()`,
aggregates, traversal and collection iteration. Larger or other unsupported
probe shapes still execute through
their existing plans; this is an optimization boundary, not a new restriction
on expressions. A previously projected random scalar is safe to reuse.
List/map keys probe by property presence, then compare recursively; scalar
keys probe by value, with the exact comparison still enforcing numeric and
boolean distinctions. Probes never replace the remaining pattern constraints
or deduplicate matches.

Read filters retain their existing probe forms: literal/parameter values,
scalar aliases, and direct properties of other entities. A computed read key
can be projected with `WITH` to make it indexable. This boundary preserves
error timing: a read with no candidate nodes need not evaluate a fallible map
access, whereas MERGE needs the key to create an unmatched pattern.

## Pattern predicates and entity identity

A fixed-length pattern in a boolean position, such as `WHERE (n)-[:KNOWS]->()`,
`NOT (a)-->(c)`, or `RETURN (n)-->() AS connected`, is true when the pattern
matches at least once from the row's bound entities. Every variable in the
pattern must already be bound; the pattern's anonymous nodes and
relationships are scoped to the predicate and are never part of the row.
Labels, relationship types, inline property maps, and several pattern parts
are allowed, and relationships within one predicate are distinct from each
other as in `MATCH`. The predicate compiles to a correlated `EXISTS`
subquery, so it works in `WHERE`, `WITH ... WHERE`, projections, list
comprehensions, list quantifiers, and the filters of updating statements,
where it sees the graph before the statement's writes. `exists(pattern)` is the same test and
`exists(n.prop)` is `n.prop IS NOT NULL`. A pattern without a relationship, a
pattern that introduces a new variable, a path binding, a variable-length
relationship, and relationship-type alternatives are rejected.

Nodes and relationships compare by identity: `a = b` is true when both name
the same stored entity and `a <> b` when they differ, null when either side
is null, and `r IS NULL` tests an entity the same way. An entity compared
with a non-entity value is not equal, including its own `elementId()` string.
Node and relationship identities remain distinct even when their IDs match.
Entities can be collected and then expanded with `UNWIND`. They still cannot
be ordered directly. Literal lists and maps may contain graph entities as
described below.

### Path and graph-list predicates

Direct paths, `nodes(p)`, `relationships(p)`, legacy relationship-list
bindings, and collected entity lists support `=`, `<>`, `IN`, `IS NULL`,
and `IS NOT NULL`. Aliases and selected or nested values use the same rules.
For example, `p = q` compares two paths, `p IN routes` searches a path list,
and `n IN nodes(p)` tests whether that node occurs along the route. These
predicates work across fixed, bounded, deep/open-ended, and shortest-path
transports and compose with filters, projections, conditionals, quantifiers,
aggregation, and grouping. Existing clause restrictions still apply; a
shortest-path predicate belongs after a `WITH` boundary.

Paths compare their ordered node and relationship identities, independent of
properties and internal segment boundaries. Reversing a route changes equality
only if that identity sequence changes: opposite traversals of the same
self-loop are equal. Parallel relationships remain distinct. Lists compare
ordered members by typed identity, retaining duplicates; node and relationship
identities are different kinds. These are the same identity rules used for
grouping and distinctness, with the ordinary three-valued equality rules for
null members. Chained equality comparisons compare adjacent pairs.

A null operand makes equality or inequality null. `IS NULL` tests the value
itself, so a zero-hop path and its empty relationship list are non-null.
`IN` requires a list on the right: an empty list returns false, even for a
null left operand; a definite match returns true; otherwise an unknown member
comparison makes the result null, and all definite non-matches return false.
A null right operand returns null; a non-null non-list raises `Error::Type`.
Ordinary JSON parameters cannot impersonate graph entities or paths.

These predicates materialize complete paths or graph lists internally, even
when the output is only a boolean. Ordinary direct projections retain deferred
hydration. Query resource limits apply. Whole graph-value ordering and writes
fed by named paths or traversal remain unsupported.

## Nested graph collections

Lists and maps may mix nodes, relationships, paths, ordinary values, nulls,
and other lists or maps. For example:

```cypher
MATCH (person:Person)-[edge:KNOWS]->(friend)
WITH person, collect({friend: friend, edge: edge}) AS connections
RETURN {person: person, connections: connections} AS result
```

These values retain their public `CypherValue` kinds across projections,
`WITH`, indexing, slicing, `head`/`last`/`tail`/`reverse`, comprehensions,
quantifiers, and `UNWIND`. Maps support literal and dynamic keys; absent keys and
out-of-range indexes return null. Lists keep order, duplicates, and null
positions. Homogeneous node or relationship members can become entity
bindings for later matches and supported writes. A heterogeneous member can
be projected or read with property and graph functions; it cannot become a
pattern or mutation target until its entity kind is statically known. Rebind
an entity with `WITH box.node AS node` before using it as a write target.

`count`, `collect`, and their `DISTINCT` forms accept nested collections.
Grouping, `WITH DISTINCT`, result distinctness, and unions compare containers
recursively, graph entities by kind and identity, and paths by their ordered
identities. Equality and `IN` over nested collections preserve three-valued
null semantics; grouping treats corresponding nulls as equivalent. Ordinary
JSON maps or lists with entity-like fields remain ordinary values.

Property and label updates refresh every retained entity copy in a nested
collection; deleted entities preserve the same snapshots as direct bindings.
Graph values cannot be stored as properties. Known scalar fields and scalar
projections may be stored. Unknown heterogeneous selections remain graph
values until a scalar operation validates them. Wrong-kind dynamic graph
access raises `Error::Type`; a failure after writes rolls back the statement.
Eager reads and read cursors share the same value and error behavior.

Collections allow at most 100 nested list/map containers. Literal depth is
checked during planning; excess depth assembled across clauses is checked at
runtime with rollback. Graph entities and paths count as leaves, with their
properties governed by the separate property-value depth limit. Paths and
path-function lists are materialized inside nested collections, so a small
selection can still construct the complete source. Existing path-fed write
restrictions apply.

`+` joins graph lists and mixed nested lists, or appends/prepends an individual
node, relationship, path, map, or scalar when the other operand is a list:

```cypher
MATCH (person:Person)-[:KNOWS]->(friend)
WITH person, collect(friend) AS friends
RETURN [person] + friends AS people
```

Only the outer list level is joined; `[node] + [[edge]]` retains the nested
relationship list. Order, duplicates, and null members are preserved. A null
operand makes the complete result null, so `[node] + null` differs from
`[node] + [null]`. Empty lists are neutral. This applies to collected entities,
path-function lists, and legacy relationship-list bindings across all traversal
transports. The result supports the same selection, iteration, aggregation,
identity, and mutation snapshot behavior described above.

Known homogeneous node/relationship members retain their kind for later
matches and writes; unknown parameters or heterogeneous selections keep
runtime tags and require a statically known kind before binding a graph
pattern. Known positions in literal concatenations retain their individual
types. Adding graph values without a list operand is a semantic error when
known during planning, or `Error::Type` when discovered at runtime. Scalar
members selected from mixed collections retain ordinary numeric, string, and
list `+` behavior. Concatenation materializes its inputs and applies the same
100-container result-depth guard, including rollback on failure.

`head`, `last`, `tail`, and `reverse` accept collected entities, path-function
lists, legacy relationship lists, and mixed nested lists. They preserve graph
identities, duplicates, null members, and nested containers. `reverse` changes
only the outer order; it does not reverse a nested path's traversal direction.
`head` and `last` of an empty list are null; `tail` and `reverse` of an empty
list are empty. All four return null for a null list. Known tuple positions
remain known after `tail` and `reverse`, so selecting a node or relationship
can feed later matches and supported writes:

```cypher
MATCH (person:Person)
WITH person ORDER BY person.name
WITH reverse(collect(person)) AS people
DETACH DELETE head(people)
RETURN tail(people)
```

These functions compose with aggregation, comprehensions, quantifiers,
grouping/distinctness, and unions, and retain mutation refresh and deletion
snapshots. Graph maps, individual entities, and whole paths are not list
arguments; a list containing a path is valid. A dynamically selected list or
string retains ordinary `reverse` behavior, including Unicode string reversal;
an incompatible dynamically selected graph value raises `Error::Type`.
Existing indexing/slicing rules govern `head`, `last`, and `tail`. The same
depth limits, materialization costs, and path-fed write restrictions apply.

`coalesce` and both forms of `CASE` also preserve graph values; see
[conditional graph values](#conditional-graph-values). Other general scalar
functions, ordering, and numeric/extremum aggregates over graph collections
remain unsupported. Bare paths and path lists support `count`, `collect`, and
grouping as described in [path aggregation](#path-aggregation-and-grouping).
Node and relationship expressions, including
dynamically selected members, may be used directly as `DELETE` targets. A literal
relationship list does not become a legacy pre-bound relationship-list
pattern. These boundaries are independent of the typed collection transport.

## Membership and string predicates

`IN` uses the same typed equality rules for each list member. It does not
coerce element kinds or expand duplicate members into duplicate result rows.
An empty right-hand list produces false even when the left side is null. For a
nonempty list, a matching member produces true; otherwise a null/missing left
operand or a member whose comparison is null (a null member, or a list member
that equals the left side except for nulls) makes the result null, and a
definite miss produces false. A null or missing right-hand operand produces
null, and any other non-list right-hand operand is a type error.

`STARTS WITH`, `CONTAINS`, and `ENDS WITH` require two strings and return a
boolean. They are case-sensitive, treat `%` and `_` literally, and support
empty strings, Unicode, and embedded NUL characters. Text after a NUL remains
part of the value, including when matching a suffix. If either operand is null, missing, or not a string,
the result is null; there is no string coercion. These predicates and `IN` may
appear in `WHERE` or as computed read projections, where their result is a
typed boolean or null cell.

## List quantifiers

`all`, `any`, `none`, and `single(x IN list WHERE predicate)` evaluate the
predicate once per element and combine the outcomes three-valued. Each
outcome is true, false, or null; a null outcome could be either truth value, so
it only matters when the definite outcomes leave the answer open:

- `all` is false if any outcome is false, otherwise null if any is null,
  otherwise true (so it is true on an empty list);
- `any` is true if any outcome is true, otherwise null if any is null,
  otherwise false;
- `none` is the negation of `any`;
- `single` is false if more than one outcome is true, null if any outcome is
  null, true if exactly one is true, and false otherwise.

The bound variable shadows any outer binding of the same name inside the
predicate only; the list expression is evaluated in the enclosing scope, so
`all(x IN x WHERE ...)` iterates the outer `x`. Quantifiers nest, may mention
outer scalars, properties, and parameters, and may appear anywhere a boolean
expression may: `WHERE`, projections, `ORDER BY`, and aggregate arguments.
Aggregates are rejected inside the predicate. A null operand produces null;
a non-null non-list operand is a type error. Predicates that
apply an operator to an element of the wrong kind follow the operator's type
checks; non-boolean predicate results are type errors.

Quantifiers also accept collected node/relationship lists, `nodes(path)`,
`relationships(path)`, legacy relationship-list bindings, and their aliases,
slices, and typed comprehension results. Members retain property access,
entity functions, identity comparisons, label/type tests, and supported
fixed-length pattern predicates. Local names shadow outer entity bindings in
patterns too. Duplicate members are evaluated separately: two occurrences of
the same matching entity make `single` false. Null members and missing
properties follow the truth table above; a null whole list returns null.

An aggregate can supply the input directly, as in
`any(n IN collect(n) WHERE n.age >= 18)`. For pattern predicates over an
aggregate-produced list, bind the collection in an earlier `WITH`; the
existing aggregate grouping checks reject pattern predicates in mixed
aggregate expressions. Boolean/null results may be stored as properties and
composed inside scalar lists or maps. Retained entities observe property and
label updates and preserve deletion snapshots. Whole entities and paths are
rejected as list operands. Nested graph lists are accepted with the same
scoping and truth rules. The restrictions on path-fed writes still apply;
path-list inputs may materialize the full source entity list internally.

## Map key access

`value.key` on a non-entity value reads a key of a map: a projected or
parameter map, a nested property such as `n.meta.tag`, a parenthesized
expression, or a quantified list element. A missing key, a key holding null,
and a null map all read as null. Reading a key of any other kind of value is a
type error. Keys are case-sensitive and
may be escaped identifiers. Graph entities keep their own property semantics,
and property access on a path variable is a semantic error.

## List comprehensions, `size()`, and computed collections

`[x IN list WHERE predicate | projection]` evaluates the predicate once per
element, keeps the elements whose outcome is true (a null outcome drops the
element, as in `WHERE`), and collects their projections in list order. Either
part may be omitted: without `WHERE` every element is kept, and without a
projection the element itself is collected. The bound variable shadows outer
bindings inside the predicate and projection only; the list is evaluated in
the enclosing scope. Comprehensions nest, may mention outer scalars,
properties, and parameters. Scalar results compose with `size`, quantifiers,
`IN`, `UNWIND`, and equality. Aggregates are rejected inside either part, but
may supply the input list, as in `[n IN collect(n) | n.name]`. A null operand
produces null; a non-null non-list operand is a type error, even when the
predicate would discard every element.

Comprehensions also accept collected node/relationship lists, `nodes(path)`,
`relationships(path)`, their aliases and slices, and legacy relationship-list
bindings. Members retain graph behavior for property access, entity functions,
identity comparisons, label/type tests, and supported fixed-length pattern
predicates. Local variables shadow outer bindings in patterns as well as in
ordinary expressions. `[n IN people WHERE n.active]` returns a typed node list;
`[n IN people | n.name]` returns an ordinary property-value list. A projection
can return an outer entity or an indexed entity, including null. Typed results
preserve order, duplicates, and null positions across `WITH`, `UNWIND`, indexing,
slicing, grouping, distinctness, unions, and supported writes. Null members
count toward `size`, unlike `collect`, which omits null input values. Mutation
refreshes and deletion snapshots also apply to entities in these lists.

Comprehensions may return nested lists or maps containing entities, paths,
and other graph collections, or iterate the result of another comprehension.
Scalar projections can be stored as properties; graph-valued results cannot.
Path-derived inputs retain the existing restrictions on path-fed writes and
may materialize the full source entity list internally.

`size(list)` is the element count and `size(string)` the character count.
String counts use Unicode scalar values, including embedded NUL; combining
marks count separately. For example, the three-character string `a\0b` has
size 3 (here `\0` denotes a NUL character, not a Cypher escape spelling).
`size(null)` is null; other non-null kinds are type errors.
`size` of a statically known path or graph entity is a semantic error;
paths use `length`.

For `size`, `head`, `last`, `tail`, `reverse`, comprehensions, and quantifiers,
known invalid input kinds fail before execution with `Error::Semantic`.
Invalid parameters, properties, or dynamically selected mixed graph values
raise `Error::Type` when evaluated. Unused runtime branches in `CASE` and
`coalesce` remain lazy; static errors and missing parameters still fail preflight.
Errors end a cursor and release its snapshot, and errors after writes roll back
the statement or explicit transaction. Already yielded transaction rows remain
provisional.

`size(nodes(p))`, `size(relationships(p))`, and `size(rels)` for a legacy
relationship-list binding work directly and through aliases across fixed,
bounded, deep/open-ended, and shortest-path traversal. They count list
positions: repeated nodes in cycles/self-loops count again, while shared
segment junctions occur once. A zero-hop path has one node and zero
relationships. Null lists return null and empty lists return zero.
These integer results compose with filters, arithmetic, ordering, grouping,
and aggregate arguments under the existing clause and aggregate-scope rules.

When a compact path identity is available, SQL derives relationship-list size
from its hop count and node-list size from that count plus one. This avoids
constructing or hydrating entity records solely for `size`. Traversal still
runs as required to find the paths. Grouped, selected, sliced, conditional,
and other materialized lists use their actual contents, preserving null
positions and changes to list length. Path-fed write restrictions remain.

A list or map literal whose elements are all literals or parameters is bound
as a single value. A literal with a computed element, such as `[n.a, n.b + 1]`
or `{key: n.a}`, is built per row instead; null elements are kept, including
null map values, which differs from stored properties, where null entries are
dropped. Both forms compare, group, and order identically.

## Numeric arithmetic and computed projections

Read predicates and projections support binary `+`, `-`, `*`, `/`, and `%`,
plus unary `+` and `-`, with the parser's normal precedence and left
associativity. Integer-only operations retain integer results, including
truncating integer division, while an operation with a float operand produces
a float. A null or missing operand produces null. A boolean, string, list, or
map operand of `-`, `*`, `/`, `%`, or a unary sign is a type error; Crabnet
never coerces it to a number.

Evaluated division or remainder by zero raises `Error::Argument`, for integer
and float operands (including negative float zero). Finite float operands whose
arithmetic result overflows to infinity also raise `Error::Argument`. Invalid
math domains such as `sqrt(-1)`, `log(0)`, and `asin(2)` follow the same rule.
NaN and infinity are outside the supported value domain: they cannot become
comparison results, collection members, or stored properties. This deliberately
differs from Cypher's NaN comparison behavior; the relevant TCK scenarios remain
explicit exclusions. Earlier revisions returned null for division/remainder by
zero and non-finite math-function results; callers relying on that behavior must
guard the operation with `CASE`.

Null operands still propagate (`null / 0` is null), and finite underflow to zero
is accepted. Only evaluated expressions can raise these runtime errors:
unselected `CASE` alternatives and unused `coalesce` alternatives do not run.
Floating remainder corresponds to a quotient truncated toward zero and has the
dividend's sign. It does not cast the quotient to an integer, so large finite
ratios remain valid.
These rules apply to eager reads, incremental and buffered cursors, and writes.
A failure rolls back the statement, or aborts and rolls back the entire explicit
transaction. Rows already yielded by a transaction cursor remain provisional.

`+` is also concatenation: two strings join, two lists join, and a list beside
a non-list value appends or prepends that value as one element. A null on
either side, or any other pairing such as a string with a number, produces
null; Cypher raises a type error for the latter. Graph values also participate
in list concatenation, as described under [nested graph collections](#nested-graph-collections);
graph values without a list operand are rejected.

## List indexing, slicing, and `range()`

`list[i]` reads the element at integer index `i`, counting from the end when
negative, and `map[k]` reads the string key `k`. `entity[k]` reads the property
named by a runtime string. An out-of-range index, a missing key, or a null base
or index reads as null. A non-integer list index, a non-string map key, or a
base that is neither list nor map is a type error. A graph entity indexed by
a non-string property key yields null. Indexing a whole path is a semantic
error; index `nodes(path)` or `relationships(path)` instead.

`list[lower..upper]` copies the elements from `lower` (inclusive, default 0)
to `upper` (exclusive, default the length). Negative bounds count from the
end and both bounds are clamped, so an inverted or exceeding range yields a
shorter list rather than an error. A null bound, a non-integer bound, or a
non-list base produces null.

These bounds rules also apply to `collect(n)`, `collect(r)`, `nodes(path)`,
`relationships(path)`, their aliases, and legacy relationship-list bindings.
An index returns a typed node or relationship (or null); a slice returns a
list of the same entity kind (or null). Order, repeated identities, labels,
properties, relationship types, and stored endpoint direction are preserved.
Shared path-segment endpoints appear once, while cycles and self-loops retain
each visit. For example:

```cypher
MATCH (person:Person)
WITH person ORDER BY person.name
WITH collect(person) AS people
RETURN people[0].name AS first, people[-2..] AS lastTwo
```

Indexed entities support property access, `elementId`, `labels`, `type`,
`keys`, `properties`, label/type predicates, null tests, identity comparisons,
and `count`/`collect`. `WITH list[i] AS entity` binds an entity for later
matches and, for collected entities, writes. Slices retain their entity kind
through aliases, `UNWIND`, grouping, `DISTINCT`, unions, and write boundaries.
Null slices stay null rather than becoming empty lists. Entities can be nested
in result lists/maps, but cannot be stored as properties. Existing restrictions
on path-fed writes and optional named paths still apply.

For path lists, indexed/sliced expressions materialize ordered entity records
inside SQLite; bare path-list outputs retain batch hydration. An index or
small slice can therefore build the entire source list. Eager reads and read
cursors share these semantics, including the existing cursor buffering rules.

`range(start, end)` and `range(start, end, step)` build the inclusive integer
sequence; a step pointing away from `end` gives an empty list and a null
argument gives null. A non-integer argument or a zero step raises
`Error::Argument` at runtime.

Exponentiation (`^`) and concatenation (`||`) remain explicit planning-time
rejections.

## `CASE` expressions

The simple form `CASE operand WHEN a, b THEN x ... ELSE y END` returns the
result of the first alternative whose `WHEN` value equals the operand under
Cypher equality: integers and floats compare numerically, other kinds must
match exactly, and a null operand or `WHEN` value never matches. The generic
form `CASE WHEN predicate THEN x ... END` selects the first alternative whose
predicate is true; a null predicate is not true, and a non-boolean one is a
type error. Without a
matching alternative the result is the `ELSE` value, or null when there is
none. Alternatives may produce different types; the chosen alternative
decides the type, and the result feeds arithmetic, comparisons, predicates,
`size()`, and aggregates like any other value. In an aggregating `RETURN` or
`WITH`, `CASE` may contain aggregate calls in its operand, conditions, and
results. Those aggregates run over the group before branch selection.

### Conditional graph values

`coalesce` and both forms of `CASE` can select nodes, relationships, paths,
entity lists, and mixed or nested graph collections. The selected value
retains graph identity, list order, duplicate and null members, and the usual
mutation refresh and deletion snapshots. Ordinary parameter maps and lists
remain ordinary values, even if they resemble internal entity records.

If all non-null result shapes agree, the result retains that shape. Node and
relationship selections can therefore feed a later match, property
`SET`/`REMOVE`, or `DELETE`, including direct expression targets:

```cypher
MATCH (person:Person)
OPTIONAL MATCH (person)-[:KNOWS]->(friend)
WITH coalesce(friend, person) AS chosen
RETURN chosen.name
```

Different result shapes retain their runtime tags. They support dynamic
property access, collection selection, and other operations that accept mixed
graph values, but cannot become a statically typed pattern binding. A null
literal or omitted `ELSE` does not erase an otherwise known shape; an unknown
parameter or property may have a different type and is treated conservatively.
Selected paths support `length`, `nodes`, and `relationships`. Conditional
results compose with collection functions, comprehensions, quantifiers,
aggregation, grouping, distinctness, and unions. Existing depth limits,
materialization costs, and restrictions on writes fed by paths still apply.

Simple `CASE` selectors accept graph values and nested collections. Nodes
and relationships compare by typed identity, paths by their graph identity
sequence, and lists/maps recursively; a null comparison does not match.
Generic `CASE` conditions retain the boolean/null predicate rules.

`coalesce` evaluates arguments in order until a non-null value is found;
false, zero, empty strings, and empty collections are all non-null. `CASE`
stops testing conditions at the first match and evaluates its selected
result. Runtime errors in unused arguments, later conditions, or unselected
results are skipped. Static errors and missing parameters anywhere in the
statement are still rejected before execution. Aggregate inputs are evaluated
for the group before conditional selection, including aggregates in unused
branches. The upstream parser requires an escaped function name for `collect`
in some `CASE` results (for example, ``THEN `collect`(n)``); binding the
collection in an earlier `WITH` also works.

## Scalar functions

`keys()`, `properties()`, and the conversion functions reject an argument of a
kind they cannot take with a type error: `toInteger()` and `toString()` a list
or map, `toFloat()` a boolean, list, or map, and `toBoolean()` a float, list,
or map. The collection functions `size`, `head`, `last`, `tail`, and `reverse`
also reject non-null arguments outside their accepted kinds, as do the numeric
and string functions listed below. Function names are case-insensitive, and
calling one with the wrong number of arguments is a semantic error.

- `coalesce(a, b, ...)` returns the first non-null argument, including graph
  values, and skips the remaining arguments (see [conditional graph values](#conditional-graph-values)).
- `keys(x)` returns the property names of a node, relationship, or map as a
  list, and `properties(x)` returns them as a map. Stored property maps and
  map literals keep their keys sorted, so the order is alphabetical. An
  entity never stores a null property (see "Values and properties"); map
  values may hold null, so the map forms keep such entries.
  Both functions read a null entity (from `OPTIONAL MATCH`) as null.
- `head(list)`, `last(list)`, and `tail(list)` select `list[0]`, `list[-1]`, and
  `list[1..]`, respectively. They require a list or null, including when the
  input comes from a mixed graph collection. Their stricter input check is
  independent of general indexing and slicing rules.
- `reverse(x)` reverses a string or a list.
  These four list functions preserve entity and nested graph values; see
  [nested graph collections](#nested-graph-collections).
- `toInteger`, `toFloat`, `toString`, and `toBoolean` convert between
  numbers, strings, and booleans. `toInteger` truncates floats toward zero,
  parses integer or float text, and maps booleans to `1`/`0`; `toFloat`
  parses numeric text; `toString` renders numbers and booleans; `toBoolean`
  accepts booleans, the strings `true` and `false` in any letter case, and
  integers (zero is false). Anything else, including text that does not
  parse, is null.
  `toInteger` also returns null when the truncated result is outside the signed
  64-bit range; it never clamps to an endpoint. Trimmed text consisting of an
  optional sign and decimal digits is parsed exactly as an integer, so
  `'-9223372036854775809'` is null rather than rounded to `i64::MIN`. Decimal
  point/exponent text uses binary64 parsing before truncation, just like a float
  input. In particular, the float `2^63` is out of range, whereas the integer
  string `'9223372036854775807'` remains exact. Non-finite numeric text returns
  null; errors evaluating an argument (such as arithmetic overflow) still
  propagate before conversion. These rules apply equally to parameters,
  properties, eager/compiled execution, and cursors.
- `toLower`, `toUpper`, `trim`, `ltrim`, `rtrim`, `substring(s, start[,
  length])` (zero-based), `left(s, n)`,
  `right(s, n)`, `replace(s, search, replacement)`, and `split(s, delimiter)`
  operate on strings; `split` with an empty delimiter splits into characters.
  All text arguments require strings; positions and lengths require integers,
  without coercing floats, booleans, or numeric strings. Known invalid kinds
  raise `Error::Semantic` during planning, including unused branches and empty
  matches. Dynamic invalid kinds raise `Error::Type` when evaluated, including
  values selected from mixed graph collections. Composed expressions and
  aggregates preserve these errors; lazy `CASE` and `coalesce` still skip
  unselected calls.
  Null or missing arguments produce null. For ordinary dynamic argument values,
  null propagation takes priority over wrong-kind checks in another position;
  it does not suppress planning errors or errors while evaluating an argument.
  Negative `substring` positions or lengths and negative `left`/`right`
  lengths raise `Error::Argument` when evaluated, including literal bounds,
  empty strings, and positions beyond the end paired with a negative length.
  These value checks follow null propagation and argument-kind validation;
  lazy branches and empty matches can skip them. Zero lengths produce empty
  strings. These errors propagate through composed and
  aggregate expressions, eager/compiled reads, and cursors; failed writes and
  explicit transactions roll back under the normal error contract.
  Character indexing and empty-delimiter splitting preserve Unicode scalar
  values, including embedded NUL. `substring`, `left`, and `right` retain text
  after NUL and count combining marks separately. Nonnegative positions beyond
  the end produce an empty substring, and lengths exceeding the available text
  return the available characters, including bounds up to `i64::MAX`.
  `replace` matches complete search strings, including those beginning with
  NUL, and preserves NUL in replacement text. Matches do not overlap; an empty
  search string leaves the input unchanged. Case conversion is ASCII-only.
- `abs` keeps the argument's numeric type and `sign` returns an integer;
  `ceil`, `floor`, `round`, `sqrt`, `exp`, `log`, `log10`, `sin`, `cos`,
  `tan`, `asin`, `acos`, and `atan` return floats. A non-finite result
  such as `sqrt(-1)` or `exp(1000)` raises `Error::Argument`. Null or missing
  inputs produce null. Non-numeric inputs, including numeric strings and
  booleans, are rejected without coercion: a kind known from the query raises
  `Error::Semantic` during planning, and a parameter, property, or mixed graph
  selection of the wrong kind raises `Error::Type` when evaluated. Static
  checks also apply to unused branches and queries with no matching rows;
  runtime checks preserve lazy `CASE` and `coalesce` branches. Composition
  (including `IS NULL` and aggregates) preserves evaluated input errors.
  `pi()` and `e()` are constants.
- `rand()` draws a float in `[0, 1)` once per call site per row, so a value
  bound in `WITH` is stable across later references and a `CASE` whose
  condition uses `rand()` is decided once. Inside a list comprehension or
  quantifier the draw is per element. `rand()` in an aggregate argument is a
  semantic error, as in Cypher.

## Names and identifiers

Escaped symbolic names are decoded once and remain exact and case-sensitive.
Variables, labels, relationship types, property keys, parameter names, and
projection aliases may therefore contain whitespace, punctuation, Unicode,
embedded backticks, or contextual keywords when the name is escaped. Labels
and relationship types are compared case-sensitively; property and named
parameter lookup is likewise case-sensitive.

Graph variables and aliases are mapped to generated internal slots and SQL
aliases. Labels, relationship types, property keys, and parameter values are
carried as bound data; their source spelling is never pasted into generated
SQL. Two `RETURN` items whose decoded column names are identical are rejected
during semantic planning, including a bare expression repeated twice or an
escaped alias colliding with an unescaped one. A created relationship type must
also decode to a nonempty string.

The current `open-cypher 0.2.1` parser still rejects a labeled node that
follows an earlier node-local `WHERE` in the same pattern or clause, even
when the label is escaped. Split the pattern across `MATCH` clauses until
an upstream parser fix is released.

Bare contextual-keyword labels such as `Where` can also cause a later node
label to be misclassified: `MATCH (a:Where)-[:LINK]->(b:Node) RETURN b` fails
to parse. Escape the keyword label with backticks to avoid this separate
parser limitation.

## Labels and relationship types

A node has any number of labels, stored as a JSON list in creation order
without repeats; `labels(node)` returns that list, empty for an unlabeled
node. `CREATE`, `MERGE`, `SET`, and `REMOVE` take a plain conjunction of
labels (`:A:B` or `:A&B`) and ignore repeats, so `CREATE (:B:A:B)` stores
`["B", "A"]`. `SET n:A:B` adds the labels the node lacks and `REMOVE n:A`
removes the ones it has; `QuerySummary::labels_added` and `labels_removed`
count the node/label pairs that actually changed, and creating a labeled node
counts its labels as added. A null target is skipped.

A `MATCH` node pattern accepts any label expression: a conjunction is a set of
label tests, and `|`, `!`, and parentheses become a filter over the same
tests, so `(n:A|B)` and `(n:(A|Z)&B)` work. `n:Label`, `n:A:B`, and `n:A|B`
are also boolean predicates in `WHERE` and projections, null for a null
node. Matching by label is served by the `node_labels` index table. The
label wildcard `%` is unsupported.

A stored relationship has exactly one nonempty type. A `MATCH` relationship
pattern may omit its type or specify one type; `r:T` and `r:T|U` work as
predicates on a bound relationship. A created relationship must specify
exactly one type. Multiple and boolean relationship-type expressions inside
patterns are unsupported. `type(relationship)` returns the relationship's
single type.

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
duplicate endpoint projections. The hybrid producer seeds selectively: label
and inline-property constraints that the same `MATCH` places on the start
endpoint compile to a bound node-domain query, and only those nodes are
expanded. An unconstrained start with a positive lower bound seeds only nodes
that begin an eligible edge, while a zero-hop lower bound still seeds every
stored node because each is its own identity trail. Constraints on the end
endpoint filter emitted rows the same way but never prune intermediate
states. Both domains are supersets of the endpoint's final matches, so the
consuming SQLite join remains the semantic boundary; the seeding only reduces
explored states and serialized payload. The SQLite recursive-CTE route seeds
the same way: the start endpoint's label and inline-property constraints are
repeated in the walk's seed `WHERE`, so a bounded traversal from one matched
node enumerates trails from that node rather than from every stored node.
Constraints from earlier clauses or a node-local `WHERE` are not pushed into
either seed.

An omitted upper bound has no hidden hop cutoff: relationship uniqueness limits
one trail to at most the number of eligible stored edges, although the number of
distinct trails can still grow rapidly. Deep/open-ended enumeration and
shortest-path BFS default to a 100,000-trail-state budget across one top-level query,
including `UNION` branches and multiple hybrid expansions. Their runtime
relations also share a default 128 MiB (134,217,728-byte) serialized-payload budget.
That second counter is the exact UTF-8 byte length of each compact JSON array,
including `[]`, row separators, JSON escapes, keys, and values. The relation is
encoded incrementally, and a byte is appended only after the budget admits it.
An exact-limit payload succeeds; the next byte fails the query with
`Error::ResourceLimit`. Both counters are cumulative across runtime producers
and `UNION` branches, and neither failure returns a truncated or partial result.

The state counter is an explored-work guard, while the payload counter bounds
only the serialized Rust-to-SQLite relations. A third counter bounds the graph
inputs a hybrid producer retains while it runs: eligible edges, seed and
end-domain node lists, adjacency maps, search frontiers, shortest-path visited
maps, and retained candidate cycles share a default 256 MiB (268,435,456-byte)
retained-memory budget. That counter is deterministic rather than an allocator
measurement: every retained ID string costs its UTF-8 length plus a fixed 24
bytes, and each visited-map entry costs a further 8 bytes. Edges, seeds, and
adjacency steps are charged as they are read or inserted, so an oversized input
fails before it is fully loaded; frontier states are charged when queued and
released when expanded; and a producer releases its whole charge when it
finishes, so the counter tracks live inputs rather than accumulating across
producers or `UNION` branches. Exhausting it fails the query with
`Error::ResourceLimit`. Encoded output rows remain under the payload counter,
and Rust allocator overhead, SQLite's own memory, and the final result set
remain outside all three counters.

The physical read plan represents Rust-produced relations as explicit value
operators with branch-local runtime slots; they are not special SQL parameter
variants. Before any producer or SQLite fragment executes, preparation walks
the complete tree and resolves ordinary parameters, deferred pagination, and
traversal-edge filters in every `UNION` branch. A preparation error therefore
does not spend traversal budget or expose partial rows.

Runtime slots are linear within a fragment: each has exactly one producer and
one consumer. Preparation rejects duplicate, missing, unreferenced, or reused
slots before execution. Exhaustive zero-to-four-slot tests cover every producer
and consumer permutation with static arguments in every possible gap.

Legacy pattern-position `shortestPath` and `allShortestPaths` are supported in
mandatory read-only `MATCH`. The inner pattern must be exactly two nodes joined
by one legacy variable-length relationship. Its lower bound must be zero or
one; an omitted lower bound is one. Explicit and omitted upper bounds are
supported. Selection is unweighted: path length is the number of relationships,
and the requested bounds are enforced before choosing a minimum.

Selection is independent for each matched start/end node pair after endpoint
constraints, direction, optional relationship type, inline edge-property map,
and parameters have restricted the candidates. Rust uses breadth-first search
over relationship-unique trails. `shortestPath` emits one minimum trail; when
several tie, the choice is deterministic for a fixed stored graph but
otherwise unspecified. `allShortestPaths` emits every tied minimum trail, so
parallel edges remain distinct paths. An unreachable pair emits no row. With
an explicit zero lower bound, a start node paired with itself selects the
zero-hop identity.
With lower bound one, the same endpoint pair instead selects its shortest
positive relationship-unique cycle; `shortestPath` emits one such tie and
`allShortestPaths` emits all ties. Crabnet makes this common-endpoint behavior
unconditionally permissive; unlike Neo4j, it has no configuration gate for
legacy shortest-path common nodes.

When the same `MATCH` constrains the start endpoint with a label or inline
property, shortest search loads neighbors on demand through SQLite's outgoing
or incoming endpoint index. Undirected search probes both indexes and includes
self-loops once. Each node's ordered, filtered neighbor list is cached for the
producer, including empty lists, so tied paths and later seeds reuse it.
Disconnected edges are not loaded, and nodes at the upper hop bound are not
expanded. Type and typed property filters retain their bound parameters.
Search still explores the reachable region within its bounds; it does not
stop at the first requested endpoint, since selection is per endpoint pair.
Without a start domain, the producer keeps the eager eligible-edge source.
Constraints from earlier clauses alone do not enable neighbor loading.

Cached keys and steps count against retained traversal memory, while each
fetched edge row counts against SQL-to-Rust materialization limits. Cache hits
do not transfer another SQL row. Frontier and visited entries are released per
start; cached neighbors are released when the producer finishes. All probes
use the query's connection, snapshot, cancellation/deadline checks, and SQLite
work budget. `explain_physical` shows `neighbor-source` and its final
`current-node` binding for this strategy.

When bound, a shortest result uses the same `PathValue`, fresh relationship-list
binding, `length`, `nodes`, `relationships`, pending identity, and batch
hydration contracts as other variable-length paths. It preserves incoming bag
rows and composes through `WITH` and `UNION`; the BFS relation is fully
materialized as JSON before SQLite consumes it. Modern path selectors and
expression-position shortest calls remain rejected. Legacy shortest factors
are also rejected in `OPTIONAL MATCH`, with a `WHERE` attached to the same
`MATCH`, alongside another comma-separated pattern part, with a lower bound
above one, or anywhere in a write query.

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
distinctness, pagination, and unions before hydration. `UNWIND` resolves each
member's identity into a node or relationship binding for subsequent reads.

Each function requires exactly one in-scope path argument; bad arity, unknown
variables, and wrong variable kinds fail during planning. Property access and
selectors on a path, plus raw path use as a general scalar or ordering
expression, remain planning-time rejections. Paths and path lists support
the [graph predicates](#path-and-graph-list-predicates), `count`, `collect`,
and grouping (see [path aggregation](#path-aggregation-and-grouping)). The typed
lists from `nodes` and `relationships` retain the expression-context restrictions
above.
A fresh legacy relationship variable in `[rels:TYPE*min..]` is a
relationship-list binding, not a stored relationship or a first-class path. It
returns a public `CypherValue::List` of typed relationships in traversal order;
zero hops return `[]`, and each member retains its stored endpoint direction.
The list can be returned directly, carried through `WITH`, expanded with
`UNWIND`, shaped by semantic distinctness and pagination, combined through
unions, and bound alongside the named path for the same pattern.

A relationship-list variable that is already bound, whether by an earlier
pattern in the same `MATCH`, by an earlier clause, or as a `WITH` alias of
`relationships(path)`, may appear again on a variable-length relationship. That
occurrence does not rebind the variable: it constrains the new traversal so
that its relationship sequence, in traversal order, equals the bound list
position by position. A list projected from a multi-segment path compares as
the concatenation of its segments' relationships. The new pattern keeps its own
direction, bounds, type, and property filters, so a bound list traversed the
other way or outside the new bounds simply produces no row. Within one `MATCH`,
a re-matched occurrence denotes the same relationships as the binding it refers
to and therefore does not take part in relationship uniqueness against it; a
list bound by an earlier clause still cannot share relationships with the
other traversals of the clause that re-matches it. Use in the scalar,
aggregate, grouping, or ordering contexts above remains unsupported.
Named paths or relationship-list bindings in `OPTIONAL MATCH`, postfix graph
quantifiers, and writes fed by variable-length traversal, a named path, or a
relationship list also remain unsupported.

SQLite carries a compact encoding of the ordered node and relationship IDs.
For eager execution, Rust drains each branch cursor into pending typed rows
before another branch or hydration query runs. Branch-local semantic
`RETURN DISTINCT` and deferred pagination run on those pending cells, followed by left-to-right
`UNION`/`UNION ALL` operations. Only IDs required by surviving whole paths or
graph lists are collected and deduplicated query-wide. A
nodes-only result does not load relationships, and a relationships-only result
does not load nodes. Directly projected graph entities seed the hydration maps
from their already-materialized snapshots; Rust then loads every remaining ID
in batches, using at most one node query and one edge query before constructing
public paths and typed lists in traversal order.

Eligible [read cursors](#read-cursors) apply the same shaping order, then hydrate
one row initially and up to 64 surviving rows per later batch. Entity reuse is
per batch rather than query-wide; all batches share the cursor's snapshot.

The complete read—runtime traversal producers, every branch SQL fragment, and
union evaluation plus every hydration query—shares one deferred
SQLite read snapshot. Independent Crabnet connections follow the
[file-opening and contention contract](#file-opening-and-contention).

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

Matched `SET` and `REMOVE` accept property targets on nodes or relationships.
`SET entity.key = value` assigns a value and `REMOVE entity.key` deletes that
key. The entity may be a variable or an expression that selects a node or
relationship from a collection, map, function, or conditional:

```cypher
MATCH (person:Person)
WITH collect(person) AS people
SET (people[0]).active = true
REMOVE (head(people)).temporary
RETURN people
```

Parenthesized bindings such as `(n).key`, nested/dynamic selections such as
`(box[$key][i]).key`, and `coalesce`/`CASE` selections preserve typed entity
identity. A dynamic selection may choose a node or relationship per row.
Null, missing, and out-of-range selections are no-ops. Statically invalid
targets and aggregate calls in targets fail during planning; other non-null
non-entity targets raise `Error::Type` when evaluated. Ordinary parameter
maps and lists cannot impersonate graph entities. Target parameters are
preflighted even when no rows match.

For each property mutation item, computed targets and stored values are
evaluated once per input row against the state at the start of that item,
before it writes. Duplicate targets retain input-bag multiplicity; each row
updates the current stored properties in input order. Later items see all
earlier updates, and every matching entity copy in aliases, lists, and nested
maps is refreshed. Unchanged assignments and absent removals count zero;
`SET ... = null` removes the key. `MERGE` actions use the same rules over their
row subsets. Target, value, constraint, and output-resource failures roll back
the statement, or abort and roll back an enclosing application transaction.
Internal target evaluation does not consume final-result budgets.

The final property key is still a static identifier. Whole-map replacement,
whole-map merge, and label mutation retain variable targets.
`SET entity = map` replaces the complete property map, while
`SET entity += map` performs a shallow merge. Values and maps may be literals,
named parameters, or expressions over the row: `SET a.total = a.price * a.qty`,
`SET a += {copy: b.name}`, and `SET a = b` (which copies `b`'s properties) all
work, as do computed `CREATE` properties such as `CREATE (c {next: a.n + 1})`.
A `CREATE` may precede `SET`, `REMOVE`, and `DELETE` in the same statement.
A computed value is evaluated once per input row, over the rows as they stand
when that `SET` item or `CREATE` pattern starts, so later items see earlier
writes. It must not contain aggregates, nodes, relationships, or paths; a
whole-map form must evaluate to a map. Those are semantic errors. Writes fed
by variable-length traversal or by any named path remain planning-time
rejections. `SET n:Label` and `REMOVE n:Label` are described under labels
below.

Reading and updating clauses may alternate freely within one statement:
`UNWIND`, `WITH`, `MATCH`, and `OPTIONAL MATCH` may precede, separate, or
follow `CREATE`, `MERGE`, `SET`, `REMOVE`, and `DELETE`, and the statement
ends with an updating clause or `RETURN`. The reading clauses before the
first update produce the input rows; each later run of reading clauses reads
on from the rows the updates so far produced, seeing only the variables in
scope at that point and the graph as those updates left it, and the updates
after it apply to the rows it produces. So `CREATE (a) WITH a MATCH (b)`
matches the node just created, `WITH n AS m` keeps `m` bound to the same
node so no second node is created, a `WITH ... LIMIT` or `WHERE` after a
write shapes the result and the rows that later clauses see but never undoes
a side effect, and an aggregating `WITH` collapses the rows for what follows.
Updating clauses skip rows whose target is null, which is how an `OPTIONAL
MATCH` with no match feeds `SET`, `REMOVE`, and `DELETE`. An updating clause
after `DELETE` in the same statement is still a planning-time rejection.

Direct assignment of `null` removes the targeted key. A null-valued entry in a
`+=` map also removes that key, and whole-map replacement with
`SET entity = {key: null}` leaves `key` out of the replacement, as `CREATE`
does for a null entry in its property map. Map merge is shallow; nested maps replace their existing value rather
than recursively merging it.

`QuerySummary::properties_set` counts top-level property keys whose observable
stored value changed or was removed. Reassigning the same value and removing an
absent key contribute zero. A whole-map replacement counts additions,
removals, and value changes rather than counting the clause as one operation.

`MERGE pattern` matches the whole pattern from each input row, with variables
bound by earlier clauses fixed, and creates the pattern's new parts when the
row has no match. Rows are processed in order against the graph as it stands,
so a row without a match creates the pattern before the next row is matched
and later rows match what earlier rows created; a row with several matches
becomes one row per match. Rows that match before the merge writes anything
are matched together in one read, and a computed action value reads each
row's entities as they stood when that row was matched, so two such rows
that update the same entity both see its pre-merge value. Pattern properties may be literals, parameters, or
expressions over the row. A literal null property value is a semantic error,
since the merge could never match what it would create; a computed value that
is null never matches. An
undirected relationship matches in either direction and is created left to
right; a created relationship needs exactly one type. `ON CREATE SET` applies
to the rows the merge created and `ON MATCH SET` to the rows it matched, with
the same forms as `SET`. A pattern whose variables are all already bound, or
that decorates a bound variable with labels or properties, is a semantic
error; path bindings and variable-length relationships are planning-time
rejections. `MERGE` may follow `WITH` and `UNWIND`, so `UNWIND names AS name
MERGE (:Person {name: name})` creates each distinct name once. The merge and
its actions run in the statement's write transaction like every other
mutation.

`DELETE` and `DETACH DELETE` accept node and relationship variables and
expressions such as `people[0]`, `box.node`, and `box[$key][$index]`.
Selections from slices, comprehensions, concatenated lists, and nested maps
retain their graph identity. A heterogeneous selection may resolve to either
a node or a relationship at runtime. Null targets, including missing map
members, out-of-range indexes, and null optional bindings, are skipped.

Target expressions run over the current materialized rows after preceding
mutations, so they see earlier property and label updates. Every expression
is evaluated for every input row and all targets are validated before any
deletion begins. Aggregate calls directly inside a target are rejected; use
an earlier `WITH` to collect entities. Whole lists, maps, paths, and non-null
property values are not deletion targets. A known invalid target fails with
`Error::Semantic` during planning; an invalid runtime value fails with
`Error::Type` and rolls back the statement, including earlier writes. For
example, `DELETE n.property` skips a missing/null property but rejects a
non-null value. Parameters contain property JSON and cannot impersonate graph
entities, even if they contain a real element ID or resemble an internal
record. Target parameters are checked before mutation execution, including
when the input has no rows. Path-fed writes remain unsupported.

Targets are gathered and deduplicated across the complete materialized match
bag, including duplicates shared by variable and expression targets, then
explicitly targeted relationships are deleted before nodes regardless of
target spelling order. Plain deletion of a node that still has an incident relationship fails
with `Error::Constraint` and rolls the entire statement back.
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

For standalone `Database::execute` / `execute_with`, all parameters and storable
property values for a write are resolved and validated before the write
transaction starts, including recursive value depth, integer range,
complete-map shape, and every parameter referenced by a later
mutation or result. Match materialization, all mutations, and post-write result
evaluation then occur in one immediate SQLite transaction. Any execution
failure rolls back every mutation performed by the statement. An optional
`RETURN` emits one row per mutation input row and may
project whole matched or created entities as well as supported properties and
metadata; for deleted entities it follows the snapshot rule above.

### Node uniqueness constraints

`Database::create_unique_constraint(label, property)` declares uniqueness of
one top-level property among nodes carrying that label. It returns `true` for
a new declaration and `false` for an existing one. Installation validates all
existing members in one immediate transaction; conflicts return
`Error::Constraint` without installing any part of the constraint or changing
the graph. Existing duplicates remain legal until an application installs a
constraint. `drop_unique_constraint(label, property)` returns whether a
declaration was removed and leaves graph data unchanged.
`unique_constraints()` returns `Vec<UniqueConstraint>` in label/property
order. The public record contains `label` and `property` strings.

Names are exact, case-sensitive strings, without Cypher backticks or a leading
colon. Empty names and unusual characters follow the same rules as decoded
Cypher labels and property keys; the API binds them as data. Each declaration
is independent, including constraints on overlapping labels or different
properties. Relationship properties are unaffected.

Missing and top-level null properties do not reserve a value. Every other
stored property kind is supported using the same typed equivalence as
`DISTINCT`: numerically equal integers and floats conflict, signed zero is
normalized, booleans and strings remain distinct from numbers, and strings
are case-sensitive without Unicode normalization. Lists compare in order;
maps compare by keys and values, independent of key order. Both recurse with
the same numeric rules and treat corresponding null members as equal. Thus
`[1, null]` conflicts with `[1.0, null]`, even though comparing these values
with the three-valued `=` predicate produces null. Floating-point keys are
derived from the original stored JSON without SQLite JSON reformatting.

Checks run immediately for each node mutation, including `CREATE`, `MERGE`,
property replacement/merge/removal, and label changes. They are not deferred
to the end of a query or transaction. Reassigning a node's own key is legal;
assigning another node's key fails even if a later clause would free it.
Release a key first by changing/removing its property, removing the label, or
deleting the node. `MERGE` still matches the whole pattern: a conflicting
partial match fails with `Error::Constraint` instead of silently reusing a
node that does not match. Errors identify the label/property pair and roll
back the whole statement, including prior mutations and index changes.

`Transaction` exposes the same three methods. Installation sees prior writes
and applies to subsequent writes; create/drop changes remain provisional
until commit. Any query or constraint-management error aborts the entire
transaction. Rollback, drop, and query-result failures restore both data and
constraints. These schema methods do not produce `QuerySummary` counters.

Declarations and their keys persist in the database. Schema version 5 retains
unchanged property-index entries and skips uniqueness-index writes when node
identity, labels, and canonical constrained values are unchanged. A changed
key or label still receives the same immediate uniqueness check; updates are
not deferred or combined across rows or `SET` items. Property-index values
retain SQLite's extracted storage types, including integer-to-real changes.
Stable node/relationship insert and property read/write statements use the
connection's prepared-statement cache; every execution binds fresh values.

Opening main's versions 1–4 in a writable mode upgrades atomically, retaining
existing data and constraints; versions before 4 install no constraints by
default. Failed migrations leave the previous schema and version marker intact.
Triggers enforce uniqueness under SQLite's writer
reservation; another Crabnet connection observes committed constraint
changes. Direct external SQLite writes are not a supported application API.
Composite keys, relationship constraints, named constraints, and Cypher
schema DDL are not implemented.

### Application transactions

`Database::transaction()` returns a `Transaction` that exclusively borrows the
database and begins one immediate SQLite transaction. It reserves the writer
before the application's first query, avoiding a deferred read-to-write
upgrade. Its `execute` and `execute_with` methods use the same planner and
executor as standalone queries, without starting or committing an inner
transaction. A later query sees earlier queries' writes, including property
and label indexes, relationship changes, hybrid traversal, and graph-value
hydration. Query variables and parameters remain local to each call.

`commit(self)` makes every successful query's writes persistent and consumes
the handle. `rollback(self)` undoes them and consumes the handle. Dropping an
active handle also attempts rollback, including on early return or panic
unwinding; explicit rollback allows the caller to observe cleanup errors.
The parent database cannot be used until the handle is consumed or dropped.
Empty transactions may be committed or rolled back. This initial API exposes
neither nested transactions nor savepoints nor a read-only transaction mode.

Any query or constraint-management error aborts the entire application
transaction, including parse, semantic, unsupported-feature, missing-parameter,
runtime type, constraint, resource-limit, cancellation, and deadline errors. The failing call rolls back immediately and returns
the query's original error unless rollback itself fails. If SQLite has
already rolled back, cleanup does not replace the original error with a
redundant-rollback error. Further query execution and commit return
`Error::TransactionAborted`; explicit rollback on an aborted handle succeeds
as a no-op. A failed SQLite commit consumes the handle and attempts rollback
on drop. An error proves neither persistence nor rollback: applications must
reconcile the stored outcome before retrying. See [commit I/O failures](#commit-io-failures)
for tested outcomes on either side of journal deletion.

Each call completes planning and parameter validation before that query's
execution, although the outer transaction is already open. Read results are
owned snapshots; `QuerySummary` reports only the current query's changes.
Both remain provisional until the application transaction commits: retaining
a returned node or summary does not preserve a write that was rolled back.
Resource budgets reset for each query, while all operators within a
query continue sharing that query's budget. Standalone execution retains its
existing per-statement transaction and preflight behavior.

`Transaction::prepare_read*` exposes the [read cursor API](#read-cursors) using
this same transaction and its earlier writes. The prepared read borrows the
parent exclusively. Successful cursor completion/drop leaves it active; any
preparation, cursor-creation, or iteration error follows the rollback contract
above. Per-query counters reset for each cursor, while the snapshot remains
the parent's. Returned rows remain provisional until commit.

## File opening and contention

`Database::open(path)` delegates to `Database::open_with_options(path,
&OpenOptions::default())`. Each database owns one connection; the options are
connection-local, including when several handles use the same file.

| `OpenMode` | Missing or uninitialized file | Existing Crabnet graph |
| --- | --- | --- |
| `OpenOrCreate` (default) | Creates a missing file or initializes an empty file | Opens read/write, migrating supported older schemas |
| `ReadWrite` | Rejects it without creating or initializing a graph | Opens read/write, migrating supported older schemas |
| `ReadOnly` | Rejects it without creating or initializing a graph | Opens only the current schema; never migrates |

Create-or-open does not mean exclusive creation and never truncates an existing
graph. Paths are filesystem paths: empty paths, `:memory:`, and `file:` SQLite
URIs return `Error::Argument`. Use `Database::in_memory()` for a transient
graph. URI parameters cannot override the access mode or enable shared cache.
Read/write opens reject SQLite's fallback to read-only filesystem access with
`Error::ReadOnly`. Missing files, inaccessible directories, and other open/I/O
failures retain `Error::Sqlite` and its underlying code.

A read-only handle supports all supported read queries, including hybrid
traversal and hydration, and `unique_constraints()`. Valid mutation queries
(even when they would match zero rows), create/drop-constraint calls, and
`transaction()` return `Error::ReadOnly`. The latter reserves a writer, even
if the caller only intended to read. Planning, parameter, and query-resource
preflight errors can still be returned before the write-access check. Read-only
application transactions spanning several queries are not exposed.

Schema version 5 is current. Writable opens atomically migrate main's
versions 1–4. A read-only open of an older version returns `Error::Data`
explaining that a writable migration is
required. Future versions, foreign application IDs, and unrelated nonempty
SQLite databases are refused. The earlier experimental single-label version 2
is still unsupported. Inspection and validation use one read snapshot. When
initialization or migration needs a writer, it releases that snapshot, acquires
the writer, and rechecks the version before applying DDL: concurrent openers
must not apply the same migration twice. Current-schema opens do not reserve
a writer. Validation checks the expected table/column layout; it is not a full
integrity or corruption-repair check.

The [compatibility policy](compatibility.md) defines API versioning, the retained
historical upgrade baselines, and the application upgrade/rollback procedure.
There is no automatic downgrade: keep an unchanged pre-upgrade backup and the
old application version. Restoring an older-schema backup requires upgrading a
copy and producing a current-schema backup first; the original stays available
for rollback.

### Readers, writers, and retries

The tested concurrency contract uses independent connections on a local file
with SQLite's default DELETE rollback journal. Each connection is used
serially; different threads can own different `Database` handles. Crabnet
opens file connections with private caches. It does not switch a file's
existing journal mode or expose journal/durability tuning. WAL configuration,
shared cache, network filesystems, and direct external SQLite mutations are
outside this contract.

Standalone eager reads retain one snapshot until all operators and hydration finish,
then release it before returning owned, eager results. Holding those results
does not hold a database lock. Incremental [read cursors](#read-cursors) retain
their snapshot until exhaustion, error, or drop. Other connections observe committed graph and
constraint changes on later calls, never another connection's partial writes.
There is one writer at a time. Standalone mutations and application
transactions acquire that reservation with `BEGIN IMMEDIATE` before executing
writes. Readers can coexist with a writer's reservation, but rollback-journal
writes/commit may need an exclusive lock and can block readers or be blocked
by an active reader. This follows [SQLite's isolation model](https://sqlite.org/isolation.html).

`OpenOptions::busy_timeout` defaults to five seconds and is installed before
schema inspection or migration. Zero disables waiting. Durations are truncated
to whole milliseconds; values greater than `i32::MAX` milliseconds return
`Error::Argument` before opening a file. The timeout applies to individual
SQLite lock waits during opening, queries, constraint operations, and
begin/commit. It is not a whole-operation deadline, and SQLite may skip waiting
to avoid deadlock. See SQLite's [timeout](https://sqlite.org/c3ref/busy_timeout.html)
and [busy-handler](https://sqlite.org/c3ref/busy_handler.html) contracts.

SQLite `SQLITE_BUSY`, including its extended codes, becomes
`Error::Busy(Box<rusqlite::Error>)`, retaining the original error through `source()`.
Other SQLite errors, including `SQLITE_LOCKED` (a different same-connection or
shared-cache condition), remain `Error::Sqlite`. Crabnet does not automatically
replay a query or application transaction. A failed begin leaves the connection
reusable; a later statement failure aborts the enclosing transaction. A busy
commit consumes the transaction and attempts rollback, so its provisional
results must not be treated as persisted. Retry the entire standalone operation
or application transaction, including its reads and decisions, after handling
the failure. Tests verify rollback of graph and constraint changes after a
reader blocks commit and verify subsequent connection reuse.

Query deadlines and cancellation do not interrupt lock sleeping, opening,
constraint management, or explicit begin/commit/rollback. Choose the connection
timeout alongside `QueryOptions`; a short query deadline does not shorten a
longer lock wait. Backup/restore and process interruption follow the contract
below. Named release compatibility remains subject to qualification under the
[compatibility policy](compatibility.md).

## Backups and process recovery

`Database::backup_to(path)` creates a standalone SQLite snapshot at a new
filesystem path. It works from an in-memory database or a writable/read-only
file handle. Graph identities, properties, labels, relationships, materialized
indexes, and uniqueness declarations/keys are preserved. Connection settings
and temporary query tables are not part of the backup. Another connection's
uncommitted changes are excluded.

The method validates one pinned source read snapshot, copies SQLite pages into
a temporary directory beside the destination, closes the SQLite destination,
syncs the file, and publishes it with a hard link. The destination appears only
after the complete copy exists. Hard-link publication requires filesystem
support and never replaces an existing entry, including an empty file,
hard-link alias, or symlink. An existing destination returns
`Error::Io` containing `std::io::ErrorKind::AlreadyExists`. Other staging or
publication failures also preserve the underlying filesystem error. Cleanup
attempts file removal and then recursive directory removal on success or
failure. Cleanup errors are best effort: they do not replace an earlier error
or turn a successfully published backup into a reported failure. Cleanup
failure or process death can leave a `.crabnet-backup-*` directory, containing
partial files or an additional hard link to a published backup. Once no backup
operation is using it, remove the staging directory; do not modify or truncate
its files, which may share storage with the published backup. A process killed
after publication can leave a valid final backup even if its caller never received
success. The file is synced, but its parent directory is not: this is not a
power-loss guarantee for the new directory entry. Source file permissions and
ACLs are not copied; the published file retains the temporary file's permissions
(owner-only on Unix).

`Database::restore_from(path)` opens an existing source read-only, validates it,
and copies it into this handle's main database using a single SQLite backup
transaction. It **replaces** all existing graph data and constraints on success.
The destination may be an in-memory graph or an existing writable file handle.
Read-only destinations return `Error::ReadOnly`. Prepared statements on the
destination are flushed; later queries and writes use the restored schema and
data. Other connections observe the replacement on subsequent reads. Retained
Rust query results remain their earlier owned snapshots.

Both methods check the application ID/current schema layout, run
`PRAGMA main.integrity_check` and `foreign_key_check`, and explicitly check the
stored JSON-container and relationship-type constraints. The explicit checks
also run for read-only sources, where the bundled SQLite omits CHECK expressions
from its loaded schema. Validation and copying use the same source snapshot.
These checks catch storage, index, foreign-key, and persisted CHECK violations;
they do not authenticate files or verify every semantic invariant of arbitrary
external SQLite edits (such as deleted triggers or stale derived graph tables).

Restore never creates, migrates, or modifies its source. Missing, uninitialized,
foreign, future-version, and older-schema sources are rejected before changing
the destination. To use an older backup, migrate a separate working copy with
a writable open first. Source and destination must be distinct files; trying
to restore from the destination itself conflicts with the pinned read lock and
returns `Error::Busy`. Restoring into memory can also fail if SQLite page sizes
differ. The destination remains unchanged on validation or ordinary copy
failure; incomplete copies are rolled back when the backup handle is released.
This relies on [SQLite's backup transaction contract](https://sqlite.org/c3ref/backup_finish.html).

Backup/restore are synchronous and hold a read snapshot for the validation and
copy duration. They can delay writers. Restore also needs an exclusive
destination lock; active readers or another writer can cause `Error::Busy`.
Source and destination use the calling handle's busy timeout. There is no
unbounded retry loop, progress callback, cancellation, or `QueryOptions`
budget for these operations. Retry the whole operation after resolving
contention. Copying a live database file with an ordinary filesystem copy is
not this API's backup procedure; SQLite's
[online backup API](https://sqlite.org/backup.html) supplies the consistent copy.

### Recovery after process termination

For the supported local-file DELETE-journal configuration, reopening read/write
lets SQLite recover a hot rollback journal before Crabnet reads schema metadata.
Use `OpenMode::ReadWrite` to require that the original file exists. Keep the
database and its `-journal` together until recovery finishes; removing the
journal would discard information needed to roll back incomplete writes.
A read-only open can return `Error::Sqlite` with SQLite's read-only code while
journal recovery requires writes. After writable recovery, read-only opens work
normally. Applications do not replay incomplete queries automatically.

`tests/recovery.rs` kills child processes without running Rust destructors.
Before the uncommitted kill, it verifies a hot journal header and that dirty
pages have actually reached the main file. Reopening must recover the exact
earlier graph and constraints, restore property/label indexes, and permit new
writes, backup, and restore. A second child is killed after commit; its graph
and constraint changes must survive. The tests check SQLite integrity and
foreign keys and recompute property/label index contents.

These are process-termination tests, not simulated power failure or faulty
storage. Torn writes, corruption repair, network filesystems, and
journal/durability tuning remain outside this tested contract. Allocation
failures and modeled commit/migration/restore/backup I/O errors have separate
coverage below, including [hot-journal replay failures](#hot-journal-recovery-io-failures).
Existing atomic migration and failed-migration tests remain in place.

## Storage allocation failures

SQLite's `SQLITE_FULL` error is exposed as `Error::Sqlite` with the underlying
`rusqlite::Error::SqliteFailure`, `ErrorCode::DiskFull`, and SQLite result code
preserved. It is distinct from `Error::ResourceLimit`, which represents Crabnet's
query budgets. A failed standalone write rolls back its earlier mutations. In
an application transaction, a query failure aborts earlier successful queries
as well: subsequent execution or commit returns `Error::TransactionAborted`,
explicit rollback is a successful no-op, and dropping the handle is safe.
The original error is retained unless rollback itself fails, as in the existing
transaction error contract.

Two unit tests in `src/database/storage_failures.rs` exercise file-backed graphs
through the public query/transaction API. They use private test access to
[SQLite's `max_page_count` pragma](https://www.sqlite.org/pragma.html#pragma_max_page_count)
to force a real pager allocation failure. Four cases fail while inserting or
updating a node or relationship after earlier graph mutations have run. A
transaction matrix includes prior deletion and constraint changes, then tests
commit, rollback, and drop after failure. Update-hook observations prove earlier
row changes occurred; exact typed graph bags and stored index/key snapshots
prove rollback. Another connection with zero lock timeout can commit while the
aborted handle remains alive. Reopened handles see the original graph; lifting
the cap allows writes on the original connection, and those commits survive
reopening. Integrity and foreign-key checks also pass.

This fault model caps database growth without exhausting the host filesystem.
It tests handling of `SQLITE_FULL` during statement execution, with the filesystem
still able to perform journal and rollback I/O. It does not inject OS-level
ENOSPC or allocation failures in migration, backup, or restore. Commit,
migration, restore, and backup write/sync failures have separate coverage below.
Applications should resolve the storage problem and verify recovery before
retrying the whole failed query or transaction;
earlier provisional results do not represent committed work.

## Commit I/O failures

Statements in an application transaction can succeed before `commit()` returns
an I/O error. Their earlier results remain provisional. **A commit error does
not guarantee rollback.** Once journal deletion has succeeded, a subsequent
error can accompany committed writes. The same applies to a standalone write
that reports an error during its implicit commit.

For the one-shot pre-call write/sync failures covered here, both explicit commit
and a standalone write's implicit commit preserve `Error::Sqlite(rusqlite::Error::SqliteFailure(...))`, with
`ErrorCode::SystemIoFailure` and the extended `SQLITE_IOERR_WRITE` or
`SQLITE_IOERR_FSYNC` code. Rollback restores the graph, derived indexes, and
uniqueness declarations, and releases locks before the failing API returns.
The original connection supports retrying the entire operation.

`src/database/storage_failures/commit_io.rs` exercises these paths through the
public query/transaction API. Its test-only forwarding
[SQLite VFS](https://www.sqlite.org/c3ref/vfs.html) intercepts
[`xWrite` and `xSync`](https://www.sqlite.org/c3ref/io_methods.html) on the main
database and rollback journal. Tests use DELETE journaling, `synchronous=FULL`,
and disabled cache spill to isolate commit. Each test records a healthy commit
trace, then repeats the workload with a fresh graph for every write/sync in
that trace. Explicit transactions are armed after successful queries and
constraint changes; standalone writes are armed by SQLite's commit hook.
Assertions match the successful I/O prefix and the exact failed event, including
file kind, operation, offset, and byte count or sync flags. Later main-file
failures therefore occur after earlier page writes have completed.

Each injected call returns its error **before** invoking the native operation,
then disarms itself. All other calls delegate to the native VFS. Tests verify
successful rollback page writes after main-file faults, exact typed graph bags
and stored index/key rows, restored constraints, autocommit state, another
writer committing while the original connection remains open, integrity and
foreign keys, reopening before retry, and a successful retry surviving reopen.
A separate test verifies that an armed fault cannot affect another database
using the same VFS and that it is consumed without an explicit reset. The VFS
is never the default, its fault state is scoped to a fixture path, and no public
raw-connection or fault-control API is added.

`src/database/storage_failures/commit_delete.rs` separately covers journal
finalization. The VFS can return `SQLITE_IOERR_DELETE` before native `xDelete`,
or invoke native deletion successfully and then return
`SQLITE_IOERR_DIR_FSYNC`. The latter models a reported error after unlink; it
**does not** actually fail the host's directory synchronization or simulate
loss of the directory entry after power failure. The bundled SQLite Unix VFS
performs unlink before its optional directory sync, so this ordering matters.
Deletion interception is opt-in and leaves earlier write/sync traces unchanged.

| Injected commit failure | State when the API returns | State after healthy recovery / reopen | Retry implication |
| --- | --- | --- | --- |
| Before journal deletion (`SQLITE_IOERR_DELETE`) | Main-file writes have completed; the journal remains and the connection is in autocommit mode. | Healthy access recovers the journal and restores the original graph, derived indexes, and constraint declarations. | The tested operation can be retried after verifying recovery restored the original state. |
| After successful native deletion (`SQLITE_IOERR_DIR_FSYNC`) | The journal is absent and the connection is in autocommit mode despite the reported error. | The complete committed graph, indexes, and constraint changes remain visible. | Blindly repeating a non-idempotent write can duplicate it. |

Both standalone writes and explicit transactions match a healthy commit trace
through the final database sync and the exact deletion event. Pre-deletion
faults run with FULL and EXTRA synchronization; post-deletion faults use EXTRA,
which requests directory sync. Tests independently exercise recovery through
an already-open observer before touching the original handle, through the
original connection first, and after closing the original connection. They
check exact typed graph bags and stored index/key rows, constraint changes,
lock release to another writer, integrity, reopen, and retry after recovered
rollback. A separate path-isolation/one-shot test leaves the fault controller
armed across retry and demonstrates two persisted copies of an unconditional
CREATE after the post-deletion error.

Applications should resolve the storage problem, allow healthy writable
recovery, and reconcile the operation's outcome before retrying a failed
commit. Use an application operation identifier or another durable check made
in the same transaction to distinguish applied work from unapplied work;
autocommit state and an error return alone do not establish rollback. Retrying
an operation designed to be idempotent avoids duplicating already-applied work.
These tests preserve the original SQLite error; Crabnet does not automatically
retry commits or promise that an extended error code alone classifies every
possible storage failure.

These are models of one failed SQLite I/O call with healthy later I/O. They do
not exhaust the host disk, inject OS-level ENOSPC, partially perform a failed
write, or model persistent errors during rollback. Journal truncation, WAL,
torn writes, and power loss remain outside this coverage. In particular,
reopening successfully after an injected error is not evidence of durability
under a later power failure.

## Migration I/O failures

For the one-shot write/sync fault model above, a failed upgrade from a valid
main schema 1–4 file restores the original schema and contents, including
`application_id`, `user_version`, SQLite's schema cookie, table/index/trigger
definitions, and all table rows. The error retains `Error::Sqlite` and the
extended `SQLITE_IOERR_WRITE` or `SQLITE_IOERR_FSYNC` code. After the fault
clears, a normal writable open can retry the upgrade; a read-only open still
refuses the older format until that upgrade succeeds.

Two tests in `src/database/storage_failures/migration_io.rs` copy the retained
[historical files and independent graph oracles](../tests/fixtures/compatibility/README.md)
and arm the forwarding VFS before initialization. One calls the initialization
routine used by public opens while retaining the failed connection; the other
uses the owning connection constructor to exercise cleanup on failed open.
For each historical input and path, a healthy upgrade supplies the write/sync
trace. Every event is then failed in a fresh copy. A commit-hook marker proves
coverage both during migration statements and at their final commit. Assertions
match the healthy I/O prefix and exact failed event, and check successful
rollback page writes after main-file faults.

Before any retry, raw SQLite readers compare every stored object and table row
with the original snapshot. Row comparisons retain SQLite types, real-value
bits, BLOB keys, and multiplicity; integrity and foreign-key checks cover the
physical indexes as well. Another connection with zero busy timeout can write
and commit while the retained failed connection is still alive. Closing and
reopening raw readers must still show the old format, so a later migration
cannot hide partial DDL, backfills, or a prematurely advanced version marker.
Retry uses the public existing-file opener with the native VFS. Typed graph
results must match the historical JSON oracle after upgrade and read-only
reopen, including IDs, label order, nested values, relationship bags, and
constraints. Subsequent writes exercise migrated label/property/uniqueness
maintenance and must survive another reopen.

These tests use DELETE journaling, FULL synchronization, disabled cache spill,
and one failed call before native I/O with healthy rollback afterward. They
do not model persistent/partial I/O errors, allocation failure during migration,
journal deletion/truncation, WAL, or power loss. Retained
inputs are pinned historical commits; published release-pair qualification is
still a separate gate under the [compatibility policy](compatibility.md).

## Migration read failures

Opening and upgrading retained main schema 1–4 files also has one-shot `xRead`
coverage. When SQLite reports the injected error to the opener, Crabnet retains
`Error::Sqlite`, `ErrorCode::SystemIoFailure`, and `SQLITE_IOERR_READ`. The failed
upgrade leaves the original file bytes, schema/version markers, and typed table
contents intact, releases locks and owned file handles, and permits a later
public writable open to retry. Read-only opening still refuses the old format
until migration succeeds.

A failed `xRead` does not necessarily make SQLite reject the migration.
Schema 1's `ALTER TABLE ... ADD COLUMN` runs an internal `quick_check` whose SQL
filters constraint/type diagnostics. With the bundled SQLite and these valid
fixtures, a one-shot read error in that check can be absorbed and migration can
complete after healthy subsequent I/O. Tests pin those outcomes separately:
success must produce exactly the same persisted schema and typed rows as the
healthy upgrade, checked **before** another public open could conceal an
incomplete migration. The independently retained graph oracle, integrity,
constraints, and later writes must also remain correct. This is not a claim
about persistent faults or damaged inputs.

Two tests in `src/database/storage_failures/migration_read_io.rs` sweep every
database/journal read in healthy traces for each retained format. One retains
the connection after initialization failure, with a cold one-page cache and
spill disabled to force repeated reads during schema changes and backfills.
It runs alone in a child test process because SQLite pools cache budgets across
connections; unrelated live caches could otherwise prevent those evictions.
The other registers the VFS before connection construction and uses the shared
public opening path in both `ReadWrite` and `OpenOrCreate` modes with default
cache settings. Connection and commit markers identify header, initialization,
migration-statement, and final-commit reads. Each target is selected by file,
byte range, and occurrence; the successful I/O prefix and exact failed event
must match. SQLite's normal short read when probing beyond the journal's end
is distinguished from the injected `SQLITE_IOERR_READ`, which fails commit.

Before retrying a rejected upgrade, tests compare file bytes, all raw schema
objects and typed table-row bags, integrity and foreign keys, and acquire and
commit a competing write while the retained failed connection remains alive.
They check autocommit, file counts, sidecar cleanup, read-only refusal, and raw
reopening. Public retries and successful upgrades must match the historical
JSON oracle, survive read-only reopen, enforce uniqueness, and maintain migrated
indexes through writes and another reopen. These sweeps join the source-cache
guard so another test's deliberate cache eviction cannot change their traces.

The model uses existing valid local DELETE-journal files, FULL synchronization,
and one failure before native I/O, followed by healthy I/O. Healthy traces must
show no main-file writes before commit. Empty/new-file initialization, allocation
failures, persistent/partial I/O, hot journals on historical files, native
open/close/lock errors, journal
truncation/deletion faults, WAL, and power loss remain outside this model.

## Empty and new-file initialization I/O failures

Under the one-shot read/write/sync model below, failed initialization leaves no
partial Crabnet schema, application marker, or version marker. An existing
zero-byte file remains zero bytes; an empty SQLite database with a valid header
retains its original bytes. Opening a missing path can create the file before
the failing I/O: in these tests, it remains as a zero-byte file after the error.
A failed create does **not** promise to remove the newly created path.

Errors retain `Error::Sqlite`, `ErrorCode::SystemIoFailure`, and the selected
`SQLITE_IOERR_READ`, `SQLITE_IOERR_WRITE`, or `SQLITE_IOERR_FSYNC`. Owned openers
close their file handles, and failed initialization releases locks even when
the connection remains alive. `ReadOnly` and `ReadWrite` still refuse the
uninitialized file. A later public `OpenOrCreate` can retry on that same path
without deleting or replacing it.

Three tests in `src/database/storage_failures/initialization_io.rs` exercise the
shared public opener and initialization routine. The first sweeps missing
paths, zero-byte files, and empty SQLite files through `OpenOrCreate`, registering
the forwarding VFS before connection construction. Registering an absent leaf
canonicalizes its existing parent without creating the file. The second keeps
failed connections alive for both existing empty-file forms, with FULL sync and
cache spill disabled. Each healthy trace supplies every database/journal read,
write, and sync target, selected by operation, byte range or flags, and occurrence.
Connection/commit markers and exact I/O prefixes verify coverage, including
later page writes and final sync after earlier main-file writes. Expected native
short reads from empty headers and journal-end probes are distinguished from
injected errors.

Before retrying, tests compare exact bytes and raw application/version markers,
require an empty `sqlite_schema`, run integrity and foreign-key checks, and
reject leftover sidecars. A zero-timeout competing writer obtains an exclusive
lock, creates and populates a probe table, and rolls it back while the failed
connection is still alive. Bytes and metadata must remain intact. Public retry
must produce the same complete empty schema and typed table contents as a
healthy initialization. Subsequent graph writes, nested properties, parallel
relationships, uniqueness enforcement, index maintenance, backup/restore, and
read-only reopening verify that the resulting graph is usable.

The third test arms a final database-sync fault for a missing path, successfully
initializes another path through the same VFS, fails the selected path after
page writes, then retries it through the same controller without resetting or
rearming. It verifies path isolation, one-shot consumption, rollback to zero
bytes, released handles/locks, and exact successful retry I/O.

These are valid local single-database DELETE/FULL files with no hot journal.
Public opening uses default cache settings; healthy traces must contain no
main-file writes before commit. Faults occur before one native call and all
later I/O is healthy. This does not inject native creation/open/close/lock errors,
allocation failures, persistent/partial I/O, journal deletion/truncation failures,
WAL, filesystem ENOSPC, or power loss.

## Restore I/O failures

For the one-shot destination write/sync fault model above, a failed
`Database::restore_from` rolls back the replacement before returning. The
original graph, identities, constraints, and derived indexes remain usable;
the error retains `Error::Sqlite`, `ErrorCode::SystemIoFailure`, and the extended
`SQLITE_IOERR_WRITE` or `SQLITE_IOERR_FSYNC` code. The backup source remains
unchanged. After the fault clears, the same destination connection can retry.

Two tests in `src/database/storage_failures/restore_io.rs` restore public-API
backups that are strictly larger and smaller than their existing destinations,
with equal page sizes. Each records a healthy restore's destination journal and
database write/sync trace, then fails every event in a fresh copy of the same
fixture. The successful prefix and exact target must match, including later
database writes after earlier replacement pages reached disk. Main-file faults
must be followed by successful rollback page writes. Tests use DELETE
journaling, FULL synchronization, disabled cache spill, and a single error
before the selected native call; subsequent rollback I/O stays healthy.

Before any connection reuse or reopening can hide a partial replacement, the
destination file must match its original bytes. An existing observer then
verifies the original typed graph and exact stored label/property/uniqueness
index rows, and commits a write with zero lock timeout while the failed
destination remains open. The original connection and a fresh opener also
check the graph, constraints, page count, integrity, and foreign keys before
retry. A separate exclusive-lock probe verifies that the source's read snapshot
was released; source bytes and its read-only typed snapshot remain unchanged.

Successful retry replaces the graph and constraints on the original connection
and its existing observer. Tests verify the replacement page count and file
length, covering both growth and shrinkage. Primed prepared statements and
mutation TEMP state remain usable, duplicate keys fail under the replacement
constraint, and a subsequent mutation survives reopening along with the exact
replacement graph. Source invariance is checked after failure, retry, and
subsequent destination writes.

This coverage is limited to destination `xWrite`/`xSync` failures in the stated
configuration. Source-read coverage is described below. It does not inject
database/journal truncation, journal deletion, persistent/partial I/O, or
allocation failures; nor does it qualify differing page sizes, WAL, or power loss. Backup copy/publication faults
have separate coverage below. Resolve storage faults outside this model and
verify the destination before retrying a restore.

## Backup copy and publication failures

For the modeled failures here, `Database::backup_to` leaves no partially
published backup, preserves an existing destination, removes its staging
directory and sidecars, and releases its source snapshot before returning.
SQLite copy failures retain `Error::Sqlite`, `ErrorCode::SystemIoFailure`, and
the extended `SQLITE_IOERR_WRITE` or `SQLITE_IOERR_FSYNC` code. Final file-sync
and publication failures retain `Error::Io` and the original raw OS error code.
A competing publisher that creates the destination before our hard link wins:
our call returns the native `AlreadyExists` error and leaves that file intact.

Five tests in `src/database/storage_failures/backup_io.rs` exercise writable
file, read-only file, and in-memory sources. A private per-call I/O adapter
selects the staging connection and final sync/publication operations; the
public method uses native operations through the same implementation. Fault
controls are not exposed to consumers, and no global hook or default VFS is
changed. Retries use the public `backup_to` method with normal I/O on the same
source handle, retaining an existing destination and choosing a new name when
necessary.

Three sweeps record healthy copies, then fail every staging journal/database
write and sync against both absent and existing destinations. Assertions match
the exact successful prefix and failed event, including faults after earlier
staging pages were written. The forwarding VFS returns one error before native
I/O and leaves subsequent cleanup I/O healthy. The copy uses DELETE journaling,
FULL synchronization, and disabled cache spill. Copy failures must never reach
the final sync or publication boundary.

A final-operation matrix injects a raw OS error before `File::sync_all` and
`fs::hard_link`, also with absent and existing destinations. These checks prove
the staged graph is complete and internally consistent before the final sync,
and that publication is reached only after a successful sync. A separate race
test actually publishes a different valid backup at that boundary, then lets
the native hard link fail. It verifies that the winner's bytes and graph survive
both the failed attempt and the later retry at a fresh name.

Before reusing the source handle, tests verify staging cleanup and autocommit
state. For file sources, they also verify unchanged bytes and another source
writer's immediate commit with zero lock timeout. Typed snapshots preserve graph
identities, parallel edges, nested values, constraints, and exact stored
index/key rows; read-only sources stay read-only. A successful retry is reopened
read-only, restored over an
existing graph, checked for constraints and integrity, and used for a mutation
that survives reopening. Restoring and mutating that destination must leave the
published backup unchanged.

This covers one-shot staging `xWrite`/`xSync` errors, pre-call final sync/link
failures, and the stated publication race. Source-read coverage is described
below. Staging lifecycle failures have their own model below. Filesystem ENOSPC,
persistent/partial I/O, deletion/truncation, WAL, and power-loss guarantees
remain unqualified. In particular, the parent directory is not synced, and
process death may leave unpublished staging files as described in the
[backup contract](#backups-and-process-recovery).

## Backup and restore source-read failures

For the one-shot source-read model here, a failed backup leaves its destination
absent or preserves an existing file, removes staging files and sidecars, and
releases the source snapshot. A failed restore preserves the original
destination bytes, graph, identities, constraints, and derived indexes before
returning. Both operations preserve source bytes and allow a successful public
retry on the original caller-owned handle after the fault clears.

Errors returned directly by SQLite retain `Error::Sqlite`,
`ErrorCode::SystemIoFailure`, and `SQLITE_IOERR_READ`. SQLite's b-tree integrity
checker can instead report a failed read in a result row, for example an
unreadable page with error code 266. Crabnet preserves that diagnostic in
`Error::Data("backup integrity check failed: ...")`; it does not infer SQLite
codes by parsing diagnostic text. Thus a validation diagnostic does not by
itself distinguish persistent corruption from a transient read problem. Either
form of validation failure prevents copying or publishing a snapshot.

Five tests in `src/database/storage_failures/source_read_io.rs` use the private
per-call adapter shared with the public backup/restore implementation and the
test-only forwarding VFS. Four sweeps cover writable and read-only backup
sources, each with absent and existing destinations, and restores that grow or
shrink an existing file. Source opening and initialization complete before
arming. Each healthy operation supplies validation and copy read targets;
tests fail each target's byte range and occurrence within its phase, then
verify exactly one matching `xRead` returned `SQLITE_IOERR_READ` before native
I/O. SQLite's validation scan order can vary between connections, so the read
sweeps do not assume a fixed global event order. All other source and
destination I/O remains healthy. A fifth test proves path isolation and a
successful reread of the failed byte range without resetting the controller.

Tests use DELETE journaling, FULL destination synchronization, disabled
destination cache spill, and equal page sizes. The source cache holds the
fixture, and unpinned pages are discarded before validation and at the copy
boundary without ending the read snapshot. This forces actual disk reads in
both phases. The read sweeps serialize these deliberate cache resets because
bundled SQLite's memory-management configuration shares an eviction group
across connections; other storage tests continue to run in parallel.
Copy-boundary markers prove phase coverage, and destination VFS
observations ensure later read faults occur after copy writes have begun.

Before connection reuse or reopening, tests compare file bytes and check
autocommit state. An exclusive-lock probe proves source snapshot release;
another destination writer can commit immediately with zero busy timeout
while the failed restore handle remains open. Typed graph bags, exact stored
index/key rows, constraints, and integrity are checked before retry. Backup
retries use the same source handle, preserve any existing destination, and
produce a file that can be restored over another graph. Restore retries use
the same destination handle and verify observer visibility, replacement file
size, primed statements and mutation TEMP state. Subsequent mutations and
constraints are verified after closing and reopening the restored graph.

This model covers source reads during snapshot validation and copying.
Opening/initialization reads and hot-journal recovery have separate coverage
below; persistent/partial reads remain unqualified.
Staging lifecycle failures also have separate coverage below. Filesystem ENOSPC,
deletion/truncation, differing page sizes, WAL, and power loss remain unqualified.
In-memory sources have no main-file `xRead` and are outside these read sweeps;
their destination-copy failures remain covered separately above.

## Source opening and initialization read failures

For an existing current-version graph with no hot journal, a source read error
during SQLite connection construction or Crabnet initialization returns
`Error::Sqlite` with the original `SQLITE_IOERR_READ`. The failed opener releases
its file handles and locks and leaves source bytes unchanged. This applies to
`OpenOrCreate`, `ReadWrite`, and `ReadOnly` access to that existing graph; it
does not qualify creation of an empty/new graph or migration of an old one.
When `restore_from` encounters the error while opening its read-only source,
it fails before copying or writing the destination. The destination remains
usable on its original handle, with its graph, indexes, and constraints intact.

Six tests in `src/database/storage_failures/source_open_io.rs` use the same
forwarding VFS as the other storage tests, registered before SQLite opens the
file. A private connector selects that VFS while retaining the public opener's
path/option validation, access flags, timeout, initialization, and function
registration. The native/default VFS and public API are unchanged.

Five sweeps cover all three access modes and restore sources whose contents
grow or shrink the destination. Each healthy opening trace supplies every
main-file `xRead` target by byte range and occurrence, separately for connection
construction and initialization. One selected call returns `SQLITE_IOERR_READ`
before native I/O; later I/O stays healthy. A connection-return marker proves
phase coverage, and tracing stops when opening returns, so a later validation
or copy read cannot accidentally satisfy an opening fault. The source traces
contain no writes or syncs, and failed restores have no destination write/sync
events or copy call. These sweeps share the source-read cache-reset guard to
avoid cross-test cache eviction changing the measured reads.

Before public retry, tests count live wrapped file handles, acquire an immediate
exclusive source lock, compare exact file bytes and typed graph/index/constraint
snapshots, check integrity, and reject unexpected staging files or sidecars.
Standalone retries use the original access mode, produce restorable backups,
preserve read-only enforcement, and verify writable mutations after reopening.
Restore retries retain the original destination handle and check observer
visibility, replacement page count/file size, constraints, and writes surviving
reopen. The sixth test checks path and native-VFS isolation and retries through
the same VFS without resetting its controller, proving one-shot consumption of
the opening header fault.

This model uses existing current-version local files with DELETE journaling,
no hot journal, and mmap disabled by the test VFS. It covers actual `xRead`
failures, not synthetic open API errors. It does not inject native `xOpen`,
lock, or descriptor-close failures. Hot-journal recovery has its own model below,
and [migration reads](#migration-read-failures) are qualified separately.
[Empty/new-file initialization](#empty-and-new-file-initialization-io-failures)
also has a separate model. Persistent/partial reads, WAL, filesystem ENOSPC,
and power loss remain outside this qualification.

## Hot-journal recovery I/O failures

Opening an existing graph with `ReadWrite` or `OpenOrCreate` lets SQLite replay
a hot DELETE journal before Crabnet inspects schema metadata. Under the one-shot
fault model below, failed journal reads, recovery writes, and syncs preserve the
original `SQLITE_IOERR_READ`, `SQLITE_IOERR_WRITE`, or `SQLITE_IOERR_FSYNC` in
`Error::Sqlite`. The failed opener closes its files and releases its locks.
The journal stays intact for retry, but the main file may already contain
restored pages or have been truncated to its original size. A failed recovery
does not promise that the main file stays byte-for-byte unchanged.

Keep the main file and its journal together. Once I/O works again, retry a
writable open so SQLite can finish recovery; do not remove the journal to force
the graph open. Even when all replay writes finished before a sync failure,
the retained hot journal still prevents read-only access. With healthy journal
detection, `ReadOnly` returns `SQLITE_READONLY_ROLLBACK`; an injected detection
read error retains `SQLITE_IOERR_READ` instead. Restore opens its source
read-only and never silently recovers it using write access. A refused restore
leaves both source files and its destination unchanged. Explicitly recover the
source with a writable open, then retry restore on the same destination handle.

Three tests in `src/database/storage_failures/recovery_io.rs` build a real hot
journal by killing and reaping a synchronized child writer without destructors.
The writer changes typed nodes, parallel relationships, labels, properties,
deletions, and constraints in one uncommitted transaction. A small page cache,
enabled spill, and larger inserted payloads force writes to existing pages and
file growth. The parent checks the changed original-page bytes, increased file
size, and a valid journal header containing records before termination. It
retains the resulting main/journal pair and makes a fresh identical copy for
each recovery attempt.

The writable sweep records a healthy open in each access mode, then fails every
journal read/sync and main-file recovery write/sync in those traces before the
native call. Targets include file kind, operation, byte range or sync flags,
and occurrence. Each failure must match the exact healthy prefix and selected
extended error; later cleanup I/O remains healthy. Coverage includes journal
reads after earlier replay writes. Main-file reads during ordinary opening have
the separate model above. The recovery sweeps share the source-read cache guard.

Before retry, tests check live wrapped-file counts, the entire retained journal,
and the exact partially replayed main-file image reconstructed from successful
page writes and permitted native truncation. An exclusive-lock probe uses native
file locks directly and verifies that neither file changes; running `BEGIN`
instead would trigger recovery and could conceal the failed state. Public retry
must restore the exact pre-transaction bytes, file size, typed graph bags, and
label/property/unique-key indexes and constraints, and remove the journal.
Integrity, backup/restore, subsequent mutations, and read-only reopen are checked.

A second test covers read-only opening and restore source refusal, both with
healthy detection and a journal read fault. Destination tracing proves that
restore never starts copying or writing. It checks retained source files,
destination bytes/graph/constraints, released locks, separate writable source
recovery, and public restore retry with observer visibility and writes surviving
reopen. The third test proves path isolation and one-shot journal fault
consumption by retrying through the same VFS without resetting its controller.

This model uses current-version, single-database local files, DELETE journals,
FULL synchronization, and a VFS without mmap or WAL. All injected failures occur
before one native read/write/sync; remaining I/O is healthy. Native journal
deletion and file truncation occur but are not faulted. Native open/close/lock
errors, persistent or partial I/O, damaged journals, attached-database recovery,
a second process interruption during recovery, filesystem ENOSPC, and power loss
remain unqualified. These tests do not establish corruption repair.

## Backup staging lifecycle failures

Staging directory/file creation, SQLite open, and explicit SQLite close failures
prevent publication. The source and any existing destination remain unchanged;
the source snapshot is released before return. Filesystem errors retain
`Error::Io` and their raw OS code, and SQLite errors retain their structured
code and diagnostic. A later cleanup error does not replace that primary error,
including a native `AlreadyExists` publication error.

Cleanup attempts file removal followed by recursive directory removal, even
when file removal fails. If directory removal succeeds, it also removes any
remaining staging file and sidecars. If directory removal fails, an empty or
partially cleaned directory can remain. After publication, cleanup errors leave
the return value successful and the final backup intact. Neither a retry nor
opening a database scans for or removes staging directories from earlier calls.
Remove an inactive leftover directory after resolving its removal problem;
do not modify its files, since the staging main file may still be a hard link
to the published backup.

Five tests in `src/database/storage_failures/backup_lifecycle.rs` extend the
private per-call I/O adapter used by the public method. The matrix covers
writable, read-only, and in-memory sources, with absent and existing
destinations. It injects errors before directory/file creation, SQLite open,
and explicit SQLite close. Separate file/directory cleanup failures are tested
individually and together after setup, copy, close, sync, and publication errors,
and after successful publication. The copy error is a one-shot VFS write fault;
the existing-destination error comes from a real native hard link. Healthy
controls cover ordinary success and no-overwrite errors.

Step traces prove operation and cleanup order, including skipped operations
after a failure. Tests compare source and protected destination bytes before
source reuse, verify autocommit and immediate exclusive-lock acquisition on
file sources, and check exact typed graph/index/constraint snapshots. Cleanup
uses an inert sidecar marker to check recursive removal and exact remaining
files. Public retries use the same source handle, preserve earlier remnants,
and produce backups that restore over an existing graph with constraints and
subsequent writes surviving reopen. Published success is checked even when
both cleanup operations fail; later native removal of the leftover directory
must preserve the final backup. Four panic points before publication verify
that native RAII cleanup and source lock release still work during unwinding.

This is an API-boundary model: creation/open/close errors precede the selected
native operation, and the close-error path still drops its connection through
a healthy native close. Cleanup errors skip the selected removal, disable the
temporary object's automatic removal, and release its file handle normally.
It does not inject OS descriptor-close failures, partially completed creation
or recursive removal, hostile path replacement, persistent SQLite I/O failure,
filesystem ENOSPC, or power loss. Such failures require separate qualification;
the source-read and destination write/sync models above retain their own limits.

## Reusable compiled reads

`compile_read` returns an owned, parameter-unbound `CompiledRead`. Compilation
reports static errors and rejects mutations; it does not access a database.
`columns()` is available immediately. `compile_read_with_options` checks
cancellation and deadlines before and after successful planning, without
preempting parser/planner operations or retaining the options. Execution budgets
do not cap planning or parameter memory.

`Database::execute_compiled*` and `Transaction::execute_compiled*` resolve fresh
parameters through the complete plan before executing it, and start fresh
resource counters with the supplied options. They skip parsing, lowering, and SQL
generation but still bind values and prepare/reuse SQLite statements. Results
are eager: an error returns no partial output. Standalone calls share a new read
snapshot across all branches, traversal, and hydration, and release it on
return. Transactional calls use the parent snapshot; any execution-call error
aborts all earlier writes. Free-standing compilation errors cannot abort a
transaction. Retaining a compiled plan or a returned result holds no locks.

Plans can be shared across independent connections from the same engine build,
including read-only or normally migrated databases. No stored data or installed
constraint influences compilation, so supported writes, constraint changes, and
restore do not invalidate a plan. There is no plan serialization or cross-version
plan compatibility contract. External schema edits remain unsupported. See
[compiled reads](compiled-reads.md) for the full contract and measurements.

## Read cursors

`Database::prepare_read`, `prepare_read_with`, and `prepare_read_with_options`,
and the equivalent `Transaction` methods, return a `PreparedRead` that exclusively
borrows its database or transaction. Planning and
whole-tree parameter resolution finish before this returns; query text,
parameters, and options are owned by the preparation and can be dropped by
the caller. Writes return `Error::Unsupported` without executing. This API is
available on writable and read-only databases and inside an application
`Transaction`. Rejecting a mutation inside a transaction aborts earlier writes,
as for every other query error.

`PreparedRead::columns()` exposes ordered column names. `mode()` makes the
execution strategy explicit before starting:

- `ReadMode::Incremental`: a SQLite fragment or eligible single Rust trail
  projection, or a tree of those fragments with UNION/UNION ALL, semantic DISTINCT,
  pagination, and root path/list hydration. Each iterator advance produces at
  most one output row, but may decode multiple inputs while discarding duplicates
  or skipping rows.
  Scalar values, whole entities, nulls, nested values, and duplicate
  rows use the same decoding as eager execution. SQLite itself can materialize
  sorts, aggregates, intermediate `WITH` stages, JSON arrays, and recursive
  traversals before producing the first row. Each individual value is decoded
  in full; this is not a whole-query memory cap.
- `ReadMode::Buffered`: the existing eager executor runs at cursor creation.
  Ineligible Rust traversal shapes use this mode. A union with any such boundary
  uses it for the entire query. All rows and resource checks finish before the cursor
  is returned; the cursor then owns the result buffer. This fallback preserves
  the existing operator order and query-wide hydration behavior.

Eligible Rust traversal is one MATCH with a single deep/open-ended relationship
and its two endpoint nodes, followed by non-aggregating, unordered projections
and filters, including chained UNWINDs and WITH DISTINCT/pagination between
expansions.
Filters, path/relationship-list outputs, DISTINCT, and final pagination compose
incrementally. Each trail produces at most one consuming SQL row without UNWIND,
or one per surviving member with it. Additional joins, multiple producers,
OPTIONAL MATCH, global sorting or aggregation,
and shortest-path producers retain buffered execution. WITH aggregation or
ORDER BY, or a stage before the initial MATCH, also retains buffering. Eager
execution and its SQL pagination are unchanged. Unordered pagination selects an
unspecified subset; no cross-mode ordering guarantee is added.

The producer starts on first demand, loading endpoint domains, then yielding
singleton JSON relations from a resumable trail walk. A same-clause start label
or inline property enables indexed neighbor probes, caching each expanded
node's ordered eligible steps, including empty lists. Unconstrained starts,
eager execution, and buffered cursor shapes retain whole-edge loading.
Neighbor probes preserve direction, parallel-edge multiplicity, and one copy
of each undirected self-loop. They stop at hop bounds and early exit; zero-hop
rows need no neighbor probe. Domains, cached neighbors, and frontier remain in
memory until producer completion or drop. A high-degree expansion loads the
whole eligible neighbor list. Transferred edge rows and retained keys/steps
use the existing query budgets. Relationship uniqueness bounds open-ended
walks without loading a global edge count; an impossible minimum depth can
therefore consume explored-state budget before returning no rows. A
reached page limit stops enumeration; LIMIT 0 starts no producer. Final ordinary
SKIP suppresses discarded RETURN expressions while still charging matching
placeholder rows as SQL transfers. DISTINCT pagination retains projection
evaluation before deduplication. Hydration retains its existing read-ahead/error
boundary. Plain WITH stages retain per-row typed values and evaluated aliases;
their computations still run for rows skipped by final pagination. A later
WITH or member projection error can follow already-yielded rows, and early exit
can leave suffix errors unobserved. Each UNWIND freezes its live input bindings
and encoded list once per incoming row; following projections run once per member.
Inner expressions can depend on outer members. Resumption preserves ancestor
values and volatile aliases, with one frozen row/list per active expansion.
Exhausted frames release their lists and carried bindings. Null members retain
their types, while empty/null/non-list inputs produce no rows; an empty inner
list still admits a frozen input transfer, whereas a filtered input does not.
Bare path-list expansion retains identities and reads member entities on demand.
List construction still completes before the first member; a large single
list or input row can consume substantial memory, and nesting retains multiple
such values. Every stage's frozen row and list count as an additional SQL
transfer, before copying, followed by each consumer row;
these values are outside the retained traversal-memory counter. See the
[traversal cursor guide](cursors.md#incremental-rust-traversal).

Each intermediate WITH page owns offset/limit counters across all incoming
trails and UNWIND members, rather than resetting them for each parent. WHERE
runs before pagination. Without DISTINCT, SKIP suppresses this WITH's projection,
but not its filter or preceding stages. A matching skipped row transfers a placeholder;
an admitted row transfers its frozen projected bindings. Both count against
SQL transfer limits before copying; filtered inputs do not. Admitted rows
remain counted even when later filters or empty lists produce no output.
At a reached limit the cursor drains downstream expansions for the last
admitted row, then stops all upstream work. LIMIT 0 anywhere in the eligible
pipeline avoids producer execution, while all parameters/pages still preflight.
Page frames retain one row and counters, sharing snapshot, cancellation,
hydration, error/rollback, and cumulative resource rules with other stages.

WITH DISTINCT evaluates the projection once before its modifiers, preserving
incoming variables for WHERE and dropping them before constructing the key.
Only projected columns participate in typed semantic equality. Each stage's
seen set spans trails and parent lists, retains the first encountered
representative, and is independent of other stages and UNION branches. Path,
node-list, and relationship-list keys compare their respective ordered
identities even when carried in the same encoded column; compact path keys
are compared before hydration. SKIP/LIMIT count unique rows, with skipped
keys retained to reject subsequent duplicates. DISTINCT projections still
evaluate on skipped inputs. Duplicate and skipped matching rows spend SQL
transfer budgets before decoding/copying but do not count as final outputs.
Retained seen rows grow with key count and size, outside the traversal-memory
counter. Completion/error/drop releases them; each new cursor starts empty.
Eager window ranking and incremental first encounters can select different
unordered page subsets. No cross-mode ordering guarantee is introduced.

Incremental `UNION ALL` visits branches from left to right and preserves bags,
branch-local ordering, and each branch's output shapes. Empty branches are
skipped when advancing. All consuming branch statements are prepared and their
static parameters resolved before fetching rows; SQLite execution of a branch begins only when
reached. This retains statements and bindings proportional to branch count and
parameter size, without collecting the union's output in Rust. A later branch's
runtime error may follow rows yielded by earlier branches. Planning and missing
parameter errors in every branch still fail preparation.

Incremental DISTINCT preserves the first encountered representative using the
same recursive row equivalence as eager execution, including nulls, exact
integer/float equality, and ordered graph identities. Randomized hash buckets
narrow candidate comparisons; collisions are checked with that equivalence.
Each active distinct operator retains representative pending rows: compact
identities for deferred paths/lists and full values, including property payloads,
for materialized cells. Its memory grows with the count and size of unique rows.
Branch-local seen sets are released when the branch finishes; consecutive
distinct UNION branches share one union-level set. These allocations are not
a new row transfer and are not separately charged as result bytes; resource
controls remain transfer/work/output limits rather than a total heap cap.

Branch-local DISTINCT and pagination run before the union's duplicate policy.
SKIP and LIMIT after DISTINCT count unique rows. Reaching a cursor's LIMIT
stops requesting input from that operator, and LIMIT 0 does not step its input.
Early completion can therefore avoid a runtime error that eager execution
would encounter while draining the input before pagination. SQLite can still
materialize or sort internally, so this does not promise to avoid all suffix
work inside a SQLite fragment. Parameter and pagination validation in all
branches still finish during preparation, including zero-length pages.

For SQLite-produced paths, `nodes(p)`, `relationships(p)`, and legacy
relationship lists, root hydration fills its first batch with one surviving
pending row and later batches with up to 64. Distinct, branch-local pages, and
union processing precede batching. Each batch deduplicates required IDs and
seeds its cache from directly projected entities, then issues at most one node
query and one relationship query. Nodes-only lists do not load relationships
and vice versa. The prior cache is dropped before fetching the next batch.
Paths preserve traversal order, repeated nodes, stored relationship direction,
and graph identity. Fully materialized cells pass through unchanged.

This bounds batch row count, not bytes or whole-query memory. Individual
paths/lists can be large, SQLite can buffer internally, and distinct retains
unique rows. Only pending values and their entity cache are buffered for the
batch; each output row is cloned on demand, after its final row/byte admission.
All source reads and hydration queries use the same pinned snapshot and
cumulative resource context. The cache does not grow across batches: repeated
IDs are reloaded and charged again. Eager execution keeps query-wide reuse,
so the same transfer/work budget can succeed eagerly and fail incrementally.

Batch read-ahead can cross union branches and can consume up to 64 surviving
rows after the first row, plus any input duplicates or skipped rows. An error
while filling or loading a batch discards the entire batch and immediately
closes the cursor. An error during row admission or hydration closes it after
any previously yielded rows. Early drop avoids further reads, but does not
undo work already performed for the current batch. Batch sizes are execution
policy, not configurable query options.

For a standalone prepared read, `PreparedRead::cursor()` starts a new deferred
transaction and pins the main database snapshot before returning, even when no
row has been requested yet.
Preparation alone holds no snapshot. An incremental cursor keeps the snapshot
until it is exhausted, errors, or is dropped, including early drop without any
advances. A buffered cursor releases it before returning. In rollback-journal
mode an active read snapshot can block another connection's commit; callers
should drop idle cursors promptly. Dropping after the last row releases the
snapshot without requiring an extra advance to `None`.

A prepared read created by `Transaction::prepare_read*` instead uses its
parent's existing immediate transaction, snapshot, and earlier writes. It does
not open a nested transaction, create a savepoint, or perform the standalone
snapshot-pinning metadata query. Successful exhaustion, early drop, or dropping
an unconsumed cursor closes its statements without ending the parent transaction.
This holds for buffered reads too. The parent's write reservation and locks
remain until commit, rollback, drop, or a query error; closing a transactional
cursor is not a way to release them.

`ReadCursor` implements `Iterator<Item = Result<Vec<CypherValue>>>` and
`FusedIterator`; rows are owned and remain usable after the cursor and prepared
handle are dropped. A runtime, resource, cancellation, or deadline error can
follow already yielded rows. The error is yielded once and subsequent calls
return `None`. For standalone reads, exhaustion and failure release the snapshot
immediately, even if the caller retains the terminal cursor. Cleanup resets all SQLite statements
before rolling back the read transaction, with no progress callback active.
Ordinary drop and panic unwinding use the same order.

For transactional reads, preparation or cursor-creation errors also roll back
the parent, including all earlier writes. Iteration errors first reset the row
sources and discard caches, then roll back with no progress callback active.
The parent is marked permanently aborted before rollback is attempted. If
SQLite already rolled back, cleanup skips a redundant ROLLBACK. The original
query error is preserved unless rollback itself fails. All this occurs before
returning the error, even if the caller retains the failed cursor/prepared read
and parent handles. Iterators remain exhausted; subsequent `cursor()` calls,
query execution, constraint operations, and commit return `Error::TransactionAborted`.
Explicit rollback on an already aborted parent is a successful no-op.

Previously yielded rows remain owned values, but writes they describe have
been undone. External application side effects from processing those rows are
outside the transaction. A consumer panic that unwinds only the cursor/prepared
read leaves the parent active, like ordinary early drop. If the parent also
unwinds, its transaction guard rolls back.

Only one cursor can borrow a prepared read at a time. Once it is dropped,
`cursor()` can run again with the same parameters/options and new counters.
Standalone reads get a fresh snapshot; transactional reads reuse the parent's
snapshot and require a parent that has not aborted. Drop the prepared read
before using its `Database` or `Transaction` again, including commit/rollback.
The borrow rules enforce both restrictions. Options retain their absolute
deadline and shared cancellation token across repeated cursors.

Incremental row-transfer and result limits accumulate over advances and branches.
Transfer limits count every fetched input and hydration record, including
duplicates and skipped rows; result limits count only rows surviving distinct,
pagination, and union.
A limit fails on the first disallowed row, before exposing it; earlier rows remain
visible to the caller. Early drop does not execute or validate any suffix beyond
the current hydration batch. SQLite VM steps accumulate across snapshot pinning,
branches, hydration queries, and advances; the metadata read used to pin the
snapshot is excluded from row/byte counters.
Buffered plans keep eager accounting, including all branches and hydration,
and also check cancellation/deadline when yielding their buffered rows. There
is no background deadline timer: an idle cursor keeps its lock until it is
resumed or dropped. All other [resource boundaries](#query-resource-controls)
still apply. See [examples and measurements](cursors.md).

## Query resource controls

`Database::execute_with_options(cypher, parameters, options)` and the equivalent
`Transaction` method accept `QueryOptions`. Existing execution methods delegate
with default options. Each call gets a fresh budget; all UNION branches,
mutation stages, computed write values, MERGE reads, traversal producers, and
hydration within that call share it.
Prepared reads accept the same options and reset counters for each cursor;
incremental accounting and partial-error behavior are described above.

`TraversalLimits` preserves the defaults of 100,000 explored states, 128 MiB
serialized traversal payload, and 256 MiB retained traversal accounting. All
three limits are configurable, and zero is an actual limit. The state and
payload counters are cumulative; retained memory is released as producers
finish or are dropped. Incremental trail projections count singleton-array
framing in their cumulative payload bytes, retain graph/frontier charges while
paused, and stop enumeration on early exit beyond hydration read-ahead. These traversal-specific counters apply to Rust-backed traversal,
not the bounded recursive SQLite strategy.

The remaining limits default to `None`:

- `deadline` is an absolute monotonic `Instant`. A past deadline fails before
  execution. It remains absolute when the same options are used again.
- `cancellation` accepts a clonable, thread-safe `CancellationToken`. Calling
  `cancel()` permanently cancels that token and its clones. A later independent
  query needs a new token or options without that token.
- `max_sqlite_steps` limits progress-callback work across SQLite statements in
  one query. The callback runs at SQLite's one-VM-instruction interval when this
  limit is set, adding overhead. This is a work guard, not a stable instruction
  count across SQLite versions, query plans, or statement-cache state. Transaction
  begin/commit/rollback are outside the query's progress handler.
- `max_materialized_rows` and `max_materialized_bytes` count cumulative row
  transfers at query fragments, mutation row sources, traversal input reads,
  and hydration reads. The bytes are the encoded UTF-8 cells (including JSON
  quotes and escapes). Rows are charged before copying their cells into Rust.
  Repeated reads count again, including internal computed-value and MERGE reads;
  DISTINCT, pagination, and producer completion do not refund these counters.
  Metadata and individual mutation property-refresh lookups are outside these
  row-transfer counters, as are SQLite's internal aggregates, sorts, CTEs, and
  temporary storage, Rust allocator overhead, parameters, and planner memory.
- `max_result_rows` and `max_result_bytes` apply to the final result after
  DISTINCT, pagination, and UNION, including post-write RETURN. Internal write
  reads are not final results. A write with no RETURN has zero result rows and
  bytes. Exceeding a result limit fails rather than returning a truncated bag.

Result bytes use deterministic value accounting, not JSON size or allocator
usage. A row costs 8 plus its values. Every value costs 8, with these additions:
strings add their UTF-8 length; lists add their values; maps add each key's
UTF-8 length and value. Nodes add their ID bytes, an 8-byte label container
with each label counted as a string value, and an 8-byte property container
with keys and values counted as a map. Relationships add their ID, type, and
endpoint ID bytes and the same property-container cost. Paths add two 8-byte
containers holding complete node and relationship values. Every occurrence
counts, including repeated graph identities. Column names and summary counters
are excluded. Hydrated result costs are checked before cloning snapshots into
output paths and lists. These budgets do not promise a total process-memory cap.

Cancellation and deadlines are cooperative. Checks run around planning, between
Rust work units, and through SQLite's progress callback (every 1,000 VM
instructions when only time/cancellation controls are enabled). A parser call,
individual scalar/aggregate callback, allocation, or lock/I/O wait cannot be
preempted by these checks. An absolute deadline is not a hard wall-clock bound.

Budget exhaustion returns `Error::ResourceLimit`, cancellation returns
`Error::Cancelled`, and deadline expiry returns `Error::DeadlineExceeded`.
Progress callbacks are removed before commit or rollback, including on panic
unwinding. Failure rolls back a standalone statement or aborts the entire
explicit transaction, including earlier successful statements; cleanup remains
able to execute after the query has exhausted its budget. The database can be
used again after the transaction handle is consumed or dropped. The `execute`
methods return eager results; read cursors have the lifetime contract above.

## Unsupported constructs and errors

Crabnet parses a statement before deciding whether its constructs are in the
supported slice. A syntactically valid but unsupported construct fails during
planning with `Error::Unsupported`; it is never silently approximated.
Malformed input uses `Error::Parse`, and supported syntax with invalid names,
entity kinds, or statically wrong value kinds uses `Error::Semantic`. During
execution, a value of the wrong kind fails with `Error::Type`, a function
argument out of its domain with `Error::Argument`, and a write that would
break the graph, such as a uniqueness violation or a plain `DELETE` of a node
that still has relationships, with `Error::Constraint`. Planning completes
before mutation execution, so an unsupported write statement cannot partially
modify the graph.

Supported queries can fail with `Error::ResourceLimit`, `Error::Cancelled`, or
`Error::DeadlineExceeded` under the resource controls above. These are explicit
execution failures; Crabnet does not silently truncate results. An incremental
cursor may already have yielded earlier rows before reporting its error.

Semantics for unsupported constructs—including modern path selectors,
expression-position shortest-path calls, other path expressions beyond the
three path functions above, optional-bound paths or relationship lists,
postfix graph quantifiers,
the deferred legacy shortest forms listed above, and quantified optional
matching—remain unresolved until those constructs are designed and
implemented.
