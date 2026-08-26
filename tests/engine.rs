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
        database.execute("MATCH (a)-[*]->(b) RETURN a"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(database.execute("  ;"), Err(Error::EmptyQuery)));
    assert!(matches!(database.execute(";;"), Err(Error::Parse(_))));
    assert!(matches!(
        database.execute("CREATE (:Person:Author)"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("MATCH (node) RETURN *"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("MATCH (node) RETURN id(node)"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("CREATE (node) RETURN node.value + 1"),
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
fn unsupported_merge_does_not_mutate_the_graph() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Person {name: 'Ada'})").unwrap();

    assert!(matches!(
        database.execute(
            "MATCH (person:Person) \
             MERGE (:Person {name: 'Grace'})"
        ),
        Err(Error::Unsupported(_))
    ));

    let remaining = database
        .execute("MATCH (person:Person) RETURN person.name AS name")
        .unwrap();
    assert_eq!(remaining.rows, [vec![json!("Ada")]]);
}

#[test]
fn match_create_uses_existing_nodes_and_returns_the_created_relationship() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'}), \
                     (:Person {name: 'Carol'})",
        )
        .unwrap();

    let values = parameters([("since", json!(2024))]);
    let created = database
        .execute_with(
            "MATCH (alice:Person {name: 'Alice'}), (bob:Person {name: 'Bob'}) \
             CREATE (alice)-[knows:KNOWS {since: $since}]->(bob) \
             RETURN alice.name AS from, type(knows) AS kind, \
                    knows.since AS since, bob.name AS to, \
                    elementId(knows) AS relationship_id",
            &values,
        )
        .unwrap();

    assert_eq!(
        created.columns,
        ["from", "kind", "since", "to", "relationship_id"]
    );
    assert_eq!(created.rows.len(), 1);
    assert_eq!(
        created.rows[0][..4],
        [json!("Alice"), json!("KNOWS"), json!(2024), json!("Bob")]
    );
    assert_eq!(created.rows[0][4].as_str().unwrap().len(), 32);
    assert_eq!(created.summary.nodes_created, 0);
    assert_eq!(created.summary.relationships_created, 1);

    let matched = database
        .execute(
            "MATCH (alice:Person)-[knows:KNOWS]->(bob:Person) \
             RETURN alice.name AS from, knows.since AS since, bob.name AS to",
        )
        .unwrap();
    assert_eq!(
        matched.rows,
        [vec![json!("Alice"), json!(2024), json!("Bob")]]
    );
}

#[test]
fn match_create_runs_once_per_input_row() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada'}), (:Person {name: 'Grace'}), \
                     (:Project {name: 'Compiler'})",
        )
        .unwrap();

    let created = database
        .execute(
            "MATCH (person:Person), (project:Project {name: 'Compiler'}) \
             CREATE (person)-[contribution:CONTRIBUTED {role: 'author'}]->(project) \
             RETURN person.name AS person, contribution.role AS role, \
                    elementId(contribution) AS id",
        )
        .unwrap();

    let mut rows = created.rows;
    rows.sort_by(|left, right| left[0].as_str().unwrap().cmp(right[0].as_str().unwrap()));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][..2], [json!("Ada"), json!("author")]);
    assert_eq!(rows[1][..2], [json!("Grace"), json!("author")]);
    assert_eq!(rows[0][2].as_str().unwrap().len(), 32);
    assert_eq!(rows[1][2].as_str().unwrap().len(), 32);
    assert_ne!(rows[0][2], rows[1][2]);
    assert_eq!(created.summary.nodes_created, 0);
    assert_eq!(created.summary.relationships_created, 2);
}

#[test]
fn match_create_can_add_fresh_nodes_and_no_match_is_a_noop() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Alice'})")
        .unwrap();

    let created = database
        .execute(
            "MATCH (alice:Person {name: 'Alice'}) \
             CREATE (alice)-[:KNOWS]->(bob:Person {name: 'Bob'}) \
             RETURN alice.name AS from, bob.name AS to, labels(bob) AS labels",
        )
        .unwrap();
    assert_eq!(
        created.rows,
        [vec![json!("Alice"), json!("Bob"), json!(["Person"])]]
    );
    assert_eq!(created.summary.nodes_created, 1);
    assert_eq!(created.summary.relationships_created, 1);

    let not_created = database
        .execute(
            "MATCH (missing:Person {name: 'Nobody'}) \
             CREATE (missing)-[:KNOWS]->(:Person {name: 'Never'}) \
             RETURN missing.name",
        )
        .unwrap();
    assert!(not_created.rows.is_empty());
    assert_eq!(not_created.summary.nodes_created, 0);
    assert_eq!(not_created.summary.relationships_created, 0);

    let people = database
        .execute("MATCH (person:Person) RETURN person.name AS name")
        .unwrap();
    let mut names = people.rows;
    names.sort_by(|left, right| left[0].as_str().unwrap().cmp(right[0].as_str().unwrap()));
    assert_eq!(names, [vec![json!("Alice")], vec![json!("Bob")]]);
}

#[test]
fn match_create_validates_the_whole_write_before_mutating() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Alice'}), (:Person {name: 'Bob'})")
        .unwrap();

    assert!(matches!(
        database.execute(
            "MATCH (person:Person {name: 'Alice'}) \
             CREATE (person:Person)-[:KNOWS]->(:Person)"
        ),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (person:Person) CREATE (person)"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute(
            "MATCH (from)-[relationship:KNOWS]->(to) \
             CREATE (from)-[relationship:OTHER]->(to)"
        ),
        Err(Error::Semantic(_))
    ));

    let values = parameters([("present", json!(true))]);
    assert!(matches!(
        database.execute_with(
            "MATCH (person:Person) \
             CREATE (person)-[:KNOWS {ok: $present, bad: $missing}]->(:Person)",
            &values,
        ),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));
    assert!(matches!(
        database.execute_with(
            "MATCH (:Missing) CREATE (:Thing) RETURN $missing",
            &Parameters::new(),
        ),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));

    let no_match = database
        .execute("MATCH (:Missing) CREATE (:Thing)")
        .unwrap();
    assert!(no_match.rows.is_empty());
    assert_eq!(no_match.summary.nodes_created, 0);
    assert_eq!(no_match.summary.relationships_created, 0);

    let relationships = database
        .execute("MATCH ()-[relationship]->() RETURN elementId(relationship)")
        .unwrap();
    assert!(relationships.rows.is_empty());
    let people = database
        .execute("MATCH (person:Person) RETURN person.name")
        .unwrap();
    assert_eq!(people.rows.len(), 2);
}

#[test]
fn match_create_keeps_matched_relationships_in_scope() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (from:Node {name: 'from'})-\
             [old:OLD {version: 1}]->(to:Node {name: 'to'})",
        )
        .unwrap();

    let created = database
        .execute(
            "MATCH (from:Node)-[old:OLD]->(to:Node) \
             CREATE (from)-[new:NEW {version: 2}]->(to) \
             RETURN type(old) AS old_type, old.version AS old_version, \
                    type(new) AS new_type, new.version AS new_version",
        )
        .unwrap();

    assert_eq!(
        created.rows,
        [vec![json!("OLD"), json!(1), json!("NEW"), json!(2)]]
    );
    assert_eq!(created.summary.nodes_created, 0);
    assert_eq!(created.summary.relationships_created, 1);
}

#[test]
fn match_create_reuses_one_fresh_node_across_pattern_parts() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Anchor {name: 'root'})").unwrap();

    let created = database
        .execute(
            "MATCH (anchor:Anchor {name: 'root'}) \
             CREATE (bridge:Bridge {name: 'shared'}), \
                    (anchor)-[:OUT]->(bridge), \
                    (bridge)-[:BACK]->(anchor) \
             RETURN elementId(bridge) AS bridge_id",
        )
        .unwrap();
    assert_eq!(created.rows.len(), 1);
    assert_eq!(created.summary.nodes_created, 1);
    assert_eq!(created.summary.relationships_created, 2);

    let matched = database
        .execute(
            "MATCH (anchor:Anchor)-[:OUT]->(bridge:Bridge)-[:BACK]->(anchor) \
             RETURN elementId(bridge) AS bridge_id",
        )
        .unwrap();
    assert_eq!(matched.rows, created.rows);
}

#[test]
fn match_create_supports_self_relationships_and_left_arrows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Node {name: 'one'}), (:Node {name: 'two'})")
        .unwrap();

    let self_relationship = database
        .execute(
            "MATCH (node:Node {name: 'one'}) \
             CREATE (node)-[relationship:SELF]->(node) \
             RETURN node.name AS name, type(relationship) AS kind",
        )
        .unwrap();
    assert_eq!(self_relationship.rows, [vec![json!("one"), json!("SELF")]]);

    database
        .execute(
            "MATCH (one:Node {name: 'one'}), (two:Node {name: 'two'}) \
             CREATE (one)<-[:LEFT]-(two)",
        )
        .unwrap();
    let direction = database
        .execute(
            "MATCH (two:Node)-[:LEFT]->(one:Node) \
             RETURN two.name AS from, one.name AS to",
        )
        .unwrap();
    assert_eq!(direction.rows, [vec![json!("two"), json!("one")]]);
}

#[test]
fn match_input_is_materialized_before_create_runs() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Seed {number: 1}), (:Seed {number: 2})")
        .unwrap();

    let created = database
        .execute("MATCH (:Seed) CREATE (:Seed {created: true})")
        .unwrap();
    assert_eq!(created.summary.nodes_created, 2);

    let all = database
        .execute("MATCH (seed:Seed) RETURN elementId(seed)")
        .unwrap();
    assert_eq!(all.rows.len(), 4);
    let generated = database
        .execute("MATCH (seed:Seed {created: true}) RETURN elementId(seed)")
        .unwrap();
    assert_eq!(generated.rows.len(), 2);
}

#[test]
fn signed_constants_keep_typed_equality_in_where() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Number {value: -1}), (:Number {value: 1})")
        .unwrap();

    let negative = database
        .execute(
            "MATCH (number:Number) WHERE number.value = -1 \
             RETURN number.value AS value",
        )
        .unwrap();
    assert_eq!(negative.rows, [vec![json!(-1)]]);
}
