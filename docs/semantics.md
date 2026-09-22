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
applied after ordering when an order is present. The same rules apply to a
`RETURN` after a mutation, which runs as a read over the post-mutation rows.

## `WITH` clause boundaries

`WITH` may begin a query, appear more than once, separate `MATCH` clauses, or
sit between reading and updating clauses. Each `WITH` consumes the current row
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
a larger projection expression, and aggregate-only ordering/filtering without
an aggregating projection are structured planning-time rejections; a `RETURN`
after a mutation may aggregate over the post-mutation rows. Aggregates are not allowed in `MATCH` predicates or
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
Supported `AND`, `OR`, `XOR`, and `NOT` expressions then propagate those
values with three-valued boolean logic; `XOR` is null when either side is
null and otherwise true when the sides differ.

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

For the current implementation, same-kind list and map equality ultimately
uses their stored JSON representation rather than recursively normalizing
numeric members. Consequently, `[1] = [1.0]` and `{x: 1} = {x: 1.0}` are
false, while `[null] = [null]` and `{x: null} = {x: null}` are true. `IN`
inherits the same member-equality rule. The finite predicate oracle in
`tests/predicate_truth_tables.rs` deliberately locks this current contract.
Full Cypher structural equality rules, including whether map key order must be
ignored in every storage situation, remain unresolved.

Ordering comparisons are deliberately narrower than SQLite's storage-class
ordering. Integers and floats are mutually comparable as numbers; strings are
comparable only with strings; and booleans are comparable only with booleans.
A null or missing operand, or any incompatible or unsupported pair of value
kinds, produces null. Crabnet does not coerce text to numbers or booleans to
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
with a non-entity value is not equal. Entities still cannot be ordered,
collected, or placed in lists.

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
apply an operator to an element of the wrong kind follow the engine's general
rule and produce null where Cypher raises a type error.

## Map key access

`value.key` on a non-entity value reads a key of a map: a projected or
parameter map, a nested property such as `n.meta.tag`, a parenthesized
expression, or a quantified list element. A missing key, a key holding null,
and a null map all read as null. Reading a key of a non-map, non-null value
also produces null; Cypher raises a type error. Keys are case-sensitive and
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
null map values, which differs from stored properties where explicit nulls
are dropped. Both forms compare, group, and order identically.

## Numeric arithmetic and computed projections

Read predicates and projections support binary `+`, `-`, `*`, `/`, and `%`,
plus unary `+` and `-`, with the parser's normal precedence and left
associativity. Integer-only operations retain integer results, including
truncating integer division, while an operation with a float operand produces
a float. Null, missing, boolean, string, list, or map operands produce null;
Crabnet never dynamically coerces them to numbers.

`+` is also concatenation: two strings join, two lists join, and a list beside
a non-list value appends or prepends that value as one element. A null on
either side, or any other pairing such as a string with a number, produces
null; Cypher raises a type error for the latter.

## List indexing, slicing, and `range()`

`list[i]` reads the element at integer index `i`, counting from the end when
negative, and `map[k]` reads the string key `k`. `entity[k]` reads the property
named by a runtime string. An out-of-range index, a missing key, a null base
or index, a non-integer list index, a non-string map key, or a base that is
neither list nor map all read as null; Cypher raises a type error for the
kind mismatches. Indexing a path or path collection is a semantic error.

`list[lower..upper]` copies the elements from `lower` (inclusive, default 0)
to `upper` (exclusive, default the length). Negative bounds count from the
end and both bounds are clamped, so an inverted or exceeding range yields a
shorter list rather than an error. A null bound, a non-integer bound, or a
non-list base produces null.

`range(start, end)` and `range(start, end, step)` build the inclusive integer
sequence; a step pointing away from `end` gives an empty list, a null or
non-integer argument gives null, and a zero step raises `Error::Argument` at
runtime.

Exponentiation (`^`) and concatenation (`||`) remain explicit planning-time
rejections.

## `CASE` expressions

The simple form `CASE operand WHEN a, b THEN x ... ELSE y END` returns the
result of the first alternative whose `WHEN` value equals the operand under
Cypher equality: integers and floats compare numerically, other kinds must
match exactly, and a null operand or `WHEN` value never matches. The generic
form `CASE WHEN predicate THEN x ... END` selects the first alternative whose
predicate is true; a null or non-boolean predicate is not true. Without a
matching alternative the result is the `ELSE` value, or null when there is
none. Alternatives may produce different types; the chosen alternative
decides the type, and the result feeds arithmetic, comparisons, predicates,
`size()`, and aggregates like any other value. As with other nested
aggregates, an aggregate used inside a `CASE` must be bound by `WITH` first.

## Scalar functions

Built-in scalar functions follow the same rule as the operators: an argument
of the wrong kind produces null where Cypher raises a type error. Function
names are case-insensitive, and calling one with the wrong number of
arguments is a semantic error.

- `coalesce(a, b, ...)` returns the first non-null argument.
- `keys(x)` returns the property names of a node, relationship, or map as a
  list, and `properties(x)` returns them as a map. Stored property maps and
  map literals keep their keys sorted, so the order is alphabetical. A
  stored explicit-null entry (see "Values and properties") is not a Cypher
  property, so the entity forms leave it out even though a returned whole
  entity still shows it; map values may hold null, so the map forms keep it.
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

The current `open-cypher 0.2.0-alpha.2` parser still rejects a labeled node
that follows an earlier node-local `WHERE` in the same pattern or clause, and
can misclassify a bare contextual-keyword label in some later-pattern
positions. Splitting the pattern across `MATCH` clauses, or escaping the label,
is the supported spelling until an upstream parser fix is released.

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
explored states and serialized payload. Constraints from earlier clauses or a
node-local `WHERE` are not pushed into the domain.

An omitted upper bound has no hidden hop cutoff: relationship uniqueness limits
one trail to at most the number of eligible stored edges, although the number of
distinct trails can still grow rapidly. Deep/open-ended enumeration and
shortest-path BFS share a 100,000-trail-state budget across one top-level query,
including `UNION` branches and multiple hybrid expansions. Their runtime
relations also share a 128 MiB (134,217,728-byte) serialized-payload budget.
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
maps, and retained candidate cycles share a 256 MiB (268,435,456-byte)
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
aggregate, grouping, ordering, or `UNWIND` contexts above remains unsupported.
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

`MERGE pattern` matches the whole pattern from each input row, with variables
bound by earlier clauses fixed, and creates the pattern's new parts when the
row has no match. Rows are processed in order against the graph as it stands,
so a row without a match creates the pattern before the next row is matched
and later rows match what earlier rows created; a row with several matches
becomes one row per match. Pattern properties may be literals, parameters, or
expressions over the row, and a computed value that is null never matches. An
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
validated before the write transaction starts, including recursive value depth,
integer range, complete-map shape, and every parameter referenced by a later
mutation or result. Match materialization, all mutations, and post-write result
evaluation then occur in one immediate SQLite transaction. Any execution
failure rolls back every mutation performed by the statement. An optional
`RETURN` emits one row per mutation input row and may
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

Supported deep/open-ended or shortest-path traversal can instead fail during
execution with `Error::ResourceLimit` when either shared query-wide limit is
exhausted: 100,000 explored trail states or 128 MiB of serialized runtime JSON.
This is an explicit resource failure; Crabnet does not return a truncated
result.

Semantics for unsupported constructs—including modern path selectors,
expression-position shortest-path calls, other path expressions beyond the
three path functions above, optional-bound paths or relationship lists,
postfix graph quantifiers,
the deferred legacy shortest forms listed above, and quantified optional
matching—remain unresolved until those constructs are designed and
implemented.
