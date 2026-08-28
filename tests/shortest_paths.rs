use crabnet::{
    CypherValue, Database, Error, Parameters, PathValue, QueryResult, RelationshipValue, explain,
    explain_physical,
};
use serde_json::json;

fn integer(value: &CypherValue) -> i64 {
    let CypherValue::Integer(value) = value else {
        panic!("expected an integer, got {value:?}");
    };
    *value
}

fn string(value: &CypherValue) -> &str {
    let CypherValue::String(value) = value else {
        panic!("expected a string, got {value:?}");
    };
    value
}

fn path(value: &CypherValue) -> &PathValue {
    let CypherValue::Path(value) = value else {
        panic!("expected a path, got {value:?}");
    };
    value
}

fn property_string<'a>(
    properties: &'a std::collections::BTreeMap<String, CypherValue>,
    key: &str,
) -> &'a str {
    string(
        properties
            .get(key)
            .unwrap_or_else(|| panic!("missing property {key}")),
    )
}

fn node_names(path: &PathValue) -> Vec<&str> {
    path.nodes
        .iter()
        .map(|node| property_string(&node.properties, "name"))
        .collect()
}

fn relationship_names(path: &PathValue) -> Vec<&str> {
    path.relationships
        .iter()
        .map(|relationship| property_string(&relationship.properties, "name"))
        .collect()
}

fn relationship_list(value: &CypherValue) -> Vec<&RelationshipValue> {
    let CypherValue::List(values) = value else {
        panic!("expected a relationship list, got {value:?}");
    };
    values
        .iter()
        .map(|value| {
            let CypherValue::Relationship(relationship) = value else {
                panic!("expected a relationship-list member, got {value:?}");
            };
            relationship
        })
        .collect()
}

fn node_list_names(value: &CypherValue) -> Vec<&str> {
    let CypherValue::List(values) = value else {
        panic!("expected a node list, got {value:?}");
    };
    values
        .iter()
        .map(|value| {
            let CypherValue::Node(node) = value else {
                panic!("expected a node-list member, got {value:?}");
            };
            property_string(&node.properties, "name")
        })
        .collect()
}

fn relationship_list_names(value: &CypherValue) -> Vec<&str> {
    relationship_list(value)
        .iter()
        .map(|relationship| property_string(&relationship.properties, "name"))
        .collect()
}

fn assert_path_shape(path: &PathValue, nodes: &[&str], relationships: &[&str]) {
    assert_eq!(node_names(path), nodes);
    assert_eq!(relationship_names(path), relationships);
    assert_eq!(path.nodes.len(), path.relationships.len() + 1);
}

fn assert_unsupported(database: &mut Database, query: &str) {
    assert!(
        matches!(database.execute(query), Err(Error::Unsupported(_))),
        "expected a structured unsupported error for: {query}"
    );
}

fn assert_stage_precedes(explanation: &str, earlier: &str, later: &str) {
    let earlier_position = explanation
        .find(earlier)
        .unwrap_or_else(|| panic!("missing stage {earlier:?} in:\n{explanation}"));
    let later_position = explanation
        .find(later)
        .unwrap_or_else(|| panic!("missing stage {later:?} in:\n{explanation}"));
    assert!(
        earlier_position < later_position,
        "expected {earlier:?} before {later:?} in:\n{explanation}"
    );
}

fn direct_and_longer_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ShortestDirect {name: 'a'}), (:ShortestDirect {name: 'b'}), \
                    (:ShortestDirect {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestDirect {name: 'a'}), (b:ShortestDirect {name: 'b'}), \
                   (d:ShortestDirect {name: 'd'}) \
             CREATE (a)-[:LINK {name: 'ab'}]->(b), \
                    (b)-[:LINK {name: 'bd'}]->(d), \
                    (a)-[:LINK {name: 'ad'}]->(d)",
        )
        .unwrap();
    database
}

fn diamond_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ShortestDiamond {name: 'a'}), (:ShortestDiamond {name: 'b'}), \
                    (:ShortestDiamond {name: 'c'}), (:ShortestDiamond {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestDiamond {name: 'a'}), (b:ShortestDiamond {name: 'b'}), \
                   (c:ShortestDiamond {name: 'c'}), (d:ShortestDiamond {name: 'd'}) \
             CREATE (a)-[:LINK {name: 'ab'}]->(b), \
                    (a)-[:LINK {name: 'ac'}]->(c), \
                    (b)-[:LINK {name: 'bd'}]->(d), \
                    (c)-[:LINK {name: 'cd'}]->(d)",
        )
        .unwrap();
    database
}

fn named_linear_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ShortestLinear {name: 'a'}), (:ShortestLinear {name: 'b'}), \
                    (:ShortestLinear {name: 'c'}), (:ShortestLinear {name: 'd'}), \
                    (:ShortestLinear {name: 'dead'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestLinear {name: 'a'}), (b:ShortestLinear {name: 'b'}), \
                   (c:ShortestLinear {name: 'c'}), (d:ShortestLinear {name: 'd'}) \
             CREATE (a)-[:NEXT {name: 'ab'}]->(b), \
                    (b)-[:NEXT {name: 'bc'}]->(c), \
                    (c)-[:NEXT {name: 'cd'}]->(d)",
        )
        .unwrap();
    database
}

#[test]
fn shortest_path_prefers_a_direct_route_over_a_longer_route() {
    let mut database = direct_and_longer_graph();

    let result = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestDirect {name: 'a'})-[:LINK*]->(:ShortestDirect {name: 'd'}) \
             ) RETURN route",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_path_shape(path(&result.rows[0][0]), &["a", "d"], &["ad"]);
}

#[test]
fn shortest_returns_one_deterministic_tie_and_all_shortest_returns_every_tie() {
    let mut database = diamond_graph();
    let shortest_query = "MATCH route = shortestPath( \
             (:ShortestDiamond {name: 'a'})-[:LINK*]->(:ShortestDiamond {name: 'd'}) \
         ) RETURN route";

    let first = database.execute(shortest_query).unwrap();
    let second = database.execute(shortest_query).unwrap();
    assert_eq!(first, second, "tie-breaking must be deterministic");
    assert_eq!(first.rows.len(), 1);
    let chosen_nodes = node_names(path(&first.rows[0][0]));
    assert!(
        chosen_nodes == ["a", "b", "d"] || chosen_nodes == ["a", "c", "d"],
        "unexpected tied route: {chosen_nodes:?}"
    );

    let all = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (:ShortestDiamond {name: 'a'})-[:LINK*]->(:ShortestDiamond {name: 'd'}) \
             ) RETURN route",
        )
        .unwrap();
    let mut routes = all
        .rows
        .iter()
        .map(|row| node_names(path(&row[0])).join("-"))
        .collect::<Vec<_>>();
    routes.sort();
    assert_eq!(routes, ["a-b-d", "a-c-d"]);
}

#[test]
fn parallel_edges_are_distinct_tied_shortest_paths() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:ShortestParallel {name: 'a'}), (:ShortestParallel {name: 'b'})")
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestParallel {name: 'a'}), (b:ShortestParallel {name: 'b'}) \
             CREATE (a)-[:LINK {name: 'first'}]->(b), \
                    (a)-[:LINK {name: 'second'}]->(b)",
        )
        .unwrap();

    let all = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (:ShortestParallel {name: 'a'})-[:LINK*]->(:ShortestParallel {name: 'b'}) \
             ) RETURN route",
        )
        .unwrap();
    let mut names = all
        .rows
        .iter()
        .map(|row| relationship_names(path(&row[0]))[0].to_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["first", "second"]);

    let one = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestParallel {name: 'a'})-[:LINK*]->(:ShortestParallel {name: 'b'}) \
             ) RETURN route",
        )
        .unwrap();
    assert_eq!(one.rows.len(), 1);
    assert!(matches!(
        relationship_names(path(&one.rows[0][0])).as_slice(),
        ["first"] | ["second"]
    ));
}

#[test]
fn shortest_selection_is_applied_per_endpoint_pair() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ShortestPairStart {name: 'a'}), (:ShortestPairMiddle {name: 'b'}), \
                    (:ShortestPairMiddle {name: 'c'}), (:ShortestPairEnd {name: 'x'}), \
                    (:ShortestPairEnd {name: 'y'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestPairStart), (b:ShortestPairMiddle {name: 'b'}), \
                   (c:ShortestPairMiddle {name: 'c'}), (x:ShortestPairEnd {name: 'x'}), \
                   (y:ShortestPairEnd {name: 'y'}) \
             CREATE (a)-[:LINK]->(x), (a)-[:LINK]->(b), (a)-[:LINK]->(c), \
                    (b)-[:LINK]->(y), (c)-[:LINK]->(y)",
        )
        .unwrap();

    let one = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestPairStart)-[:LINK*]->(end:ShortestPairEnd) \
             ) RETURN end.name AS name, length(route) AS hops ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        one.rows,
        [
            vec![CypherValue::String("x".into()), CypherValue::Integer(1)],
            vec![CypherValue::String("y".into()), CypherValue::Integer(2)],
        ]
    );

    let all = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (:ShortestPairStart)-[:LINK*]->(end:ShortestPairEnd) \
             ) RETURN end.name AS name, length(route) AS hops ORDER BY name",
        )
        .unwrap();
    assert_eq!(all.rows.len(), 3);
    assert_eq!(string(&all.rows[0][0]), "x");
    assert_eq!(integer(&all.rows[0][1]), 1);
    assert!(
        all.rows[1..]
            .iter()
            .all(|row| { string(&row[0]) == "y" && integer(&row[1]) == 2 })
    );
}

#[test]
fn prebound_endpoints_and_duplicate_input_rows_feed_shortest_expansion() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH (start:ShortestLinear {name: 'a'}), \
                   (end:ShortestLinear {name: 'd'}) \
             WITH start, end \
             UNWIND [1, 1] AS copy \
             MATCH route = shortestPath((start)-[:NEXT*]->(end)) \
             RETURN copy, route, length(route) AS hops",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    for row in &result.rows {
        assert_eq!(integer(&row[0]), 1);
        assert_eq!(integer(&row[2]), 3);
        assert_path_shape(path(&row[1]), &["a", "b", "c", "d"], &["ab", "bc", "cd"]);
    }
}

#[test]
fn all_shortest_extends_every_tied_prefix_through_a_shared_suffix() {
    let mut database = diamond_graph();
    database
        .execute("CREATE (:ShortestSuffix {name: 'e'})")
        .unwrap();
    database
        .execute(
            "MATCH (d:ShortestDiamond {name: 'd'}), (e:ShortestSuffix {name: 'e'}) \
             CREATE (d)-[:LINK {name: 'de'}]->(e)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (:ShortestDiamond {name: 'a'})-[:LINK*]->(:ShortestSuffix {name: 'e'}) \
             ) RETURN route",
        )
        .unwrap();

    let mut routes = result
        .rows
        .iter()
        .map(|row| node_names(path(&row[0])).join("-"))
        .collect::<Vec<_>>();
    routes.sort();
    assert_eq!(routes, ["a-b-d-e", "a-c-d-e"]);
}

#[test]
fn incoming_and_undirected_shortest_paths_follow_the_requested_direction() {
    let mut database = named_linear_graph();

    let incoming = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestLinear {name: 'd'})<-[:NEXT*]-(:ShortestLinear {name: 'a'}) \
             ) RETURN route",
        )
        .unwrap();
    assert_path_shape(
        path(&incoming.rows[0][0]),
        &["d", "c", "b", "a"],
        &["cd", "bc", "ab"],
    );

    let undirected = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestLinear {name: 'd'})-[:NEXT*]-(:ShortestLinear {name: 'a'}) \
             ) RETURN route",
        )
        .unwrap();
    assert_path_shape(
        path(&undirected.rows[0][0]),
        &["d", "c", "b", "a"],
        &["cd", "bc", "ab"],
    );
}

#[test]
fn edge_type_property_and_parameters_filter_every_shortest_candidate() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ShortestFilter {name: 'a'}), (:ShortestFilter {name: 'b'}), \
                    (:ShortestFilter {name: 'c'}), (:ShortestFilter {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestFilter {name: 'a'}), (b:ShortestFilter {name: 'b'}), \
                   (c:ShortestFilter {name: 'c'}), (d:ShortestFilter {name: 'd'}) \
             CREATE (a)-[:ROAD {name: 'ab', enabled: true}]->(b), \
                    (b)-[:ROAD {name: 'bc', enabled: true}]->(c), \
                    (a)-[:ROAD {name: 'ac', enabled: false}]->(c), \
                    (a)-[:OTHER {name: 'ad', enabled: true}]->(d)",
        )
        .unwrap();
    let parameters = Parameters::from([
        ("start".to_owned(), json!("a")),
        ("end".to_owned(), json!("c")),
        ("enabled".to_owned(), json!(true)),
    ]);

    let result = database
        .execute_with(
            "MATCH route = shortestPath( \
                 (:ShortestFilter {name: $start})- \
                 [:ROAD* {enabled: $enabled}]-> \
                 (:ShortestFilter {name: $end}) \
             ) RETURN route",
            &parameters,
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_path_shape(path(&result.rows[0][0]), &["a", "b", "c"], &["ab", "bc"]);
}

#[test]
fn maximum_bound_is_enforced_before_selecting_a_shortest_path() {
    let mut database = named_linear_graph();

    let too_shallow = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestLinear {name: 'a'})-[:NEXT*..2]->(:ShortestLinear {name: 'd'}) \
             ) RETURN route",
        )
        .unwrap();
    assert!(too_shallow.rows.is_empty());

    let reachable = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestLinear {name: 'a'})-[:NEXT*..3]->(:ShortestLinear {name: 'd'}) \
             ) RETURN route",
        )
        .unwrap();
    assert_eq!(reachable.rows.len(), 1);
    assert_eq!(path(&reachable.rows[0][0]).relationships.len(), 3);
}

#[test]
fn unreachable_endpoint_pairs_produce_no_rows() {
    let mut database = named_linear_graph();

    for function in ["shortestPath", "allShortestPaths"] {
        let result = database
            .execute(&format!(
                "MATCH route = {function}( \
                     (:ShortestLinear {{name: 'a'}})-[:NEXT*]-> \
                     (:ShortestLinear {{name: 'dead'}}) \
                 ) RETURN route"
            ))
            .unwrap();
        assert!(
            result.rows.is_empty(),
            "{function} invented an unreachable path"
        );
    }
}

#[test]
fn cycles_do_not_displace_a_shorter_relationship_unique_route() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ShortestCycle {name: 'a'}), (:ShortestCycle {name: 'b'}), \
                    (:ShortestCycle {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestCycle {name: 'a'}), (b:ShortestCycle {name: 'b'}), \
                   (c:ShortestCycle {name: 'c'}) \
             CREATE (a)-[:LINK {name: 'ab'}]->(b), \
                    (b)-[:LINK {name: 'ba'}]->(a), \
                    (b)-[:LINK {name: 'bc'}]->(c)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (:ShortestCycle {name: 'a'})-[:LINK*]->(:ShortestCycle {name: 'c'}) \
             ) RETURN route",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_path_shape(path(&result.rows[0][0]), &["a", "b", "c"], &["ab", "bc"]);
}

#[test]
fn lower_bound_one_finds_shortest_positive_cycles_to_same_endpoint() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:ShortestLoop {name: 'a'})")
        .unwrap();
    database
        .execute(
            "MATCH (node:ShortestLoop {name: 'a'}) \
             CREATE (node)-[:LOOP {name: 'first'}]->(node), \
                    (node)-[:LOOP {name: 'second'}]->(node)",
        )
        .unwrap();

    let all = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (node:ShortestLoop {name: 'a'})-[:LOOP*]->(node) \
             ) RETURN route",
        )
        .unwrap();
    let mut names = all
        .rows
        .iter()
        .map(|row| relationship_names(path(&row[0]))[0].to_owned())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["first", "second"]);

    let one = database
        .execute(
            "MATCH route = shortestPath( \
                 (node:ShortestLoop {name: 'a'})-[:LOOP*]->(node) \
             ) RETURN route",
        )
        .unwrap();
    assert_eq!(one.rows.len(), 1);
    assert_eq!(path(&one.rows[0][0]).relationships.len(), 1);
}

#[test]
fn undirected_triangle_cycles_are_selected_per_relationship_sequence() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ShortestTriangle {name: 'a'}), (:ShortestTriangle {name: 'b'}), \
                    (:ShortestTriangle {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ShortestTriangle {name: 'a'}), \
                   (b:ShortestTriangle {name: 'b'}), \
                   (c:ShortestTriangle {name: 'c'}) \
             CREATE (a)-[:LINK {name: 'ab'}]->(b), \
                    (b)-[:LINK {name: 'bc'}]->(c), \
                    (c)-[:LINK {name: 'ca'}]->(a)",
        )
        .unwrap();

    let too_shallow = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (node:ShortestTriangle {name: 'a'})-[:LINK*..2]-(node) \
             ) RETURN route",
        )
        .unwrap();
    assert!(too_shallow.rows.is_empty());

    let all = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (node:ShortestTriangle {name: 'a'})-[:LINK*..3]-(node) \
             ) RETURN route",
        )
        .unwrap();
    let mut routes = all
        .rows
        .iter()
        .map(|row| node_names(path(&row[0])).join("-"))
        .collect::<Vec<_>>();
    routes.sort();
    assert_eq!(routes, ["a-b-c-a", "a-c-b-a"]);

    let one = database
        .execute(
            "MATCH route = shortestPath( \
                 (node:ShortestTriangle {name: 'a'})-[:LINK*]-(node) \
             ) RETURN route",
        )
        .unwrap();
    assert_eq!(path(&one.rows[0][0]).relationships.len(), 3);
    assert_eq!(one.rows.len(), 1);

    let directed = database
        .execute(
            "MATCH route = allShortestPaths( \
                 (node:ShortestTriangle {name: 'a'})-[:LINK*]->(node) \
             ) RETURN route",
        )
        .unwrap();
    assert_eq!(directed.rows.len(), 1);
    assert_path_shape(
        path(&directed.rows[0][0]),
        &["a", "b", "c", "a"],
        &["ab", "bc", "ca"],
    );
}

#[test]
fn explicit_zero_lower_bound_selects_the_identity_path() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:ShortestIdentity {name: 'a'})")
        .unwrap();
    database
        .execute(
            "MATCH (node:ShortestIdentity {name: 'a'}) \
             CREATE (node)-[:LOOP]->(node)",
        )
        .unwrap();

    for function in ["shortestPath", "allShortestPaths"] {
        let result = database
            .execute(&format!(
                "MATCH route = {function}( \
                     (node:ShortestIdentity {{name: 'a'}})-[:LOOP*0..4]->(node) \
                 ) RETURN route, length(route) AS hops"
            ))
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(integer(&result.rows[0][1]), 0);
        assert_path_shape(path(&result.rows[0][0]), &["a"], &[]);
    }
}

#[test]
fn named_path_relationship_list_and_path_functions_share_one_identity() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestLinear {name: 'a'})-[links:NEXT*]->(:ShortestLinear {name: 'd'}) \
             ) RETURN route, links, length(route) AS hops, \
                      nodes(route) AS path_nodes, relationships(route) AS path_edges",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    let route = path(&row[0]);
    assert_path_shape(route, &["a", "b", "c", "d"], &["ab", "bc", "cd"]);
    assert_eq!(integer(&row[2]), 3);
    assert_eq!(node_list_names(&row[3]), ["a", "b", "c", "d"]);
    assert_eq!(relationship_list_names(&row[1]), ["ab", "bc", "cd"]);
    assert_eq!(relationship_list_names(&row[4]), ["ab", "bc", "cd"]);
    let links = relationship_list(&row[1]);
    assert!(
        links
            .iter()
            .zip(&route.relationships)
            .all(|(left, right)| left.element_id == right.element_id)
    );
}

#[test]
fn shortest_path_values_survive_a_with_boundary() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = shortestPath( \
                 (:ShortestLinear {name: 'a'})-[:NEXT*]->(:ShortestLinear {name: 'c'}) \
             ) WITH path AS route, length(path) AS hops \
               WHERE hops = 2 \
             RETURN route, hops",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_eq!(integer(&result.rows[0][1]), 2);
    assert_path_shape(path(&result.rows[0][0]), &["a", "b", "c"], &["ab", "bc"]);
}

#[test]
fn shortest_path_runtime_relations_compose_across_union_branches() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = shortestPath( \
                 (:ShortestLinear {name: 'a'})-[:NEXT*]->(:ShortestLinear {name: 'b'}) \
             ) RETURN route, length(route) AS hops \
             UNION ALL \
             MATCH route = allShortestPaths( \
                 (:ShortestLinear {name: 'a'})-[:NEXT*]->(:ShortestLinear {name: 'd'}) \
             ) RETURN route, length(route) AS hops",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    assert_eq!(integer(&result.rows[0][1]), 1);
    assert_eq!(integer(&result.rows[1][1]), 3);
    assert_path_shape(path(&result.rows[0][0]), &["a", "b"], &["ab"]);
    assert_path_shape(
        path(&result.rows[1][0]),
        &["a", "b", "c", "d"],
        &["ab", "bc", "cd"],
    );
}

#[test]
fn prior_input_bag_rows_are_preserved_by_shortest_expansion() {
    let mut database = diamond_graph();

    let result = database
        .execute(
            "UNWIND [1, 1] AS copy \
             MATCH route = allShortestPaths( \
                 (:ShortestDiamond {name: 'a'})-[:LINK*]->(:ShortestDiamond {name: 'd'}) \
             ) RETURN copy, route",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 4);
    assert!(result.rows.iter().all(|row| integer(&row[0]) == 1));
    let mut routes = result
        .rows
        .iter()
        .map(|row| node_names(path(&row[1])).join("-"))
        .collect::<Vec<_>>();
    routes.sort();
    assert_eq!(routes, ["a-b-d", "a-b-d", "a-c-d", "a-c-d"]);
}

#[test]
fn logical_and_physical_explain_show_the_rust_bfs_boundary() {
    let one = "MATCH route = shortestPath((a)-[:LINK*]->(b)) RETURN route";
    let all = "MATCH route = allShortestPaths((a)-[:LINK*..4]->(b)) RETURN route";

    let one_logical = explain(one).unwrap();
    assert!(one_logical.contains("expand-shortest executor=rust"));
    assert!(one_logical.contains("mode=one"));
    assert!(one_logical.contains("bounds=1..unbounded"));

    let all_logical = explain(all).unwrap();
    assert!(all_logical.contains("expand-shortest executor=rust"));
    assert!(all_logical.contains("mode=all"));
    assert!(all_logical.contains("bounds=1..4"));

    let physical = explain_physical(one).unwrap();
    assert!(physical.contains("shortest-expand mode=one"));
    assert_eq!(physical.matches("batch-hydrate-paths").count(), 1);
    assert_stage_precedes(&physical, "shortest-expand", "sqlite-fragment");
    assert_stage_precedes(&physical, "sqlite-fragment", "batch-hydrate-paths");
}

#[test]
fn modern_selectors_and_expression_position_shortest_calls_remain_rejected() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "MATCH route = ANY SHORTEST (a)-[:LINK*]->(b) RETURN route",
        "MATCH route = ALL SHORTEST (a)-[:LINK*]->(b) RETURN route",
        "MATCH (a), (b) RETURN shortestPath((a)-[:LINK*]->(b)) AS route",
        "MATCH (a), (b) RETURN allShortestPaths((a)-[:LINK*]->(b)) AS route",
    ] {
        assert_unsupported(&mut database, query);
    }
}

#[test]
fn unsupported_legacy_shortest_pattern_shapes_fail_during_planning() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "MATCH route = shortestPath((a)-[:LINK*]->(b)), (marker) RETURN route",
        "MATCH route = shortestPath((a)-[:LINK*2..]->(b)) RETURN route",
        "MATCH route = allShortestPaths((a)-[:LINK*2..4]->(b)) RETURN route",
        "OPTIONAL MATCH route = shortestPath((a)-[:LINK*]->(b)) RETURN route",
        "MATCH route = shortestPath((a)-[:LINK*]->(b)) \
         WHERE length(route) > 1 RETURN route",
    ] {
        assert_unsupported(&mut database, query);
    }
}

#[test]
fn writes_fed_by_shortest_path_are_rejected_without_side_effects() {
    let mut database = named_linear_graph();

    assert_unsupported(
        &mut database,
        "MATCH route = shortestPath( \
             (:ShortestLinear {name: 'a'})-[:NEXT*]->(:ShortestLinear {name: 'd'}) \
         ) CREATE (:ShortestWriteSideEffect)",
    );

    let remaining: QueryResult = database
        .execute("MATCH (node:ShortestWriteSideEffect) RETURN node")
        .unwrap();
    assert!(remaining.rows.is_empty());
}
