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
| Consecutive read-only `MATCH` clauses | Covered | Multiple `MATCH` stages compose in one read pipeline and preserve their Cartesian row semantics in `consecutive_match_clauses_use_the_composable_read_pipeline` |
| `CREATE` with optional `RETURN` | Covered | Typed nodes and directed relationships, including whole created-entity results in `create_return_uses_the_same_whole_entity_values` |
| `MATCH ... CREATE` with optional `RETURN` | Covered | Executes once per materialized input row, reuses bindings, handles no-match, and is atomic; whole matched and created entities also cross the Rust execution stage in `match_create_carries_matched_and_created_entities_through_rust_execution` |
| `MATCH ... SET` / `REMOVE` with optional `RETURN` | Partial | Direct node/relationship properties and whole-map replacement/merge are covered in `matched_mutations.rs`; label mutations, computed right-hand sides, and writes in a `WITH` pipeline are rejected |
| `MATCH ... DELETE` / `DETACH DELETE` with optional `RETURN` | Covered | Node and relationship targets, stable snapshots, input-bag deduplication, detach behavior, counters, and rollback are covered in `delete_mutations.rs` |
| Node scan, optional single label, inline property map | Covered | Includes missing/null values and typed list/map equality (`missing_and_null_properties_follow_null_semantics`, `inline_structured_properties_keep_json_types`) |
| Fixed-length relationship patterns | Covered | Outgoing, incoming, undirected, self-loop, and three-hop patterns, including parallel-path multiplicity; see `creates_and_matches_a_directed_relationship`, `undirected_matches_check_both_orientations_without_duplicating_self_loops`, and `parallel_three_hop_paths_preserve_row_multiplicity` |
| Typed and untyped relationship scans | Covered | Multiple runtime types retain their own `type(r)` values in `untyped_relationship_matches_retain_each_runtime_type` |
| Relationship inline properties | Covered | Fixed patterns are covered by `relationship_properties_filter_matches`; a bounded traversal applies its inline map to every hop in `relationship_and_endpoint_properties_apply_to_every_candidate_path` |
| Element-local `WHERE` | Partial | Node- and relationship-local predicates execute in `element_local_where_filters_nodes_and_relationships`; the current parser still rejects some valid following-label combinations, so the full syntax is not claimed |
| Comma-separated patterns / bag multiplicity | Covered | Disconnected Cartesian products and repeated node bindings in `disconnected_patterns_preserve_cartesian_multiplicity` and `repeated_node_variables_reuse_the_same_binding` |
| Relationship uniqueness within one path | Covered | Fixed positions and bounded traversal segments cannot reuse a stored relationship in the same path. Traversal uniqueness tracks edge IDs rather than nodes, including across mixed fixed/quantified segments; see `one_relationship_cannot_fill_two_positions_in_a_path`, `cycles_may_revisit_nodes_but_may_not_reuse_an_edge`, and `fixed_and_quantified_segments_cannot_reuse_the_same_relationship` |
| Multiple labels/types and boolean label expressions | Rejected | Storage and lowering currently require at most one node label and one relationship type |
| Bounded legacy variable-length relationships | Covered | Mandatory read-only `MATCH` supports anonymous `*n`, `*min..max`, and `*..max` forms through depth 32 in outgoing, incoming, and undirected directions. Endpoint and named-path results, zero-hop identity, empty reversed ranges, types, per-hop inline properties, cycles, self-loops, diamonds, parallel edges, composition, and multiplicity are covered in `bounded_traversal.rs` and `path_values.rs` |
| Named path bindings and values | Partial | Mandatory read-only fixed, bounded, and mixed patterns may bind a path and return it as `CypherValue::Path(PathValue)`. Traversal order, zero-hop values, reverse stored direction, direct `WITH` aliases, `RETURN DISTINCT`, and `UNION` are covered in `path_values.rs`. Path functions/property access, scalar/aggregate/order/group use, optional-bound paths, selectors, relationship-list bindings, and writes fed by a named path are rejected |
| Other variable-length and path forms | Rejected | Unbounded or deeper traversal, postfix graph quantifiers, named relationship lists, path selectors, quantified `OPTIONAL MATCH`, and traversal-fed writes fail during planning |
| `OPTIONAL MATCH` | Covered | Fixed-length optional patterns null-extend the complete introduced pattern, preserve outer rows and bag multiplicity, and keep optional `WHERE` predicates in the join condition; see `optional_match.rs`. Quantified optional patterns, match modes, and hints remain rejected |
| Read-only `WITH` pipelines | Partial | Standalone and chained `WITH`, entity/path/scalar aliases, scope loss, `WHERE`, intermediate ordering/pagination, aggregation, and later `MATCH`/`UNWIND`/`OPTIONAL MATCH` stages are covered. `WITH ALL` is accepted. Path support is limited to direct aliases. `WITH DISTINCT`, `WITH *`, unaliased computed expressions, and writes anywhere in the pipeline remain rejected |
| `UNWIND` | Covered | Literal, parameter, property, and prior-aggregate lists expand with bag semantics; null, empty, and non-list runtime values produce no rows. Scope and composition cases are covered in `unwind.rs` |
| `UNION`, `UNION DISTINCT`, `UNION ALL` | Covered | Read-only branches are independently planned and must return the same column names in the same positions. `ALL` preserves duplicates; default/`DISTINCT` use recursive typed row equivalence in Rust, including ordered path identity. Mixed operators apply left-to-right; see `union.rs` and `path_values.rs` |

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
| Aliases and column names | Covered | Aliased `RETURN` projections and source-text fallback names are exercised; `WITH` aliases create the names visible after the clause boundary |
| `ORDER BY`, `SKIP`/`OFFSET`, `LIMIT` | Covered | Ascending/descending and alias ordering, both offset spellings, offset without limit, zero boundaries, named pagination parameters, and validation are covered. On `WITH`, these modifiers run before the next clause |
| `RETURN ALL` | Covered | `offset_without_limit_and_return_all_are_supported` locks down its accepted projection behavior |
| Computed read projections | Covered | Supported comparison, boolean, membership, string-predicate, and arithmetic expressions return typed `CypherValue` cells |
| `elementId()`, `labels()`, `type()` | Covered | Read and post-create results plus arity, argument, unknown-variable, and wrong-kind diagnostics are covered |
| Whole node and relationship values | Covered | `CypherValue::Node` and `CypherValue::Relationship` preserve labels/types, recursive properties, opaque IDs, and stored-direction endpoints for reads, writes, unlabeled nodes, and reverse traversal; see `entity_values.rs` |
| Ordinary maps versus graph entities | Covered | Entity-shaped map data remains `CypherValue::Map` in `an_entity_shaped_map_remains_a_map` |
| Whole path values | Partial | Public `PathValue` contains traversal-ordered nodes and relationships. Fixed, bounded, mixed, and zero-hop mandatory paths are covered; path functions/property access and use as scalar, aggregate, grouping, or ordering expressions remain rejected |
| Read-only `RETURN DISTINCT` | Covered | Full projected rows are decoded and semantically deduplicated in Rust; recursive numeric equivalence, nulls, booleans, entity identity, ordered path identity, pagination order, and projected-only `ORDER BY` are covered in `arithmetic_and_distinct.rs` and `path_values.rs` |
| Aggregation and implicit grouping | Covered | Top-level `count`, `sum`, `avg`, `min`, `max`, and `collect` work in `RETURN` and `WITH`; non-aggregate row-dependent projection items are grouping keys. Empty inputs, aliases, grouping scope, typed extrema/numerics, and chained stages are covered in `aggregation.rs` and `aggregation_edges.rs` |
| Aggregate modifiers and unsupported forms | Partial | `count(DISTINCT expression)` and explicit `ALL` are covered. `DISTINCT` on other aggregates, collection of whole entities, nested aggregates, aggregates inside larger projection expressions, and aggregation after mutations are rejected during planning |
| `RETURN *` | Rejected | Wildcard projection is not implemented yet |
| General scalar/list/map/path functions | Rejected | Only the three graph functions above are recognized; `length(path)`, `nodes(path)`, and `relationships(path)` are not implemented |

## Writes and execution guarantees

| Feature | Status | Current contract / evidence |
| --- | --- | --- |
| Create nodes and directed relationships | Covered | Multi-element patterns, left arrows, self-relationships, fresh/reused variables, typed properties, and parameter maps |
| Matched writes use a stable input | Covered | Input rows are materialized before mutation (`match_input_is_materialized_before_create_runs`) |
| Atomic validation and rollback | Covered | Missing parameters, invalid values, invalid reuse, and late row failures leave no partial mutations |
| Mutation summaries | Covered | Created, changed-property, and deleted node/relationship counts are asserted. `properties_set` counts observable top-level stored-map changes; deletion counters count actual stored rows removed |
| Mutations without `RETURN` | Covered | Empty columns/rows and exact summaries are asserted in `mutations_without_return_have_empty_rows_and_accurate_summaries` |
| Duplicate `CREATE` property keys | Covered | Planning fails before any mutation in `create_duplicate_property_keys_fail_before_any_mutation` |
| Direct property `SET` / `REMOVE` | Covered | `entity.key` targets work for matched nodes and relationships. Direct `SET ... = null`, `REMOVE`, and null entries in `+=` remove keys; unchanged assignments and absent removals contribute zero to `properties_set` |
| Whole-map `SET =` / `+=` | Partial | Literal and named-parameter maps replace or shallow-merge properties. Whole-map replacement retains explicit null as a Crabnet extension; computed right-hand sides are rejected |
| `DELETE` | Covered | Matched node and relationship targets are deduplicated across the input bag. Explicit relationships are removed before nodes; deleting a still-connected node fails and rolls back the statement. `DELETE` is currently the final mutation clause before optional `RETURN` |
| `DETACH DELETE` | Covered | Incident incoming, outgoing, and self-loop relationships are deleted before nodes, with shared and explicitly targeted edges counted only once |
| Post-delete `RETURN` | Covered | Rows retain input-bag multiplicity and project materialized pre-delete entity/property snapshots even though each storage target is deleted once |
| Label `SET` / `REMOVE` and computed update RHS | Rejected | The current mutation slice changes stored property maps only; it does not mutate labels or evaluate row-dependent/computed update values |
| `MERGE` and `ON CREATE` / `ON MATCH` | Rejected | No match-or-create operator exists yet |
| Undirected or untyped relationship creation | Rejected | Creation requires exactly one type and a concrete direction |
| Writes in a `WITH` pipeline | Rejected | The composable clause pipeline is read-only; a mutation clause anywhere in it fails during planning |
| Writes fed by traversal or named paths | Rejected | A mutation whose matched input contains a bounded traversal or any named path fails before execution and cannot leave side effects (`writes_fed_by_quantified_match_are_rejected_without_side_effects` and `path_values.rs`) |
| Computed projections and projection modifiers after mutations | Rejected | Computed expressions, `DISTINCT`, ordering, skip/offset, and limit after a mutation are not yet executed |
| Normalized logical-plan explanation | Covered | `crabnet::explain` renders `MATCH`/`OPTIONAL MATCH`/`WITH`/`UNWIND` stages, named-path bindings, bounded traversal ranges, aggregate expressions, union branches, scopes, graph operators, and projections without depending on incidental backend aliases |
| SQLite physical-plan explanation | Covered | `crabnet::explain_physical` renders generated SQL/CTEs, recursive traversal CTEs, path slots and Rust hydration, optional left joins, union branches, bind positions, and semantic-distinct/pagination stages; mutation pipelines are not accepted by this debug view |

## Prioritized next-test backlog

Add a case here before or alongside implementation work. Once work starts, turn the
smallest relevant case into a normal failing test, run it by name while iterating, and
move the row into the matrix when green.

1. The parser gap for a labeled node immediately following a relationship-local
   `WHERE` predicate; promote the existing partial coverage after the parser accepts it.
2. Relationship-property matching across boolean/numeric and nested structured values.
3. Path functions (`length`, `nodes`, and `relationships`) plus the separate
   relationship-list binding/result contract.
4. Batch path hydration after identity-level deduplication, with one shared
   deferred read snapshot across union branches; concurrent external writers
   remain outside the current project goals.
5. Curated openCypher TCK and differential tests after the selected scenarios' setup
   clauses and any required path values can be represented faithfully.
