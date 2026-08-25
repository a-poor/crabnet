use std::collections::BTreeMap;

use crabnet::{Database, Error, Parameters, Value};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

#[test]
fn creates_and_matches_nodes_with_typed_properties() {
    let mut database = Database::in_memory().unwrap();

    let created = database
        .execute(
            "CREATE (ada:Person {name: 'Ada', born: 1815, active: true, note: null, \
             tags: ['math', 'poetry'], details: {country: 'UK'}}) \
             RETURN ada.name AS name, ada.active AS active",
        )
        .unwrap();

    assert_eq!(created.columns, ["name", "active"]);
    assert_eq!(created.rows, [vec![json!("Ada"), json!(true)]]);
    assert_eq!(created.summary.nodes_created, 1);
    assert_eq!(created.summary.relationships_created, 0);

    let matched = database
        .execute(
            "MATCH (person:Person) RETURN person.name AS name, person.born AS born, \
             person.active AS active, person.note AS note, person.tags AS tags, \
             person.details AS details, person.missing AS missing",
        )
        .unwrap();

    assert_eq!(
        matched.columns,
        [
            "name", "born", "active", "note", "tags", "details", "missing"
        ]
    );
    assert_eq!(
        matched.rows,
        [vec![
            json!("Ada"),
            json!(1815),
            json!(true),
            Value::Null,
            json!(["math", "poetry"]),
            json!({"country": "UK"}),
            Value::Null,
        ]]
    );
}

#[test]
fn parameters_are_bound_as_data_and_can_be_reused() {
    let mut database = Database::in_memory().unwrap();
    let suspicious = "Grace'); DROP TABLE nodes; --";
    let values = parameters([
        ("name", json!(suspicious)),
        ("born", json!(1906)),
        ("active", json!(false)),
    ]);

    database
        .execute_with(
            "CREATE (:Person {name: $name, display: $name, born: $born, active: $active})",
            &values,
        )
        .unwrap();

    let matched = database
        .execute_with(
            "MATCH (person:Person {name: $name}) \
             RETURN person.name AS name, person.display AS display, person.active AS active",
            &values,
        )
        .unwrap();

    assert_eq!(
        matched.rows,
        [vec![json!(suspicious), json!(suspicious), json!(false)]]
    );
}

#[test]
fn creates_and_matches_a_directed_relationship() {
    let mut database = Database::in_memory().unwrap();

    let created = database
        .execute(
            "CREATE (alice:Person {name: 'Alice'})-\
             [knows:KNOWS {since: 2020}]->\
             (bob:Person {name: 'Bob'}) \
             RETURN alice.name AS from, knows.since AS since, bob.name AS to",
        )
        .unwrap();

    assert_eq!(
        created.rows,
        [vec![json!("Alice"), json!(2020), json!("Bob")]]
    );
    assert_eq!(created.summary.nodes_created, 2);
    assert_eq!(created.summary.relationships_created, 1);

    let forward = database
        .execute(
            "MATCH (alice:Person)-[knows:KNOWS]->(bob:Person) \
             RETURN alice.name AS from, knows.since AS since, bob.name AS to",
        )
        .unwrap();
    assert_eq!(forward.rows, created.rows);

    let incoming = database
        .execute(
            "MATCH (bob:Person)<-[knows:KNOWS]-(alice:Person) \
             RETURN alice.name AS from, knows.since AS since, bob.name AS to",
        )
        .unwrap();
    assert_eq!(incoming.rows, created.rows);

    let wrong_direction = database
        .execute(
            "MATCH (bob:Person)-[:KNOWS]->(alice:Person) \
             WHERE bob.name = 'Bob' RETURN alice.name AS name",
        )
        .unwrap();
    assert!(wrong_direction.rows.is_empty());
}

#[test]
fn where_order_by_and_limit_are_lowered_to_sql() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada', born: 1815}), \
                     (:Person {name: 'Grace', born: 1906}), \
                     (:Person {name: 'Margaret', born: 1936}), \
                     (:Language {name: 'Rust', born: 2015})",
        )
        .unwrap();

    let values = parameters([("after", json!(1800)), ("excluded", json!("Ada"))]);
    let matched = database
        .execute_with(
            "MATCH (person:Person) \
             WHERE person.born > $after AND person.name <> $excluded \
             RETURN person.name AS name, person.born AS born \
             ORDER BY person.born DESC LIMIT 1",
            &values,
        )
        .unwrap();

    assert_eq!(matched.rows, [vec![json!("Margaret"), json!(1936)]]);

    let by_alias = database
        .execute(
            "MATCH (person:Person) RETURN person.name AS name \
             ORDER BY name ASC SKIP 1 LIMIT 1",
        )
        .unwrap();
    assert_eq!(by_alias.rows, [vec![json!("Grace")]]);

    database
        .execute("CREATE (:Number {value: 2}), (:Number {value: 10})")
        .unwrap();
    let numeric_alias = database
        .execute(
            "MATCH (number:Number) RETURN number.value AS value \
             ORDER BY value ASC",
        )
        .unwrap();
    assert_eq!(numeric_alias.rows, [vec![json!(2)], vec![json!(10)]]);
}

#[test]
fn graph_functions_and_whole_property_map_parameters_work() {
    let mut database = Database::in_memory().unwrap();
    let values = parameters([(
        "props",
        json!({"name": "Alice", "active": true, "scores": [1, 2]}),
    )]);

    let created = database
        .execute_with(
            "CREATE (alice:Person $props)-[knows:KNOWS]->(bob) \
             RETURN elementId(alice) AS id, labels(alice) AS labels, \
                    labels(bob) AS no_labels, type(knows) AS kind, \
                    alice.scores AS scores",
            &values,
        )
        .unwrap();

    assert_eq!(created.rows.len(), 1);
    assert_eq!(created.rows[0][0].as_str().unwrap().len(), 32);
    assert_eq!(
        created.rows[0][1..],
        [json!(["Person"]), json!([]), json!("KNOWS"), json!([1, 2]),]
    );

    let matched = database
        .execute(
            "MATCH (alice:Person)-[knows:KNOWS]->(bob) \
             RETURN elementId(alice) AS id, labels(alice) AS labels, \
                    labels(bob) AS no_labels, type(knows) AS kind",
        )
        .unwrap();
    assert_eq!(matched.rows[0][..4], created.rows[0][..4]);
}

#[test]
fn unusual_property_names_are_bound_as_json_paths() {
    let mut database = Database::in_memory().unwrap();

    database
        .execute(
            "CREATE (:Thing {`dot.key`: 'dot', `space key`: 'space', \
                     `quote\"key`: 'quote', `sql'); DROP TABLE nodes; --`: 'safe'})",
        )
        .unwrap();
    let matched = database
        .execute(
            "MATCH (thing:Thing) \
             RETURN thing.`dot.key` AS dot, thing.`space key` AS space, \
                    thing.`quote\"key` AS quote, \
                    thing.`sql'); DROP TABLE nodes; --` AS attack",
        )
        .unwrap();

    assert_eq!(
        matched.rows,
        [vec![
            json!("dot"),
            json!("space"),
            json!("quote"),
            json!("safe")
        ]]
    );
}

#[test]
fn boolean_properties_do_not_compare_equal_to_integers() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Thing {name: 'boolean', value: true}), \
                    (:Thing {name: 'integer', value: 1}), \
                    (:Thing {name: 'false', value: false}), \
                    (:Thing {name: 'zero', value: 0}), \
                    (:Thing {name: 'array', value: [1]}), \
                    (:Thing {name: 'array text', value: '[1]'})",
        )
        .unwrap();

    let inline = database
        .execute("MATCH (thing:Thing {value: true}) RETURN thing.name AS name")
        .unwrap();
    assert_eq!(inline.rows, [vec![json!("boolean")]]);

    let predicate = database
        .execute(
            "MATCH (thing:Thing) WHERE thing.value = 1.0 \
             RETURN thing.name AS name",
        )
        .unwrap();
    assert_eq!(predicate.rows, [vec![json!("integer")]]);

    let structured = database
        .execute("MATCH (thing:Thing {value: [1]}) RETURN thing.name AS name")
        .unwrap();
    assert_eq!(structured.rows, [vec![json!("array")]]);
}

#[test]
fn one_relationship_cannot_fill_two_positions_in_a_path() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (node:Node {name: 'loop'})-[:LOOP]->(node)")
        .unwrap();

    let result = database
        .execute(
            "MATCH (a:Node)-[:LOOP]->(b:Node)-[:LOOP]->(c:Node) \
             RETURN a.name AS name",
        )
        .unwrap();
    assert!(result.rows.is_empty());

    assert!(matches!(
        database.execute(
            "MATCH (a:Node)-[relationship:LOOP]->(b:Node)-[relationship]->(c:Node) \
             RETURN a.name"
        ),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn missing_parameters_are_reported_before_a_write_starts() {
    let mut database = Database::in_memory().unwrap();
    let values = parameters([("present", json!(1))]);

    let error = database
        .execute_with(
            "CREATE (:Thing {value: $present}), (:Thing {value: $missing})",
            &values,
        )
        .unwrap_err();
    assert!(matches!(error, Error::MissingParameter(name) if name == "missing"));

    let remaining = database
        .execute("MATCH (thing:Thing) RETURN thing.value AS value")
        .unwrap();
    assert!(remaining.rows.is_empty());
}

#[test]
fn parse_semantic_and_unsupported_errors_are_distinct() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute("MATCH (node RETURN node"),
        Err(Error::Parse(_))
    ));
    assert!(matches!(
        database.execute("MATCH (node) RETURN missing.name"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (a)-[*1..3]->(b) RETURN a"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(database.execute("  ;"), Err(Error::EmptyQuery)));
    assert!(matches!(database.execute(";;"), Err(Error::Parse(_))));
    assert!(matches!(
        database.execute("CREATE (:Person:Author)"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("MATCH (node) RETURN DISTINCT node.name"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("MATCH (node) RETURN id(node)"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn pagination_requires_nonnegative_integer_constants() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Number {value: 1}), (:Number {value: 2})")
        .unwrap();

    for query in [
        "MATCH (number:Number) RETURN number.value LIMIT -1",
        "MATCH (number:Number) RETURN number.value LIMIT 1.5",
        "MATCH (number:Number) RETURN number.value LIMIT number.value",
        "MATCH (number:Number) RETURN number.value SKIP 'one'",
    ] {
        assert!(matches!(database.execute(query), Err(Error::Semantic(_))));
    }

    let invalid = parameters([("limit", json!(-1))]);
    assert!(matches!(
        database.execute_with(
            "MATCH (number:Number) RETURN number.value LIMIT $limit",
            &invalid
        ),
        Err(Error::Semantic(_))
    ));

    let valid = parameters([("limit", json!(1))]);
    let result = database
        .execute_with(
            "MATCH (number:Number) RETURN number.value AS value \
             ORDER BY value LIMIT $limit",
            &valid,
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!(1)]]);
}

#[test]
fn integer_domain_and_duplicate_match_properties_are_validated() {
    let mut database = Database::in_memory().unwrap();
    let minimum = database
        .execute(
            "CREATE (bound:Bound {value: -9223372036854775808}) \
             RETURN bound.value AS value",
        )
        .unwrap();
    assert_eq!(minimum.rows, [vec![json!(i64::MIN)]]);

    let matched_minimum = database
        .execute(
            "MATCH (bound:Bound {value: -9223372036854775808}) \
             RETURN bound.value AS value",
        )
        .unwrap();
    assert_eq!(matched_minimum.rows, minimum.rows);

    assert!(matches!(
        database.execute(
            "MATCH (bound:Bound {value: -9223372036854775808, \
                                 value: -9223372036854775808}) \
             RETURN bound.value"
        ),
        Err(Error::Semantic(_))
    ));

    let values = parameters([("too_big", json!(u64::MAX))]);
    assert!(matches!(
        database.execute_with(
            "CREATE (:TooBig {value: 1}), (:TooBig {value: $too_big})",
            &values
        ),
        Err(Error::Semantic(_))
    ));
    let none_created = database
        .execute("MATCH (value:TooBig) RETURN value.value")
        .unwrap();
    assert!(none_created.rows.is_empty());
}

#[test]
fn an_unsupported_write_does_not_mutate_the_graph() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Person {name: 'Ada'})").unwrap();

    assert!(matches!(
        database.execute("MATCH (person:Person) DELETE person"),
        Err(Error::Unsupported(_))
    ));

    let remaining = database
        .execute("MATCH (person:Person) RETURN person.name AS name")
        .unwrap();
    assert_eq!(remaining.rows, [vec![json!("Ada")]]);
}
