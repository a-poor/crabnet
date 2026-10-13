use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn semantic_error(database: &mut Database, query: &str) -> String {
    match database.execute(query) {
        Err(Error::Semantic(message)) => message,
        Err(other) => panic!("expected a semantic error for `{query}`, got {other}"),
        Ok(result) => panic!("expected `{query}` to fail, got {result:?}"),
    }
}

fn grouped_frequency(rows: &[Vec<CypherValue>], expected: &CypherValue) -> Option<i64> {
    rows.iter().find_map(|row| match row.as_slice() {
        [value, CypherValue::Integer(frequency)] if value == expected => Some(*frequency),
        _ => None,
    })
}

fn numeric_group_frequency(rows: &[Vec<CypherValue>], expected: f64) -> Option<i64> {
    rows.iter().find_map(|row| match row.as_slice() {
        [CypherValue::Integer(value), CypherValue::Integer(frequency)]
            if *value as f64 == expected =>
        {
            Some(*frequency)
        }
        [CypherValue::Float(value), CypherValue::Integer(frequency)] if *value == expected => {
            Some(*frequency)
        }
        _ => None,
    })
}

#[test]
fn grouping_keys_keep_type_families_but_coalesce_null_and_equivalent_numbers() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:TypedGroup {value: null}), (:TypedGroup), \
                    (:TypedGroup {value: true}), (:TypedGroup {value: true}), \
                    (:TypedGroup {value: 1}), (:TypedGroup {value: 1.0}), \
                    (:TypedGroup {value: '1'}), (:TypedGroup {value: '1'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:TypedGroup) \
             RETURN item.value AS value, count(*) AS frequency",
        )
        .unwrap();

    assert_eq!(result.columns, ["value", "frequency"]);
    assert_eq!(result.rows.len(), 4);
    assert_eq!(grouped_frequency(&result.rows, &CypherValue::Null), Some(2));
    assert_eq!(
        grouped_frequency(&result.rows, &CypherValue::Boolean(true)),
        Some(2)
    );
    assert_eq!(numeric_group_frequency(&result.rows, 1.0), Some(2));
    assert_eq!(
        grouped_frequency(&result.rows, &CypherValue::String("1".into())),
        Some(2)
    );
}

#[test]
fn grouping_equivalence_recurses_through_lists_and_maps() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:StructuredGroup {value: [1, {nested: 2}]}), \
                    (:StructuredGroup {value: [1.0, {nested: 2.0}]}), \
                    (:StructuredGroup {value: {nested: [3]}}), \
                    (:StructuredGroup {value: {nested: [3.0]}})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:StructuredGroup) \
             RETURN item.value AS value, count(*) AS frequency",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    assert!(
        result
            .rows
            .iter()
            .all(|row| matches!(row.as_slice(), [_, CypherValue::Integer(2)]))
    );
}

#[test]
fn count_distinct_uses_typed_and_recursive_cypher_equivalence() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:DistinctAggregate {value: null}), (:DistinctAggregate), \
                    (:DistinctAggregate {value: true}), \
                    (:DistinctAggregate {value: 1}), \
                    (:DistinctAggregate {value: 1.0}), \
                    (:DistinctAggregate {value: '1'}), \
                    (:DistinctAggregate {value: [2, {nested: 3}]}), \
                    (:DistinctAggregate {value: [2.0, {nested: 3.0}]}), \
                    (:DistinctAggregate {value: {nested: [4]}}), \
                    (:DistinctAggregate {value: {nested: [4.0]}})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:DistinctAggregate) \
             RETURN count(DISTINCT item.value) AS values",
        )
        .unwrap();

    assert_eq!(result.rows, [vec![CypherValue::Integer(5)]]);
}

#[test]
fn min_and_max_support_text_and_boolean_families() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:TextExtrema {value: 'zeta'}), \
                    (:TextExtrema {value: 'alpha'}), \
                    (:TextExtrema {value: null}), (:TextExtrema), \
                    (:BooleanExtrema {value: true}), \
                    (:BooleanExtrema {value: false}), \
                    (:BooleanExtrema {value: null})",
        )
        .unwrap();

    let text = database
        .execute(
            "MATCH (item:TextExtrema) \
             RETURN min(item.value) AS minimum, max(item.value) AS maximum",
        )
        .unwrap();
    assert_eq!(
        text.rows,
        [vec![
            CypherValue::String("alpha".into()),
            CypherValue::String("zeta".into()),
        ]]
    );

    let boolean = database
        .execute(
            "MATCH (item:BooleanExtrema) \
             RETURN min(item.value) AS minimum, max(item.value) AS maximum",
        )
        .unwrap();
    assert_eq!(
        boolean.rows,
        [vec![
            CypherValue::Boolean(false),
            CypherValue::Boolean(true)
        ]]
    );
}

#[test]
fn min_and_max_order_mixed_kinds_by_cypher_orderability() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:MixedExtrema {value: 1}), \
                    (:MixedExtrema {value: 'two'}), \
                    (:MixedExtrema {value: null}), \
                    (:MixedExtrema {value: [3]}), \
                    (:StructuredExtrema {value: [1, 'a']}), \
                    (:StructuredExtrema {value: [2]}), \
                    (:StructuredExtrema {value: [1]})",
        )
        .unwrap();

    // Lists sort before strings, and strings before numbers.
    for (label, minimum, maximum) in [
        (
            "MixedExtrema",
            CypherValue::List(vec![CypherValue::Integer(3)]),
            CypherValue::Integer(1),
        ),
        (
            "StructuredExtrema",
            CypherValue::List(vec![CypherValue::Integer(1)]),
            CypherValue::List(vec![CypherValue::Integer(2)]),
        ),
    ] {
        let result = database
            .execute(&format!(
                "MATCH (item:{label}) \
                 RETURN min(item.value) AS minimum, max(item.value) AS maximum"
            ))
            .unwrap();
        assert_eq!(result.rows, [vec![minimum, maximum]]);
    }
}

#[test]
fn sum_and_avg_reject_groups_with_non_numeric_values() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:WrongNumericKind {value: 1}), \
                    (:WrongNumericKind {value: '2'}), \
                    (:WrongNumericKind {value: null}), \
                    (:WrongNumericKind)",
        )
        .unwrap();

    for function in ["sum", "avg"] {
        let query = format!("MATCH (item:WrongNumericKind) RETURN {function}(item.value)");
        assert!(
            matches!(database.execute(&query), Err(Error::Type(_))),
            "{query}"
        );
    }
}

#[test]
fn count_entity_counts_bound_identity_and_distinct_deduplicates_bag_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:CountEntity), (:CountEntity), \
                    (:CountMultiplier), (:CountMultiplier)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (entity:CountEntity), (:CountMultiplier) \
             RETURN count(entity) AS rows, \
                    count(DISTINCT entity) AS distinct_entities",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(4), CypherValue::Integer(2)]]
    );
}

#[test]
fn aggregate_all_is_the_explicit_form_of_bag_aggregation() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:AggregateAll {value: 1}), (:AggregateAll {value: 1}), \
                    (:AggregateAll {value: 2}), \
                    (:AggregateAll {value: null}), (:AggregateAll)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:AggregateAll) \
             RETURN count(ALL item.value) AS count, \
                    sum(ALL item.value) AS sum, \
                    avg(ALL item.value) AS average, \
                    min(ALL item.value) AS minimum, \
                    max(ALL item.value) AS maximum",
        )
        .unwrap();

    assert_eq!(
        result.columns,
        ["count", "sum", "average", "minimum", "maximum"]
    );
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Integer(3),
            CypherValue::Integer(4),
            CypherValue::Float(4.0 / 3.0),
            CypherValue::Integer(1),
            CypherValue::Integer(2),
        ]]
    );
}

#[test]
fn aggregate_with_order_and_limit_runs_before_the_next_stage() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:RankedTeam {team: 'red'}), (:RankedTeam {team: 'red'}), \
                    (:RankedTeam {team: 'red'}), \
                    (:RankedTeam {team: 'blue'}), (:RankedTeam {team: 'blue'}), \
                    (:RankedTeam {team: 'green'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (member:RankedTeam) \
             WITH member.team AS team, count(*) AS members \
             ORDER BY members DESC, team LIMIT 2 \
             RETURN team, members",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("red".into()), CypherValue::Integer(3)],
            vec![CypherValue::String("blue".into()), CypherValue::Integer(2)],
        ]
    );
}

#[test]
fn consecutive_with_stages_can_aggregate_prior_aggregate_results() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ConsecutiveAggregate {team: 'red'}), \
                    (:ConsecutiveAggregate {team: 'red'}), \
                    (:ConsecutiveAggregate {team: 'blue'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (member:ConsecutiveAggregate) \
             WITH member.team AS team, count(*) AS members \
             WITH count(*) AS teams, sum(members) AS total_members \
             RETURN teams, total_members",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(2), CypherValue::Integer(3)]]
    );
}

#[test]
fn aggregate_bad_arity_and_wildcard_forms_are_structured_errors() {
    let mut database = Database::in_memory().unwrap();
    for (query, expected) in [
        ("RETURN count() AS value", "exactly one argument"),
        ("RETURN count(1, 2) AS value", "exactly one argument"),
    ] {
        let message = semantic_error(&mut database, query);
        assert!(
            message.contains(expected),
            "unexpected error for `{query}`: {message}"
        );
    }

    // The parser currently rejects these before the planner can issue its more
    // specific wildcard semantic errors. They must still be structured query
    // errors rather than reaching SQLite or panicking.
    for query in [
        "RETURN sum(*) AS value",
        "RETURN count(DISTINCT *) AS value",
    ] {
        assert!(matches!(
            database.execute(query),
            Err(Error::Parse(_)) | Err(Error::Semantic(_))
        ));
    }
}

#[test]
fn nested_aggregates_are_rejected_during_planning() {
    let mut database = Database::in_memory().unwrap();

    let nested = semantic_error(&mut database, "RETURN sum(count(*)) AS value");
    assert!(
        nested.contains("cannot be nested"),
        "unexpected error: {nested}"
    );
}

#[test]
fn aggregate_calls_are_not_allowed_in_match_where() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:AggregatePredicate)").unwrap();

    let message = semantic_error(
        &mut database,
        "MATCH (item:AggregatePredicate) WHERE count(*) > 0 RETURN item",
    );
    assert!(
        message.contains("not allowed in MATCH predicates"),
        "unexpected error: {message}"
    );
}

#[test]
fn aggregate_calls_only_in_order_or_with_filter_are_rejected_during_planning() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:AggregatePlacement {value: 1})")
        .unwrap();

    for query in [
        "MATCH (item:AggregatePlacement) RETURN item.value ORDER BY count(*)",
        "MATCH (item:AggregatePlacement) WITH item.value AS value ORDER BY count(*) RETURN value",
        "MATCH (item:AggregatePlacement) WITH item.value AS value WHERE count(*) > 0 RETURN value",
    ] {
        let error = database.execute(query).unwrap_err();
        assert!(
            matches!(error, Error::Semantic(_) | Error::Unsupported(_)),
            "query leaked past planning: {query}: {error:?}"
        );
    }
}

#[test]
fn ordered_with_controls_the_order_seen_by_collect() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:AggregateCollectOrder {value: 2}), \
                    (:AggregateCollectOrder {value: 1}), \
                    (:AggregateCollectOrder {value: 3})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:AggregateCollectOrder) \
             WITH item.value AS value ORDER BY value DESC \
             RETURN collect(value) AS values",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![CypherValue::List(vec![
            CypherValue::Integer(3),
            CypherValue::Integer(2),
            CypherValue::Integer(1),
        ])]]
    );
}

#[test]
fn aggregate_arguments_in_with_subclauses_can_use_projected_aliases() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:AggregateAliasArgument {value: 1}), \
                    (:AggregateAliasArgument {value: 2})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:AggregateAliasArgument) \
             WITH item.value AS value, count(*) AS amount \
             ORDER BY sum(value) DESC \
             WHERE count(value) > 0 \
             RETURN value, amount",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::Integer(2));
    assert_eq!(result.rows[1][0], CypherValue::Integer(1));
}

#[test]
fn distinct_is_rejected_for_numeric_and_extremum_aggregates() {
    let mut database = Database::in_memory().unwrap();

    for function in ["sum", "avg", "min", "max"] {
        let query = format!("RETURN {function}(DISTINCT 1) AS value");
        assert!(
            matches!(database.execute(&query), Err(Error::Unsupported(_))),
            "unexpectedly accepted {query}"
        );
    }
}

#[test]
fn count_literals_and_parameters_keep_sql_bindings_aligned() {
    let mut database = Database::in_memory().unwrap();

    assert_eq!(
        database
            .execute("RETURN count(null) AS value")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(0)]]
    );
    assert_eq!(
        database.execute("RETURN count(1) AS value").unwrap().rows,
        [vec![CypherValue::Integer(1)]]
    );
    assert_eq!(
        database
            .execute_with(
                "RETURN count($value) AS value",
                &Parameters::from([("value".to_owned(), json!(false))]),
            )
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn literal_types_and_constant_ordering_never_leave_unused_bindings() {
    let mut database = Database::in_memory().unwrap();

    assert_eq!(
        database
            .execute("RETURN null IS NULL AS value")
            .unwrap()
            .rows,
        [vec![CypherValue::Boolean(true)]]
    );
    assert_eq!(
        database
            .execute("RETURN 1 AS value ORDER BY 2")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1)]]
    );
    assert_eq!(
        database
            .execute("WITH count(1) AS value RETURN value")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn aggregate_values_used_directly_as_predicates_keep_argument_bindings() {
    let mut database = Database::in_memory().unwrap();
    let parameters = Parameters::from([("value".to_owned(), json!(2))]);

    for query in [
        "WITH avg($value) AS result WHERE avg($value) RETURN result",
        "WITH collect($value) AS result WHERE collect($value) RETURN result",
        "WITH avg($value) AS result WHERE NOT avg($value) RETURN result",
    ] {
        // A number or a list is not a predicate. The statement must reach
        // that type check with every argument bound, rather than fail on a
        // binding count mismatch.
        let error = database.execute_with(query, &parameters).unwrap_err();
        assert!(matches!(error, Error::Type(_)), "{query}: {error:?}");
    }
}

#[test]
fn aggregate_with_rejects_ungrouped_incoming_references_in_where_and_order() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:UngroupedReference {team: 'red', name: 'Ada'})")
        .unwrap();

    for query in [
        "MATCH (item:UngroupedReference) \
         WITH item.team AS team, count(*) AS members \
         WHERE item.name = 'Ada' RETURN team, members",
        "MATCH (item:UngroupedReference) \
         WITH item.team AS team, count(*) AS members \
         ORDER BY item.name RETURN team, members",
    ] {
        let message = semantic_error(&mut database, query);
        assert!(
            message.contains("unknown variable `item`") || message.contains("not a grouping key"),
            "unexpected error for `{query}`: {message}"
        );
    }
}

#[test]
fn aggregate_projection_after_create_counts_the_written_rows() {
    let mut database = Database::in_memory().unwrap();

    let written = database
        .execute("CREATE (:AggregateWrite), (:AggregateWrite) RETURN count(*) AS writes")
        .unwrap();
    assert_eq!(written.columns, ["writes"]);
    assert_eq!(written.rows, [vec![CypherValue::Integer(1)]]);
    assert_eq!(written.summary.nodes_created, 2);

    let remaining = database
        .execute("MATCH (:AggregateWrite) RETURN count(*) AS nodes")
        .unwrap();
    assert_eq!(remaining.rows, [vec![CypherValue::Integer(2)]]);
}

#[test]
fn aggregate_projection_after_matched_mutation_sees_updated_values() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:AggregateMutation {value: 1, group: 'a'}), \
                    (:AggregateMutation {value: 1, group: 'a'}), \
                    (:AggregateMutation {value: 1, group: 'b'})",
        )
        .unwrap();

    let updated = database
        .execute(
            "MATCH (item:AggregateMutation) \
             SET item.value = 2 \
             RETURN item.group AS grp, sum(item.value) AS total, collect(item.value) AS values \
             ORDER BY grp",
        )
        .unwrap();
    assert_eq!(updated.columns, ["grp", "total", "values"]);
    assert_eq!(
        updated.rows,
        [
            vec![
                CypherValue::String("a".into()),
                CypherValue::Integer(4),
                CypherValue::List(vec![CypherValue::Integer(2), CypherValue::Integer(2)])
            ],
            vec![
                CypherValue::String("b".into()),
                CypherValue::Integer(2),
                CypherValue::List(vec![CypherValue::Integer(2)])
            ],
        ]
    );
    assert_eq!(updated.summary.properties_set, 3);
}

#[test]
fn grouping_whole_entities_uses_identity_not_property_equality() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (first:GroupedEntity {name: 'same'}), \
                    (second:GroupedEntity {name: 'same'}), \
                    (:GroupMultiplier), (:GroupMultiplier)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (entity:GroupedEntity), (:GroupMultiplier) \
             RETURN entity, count(*) AS frequency",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    let mut ids = BTreeMap::new();
    for row in result.rows {
        let [CypherValue::Node(node), CypherValue::Integer(frequency)] = row.as_slice() else {
            panic!("expected grouped node and frequency")
        };
        assert_eq!(*frequency, 2);
        assert_eq!(
            node.properties.get("name"),
            Some(&CypherValue::String("same".into()))
        );
        ids.insert(node.element_id.clone(), *frequency);
    }
    assert_eq!(ids.len(), 2);
}
