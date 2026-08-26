# Cypher coverage

This is a living inventory of Crabnet's observable Cypher behavior. It describes the
current implementation, not the full parser AST and not a promise of openCypher
conformance. The project is still experimental: databases may be recreated when the
schema changes, and there is deliberately no schema-migration compatibility policy yet.

## Status vocabulary

- **Covered** — implemented and exercised by an end-to-end regression test.
- **Needs test** — an implementation path exists, but its contract lacks a direct,
  focused regression test.
- **Partial** — a useful subset is covered; the omitted forms are named explicitly.
- **Rejected** — accepted by the parser but intentionally fails during planning with a
  structured `Unsupported` or `Semantic` error instead of producing a guessed result.
- **Planned** — roadmap work with no active implementation claim. Planned cases belong
  here until work begins; they should not accumulate as ignored tests.

## Query forms and patterns

| Feature | Status | Current contract / evidence |
| --- | --- | --- |
| Standalone `RETURN` | Covered | Typed literals, collections, maps, and named parameters in `return_without_match_supports_typed_values_and_parameters` |
| `MATCH ... RETURN` | Covered | Scalar and whole-entity results throughout `engine.rs`, `read_semantics.rs`, and `entity_values.rs` |
| `CREATE` with optional `RETURN` | Covered | Typed nodes and directed relationships, including whole created-entity results in `create_return_uses_the_same_whole_entity_values` |
| `MATCH ... CREATE` with optional `RETURN` | Covered | Executes once per materialized input row, reuses bindings, handles no-match, and is atomic; whole matched and created entities also cross the Rust execution stage in `match_create_carries_matched_and_created_entities_through_rust_execution` |
| Node scan, optional single label, inline property map | Covered | Includes missing/null values and typed list/map equality (`missing_and_null_properties_follow_null_semantics`, `inline_structured_properties_keep_json_types`) |
| Fixed-length relationship patterns | Covered | Outgoing, incoming, undirected, self-loop, and three-hop patterns, including parallel-path multiplicity; see `creates_and_matches_a_directed_relationship`, `undirected_matches_check_both_orientations_without_duplicating_self_loops`, and `parallel_three_hop_paths_preserve_row_multiplicity` |
| Typed and untyped relationship scans | Covered | Multiple runtime types retain their own `type(r)` values in `untyped_relationship_matches_retain_each_runtime_type` |
| Relationship inline properties | Covered | `relationship_properties_filter_matches` |
| Element-local `WHERE` | Partial | Node- and relationship-local predicates execute in `element_local_where_filters_nodes_and_relationships`; the current parser still rejects some valid following-label combinations, so the full syntax is not claimed |
| Comma-separated patterns / bag multiplicity | Covered | Disconnected Cartesian products and repeated node bindings in `disconnected_patterns_preserve_cartesian_multiplicity` and `repeated_node_variables_reuse_the_same_binding` |
| Relationship uniqueness within one fixed path | Covered | The same stored relationship cannot fill two positions; explicit variable reuse is semantic error (`one_relationship_cannot_fill_two_positions_in_a_path`) |
| Multiple labels/types and boolean label expressions | Rejected | Storage and lowering currently require at most one node label and one relationship type |
| Variable-length relationships and path values/bindings | Rejected | Quantified patterns, selectors, and returned whole paths are not implemented yet |
| `OPTIONAL MATCH` and match modes/hints | Rejected | No null-extending logical operator yet |
| `WITH`, `UNWIND`, `UNION` | Rejected | Current clause pipeline supports only `MATCH`, `CREATE`, and `RETURN` in the documented arrangements |

## Expressions and projection

| Feature | Status | Current contract / evidence |
| --- | --- | --- |
| Literals and named parameters | Covered | Null, bool, signed integer, float/numeric comparison, string, list, and map values; missing read/write parameters are structured errors |
| Property access | Covered | Preserves JSON value kinds, returns null for missing properties, and binds unusual property names safely |
| `=`, `<>`, `<`, `<=`, `>`, `>=` | Covered | Equality is JSON-kind aware; ordering permits number/number, string/string, and boolean/boolean only. Null, missing, and incompatible ordering operands produce null; property-to-property cases are covered in `read_semantics.rs` and `arithmetic_and_distinct.rs` |
| Boolean contexts, `AND`, `OR`, `NOT`, and parentheses | Covered | Only JSON booleans enter boolean contexts; other dynamic kinds become null instead of using SQLite truthiness. Three-valued behavior and the type gate are covered in `read_semantics.rs` |
| `IS NULL`, `IS NOT NULL` | Covered | Missing and explicit JSON null both project as null; `= null` does not match |
| Numeric `+`, `-`, `*`, `/`, `%`, and unary `+`/`-` | Covered | Computed read predicates/projections preserve integer/float results and precedence; null, missing, and non-numeric dynamic operands produce null without coercion in `arithmetic_and_distinct.rs` |
| Exponentiation (`^`) and concatenation (`\|\|`) | Rejected | Both parser operators fail with a structured planning error rather than being approximated |
| `IN` | Covered | Typed scalar and structured membership, list parameters, empty lists, null members, and non-list RHS behavior are covered in `membership_and_string_predicates.rs` |
| `STARTS WITH`, `CONTAINS`, `ENDS WITH` | Covered | Case-sensitive, literal string behavior plus empty, Unicode, parameter, null, and wrong-kind operands are covered in `membership_and_string_predicates.rs` |
| Aliases and column names | Covered | Aliased projections and source-text fallback names are exercised |
| `ORDER BY`, `SKIP`/`OFFSET`, `LIMIT` | Covered | Ascending/descending and alias ordering, both offset spellings, offset without limit, zero boundaries, named pagination parameters, and validation are covered |
| `RETURN ALL` | Covered | `offset_without_limit_and_return_all_are_supported` locks down its accepted projection behavior |
| Computed read projections | Covered | Supported comparison, boolean, membership, string-predicate, and arithmetic expressions return typed `CypherValue` cells |
| `elementId()`, `labels()`, `type()` | Covered | Read and post-create results plus arity, argument, unknown-variable, and wrong-kind diagnostics are covered |
| Whole node and relationship values | Covered | `CypherValue::Node` and `CypherValue::Relationship` preserve labels/types, recursive properties, opaque IDs, and stored-direction endpoints for reads, writes, unlabeled nodes, and reverse traversal; see `entity_values.rs` |
| Ordinary maps versus graph entities | Covered | Entity-shaped map data remains `CypherValue::Map` in `an_entity_shaped_map_remains_a_map` |
| Whole path values | Rejected | `CypherValue` has no path representation yet, and path bindings remain unsupported |
| Read-only `RETURN DISTINCT` | Covered | Full projected rows are decoded and semantically deduplicated in Rust; recursive numeric equivalence, nulls, booleans, entity identity, pagination order, and projected-only `ORDER BY` are covered in `arithmetic_and_distinct.rs` |
| `RETURN *` and aggregates | Rejected | Wildcard and aggregate projection operators are not implemented yet |
| General scalar/list/map functions | Rejected | Only the three graph functions above are recognized |

## Writes and execution guarantees

| Feature | Status | Current contract / evidence |
| --- | --- | --- |
| Create nodes and directed relationships | Covered | Multi-element patterns, left arrows, self-relationships, fresh/reused variables, typed properties, and parameter maps |
| Matched writes use a stable input | Covered | Input rows are materialized before mutation (`match_input_is_materialized_before_create_runs`) |
| Atomic validation and rollback | Covered | Missing parameters, invalid values, invalid reuse, and late row failures leave no partial mutations |
| Mutation summaries | Covered | Created node and relationship counts are asserted across standalone and matched creates |
| Mutations without `RETURN` | Covered | Empty columns/rows and exact summaries are asserted in `mutations_without_return_have_empty_rows_and_accurate_summaries` |
| Duplicate `CREATE` property keys | Covered | Planning fails before any mutation in `create_duplicate_property_keys_fail_before_any_mutation` |
| `SET`, `REMOVE`, `DELETE`, `DETACH DELETE` | Rejected | `DELETE` is explicitly tested as non-mutating when unsupported |
| `MERGE` and `ON CREATE` / `ON MATCH` | Rejected | No match-or-create operator exists yet |
| Undirected or untyped relationship creation | Rejected | Creation requires exactly one type and a concrete direction |
| Computed projections and projection modifiers after `CREATE` | Rejected | Computed expressions, `DISTINCT`, ordering, skip/offset, and limit after a mutation are not yet executed |
| Normalized logical-plan explanation | Covered | `crabnet::explain` has exact contracts for a representative read and matched `CREATE`; it intentionally excludes incidental backend SQL formatting |

## Prioritized next-test backlog

Add a case here before or alongside implementation work. Once work starts, turn the
smallest relevant case into a normal failing test, run it by name while iterating, and
move the row into the matrix when green.

1. Milestone 4 clause-pipeline acceptance tests for `WITH`: alias scope, scope
   loss, filtering, ordering, skip, and limit between segments.
2. The parser gap for a labeled node immediately following a relationship-local
   `WHERE` predicate; promote the existing partial coverage after the parser accepts it.
3. Relationship-property matching across boolean/numeric and nested structured values.
4. Three-hop diamonds where different intermediate nodes converge, preserving one row
   per valid relationship sequence rather than collapsing endpoints.
5. A normalized physical-plan rendering and a few reviewed SQL/binding fixtures once
   that debug surface exists.
6. Aggregation, `UNWIND`, and `OPTIONAL MATCH` acceptance tests when milestone 6
   begins; do not mix their scope rules into milestone 4 prematurely.
7. Curated openCypher TCK and differential tests after the selected scenarios' setup
   clauses and any required path values can be represented faithfully.
