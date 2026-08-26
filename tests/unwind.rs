use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::{Value as JsonValue, json};

fn parameters<const N: usize>(values: [(&str, JsonValue); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn cypher_value(value: JsonValue) -> CypherValue {
    CypherValue::try_from(value).unwrap()
}

#[test]
fn standalone_literal_unwind_preserves_duplicates_and_nulls() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute("UNWIND [3, 1, 3, null] AS value RETURN value")
        .unwrap();

    assert_eq!(result.columns, ["value"]);
    let values = result.rows.iter().map(|row| &row[0]).collect::<Vec<_>>();
    assert_eq!(values.len(), 4);
    assert_eq!(
        values
            .iter()
            .filter(|value| value.as_i64() == Some(3))
            .count(),
        2
    );
    assert!(values.contains(&&CypherValue::Integer(1)));
    assert!(values.contains(&&CypherValue::Null));
    assert_eq!(result.summary, Default::default());
}

#[test]
fn parameter_lists_unwind_with_typed_values() {
    let mut database = Database::in_memory().unwrap();
    let values = parameters([("items", json!([true, 2.5, "three", null]))]);

    let result = database
        .execute_with("UNWIND $items AS item RETURN item", &values)
        .unwrap();

    let rows = result.rows;
    assert_eq!(rows.len(), 4);
    assert!(rows.contains(&vec![CypherValue::Boolean(true)]));
    assert!(rows.contains(&vec![CypherValue::Float(2.5)]));
    assert!(rows.contains(&vec![CypherValue::String("three".into())]));
    assert!(rows.contains(&vec![CypherValue::Null]));

    assert!(matches!(
        database.execute("UNWIND $missing AS item RETURN item"),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));
}

#[test]
fn empty_and_null_lists_produce_no_rows() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "UNWIND [] AS item RETURN item",
        "UNWIND null AS item RETURN item",
    ] {
        let result = database.execute(query).unwrap();
        assert_eq!(result.columns, ["item"], "query: {query}");
        assert!(result.rows.is_empty(), "query: {query}");
    }

    let result = database
        .execute_with(
            "UNWIND $items AS item RETURN item",
            &parameters([("items", JsonValue::Null)]),
        )
        .unwrap();
    assert!(result.rows.is_empty());
}

#[test]
fn nested_lists_and_maps_remain_typed_values() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "UNWIND [[1, {nested: true}], \
                     {name: 'Ada', items: [null, 2.5]}] AS item \
             RETURN item",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    assert!(
        result
            .rows
            .contains(&vec![cypher_value(json!([1, {"nested": true}]))])
    );
    assert!(result.rows.contains(&vec![cypher_value(
        json!({"name": "Ada", "items": [null, 2.5]})
    )]));
}

#[test]
fn unwind_after_match_expands_each_input_row_and_preserves_entities() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:UnwindPerson {name: 'Ada', values: [1, 1]}), \
                    (:UnwindPerson {name: 'Grace', values: [2]})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:UnwindPerson) \
             UNWIND person.values AS value \
             RETURN person, person.name AS name, value ORDER BY name, value",
        )
        .unwrap();

    assert_eq!(result.columns, ["person", "name", "value"]);
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0][1], CypherValue::String("Ada".into()));
    assert_eq!(result.rows[0][2], CypherValue::Integer(1));
    assert_eq!(result.rows[1][1], CypherValue::String("Ada".into()));
    assert_eq!(result.rows[1][2], CypherValue::Integer(1));
    assert_eq!(result.rows[2][1], CypherValue::String("Grace".into()));
    assert_eq!(result.rows[2][2], CypherValue::Integer(2));

    let first_person = result.rows[0][0].as_node().unwrap();
    assert_eq!(first_person.labels, ["UnwindPerson"]);
    assert_eq!(
        first_person.properties["name"],
        CypherValue::String("Ada".into())
    );
    assert_eq!(result.rows[0][0], result.rows[1][0]);
    assert!(matches!(result.rows[2][0], CypherValue::Node(_)));
}

#[test]
fn empty_upstream_match_stays_empty_after_unwind() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "MATCH (person:NoSuchUnwindPerson) \
             UNWIND [1, 2] AS value \
             RETURN person, value",
        )
        .unwrap();

    assert_eq!(result.columns, ["person", "value"]);
    assert!(result.rows.is_empty());
}

#[test]
fn chained_unwind_can_expand_a_previous_alias() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "UNWIND [[1, 2], [3], []] AS batch \
             UNWIND batch AS value \
             RETURN batch, value",
        )
        .unwrap();

    let mut rows = result.rows;
    rows.sort_by_key(|row| row[1].as_i64().unwrap());
    assert_eq!(
        rows,
        [
            vec![
                CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(2)]),
                CypherValue::Integer(1),
            ],
            vec![
                CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(2)]),
                CypherValue::Integer(2),
            ],
            vec![
                CypherValue::List(vec![CypherValue::Integer(3)]),
                CypherValue::Integer(3),
            ],
        ]
    );
}

#[test]
fn collected_values_can_be_unwound_after_with() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:UnwindCollect {value: 2}), (:UnwindCollect {value: 1}), \
                    (:UnwindCollect {value: 2}), (:UnwindCollect {value: null})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:UnwindCollect) \
             WITH collect(item.value) AS values \
             UNWIND values AS value \
             RETURN value ORDER BY value",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(2)],
            vec![CypherValue::Integer(2)],
        ]
    );
}

#[test]
fn unwind_output_can_filter_a_later_match_and_preserves_bag_semantics() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:UnwindTeam {name: 'red'}), (:UnwindTeam {name: 'blue'}), \
                    (:UnwindTeam {name: 'green'})",
        )
        .unwrap();

    let result = database
        .execute(
            "UNWIND ['red', 'red', 'blue', 'missing'] AS wanted \
             MATCH (team:UnwindTeam) WHERE team.name = wanted \
             RETURN wanted, team ORDER BY wanted",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0][0], CypherValue::String("blue".into()));
    assert_eq!(result.rows[1][0], CypherValue::String("red".into()));
    assert_eq!(result.rows[2][0], CypherValue::String("red".into()));
    assert!(result.rows.iter().all(|row| row[1].as_node().is_some()));
    assert_eq!(result.rows[1][1], result.rows[2][1]);
}

#[test]
fn dynamic_non_list_values_produce_no_rows() {
    // Crabnet's bounded UNWIND contract is deliberately list-only: null and
    // every other runtime shape behave like an empty expansion. This avoids
    // silently treating a scalar as a singleton list.
    let mut database = Database::in_memory().unwrap();

    for candidate in [json!(42), json!(true), json!("text"), json!({"key": 1})] {
        let result = database
            .execute_with(
                "UNWIND $candidate AS value RETURN value",
                &parameters([("candidate", candidate.clone())]),
            )
            .unwrap();
        assert!(
            result.rows.is_empty(),
            "non-list value should not expand: {candidate}"
        );
    }

    let from_scalar_alias = database
        .execute("WITH 42 AS candidate UNWIND candidate AS value RETURN value")
        .unwrap();
    assert!(from_scalar_alias.rows.is_empty());
}

#[test]
fn unknown_unwind_inputs_are_semantic_errors() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute("UNWIND missing AS value RETURN value"),
        Err(Error::Semantic(message)) if message.contains("unknown variable `missing`")
    ));
}

#[test]
fn aggregate_calls_must_cross_a_projection_boundary_before_unwind() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute(
            "MATCH (item:UnwindAggregate) \
             UNWIND collect(item.value) AS value \
             RETURN value"
        ),
        Err(Error::Semantic(message))
            if message.contains("aggregate") && message.contains("UNWIND")
    ));
}

#[test]
fn unwind_aliases_cannot_replace_names_already_in_scope() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "UNWIND [1] AS value UNWIND [2] AS value RETURN value",
        "MATCH (value) UNWIND [1] AS value RETURN value",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "duplicate alias should fail semantic analysis: {query}"
        );
    }
}
