use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters, Value, explain};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn truth(database: &mut Database, expression: &str) -> CypherValue {
    let result = database
        .execute(&format!("RETURN {expression} AS value"))
        .unwrap();
    assert_eq!(result.rows.len(), 1, "{expression}");
    result.rows[0][0].clone()
}

const TRUE: CypherValue = CypherValue::Boolean(true);
const FALSE: CypherValue = CypherValue::Boolean(false);
const NULL: CypherValue = CypherValue::Null;

#[test]
fn quantifiers_follow_three_valued_semantics_over_true_false_and_null_outcomes() {
    let mut database = Database::in_memory().unwrap();
    // Each list is chosen so the predicate `x = 1` yields the outcome pattern
    // named in the comment: 1 -> true, 2 -> false, null -> null.
    let cases: [(&str, [CypherValue; 4]); 9] = [
        // list           all    any    none   single
        ("[]", [TRUE, FALSE, TRUE, FALSE]),
        ("[1]", [TRUE, TRUE, FALSE, TRUE]),
        ("[2]", [FALSE, FALSE, TRUE, FALSE]),
        ("[null]", [NULL, NULL, NULL, NULL]),
        ("[1, 1]", [TRUE, TRUE, FALSE, FALSE]),
        ("[1, 2]", [FALSE, TRUE, FALSE, TRUE]),
        ("[1, null]", [NULL, TRUE, FALSE, NULL]),
        ("[2, null]", [FALSE, NULL, NULL, NULL]),
        ("[1, 1, null]", [NULL, TRUE, FALSE, FALSE]),
    ];
    for (list, expected) in cases {
        for (kind, expected) in ["all", "any", "none", "single"].into_iter().zip(expected) {
            let expression = format!("{kind}(x IN {list} WHERE x = 1)");
            assert_eq!(truth(&mut database, &expression), expected, "{expression}");
        }
    }
}

#[test]
fn quantifiers_preserve_null_reject_non_lists_and_predicates_see_element_types() {
    let mut database = Database::in_memory().unwrap();
    assert_eq!(truth(&mut database, "any(x IN null WHERE true)"), NULL);
    for expression in ["all(x IN 5 WHERE true)", "none(x IN 'abc' WHERE true)"] {
        assert!(matches!(
            database.execute(&format!("RETURN {expression}")),
            Err(Error::Semantic(_))
        ));
    }
    // Boolean elements are the predicate itself; strings, floats, and nested
    // collections keep their kinds inside the predicate.
    assert_eq!(
        truth(&mut database, "none(x IN [false, true] WHERE x)"),
        FALSE
    );
    assert_eq!(truth(&mut database, "all(x IN [true, true] WHERE x)"), TRUE);
    assert_eq!(
        truth(
            &mut database,
            "single(x IN ['a', 'bb', 'c'] WHERE x STARTS WITH 'b')"
        ),
        TRUE
    );
    assert_eq!(
        truth(&mut database, "any(x IN [1.5, 2.5] WHERE x > 2)"),
        TRUE
    );
    assert_eq!(
        truth(&mut database, "all(x IN [[1], [1, 2]] WHERE 1 IN x)"),
        TRUE
    );
    assert_eq!(
        truth(&mut database, "any(x IN [{a: 1}, {a: 2}] WHERE x = {a: 2})"),
        TRUE
    );
    assert_eq!(
        truth(&mut database, "all(x IN [0, null] WHERE x IS NULL)"),
        FALSE
    );
    assert_eq!(
        truth(&mut database, "single(x IN [0, null] WHERE x IS NULL)"),
        TRUE
    );
    // A predicate that never mentions the element still runs once per element.
    assert_eq!(
        truth(&mut database, "single(x IN [1, 2] WHERE true)"),
        FALSE
    );
    assert_eq!(truth(&mut database, "single(x IN [7] WHERE true)"), TRUE);
    assert_eq!(truth(&mut database, "any(x IN [1, 2] WHERE null)"), NULL);
}

#[test]
fn quantifiers_nest_and_the_bound_variable_shadows_outer_scalars() {
    let mut database = Database::in_memory().unwrap();
    assert_eq!(
        truth(
            &mut database,
            "none(x IN [['abc'], ['abc', 'def']] WHERE none(y IN x WHERE y = 'abc'))"
        ),
        TRUE
    );
    assert_eq!(
        truth(
            &mut database,
            "all(x IN [['abc'], ['abc', 'def']] WHERE any(y IN x WHERE y = 'abc'))"
        ),
        TRUE
    );
    let result = database
        .execute(
            "WITH [1, 2, 3, 4, 5, 6, 7, 8, 9] AS list \
             RETURN none(x IN list WHERE none(y IN list WHERE x <= y)) AS a, \
                    single(x IN list WHERE single(y IN list WHERE x + y = 15)) AS b, \
                    all(x IN list WHERE any(y IN list WHERE x < y)) AS c",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![TRUE, FALSE, FALSE]]);

    // `x` inside the quantifier is the element, not the projected `x`; the
    // list expression itself still sees the outer `x`.
    let result = database
        .execute("WITH 10 AS x, [1, 2] AS list RETURN any(x IN list WHERE x = 2) AS a, x AS b")
        .unwrap();
    assert_eq!(result.rows, vec![vec![TRUE, CypherValue::Integer(10)]]);
    let result = database
        .execute("WITH [1, 2] AS x RETURN all(x IN x WHERE x < 3) AS value")
        .unwrap();
    assert_eq!(result.rows, vec![vec![TRUE]]);
}

#[test]
fn quantifiers_filter_matches_and_read_parameters_and_properties() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ann', scores: [7, 9]}), \
                    (:Person {name: 'Bob', scores: [3, 9]}), \
                    (:Person {name: 'Cid', scores: []}), \
                    (:Person {name: 'Dee'})",
        )
        .unwrap();
    let names = |database: &mut Database, cypher: &str| {
        database
            .execute(cypher)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>()
    };
    let text = |value: &str| CypherValue::String(value.to_owned());
    assert_eq!(
        names(
            &mut database,
            "MATCH (p:Person) WHERE all(s IN p.scores WHERE s > 5) RETURN p.name ORDER BY p.name"
        ),
        vec![text("Ann"), text("Cid")]
    );
    assert_eq!(
        names(
            &mut database,
            "MATCH (p:Person) WHERE NOT any(s IN p.scores WHERE s > 5) RETURN p.name"
        ),
        vec![text("Cid")]
    );
    // A missing property is null, so the quantifier is null and the row is
    // filtered out rather than matched.
    assert_eq!(
        names(
            &mut database,
            "MATCH (p:Person) WHERE none(s IN p.scores WHERE s < 5) RETURN p.name ORDER BY p.name"
        ),
        vec![text("Ann"), text("Cid")]
    );
    let result = database
        .execute_with(
            "MATCH (p:Person) \
             RETURN p.name AS name, any(s IN p.scores WHERE s IN $wanted) AS hit \
             ORDER BY name",
            &parameters([("wanted", json!([3, 4]))]),
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![text("Ann"), FALSE],
            vec![text("Bob"), TRUE],
            vec![text("Cid"), FALSE],
            vec![text("Dee"), NULL],
        ]
    );
    // Quantifiers sort like any other boolean and feed aggregates.
    let result = database
        .execute(
            "MATCH (p:Person) WITH any(s IN p.scores WHERE s = 9) AS nine \
             RETURN nine, count(*) AS people ORDER BY nine",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![FALSE, CypherValue::Integer(1)],
            vec![TRUE, CypherValue::Integer(2)],
            vec![NULL, CypherValue::Integer(1)],
        ]
    );
}

#[test]
fn quantifier_rejections_and_plan_rendering_are_explicit() {
    let mut database = Database::in_memory().unwrap();
    let error = database
        .execute("MATCH (n) RETURN any(x IN [1] WHERE x = count(*)) AS value")
        .unwrap_err();
    assert!(
        matches!(&error, Error::Semantic(message) if message.contains("aggregate")),
        "{error:?}"
    );
    let error = database
        .execute("MATCH (n) RETURN any(x IN [n] WHERE type(x) = 'a') AS value")
        .unwrap_err();
    // A literal node list retains the member kind; type() requires a relationship.
    assert!(matches!(&error, Error::Semantic(_)), "{error:?}");
    let error = database
        .execute("RETURN any(x IN [1] WHERE y = 1) AS value")
        .unwrap_err();
    assert!(matches!(&error, Error::Semantic(_)), "{error:?}");

    let plan = explain("RETURN single(x IN [1, 2] WHERE x > 1) AS value").unwrap();
    assert!(
        plan.contains(
            "single(#l0 in list[literal(1), literal(2)] where greater-than(#l0, literal(1)))"
        ),
        "{plan}"
    );
}
