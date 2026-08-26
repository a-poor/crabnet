use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters, Value};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

#[test]
fn return_without_match_supports_typed_values_and_parameters() {
    let mut database = Database::in_memory().unwrap();
    let values = parameters([("answer", json!(42))]);

    let result = database
        .execute_with(
            "RETURN 1 AS one, -2 AS negative, true AS yes, null AS nothing, \
                    $answer AS answer, [1, 'two'] AS list, {nested: false} AS map",
            &values,
        )
        .unwrap();

    assert_eq!(
        result.columns,
        ["one", "negative", "yes", "nothing", "answer", "list", "map"]
    );
    assert_eq!(
        result.rows,
        [vec![
            json!(1),
            json!(-2),
            json!(true),
            Value::Null,
            json!(42),
            json!([1, "two"]),
            json!({"nested": false}),
        ]]
    );
}

#[test]
fn missing_and_null_properties_follow_null_semantics() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Thing {name: 'missing'}), \
                    (:Thing {name: 'null', value: null}), \
                    (:Thing {name: 'one', value: 1}), \
                    (:Thing {name: 'two', value: 2})",
        )
        .unwrap();

    let nulls = database
        .execute(
            "MATCH (thing:Thing) WHERE thing.value IS NULL \
             RETURN thing.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(nulls.rows, [vec![json!("missing")], vec![json!("null")]]);

    let non_nulls = database
        .execute(
            "MATCH (thing:Thing) WHERE thing.value IS NOT NULL \
             RETURN thing.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(non_nulls.rows, [vec![json!("one")], vec![json!("two")]]);

    let equality_with_null = database
        .execute(
            "MATCH (thing:Thing) WHERE thing.value = null \
             RETURN thing.name AS name",
        )
        .unwrap();
    assert!(equality_with_null.rows.is_empty());
}

#[test]
fn boolean_predicates_preserve_three_valued_logic() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Flag {name: 'true', value: true}), \
                    (:Flag {name: 'false', value: false}), \
                    (:Flag {name: 'null', value: null}), \
                    (:Flag {name: 'missing'}), \
                    (:Flag {name: 'fallback', value: false})",
        )
        .unwrap();

    let negated = database
        .execute(
            "MATCH (flag:Flag) WHERE NOT flag.value \
             RETURN flag.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        negated.rows,
        [vec![json!("fallback")], vec![json!("false")]]
    );

    let disjunction = database
        .execute(
            "MATCH (flag:Flag) \
             WHERE flag.value OR flag.name = 'fallback' \
             RETURN flag.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        disjunction.rows,
        [vec![json!("fallback")], vec![json!("true")]]
    );

    let conjunction = database
        .execute(
            "MATCH (flag:Flag) \
             WHERE flag.value AND flag.name = 'true' \
             RETURN flag.name AS name",
        )
        .unwrap();
    assert_eq!(conjunction.rows, [vec![json!("true")]]);
}

#[test]
fn boolean_contexts_do_not_coerce_non_boolean_json_values() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:BooleanContext {name: 'true', value: true}), \
                    (:BooleanContext {name: 'false', value: false}), \
                    (:BooleanContext {name: 'number', value: 1}), \
                    (:BooleanContext {name: 'text', value: 'true'}), \
                    (:BooleanContext {name: 'null', value: null}), \
                    (:BooleanContext {name: 'missing'})",
        )
        .unwrap();

    let direct = database
        .execute(
            "MATCH (sample:BooleanContext) WHERE sample.value \
             RETURN sample.name AS name",
        )
        .unwrap();
    assert_eq!(direct.rows, [vec![json!("true")]]);

    let indeterminate = database
        .execute(
            "MATCH (sample:BooleanContext) \
             WHERE (sample.value AND true) IS NULL \
             RETURN sample.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        indeterminate.rows,
        [
            vec![json!("missing")],
            vec![json!("null")],
            vec![json!("number")],
            vec![json!("text")],
        ]
    );
}

#[test]
fn every_supported_ordering_comparison_is_exercised() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Number {value: 1}), (:Number {value: 2}), \
                    (:Number {value: 3})",
        )
        .unwrap();

    for (operator, expected) in [
        ("<", vec![json!(1)]),
        ("<=", vec![json!(1), json!(2)]),
        (">", vec![json!(3)]),
        (">=", vec![json!(2), json!(3)]),
    ] {
        let result = database
            .execute(&format!(
                "MATCH (number:Number) WHERE number.value {operator} 2 \
                 RETURN number.value AS value ORDER BY value"
            ))
            .unwrap();
        let expected = expected
            .into_iter()
            .map(|value| vec![value])
            .collect::<Vec<_>>();
        assert_eq!(result.rows, expected, "operator {operator}");
    }

    let not_equal = database
        .execute(
            "MATCH (number:Number) WHERE number.value <> 2 \
             RETURN number.value AS value ORDER BY value",
        )
        .unwrap();
    assert_eq!(not_equal.rows, [vec![json!(1)], vec![json!(3)]]);
}

#[test]
fn ordering_uses_cypher_default_null_placement() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:NullOrder {value: 1}), (:NullOrder {value: null}), \
                    (:NullOrder {value: 2}), (:NullOrder)",
        )
        .unwrap();

    let ascending = database
        .execute(
            "MATCH (item:NullOrder) \
             RETURN item.value AS value ORDER BY value",
        )
        .unwrap();
    assert_eq!(
        ascending.rows,
        [
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(2)],
            vec![CypherValue::Null],
            vec![CypherValue::Null],
        ]
    );

    let descending = database
        .execute(
            "MATCH (item:NullOrder) \
             RETURN item.value AS value ORDER BY value DESC",
        )
        .unwrap();
    assert_eq!(
        descending.rows,
        [
            vec![CypherValue::Null],
            vec![CypherValue::Null],
            vec![CypherValue::Integer(2)],
            vec![CypherValue::Integer(1)],
        ]
    );

    let ascending_page = database
        .execute(
            "MATCH (item:NullOrder) \
             RETURN item.value AS value ORDER BY value LIMIT 2",
        )
        .unwrap();
    assert_eq!(
        ascending_page.rows,
        [vec![CypherValue::Integer(1)], vec![CypherValue::Integer(2)],]
    );

    let descending_page = database
        .execute(
            "MATCH (item:NullOrder) \
             RETURN item.value AS value ORDER BY value DESC SKIP 2",
        )
        .unwrap();
    assert_eq!(
        descending_page.rows,
        [vec![CypherValue::Integer(2)], vec![CypherValue::Integer(1)],]
    );
}

#[test]
fn undirected_matches_check_both_orientations_without_duplicating_self_loops() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (one:Node {name: 'one'})-[:LINK]->(two:Node {name: 'two'}), \
                    (loop:Node {name: 'loop'})-[:LOOP]->(loop)",
        )
        .unwrap();

    let link = database
        .execute(
            "MATCH (left:Node)-[:LINK]-(right:Node) \
             RETURN left.name AS left, right.name AS right ORDER BY left",
        )
        .unwrap();
    assert_eq!(
        link.rows,
        [
            vec![json!("one"), json!("two")],
            vec![json!("two"), json!("one")],
        ]
    );

    let self_loop = database
        .execute("MATCH (node:Node)-[:LOOP]-(node) RETURN node.name AS name")
        .unwrap();
    assert_eq!(self_loop.rows, [vec![json!("loop")]]);
}

#[test]
fn fixed_length_match_supports_three_hops() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Stop {name: 'a'})-[:NEXT]->(:Stop {name: 'b'})-[:NEXT]->
                    (:Stop {name: 'c'})-[:NEXT]->(:Stop {name: 'd'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (a:Stop)-[:NEXT]->(b:Stop)-[:NEXT]->(c:Stop)-[:NEXT]->(d:Stop) \
             RETURN a.name, b.name, c.name, d.name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![json!("a"), json!("b"), json!("c"), json!("d")]]
    );
}

#[test]
fn parallel_three_hop_paths_preserve_row_multiplicity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Stop {name: 'a'}), (:Stop {name: 'b'}), \
                    (:Stop {name: 'c'}), (:Stop {name: 'd'}), \
                    (:Stop {name: 'e'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:Stop {name: 'a'}), (b:Stop {name: 'b'}), \
                   (c:Stop {name: 'c'}), (d:Stop {name: 'd'}), \
                   (e:Stop {name: 'e'}) \
             CREATE (a)-[:NEXT]->(b), (a)-[:NEXT]->(b), \
                    (b)-[:NEXT]->(c), (a)-[:NEXT]->(e), \
                    (e)-[:NEXT]->(c), (c)-[:NEXT]->(d)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (a:Stop {name: 'a'})-[:NEXT]->()-[:NEXT]->()-[:NEXT]->
                   (d:Stop {name: 'd'}) \
             RETURN a.name AS start, d.name AS end",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 3);
    assert!(
        result
            .rows
            .iter()
            .all(|row| row == &vec![json!("a"), json!("d")])
    );
}

#[test]
fn disconnected_patterns_preserve_cartesian_multiplicity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Color {name: 'blue'}), (:Color {name: 'red'}), \
                    (:Shape {name: 'circle'}), (:Shape {name: 'square'}), \
                    (:Shape {name: 'triangle'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (color:Color), (shape:Shape) \
             RETURN color.name AS color, shape.name AS shape \
             ORDER BY color, shape",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![json!("blue"), json!("circle")],
            vec![json!("blue"), json!("square")],
            vec![json!("blue"), json!("triangle")],
            vec![json!("red"), json!("circle")],
            vec![json!("red"), json!("square")],
            vec![json!("red"), json!("triangle")],
        ]
    );
}

#[test]
fn repeated_node_variables_reuse_the_same_binding() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (one:Node {name: 'one'})-[:SELF]->(one), \
                    (:Node {name: 'two'})",
        )
        .unwrap();

    let self_loop = database
        .execute("MATCH (node:Node)-[:SELF]->(node) RETURN node.name AS name")
        .unwrap();
    assert_eq!(self_loop.rows, [vec![json!("one")]]);

    let repeated_pattern = database
        .execute(
            "MATCH (node:Node), (node:Node) \
             RETURN node.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        repeated_pattern.rows,
        [vec![json!("one")], vec![json!("two")]]
    );
}

#[test]
fn inline_structured_properties_keep_json_types() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Thing {name: 'array', value: [1, 2]}), \
                    (:Thing {name: 'map', value: {left: 1, right: 2}}), \
                    (:Thing {name: 'array text', value: '[1,2]'}), \
                    (:Thing {name: 'map text', value: '{left:1,right:2}'})",
        )
        .unwrap();

    let array = database
        .execute("MATCH (thing:Thing {value: [1, 2]}) RETURN thing.name AS name")
        .unwrap();
    assert_eq!(array.rows, [vec![json!("array")]]);

    let map = database
        .execute(
            "MATCH (thing:Thing {value: {right: 2, left: 1}}) \
             RETURN thing.name AS name",
        )
        .unwrap();
    assert_eq!(map.rows, [vec![json!("map")]]);
}

#[test]
fn relationship_properties_filter_matches() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (from:Node {name: 'from'})-[:LINK {weight: 1}]->
                    (to:Node {name: 'to'}), \
                    (from)-[:LINK {weight: 2}]->(to)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (from:Node)-[link:LINK {weight: 2}]->(to:Node) \
             RETURN from.name, link.weight, to.name",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!("from"), json!(2), json!("to")]]);
}

#[test]
fn element_local_where_filters_nodes_and_relationships() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (from:Node {name: 'from', enabled: true})-
                    [:LINK {weight: 1}]->(to:Node {name: 'to'}), \
                    (from)-[:LINK {weight: 2}]->(to), \
                    (:Node {name: 'disabled', enabled: false})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (from:Node WHERE from.enabled)-
                   [link:LINK WHERE link.weight >= 2]->(to) \
             RETURN from.name AS from, link.weight AS weight, to.name AS to",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!("from"), json!(2), json!("to")]]);
}

#[test]
fn missing_read_parameters_are_reported() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Thing {value: 1})").unwrap();

    let error = database
        .execute_with(
            "MATCH (thing:Thing {value: $missing}) RETURN thing.value",
            &Parameters::new(),
        )
        .unwrap_err();
    assert!(matches!(error, Error::MissingParameter(name) if name == "missing"));
}

#[test]
fn property_to_property_equality_is_type_aware() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Pair {name: 'numeric', left: 1, right: 1.0}), \
                    (:Pair {name: 'boolean-number', left: true, right: 1}), \
                    (:Pair {name: 'list', left: [1], right: [1]}), \
                    (:Pair {name: 'list-text', left: [1], right: '[1]'}), \
                    (:Pair {name: 'map', left: {x: 1}, right: {x: 1}})",
        )
        .unwrap();

    let equal = database
        .execute(
            "MATCH (pair:Pair) WHERE pair.left = pair.right \
             RETURN pair.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        equal.rows,
        [
            vec![json!("list")],
            vec![json!("map")],
            vec![json!("numeric")],
        ]
    );

    let not_equal = database
        .execute(
            "MATCH (pair:Pair) WHERE pair.left <> pair.right \
             RETURN pair.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        not_equal.rows,
        [vec![json!("boolean-number")], vec![json!("list-text")]]
    );
}

#[test]
fn a_variable_can_be_shared_across_comma_separated_patterns() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (one:Node {name: 'one'})-[:LINK]->(:Node {name: 'two'}), \
                    (:Node {name: 'isolated'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (start:Node), (start)-[:LINK]->(end:Node) \
             RETURN start.name AS start, end.name AS end",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!("one"), json!("two")]]);
}

#[test]
fn offset_without_limit_and_return_all_are_supported() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Number {value: 1}), (:Number {value: 2}), \
                    (:Number {value: 3})",
        )
        .unwrap();
    let values = parameters([("offset", json!(1))]);

    let result = database
        .execute_with(
            "MATCH (number:Number) RETURN ALL number.value AS value \
             ORDER BY value OFFSET $offset",
            &values,
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!(2)], vec![json!(3)]]);

    let empty = database
        .execute(
            "MATCH (number:Number) RETURN number.value AS value \
             ORDER BY value OFFSET 0 LIMIT 0",
        )
        .unwrap();
    assert!(empty.rows.is_empty());

    let invalid = parameters([("offset", json!(-1))]);
    assert!(matches!(
        database.execute_with(
            "MATCH (number:Number) RETURN number.value OFFSET $offset",
            &invalid,
        ),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn untyped_relationship_matches_retain_each_runtime_type() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Node)-[:FIRST]->(:Node), (:Node)-[:SECOND]->(:Node)")
        .unwrap();

    let result = database
        .execute(
            "MATCH ()-[relationship]->() \
             RETURN type(relationship) AS kind ORDER BY kind",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!("FIRST")], vec![json!("SECOND")]]);
}

#[test]
fn graph_functions_validate_arity_arguments_and_entity_kinds() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Node)-[:LINK]->(:Node)").unwrap();

    for query in [
        "MATCH (node:Node) RETURN elementId()",
        "MATCH (node:Node) RETURN labels(node, node)",
        "MATCH (node:Node) RETURN elementId(1)",
        "MATCH (node:Node) RETURN labels(missing)",
        "MATCH (node:Node) RETURN type(node)",
        "MATCH ()-[relationship:LINK]->() RETURN labels(relationship)",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "query should fail semantically: {query}"
        );
    }

    assert!(matches!(
        database.execute("MATCH (node:Node) RETURN madeUp(node)"),
        Err(Error::Unsupported(_))
    ));

    for query in [
        "MATCH (node:Node) RETURN madeUp()",
        "MATCH (node:Node) RETURN madeUp(node, node)",
        "MATCH (node:Node) RETURN coalesce(node.name, 'fallback')",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Unsupported(_))),
            "unknown functions should be classified before validating a graph-function signature: {query}"
        );
    }
}
