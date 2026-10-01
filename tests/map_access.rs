use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters, Value, explain};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn row(database: &mut Database, cypher: &str) -> Vec<CypherValue> {
    let result = database.execute(cypher).unwrap();
    assert_eq!(result.rows.len(), 1, "{cypher}");
    result.rows.into_iter().next().unwrap()
}

fn integer(value: i64) -> CypherValue {
    CypherValue::Integer(value)
}

fn text(value: &str) -> CypherValue {
    CypherValue::String(value.to_owned())
}

#[test]
fn map_keys_are_read_from_projected_maps_with_missing_and_null_keys_as_null() {
    let mut database = Database::in_memory().unwrap();
    assert_eq!(
        row(
            &mut database,
            "WITH {existing: 42, notMissing: null} AS m \
             RETURN m.missing, m.notMissing, m.existing"
        ),
        vec![CypherValue::Null, CypherValue::Null, integer(42)]
    );
    assert_eq!(
        row(&mut database, "WITH null AS m RETURN m.missing"),
        vec![CypherValue::Null]
    );
    // Keys are case-sensitive and may be delimited identifiers.
    assert_eq!(
        row(
            &mut database,
            "WITH {Key: 1, key: 2, `two words`: 3} AS m \
             RETURN m.Key, m.key, m.`two words`, m.KEY"
        ),
        vec![integer(1), integer(2), integer(3), CypherValue::Null]
    );
}

#[test]
fn map_keys_are_read_from_nested_maps_parameters_properties_and_expressions() {
    let mut database = Database::in_memory().unwrap();
    assert_eq!(
        row(
            &mut database,
            "WITH {a: {b: [1, 2], c: {d: 'deep'}}} AS m RETURN m.a.b, m.a.c.d, m.a.x.y"
        ),
        vec![
            CypherValue::List(vec![integer(1), integer(2)]),
            text("deep"),
            CypherValue::Null,
        ]
    );
    assert_eq!(
        row(&mut database, "RETURN ({a: 1}).a AS value"),
        vec![integer(1)]
    );
    let result = database
        .execute_with(
            "RETURN $m.k AS direct, $m.nested.k AS nested",
            &parameters([("m", json!({"k": "v", "nested": {"k": [true]}}))]),
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            text("v"),
            CypherValue::List(vec![CypherValue::Boolean(true)])
        ]]
    );
    database
        .execute(
            "CREATE (:Doc {name: 'a', meta: {tag: 'x', score: 1.5}}), \
                    (:Doc {name: 'b', meta: {tag: 'y'}}), \
                    (:Doc {name: 'c'})",
        )
        .unwrap();
    let result = database
        .execute(
            "MATCH (d:Doc) RETURN d.name AS name, d.meta.tag AS tag, d.meta.score AS score \
             ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![text("a"), text("x"), CypherValue::Float(1.5)],
            vec![text("b"), text("y"), CypherValue::Null],
            vec![text("c"), CypherValue::Null, CypherValue::Null],
        ]
    );
    let result = database
        .execute("MATCH (d:Doc) WHERE d.meta.tag = 'y' RETURN d.name AS name")
        .unwrap();
    assert_eq!(result.rows, vec![vec![text("b")]]);
    // Quantified list elements expose map keys the same way.
    assert_eq!(
        row(
            &mut database,
            "RETURN none(x IN [{a: 1}, {a: 2}] WHERE x.a = 2) AS none, \
                    single(x IN [{a: 1}, {a: 2}, {a: 3}] WHERE x.a = 2) AS single"
        ),
        vec![CypherValue::Boolean(false), CypherValue::Boolean(true)]
    );
}

#[test]
fn map_key_access_on_non_maps_is_a_type_error_and_paths_are_rejected() {
    let mut database = Database::in_memory().unwrap();
    // A key of null reads as null.
    assert_eq!(
        row(&mut database, "WITH null AS m RETURN m.key"),
        vec![CypherValue::Null]
    );
    // A key of any other non-map is a type error: while planning when the
    // query text fixes the kind, and at runtime otherwise.
    for query in [
        "WITH 5 AS n RETURN n.key",
        "WITH 'text' AS s RETURN s.key",
        "WITH [1] AS l RETURN l.key",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    let parameters = parameters([("value", json!(5))]);
    assert!(matches!(
        database.execute_with("WITH $value AS n RETURN n.key", &parameters),
        Err(Error::Type(_))
    ));
    let error = database
        .execute("MATCH p = (a) RETURN p.length AS value")
        .unwrap_err();
    assert!(
        matches!(&error, Error::Semantic(message) if message.contains("path variable")),
        "{error:?}"
    );
    let error = database
        .execute("MATCH p = (a) WITH p AS q RETURN q.length AS value")
        .unwrap_err();
    assert!(
        matches!(&error, Error::Semantic(message) if message.contains("path variable")),
        "{error:?}"
    );
    let plan = explain("WITH {a: 1} AS m RETURN m.a AS value").unwrap();
    assert!(plan.contains("map-property("), "{plan}");
}
