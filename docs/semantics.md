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
graph values remain compact identities until final hydration.

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
use SQLite's numeric promotion. If any non-null input has another kind, the
result is null instead of being coerced. `min` and `max` ignore nulls and
choose by the same global orderability as `ORDER BY`, so `max` over numbers and
strings is a number and `min` over a list and strings is the list. `collect` omits nulls, preserves
duplicates, and returns an empty list for empty input. Ordering established by
an immediately preceding `WITH ... ORDER BY` controls collection order.

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
or storable properties. Indexing, slicing, comprehensions, nested collections,
other scalar operations, and further aggregation of entity lists remain
unsupported; expand with `UNWIND` before operating on their entities.

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
comprehensions, and the filters of updating statements, where it sees the
graph before the statement's writes. `exists(pattern)` is the same test and
`exists(n.prop)` is `n.prop IS NOT NULL`. A pattern without a relationship, a
pattern that introduces a new variable, a path binding, a variable-length
relationship, and relationship-type alternatives are rejected.

Nodes and relationships compare by identity: `a = b` is true when both name
the same stored entity and `a <> b` when they differ, null when either side
is null, and `r IS NULL` tests an entity the same way. An entity compared
with a non-entity value is not equal, including its own `elementId()` string.
Node and relationship identities remain distinct even when their IDs match.
Entities can be collected and then expanded with `UNWIND`. They still cannot
be ordered directly or placed in literal lists.

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
empty and Unicode strings. If either operand is null, missing, or not a string,
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
Aggregates are rejected inside the predicate. A null or non-list operand
produces null; Cypher raises a type error for a non-list. Predicates that
apply an operator to an element of the wrong kind follow the operator's type
checks; non-boolean predicate results are type errors.

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
properties, and parameters, and are ordinary list values afterwards, so they
compose with `size`, quantifiers, `IN`, `UNWIND`, and equality. Aggregates are
rejected inside either part. A null or non-list operand produces null; Cypher
raises a type error for a non-list. Lists of graph entities are not scalar
values, so comprehensions over them are rejected.

`size(list)` is the element count and `size(string)` the character count.
`size(null)` is null, and so is `size` of any other kind of value, where Cypher
raises a type error. `size` of a path or graph entity is a semantic error;
paths use `length`.

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

`+` is also concatenation: two strings join, two lists join, and a list beside
a non-list value appends or prepends that value as one element. A null on
either side, or any other pairing such as a string with a number, produces
null; Cypher raises a type error for the latter.

## List indexing, slicing, and `range()`

`list[i]` reads the element at integer index `i`, counting from the end when
negative, and `map[k]` reads the string key `k`. `entity[k]` reads the property
named by a runtime string. An out-of-range index, a missing key, or a null base
or index reads as null. A non-integer list index, a non-string map key, or a
base that is neither list nor map is a type error. Indexing a path or path collection is a semantic error.

`list[lower..upper]` copies the elements from `lower` (inclusive, default 0)
to `upper` (exclusive, default the length). Negative bounds count from the
end and both bounds are clamped, so an inverted or exceeding range yields a
shorter list rather than an error. A null bound, a non-integer bound, or a
non-list base produces null.

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

## Scalar functions

`keys()`, `properties()`, and the conversion functions reject an argument of a
kind they cannot take with a type error: `toInteger()` and `toString()` a list
or map, `toFloat()` a boolean, list, or map, and `toBoolean()` a float, list,
or map. The other scalar functions still return null for an argument of the
wrong kind, where Cypher raises a type error. Function names are
case-insensitive, and calling one with the wrong number of
arguments is a semantic error.

- `coalesce(a, b, ...)` returns the first non-null argument.
- `keys(x)` returns the property names of a node, relationship, or map as a
  list, and `properties(x)` returns them as a map. Stored property maps and
  map literals keep their keys sorted, so the order is alphabetical. An
  entity never stores a null property (see "Values and properties"); map
  values may hold null, so the map forms keep such entries.
  Both functions read a null entity (from `OPTIONAL MATCH`) as null.
- `head(list)`, `last(list)`, and `tail(list)` are `list[0]`, `list[-1]`, and
  `list[1..]`, with the same null rules.
- `reverse(x)` reverses a string or a list.
- `toInteger`, `toFloat`, `toString`, and `toBoolean` convert between
  numbers, strings, and booleans. `toInteger` truncates floats toward zero,
  parses integer or float text, and maps booleans to `1`/`0`; `toFloat`
  parses numeric text; `toString` renders numbers and booleans; `toBoolean`
  accepts booleans, the strings `true` and `false` in any letter case, and
  integers (zero is false). Anything else, including text that does not
  parse, is null.
- `toLower`, `toUpper`, `trim`, `ltrim`, `rtrim`, `substring(s, start[,
  length])` (zero-based; a negative position is null), `left(s, n)`,
  `right(s, n)`, `replace(s, search, replacement)`, and `split(s, delimiter)`
  operate on strings; `split` with an empty delimiter splits into characters.
  Case conversion is ASCII-only.
- `abs` keeps the argument's numeric type and `sign` returns an integer;
  `ceil`, `floor`, `round`, `sqrt`, `exp`, `log`, `log10`, `sin`, `cos`,
  `tan`, `asin`, `acos`, and `atan` return floats, and a non-finite result
  such as `sqrt(-1)` is null. `pi()` and `e()` are constants.
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
selectors on a path, plus raw path use as a scalar, aggregate, grouping, or
ordering expression, remain planning-time rejections. The typed lists from
`nodes` and `relationships` retain the expression-context restrictions above.
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

The complete read—runtime traversal producers, every branch SQL fragment, and
union evaluation plus both possible hydration queries—shares one deferred
SQLite read snapshot. Concurrent external-writer semantics beyond that snapshot
remain a non-goal.

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
relationships. `SET entity.key = value` assigns a value and `REMOVE entity.key`
deletes that key. `SET entity = map` replaces the complete property map, while
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

Matched `DELETE` accepts node and relationship variables. Targets are gathered
and deduplicated across the complete materialized match bag, then explicitly
targeted relationships are deleted before nodes regardless of target spelling
order. Plain deletion of a node that still has an incident relationship fails
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

Declarations and their keys persist in schema version 4. Opening main's
versions 1, 2, or 3 upgrades atomically, retaining existing data and installing
no constraints by default. Failed migrations leave the previous schema and
version marker intact. Triggers enforce uniqueness under SQLite's writer
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
on drop. Applications must handle the error rather than assuming persistence.

Each call completes planning and parameter validation before that query's
execution, although the outer transaction is already open. Read results are
owned snapshots; `QuerySummary` reports only the current query's changes.
Both remain provisional until the application transaction commits: retaining
a returned node or summary does not preserve a write that was rolled back.
Resource budgets reset for each query, while all operators within a
query continue sharing that query's budget. Standalone execution retains its
existing per-statement transaction and preflight behavior.

## Query resource controls

`Database::execute_with_options(cypher, parameters, options)` and the equivalent
`Transaction` method accept `QueryOptions`. Existing execution methods delegate
with default options. Each call gets a fresh budget; all UNION branches,
mutation stages, computed write values, MERGE reads, traversal producers, and
hydration within that call share it.

`TraversalLimits` preserves the defaults of 100,000 explored states, 128 MiB
serialized traversal payload, and 256 MiB retained traversal accounting. All
three limits are configurable, and zero is an actual limit. The state and
payload counters are cumulative; retained memory is released as producers
finish. These traversal-specific counters apply to Rust-backed traversal,
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
used again after the transaction handle is consumed or dropped. Results remain
eager, with no exposed cursor or additional snapshot-lifetime contract.

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
execution failures; Crabnet does not return a truncated result.

Semantics for unsupported constructs—including modern path selectors,
expression-position shortest-path calls, other path expressions beyond the
three path functions above, optional-bound paths or relationship lists,
postfix graph quantifiers,
the deferred legacy shortest forms listed above, and quantified optional
matching—remain unresolved until those constructs are designed and
implemented.
