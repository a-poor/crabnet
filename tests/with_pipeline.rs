use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, serde_json::Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

#[test]
fn standalone_and_chained_with_carry_typed_scalars() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "WITH 2 AS start \
             WITH start * 3 AS product, start + 1 AS increment \
             RETURN product, increment",
        )
        .unwrap();

    assert_eq!(result.columns, ["product", "increment"]);
    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(6), CypherValue::Integer(3)]]
    );

    let typed = database
        .execute(
            "WITH true AS flag, null AS absent, [1, 'two'] AS items, \
                  {nested: false} AS details \
             RETURN flag, absent, items, details",
        )
        .unwrap();
    assert_eq!(
        typed.rows,
        [vec![
            CypherValue::Boolean(true),
            CypherValue::Null,
            CypherValue::List(vec![
                CypherValue::Integer(1),
                CypherValue::String("two".into()),
            ]),
            CypherValue::Map(
                [("nested".into(), CypherValue::Boolean(false))]
                    .into_iter()
                    .collect()
            ),
        ]]
    );
}

#[test]
fn with_can_alias_entities_and_drops_names_it_does_not_project() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Ada'}), (:Person {name: 'Grace'})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:Person) \
             WITH person AS candidate \
             RETURN candidate.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("Ada".into())],
            vec![CypherValue::String("Grace".into())],
        ]
    );

    for query in [
        "MATCH (person:Person) WITH person.name AS name RETURN person",
        "MATCH (person:Person) WITH person AS candidate RETURN person",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "query should lose the original `person` binding: {query}"
        );
    }
}

#[test]
fn with_where_filters_using_the_projected_scope() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada', score: 9}), \
                    (:Person {name: 'Grace', score: 10}), \
                    (:Person {name: 'Linus', score: 7})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:Person) \
             WITH person.name AS name, person.score AS score \
             WHERE score >= 9 \
             RETURN name ORDER BY name",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("Ada".into())],
            vec![CypherValue::String("Grace".into())],
        ]
    );

    let using_unprojected_input = database
        .execute(
            "MATCH (person:Person) \
             WITH person.name AS name WHERE person.score >= 9 \
             RETURN name ORDER BY name",
        )
        .unwrap();
    assert_eq!(using_unprojected_input.rows, result.rows);
}

#[test]
fn with_ordering_and_pagination_run_before_a_later_match() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'first', rank: 1}), \
                    (:Person {name: 'second', rank: 2}), \
                    (:Person {name: 'third', rank: 3}), \
                    (:Tag {name: 'a'}), (:Tag {name: 'b'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:Person) \
             WITH person ORDER BY person.rank OFFSET 1 LIMIT 1 \
             MATCH (tag:Tag) \
             RETURN person.name AS person, tag.name AS tag ORDER BY tag",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("second".into()),
                CypherValue::String("a".into()),
            ],
            vec![
                CypherValue::String("second".into()),
                CypherValue::String("b".into()),
            ],
        ]
    );

    let ordered = database
        .execute(
            "MATCH (person:Person) \
             WITH person ORDER BY person.rank DESC \
             WITH person \
             RETURN person.name AS name",
        )
        .unwrap();
    assert_eq!(
        ordered.rows,
        [
            vec![CypherValue::String("third".into())],
            vec![CypherValue::String("second".into())],
            vec![CypherValue::String("first".into())],
        ]
    );
}

#[test]
fn carried_entities_can_expand_and_preserve_match_multiplicity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'source'}), (:Person {name: 'target'})")
        .unwrap();
    database
        .execute(
            "MATCH (source:Person {name: 'source'}), (target:Person {name: 'target'}) \
             CREATE (source)-[:LINK]->(target), (source)-[:LINK]->(target)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (source:Person {name: 'source'}) \
             WITH source \
             MATCH (source)-[:LINK]->(target) \
             RETURN target.name AS name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("target".into())],
            vec![CypherValue::String("target".into())],
        ]
    );

    let empty = database
        .execute(
            "MATCH (source:Person {name: 'missing'}) \
             WITH source \
             MATCH (source)-[:LINK]->(target) \
             RETURN target.name AS name",
        )
        .unwrap();
    assert!(empty.rows.is_empty());
}

#[test]
fn parameters_can_cross_with_boundaries_and_filter_a_later_match() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada', score: 9}), \
                    (:Person {name: 'Grace', score: 10}), \
                    (:Person {name: 'Linus', score: 7})",
        )
        .unwrap();
    let values = parameters([("minimum", json!(9))]);

    let result = database
        .execute_with(
            "WITH $minimum AS minimum \
             MATCH (person:Person) WHERE person.score >= minimum \
             RETURN person.name AS name ORDER BY name",
            &values,
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("Ada".into())],
            vec![CypherValue::String("Grace".into())],
        ]
    );
}

#[test]
fn computed_with_expressions_require_aliases() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute("WITH 1 + 2 RETURN 3"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn duplicate_with_aliases_and_scalar_pattern_bindings_are_semantic_errors() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "WITH 1 AS value, 2 AS value RETURN value",
        "WITH 'not an entity' AS entity MATCH (entity) RETURN entity",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "query should fail semantic analysis: {query}"
        );
    }
}

#[test]
fn with_all_and_alias_shadowing_bind_projection_items_simultaneously() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "WITH 1 AS left, 2 AS right \
             WITH ALL left AS right, right AS original_right \
             RETURN right, original_right",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(1), CypherValue::Integer(2)]]
    );

    assert!(matches!(
        database.execute("WITH 1 AS first, first + 1 AS second RETURN second"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn a_dropped_graph_name_can_be_bound_fresh_by_a_later_match() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Ada'}), (:Tag {name: 'database'})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:Person) \
             WITH item.name AS person_name \
             MATCH (item:Tag) \
             RETURN person_name, item.name AS tag",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("Ada".into()),
            CypherValue::String("database".into()),
        ]]
    );
}

#[test]
fn relationships_keep_their_graph_kind_across_with() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Node)-[:LINK {weight: 3}]->(:Node)")
        .unwrap();

    let result = database
        .execute(
            "MATCH ()-[relationship:LINK]->() \
             WITH relationship AS carried \
             RETURN type(carried) AS kind, carried.weight AS weight, carried",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("LINK".into()));
    assert_eq!(result.rows[0][1], CypherValue::Integer(3));
    assert!(matches!(result.rows[0][2], CypherValue::Relationship(_)));
}

#[test]
fn with_pagination_parameters_are_validated_before_sql_runs() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute("WITH 1 AS value LIMIT $missing RETURN value"),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));
    assert!(matches!(
        database.execute_with(
            "WITH 1 AS value SKIP $skip RETURN value",
            &parameters([("skip", json!(-1))]),
        ),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn with_after_a_write_projects_the_created_rows() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute("CREATE (node:Created {order: 1}) WITH node RETURN node.order AS order")
        .unwrap();
    assert_eq!(result.columns, vec!["order"]);
    assert_eq!(result.rows, vec![vec![CypherValue::Integer(1)]]);
    assert_eq!(result.summary.nodes_created, 1);
    let remaining = database
        .execute("MATCH (node:Created) RETURN count(node) AS total")
        .unwrap();
    assert_eq!(remaining.rows, vec![vec![CypherValue::Integer(1)]]);
}

#[test]
fn consecutive_match_clauses_use_the_composable_read_pipeline() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Ada'}), (:Language {name: 'Rust'})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:Person) \
             MATCH (language:Language) \
             RETURN person.name AS person, language.name AS language",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("Ada".into()),
            CypherValue::String("Rust".into()),
        ]]
    );

    let explanation =
        crabnet::explain("MATCH (person:Person) MATCH (language:Language) RETURN person, language")
            .unwrap();
    assert!(explanation.contains("stage 0: match"));
    assert!(explanation.contains("stage 1: match"));
}
