use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Parameters, QueryResult, Value};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn string_rows<const N: usize>(values: [&str; N]) -> Vec<Vec<CypherValue>> {
    values
        .into_iter()
        .map(|value| vec![CypherValue::String(value.to_owned())])
        .collect()
}

fn rows(result: QueryResult) -> Vec<Vec<CypherValue>> {
    assert_eq!(result.columns, ["name"]);
    assert_eq!(result.summary, Default::default());
    result.rows
}

#[test]
fn in_preserves_typed_membership_and_three_valued_null_semantics() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Membership {name: 'hit', value: 2, allowed: [1, 2]}), \
                    (:Membership {name: 'miss', value: 3, allowed: [1, 2]}), \
                    (:Membership {name: 'different-type', value: '2', allowed: [1, 2]}), \
                    (:Membership {name: 'null-value', value: null, allowed: [1, 2]}), \
                    (:Membership {name: 'missing-value', allowed: [1, 2]}), \
                    (:Membership {name: 'null-list', value: 2, allowed: null}), \
                    (:Membership {name: 'unknown-from-null-member', value: 2, \
                                  allowed: [1, null]}), \
                    (:Membership {name: 'hit-before-null', value: 1, \
                                  allowed: [1, null]}), \
                    (:Membership {name: 'null-in-empty', value: null, allowed: []})",
        )
        .unwrap();

    let matching = database
        .execute(
            "MATCH (item:Membership) WHERE item.value IN item.allowed \
             RETURN item.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(rows(matching), string_rows(["hit", "hit-before-null"]));

    let not_matching = database
        .execute(
            "MATCH (item:Membership) WHERE NOT (item.value IN item.allowed) \
             RETURN item.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        rows(not_matching),
        string_rows(["different-type", "miss", "null-in-empty"])
    );

    let indeterminate = database
        .execute(
            "MATCH (item:Membership) \
             WHERE (item.value IN item.allowed) IS NULL \
             RETURN item.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        rows(indeterminate),
        string_rows([
            "missing-value",
            "null-list",
            "null-value",
            "unknown-from-null-member",
        ])
    );
}

#[test]
fn in_accepts_a_list_parameter_without_coercing_element_types() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Choice {name: 'two', value: 2}), \
                    (:Choice {name: 'four', value: 4}), \
                    (:Choice {name: 'text', value: '2'}), \
                    (:Choice {name: 'null', value: null})",
        )
        .unwrap();

    let values = parameters([("allowed", json!([2, 4]))]);
    let matching = database
        .execute_with(
            "MATCH (choice:Choice) WHERE choice.value IN $allowed \
             RETURN choice.name AS name ORDER BY name",
            &values,
        )
        .unwrap();
    assert_eq!(rows(matching), string_rows(["four", "two"]));

    let null_list = parameters([("allowed", Value::Null)]);
    let indeterminate = database
        .execute_with(
            "MATCH (choice:Choice) WHERE (choice.value IN $allowed) IS NULL \
             RETURN choice.name AS name ORDER BY name",
            &null_list,
        )
        .unwrap();
    assert_eq!(
        rows(indeterminate),
        string_rows(["four", "null", "text", "two"])
    );
}

#[test]
fn string_predicates_are_case_sensitive_and_null_for_non_strings() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:TextCase {name: 'true', text: 'alphabet soup', \
                                prefix: 'alpha', fragment: 'bet', suffix: 'soup'}), \
                    (:TextCase {name: 'negative', text: 'alphabet soup', \
                                prefix: 'beta', fragment: 'zzz', suffix: 'alpha'}), \
                    (:TextCase {name: 'case', text: 'Alphabet Soup', \
                                prefix: 'alpha', fragment: 'soup', suffix: 'soup'}), \
                    (:TextCase {name: 'null-left', text: null, \
                                prefix: 'a', fragment: 'a', suffix: 'a'}), \
                    (:TextCase {name: 'missing-left', \
                                prefix: 'a', fragment: 'a', suffix: 'a'}), \
                    (:TextCase {name: 'null-right', text: 'alphabet', \
                                prefix: null, fragment: null, suffix: null}), \
                    (:TextCase {name: 'wrong-left', text: 42, \
                                prefix: '4', fragment: '2', suffix: '2'}), \
                    (:TextCase {name: 'wrong-right', text: 'alphabet', \
                                prefix: 1, fragment: 2, suffix: 3})",
        )
        .unwrap();

    for (operator, operand) in [
        ("STARTS WITH", "prefix"),
        ("CONTAINS", "fragment"),
        ("ENDS WITH", "suffix"),
    ] {
        let matching = database
            .execute(&format!(
                "MATCH (sample:TextCase) \
                 WHERE sample.text {operator} sample.{operand} \
                 RETURN sample.name AS name ORDER BY name"
            ))
            .unwrap();
        assert_eq!(rows(matching), string_rows(["true"]), "{operator}");

        let not_matching = database
            .execute(&format!(
                "MATCH (sample:TextCase) \
                 WHERE NOT (sample.text {operator} sample.{operand}) \
                 RETURN sample.name AS name ORDER BY name"
            ))
            .unwrap();
        assert_eq!(
            rows(not_matching),
            string_rows(["case", "negative"]),
            "{operator}"
        );

        let indeterminate = database
            .execute(&format!(
                "MATCH (sample:TextCase) \
                 WHERE (sample.text {operator} sample.{operand}) IS NULL \
                 RETURN sample.name AS name ORDER BY name"
            ))
            .unwrap();
        assert_eq!(
            rows(indeterminate),
            string_rows([
                "missing-left",
                "null-left",
                "null-right",
                "wrong-left",
                "wrong-right",
            ]),
            "{operator}"
        );
    }
}

#[test]
fn string_predicates_accept_string_parameters_and_treat_other_types_as_null() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Word {name: 'match', text: 'alphabet'}), \
                    (:Word {name: 'miss', text: 'omega'}), \
                    (:Word {name: 'null', text: null}), \
                    (:Word {name: 'number', text: 123})",
        )
        .unwrap();

    for (operator, needle) in [
        ("STARTS WITH", "alpha"),
        ("CONTAINS", "pha"),
        ("ENDS WITH", "bet"),
    ] {
        let values = parameters([("needle", json!(needle))]);
        let matching = database
            .execute_with(
                &format!(
                    "MATCH (word:Word) WHERE word.text {operator} $needle \
                     RETURN word.name AS name ORDER BY name"
                ),
                &values,
            )
            .unwrap();
        assert_eq!(rows(matching), string_rows(["match"]), "{operator}");

        let wrong_type = parameters([("needle", json!(123))]);
        let indeterminate = database
            .execute_with(
                &format!(
                    "MATCH (word:Word) \
                     WHERE (word.text {operator} $needle) IS NULL \
                     RETURN word.name AS name ORDER BY name"
                ),
                &wrong_type,
            )
            .unwrap();
        assert_eq!(
            rows(indeterminate),
            string_rows(["match", "miss", "null", "number"]),
            "{operator}"
        );
    }
}

#[test]
fn predicates_can_be_projected_with_typed_boolean_and_null_results() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "RETURN 2 IN [1, 2] AS hit, 3 IN [1, 2] AS miss, \
                    3 IN [1, null] AS unknown, null IN [] AS null_in_empty, \
                    2 IN 2 AS non_list, \
                    'alphabet' STARTS WITH 'alpha' AS starts, \
                    'alphabet' CONTAINS 'pha' AS contains, \
                    'alphabet' ENDS WITH 'bet' AS ends",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Boolean(true),
            CypherValue::Boolean(false),
            CypherValue::Null,
            CypherValue::Boolean(false),
            CypherValue::Null,
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
        ]]
    );
}

#[test]
fn string_predicates_treat_needles_literally_and_support_empty_and_unicode_text() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "RETURN '100%_done' CONTAINS '%_' AS literal_wildcards, \
                    'anything' STARTS WITH '' AS empty_prefix, \
                    'anything' CONTAINS '' AS empty_fragment, \
                    'anything' ENDS WITH '' AS empty_suffix, \
                    'Ångström' STARTS WITH 'Ång' AS unicode_prefix, \
                    'Ångström' ENDS WITH 'ström' AS unicode_suffix",
        )
        .unwrap();

    assert_eq!(result.rows, [vec![CypherValue::Boolean(true); 6]]);
}

#[test]
fn in_supports_labels_and_structured_values_without_multiplying_rows() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Person)").unwrap();

    let result = database
        .execute(
            "MATCH (person:Person) \
             RETURN 'Person' IN labels(person) AS has_label, \
                    [1, 2] IN [[0], [1, 2], [1, 2]] AS nested_list, \
                    {a: 1} IN [{a: 0}, {a: 1}, {a: 1}] AS nested_map",
        )
        .unwrap();

    assert_eq!(result.rows, [vec![CypherValue::Boolean(true); 3]]);
}
