use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, serde_json::Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn assert_integer_bag(value: &CypherValue, expected: &[i64]) {
    let CypherValue::List(values) = value else {
        panic!("expected a collected list, got {value:?}");
    };
    let mut actual = values
        .iter()
        .map(|value| match value {
            CypherValue::Integer(value) => *value,
            other => panic!("expected an integer in the collected list, got {other:?}"),
        })
        .collect::<Vec<_>>();
    actual.sort_unstable();

    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

#[test]
fn count_star_preserves_the_complete_input_bag() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:CountPerson {name: 'Ada'}), (:CountPerson {name: 'Grace'}), \
                    (:CountTag {name: 'graph'}), (:CountTag {name: 'rust'}), \
                    (:CountTag {name: 'sqlite'})",
        )
        .unwrap();

    let result = database
        .execute("MATCH (:CountPerson), (:CountTag) RETURN count(*) AS matches")
        .unwrap();

    assert_eq!(result.columns, ["matches"]);
    assert_eq!(result.rows, [vec![CypherValue::Integer(6)]]);
    assert_eq!(result.summary, Default::default());
}

#[test]
fn count_expression_omits_nulls_and_distinct_uses_cypher_equivalence() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:CountValue {value: 1}), (:CountValue {value: 1}), \
                    (:CountValue {value: 1.0}), (:CountValue {value: 2}), \
                    (:CountValue {value: null}), (:CountValue)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:CountValue) \
             RETURN count(*) AS rows, count(item.value) AS values, \
                    count(DISTINCT item.value) AS distinct_values",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Integer(6),
            CypherValue::Integer(4),
            CypherValue::Integer(2),
        ]]
    );
}

#[test]
fn numeric_aggregates_ignore_nulls_and_preserve_integer_results() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:IntegerAggregate {value: 1}), \
                    (:IntegerAggregate {value: 2}), \
                    (:IntegerAggregate {value: 3}), \
                    (:IntegerAggregate {value: null}), \
                    (:IntegerAggregate)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:IntegerAggregate) \
             RETURN sum(item.value) AS total, avg(item.value) AS mean, \
                    min(item.value) AS smallest, max(item.value) AS largest",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Integer(6),
            CypherValue::Float(2.0),
            CypherValue::Integer(1),
            CypherValue::Integer(3),
        ]]
    );
}

#[test]
fn numeric_aggregates_promote_mixed_integer_and_float_inputs() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:MixedAggregate {value: 1}), \
                    (:MixedAggregate {value: 2.5}), \
                    (:MixedAggregate {value: null})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:MixedAggregate) \
             RETURN sum(item.value) AS total, avg(item.value) AS mean, \
                    min(item.value) AS smallest, max(item.value) AS largest",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Float(3.5),
            CypherValue::Float(1.75),
            CypherValue::Integer(1),
            CypherValue::Float(2.5),
        ]]
    );
}

#[test]
fn collect_omits_nulls_and_retains_duplicate_values() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:CollectValue {value: 2}), (:CollectValue {value: 1}), \
                    (:CollectValue {value: 2}), (:CollectValue {value: null}), \
                    (:CollectValue)",
        )
        .unwrap();

    let result = database
        .execute("MATCH (item:CollectValue) RETURN collect(item.value) AS values")
        .unwrap();

    assert_eq!(result.columns, ["values"]);
    assert_eq!(result.rows.len(), 1);
    assert_integer_bag(&result.rows[0][0], &[1, 2, 2]);
}

#[test]
fn non_aggregate_projection_expressions_are_implicit_grouping_keys() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:GroupedValue {value: 1}), (:GroupedValue {value: 2}), \
                    (:GroupedValue {value: 3}), (:GroupedValue {value: 4}), \
                    (:GroupedValue {value: 6})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:GroupedValue) \
             RETURN item.value % 2 AS parity, count(*) AS frequency, \
                    sum(item.value) AS total \
             ORDER BY parity",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::Integer(0),
                CypherValue::Integer(3),
                CypherValue::Integer(12),
            ],
            vec![
                CypherValue::Integer(1),
                CypherValue::Integer(2),
                CypherValue::Integer(4),
            ],
        ]
    );
}

#[test]
fn aggregate_aliases_can_drive_group_ordering() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:TeamMember {team: 'red'}), (:TeamMember {team: 'red'}), \
                    (:TeamMember {team: 'red'}), (:TeamMember {team: 'blue'}), \
                    (:TeamMember {team: 'blue'}), (:TeamMember {team: 'green'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (member:TeamMember) \
             RETURN member.team AS team, count(*) AS members \
             ORDER BY members DESC, team",
        )
        .unwrap();

    assert_eq!(result.columns, ["team", "members"]);
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("red".into()), CypherValue::Integer(3),],
            vec![CypherValue::String("blue".into()), CypherValue::Integer(2),],
            vec![CypherValue::String("green".into()), CypherValue::Integer(1),],
        ]
    );
}

#[test]
fn empty_input_produces_one_global_aggregate_row() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "MATCH (item:DoesNotExist) \
             RETURN count(*) AS rows, count(item.value) AS values, \
                    sum(item.value) AS total, avg(item.value) AS mean, \
                    min(item.value) AS smallest, max(item.value) AS largest, \
                    collect(item.value) AS collected",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Integer(0),
            CypherValue::Integer(0),
            CypherValue::Integer(0),
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::List(Vec::new()),
        ]]
    );
}

#[test]
fn empty_input_produces_no_grouped_aggregate_rows() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "MATCH (item:DoesNotExist) \
             RETURN item.team AS team, count(*) AS members",
        )
        .unwrap();

    assert_eq!(result.columns, ["team", "members"]);
    assert!(result.rows.is_empty());
}

#[test]
fn parameters_participate_in_aggregate_arguments_with_typed_results() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ParameterAggregate {value: 1}), \
                    (:ParameterAggregate {value: 2}), \
                    (:ParameterAggregate {value: 3})",
        )
        .unwrap();
    let values = parameters([
        ("present", json!("count me")),
        ("absent", serde_json::Value::Null),
        ("offset", json!(0.5)),
    ]);

    let result = database
        .execute_with(
            "MATCH (item:ParameterAggregate) \
             RETURN count($present) AS present_count, \
                    count($absent) AS absent_count, \
                    sum(item.value + $offset) AS total, \
                    avg(item.value + $offset) AS mean",
            &values,
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Integer(3),
            CypherValue::Integer(0),
            CypherValue::Float(7.5),
            CypherValue::Float(2.5),
        ]]
    );

    assert!(matches!(
        database.execute("MATCH (:ParameterAggregate) RETURN count($missing) AS count"),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));
}

#[test]
fn with_can_filter_and_project_aggregate_bindings() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:WithAggregate {team: 'red'}), (:WithAggregate {team: 'red'}), \
                    (:WithAggregate {team: 'blue'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (member:WithAggregate) \
             WITH member.team AS team, count(*) AS members \
             WHERE members > 1 \
             RETURN team, members",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("red".into()),
            CypherValue::Integer(2),
        ]]
    );
}

#[test]
fn aggregate_bindings_can_feed_a_later_match() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:AggregatePerson {team: 'red'}), \
                    (:AggregatePerson {team: 'red'}), \
                    (:AggregatePerson {team: 'blue'}), \
                    (:AggregateTeam {name: 'red'}), \
                    (:AggregateTeam {name: 'blue'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:AggregatePerson) \
             WITH person.team AS team, count(*) AS members \
             MATCH (team_node:AggregateTeam) WHERE team_node.name = team \
             RETURN team_node.name AS team, members ORDER BY team",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("blue".into()), CypherValue::Integer(1),],
            vec![CypherValue::String("red".into()), CypherValue::Integer(2),],
        ]
    );
}
