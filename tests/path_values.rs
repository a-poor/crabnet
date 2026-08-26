use crabnet::{CypherValue, Database, Error, PathValue, QueryResult};

fn path_at(result: &QueryResult, row: usize, column: usize) -> &PathValue {
    result.rows[row][column]
        .as_path()
        .unwrap_or_else(|| panic!("expected a path at row {row}, column {column}"))
}

fn property_string<'a>(value: &'a CypherValue, key: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected string property `{key}`, got {value:?}"))
}

fn node_names(path: &PathValue) -> Vec<&str> {
    path.nodes
        .iter()
        .map(|node| {
            property_string(
                node.properties
                    .get("name")
                    .unwrap_or_else(|| panic!("node has no `name` property: {node:?}")),
                "name",
            )
        })
        .collect()
}

fn relationship_names(path: &PathValue) -> Vec<&str> {
    path.relationships
        .iter()
        .map(|relationship| {
            property_string(
                relationship.properties.get("name").unwrap_or_else(|| {
                    panic!("relationship has no `name` property: {relationship:?}")
                }),
                "name",
            )
        })
        .collect()
}

fn assert_path_shape(path: &PathValue, nodes: &[&str], relationships: &[&str]) {
    assert_eq!(path.nodes.len(), path.relationships.len() + 1);
    assert_eq!(node_names(path), nodes);
    assert_eq!(relationship_names(path), relationships);
}

fn named_linear_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:PathNode {name: 'a'}), (:PathNode {name: 'b'}), \
                    (:PathNode {name: 'c'}), (:PathNode {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:PathNode {name: 'a'}), (b:PathNode {name: 'b'}), \
                   (c:PathNode {name: 'c'}), (d:PathNode {name: 'd'}) \
             CREATE (a)-[:NEXT {name: 'ab'}]->(b), \
                    (b)-[:NEXT {name: 'bc'}]->(c), \
                    (c)-[:NEXT {name: 'cd'}]->(d)",
        )
        .unwrap();
    database
}

#[test]
fn fixed_multi_hop_match_returns_a_public_typed_path() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (a:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->(b:PathNode)-
                           [:NEXT {name: 'bc'}]->(c:PathNode) \
             RETURN route AS journey",
        )
        .unwrap();

    assert_eq!(result.columns, ["journey"]);
    assert_eq!(result.rows.len(), 1);
    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a", "b", "c"], &["ab", "bc"]);
    assert_eq!(path.relationships[0].relationship_type, "NEXT");
    assert_eq!(
        path.relationships[0].start_node_element_id,
        path.nodes[0].element_id
    );
    assert_eq!(
        path.relationships[0].end_node_element_id,
        path.nodes[1].element_id
    );
    assert_eq!(
        path.relationships[1].start_node_element_id,
        path.nodes[1].element_id
    );
    assert_eq!(
        path.relationships[1].end_node_element_id,
        path.nodes[2].element_id
    );
}

#[test]
fn traversal_order_does_not_rewrite_stored_relationship_direction() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH reverse = (c:PathNode {name: 'c'})-
                            [edge:NEXT {name: 'bc'}]-
                            (b:PathNode {name: 'b'}) \
             RETURN reverse, edge",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["c", "b"], &["bc"]);
    let CypherValue::Relationship(projected_edge) = &result.rows[0][1] else {
        panic!("expected the relationship projection")
    };
    assert_eq!(&path.relationships[0], projected_edge);
    assert_eq!(
        path.relationships[0].start_node_element_id,
        path.nodes[1].element_id
    );
    assert_eq!(
        path.relationships[0].end_node_element_id,
        path.nodes[0].element_id
    );
}

#[test]
fn incoming_bounded_path_orders_entities_by_traversal_not_storage() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH reverse = (d:PathNode {name: 'd'})<-[:NEXT*2]-
                            (b:PathNode {name: 'b'}) \
             RETURN reverse",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["d", "c", "b"], &["cd", "bc"]);
    for (index, relationship) in path.relationships.iter().enumerate() {
        assert_eq!(
            relationship.start_node_element_id,
            path.nodes[index + 1].element_id
        );
        assert_eq!(
            relationship.end_node_element_id,
            path.nodes[index].element_id
        );
    }
}

#[test]
fn bounded_zero_hop_path_contains_one_node_and_no_relationships() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH identity = (start:PathNode {name: 'a'})-[:MISSING*0]->
                             (end:PathNode {name: 'a'}) \
             RETURN identity, start, end",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a"], &[]);
    let CypherValue::Node(start) = &result.rows[0][1] else {
        panic!("expected the start node")
    };
    let CypherValue::Node(end) = &result.rows[0][2] else {
        panic!("expected the end node")
    };
    assert_eq!(path.nodes[0], *start);
    assert_eq!(path.nodes[0], *end);
}

#[test]
fn a_named_node_pattern_is_a_zero_length_path_without_a_quantifier() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH singleton = (node:PathNode {name: 'b'}) \
             RETURN singleton, node",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["b"], &[]);
    let CypherValue::Node(node) = &result.rows[0][1] else {
        panic!("expected the projected node")
    };
    assert_eq!(&path.nodes[0], node);
}

#[test]
fn bounded_range_returns_one_ordered_path_per_edge_sequence_and_depth() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (:PathNode {name: 'a'})-[:NEXT*0..3]->(end:PathNode) \
             RETURN route, end.name AS endpoint ORDER BY endpoint",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 4);
    assert_path_shape(path_at(&result, 0, 0), &["a"], &[]);
    assert_path_shape(path_at(&result, 1, 0), &["a", "b"], &["ab"]);
    assert_path_shape(path_at(&result, 2, 0), &["a", "b", "c"], &["ab", "bc"]);
    assert_path_shape(
        path_at(&result, 3, 0),
        &["a", "b", "c", "d"],
        &["ab", "bc", "cd"],
    );
}

#[test]
fn mixed_fixed_and_bounded_segments_build_one_complete_path() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->
                           (:PathNode)-[:NEXT*2]->(end:PathNode {name: 'd'}) \
             RETURN route",
        )
        .unwrap();

    assert_path_shape(
        path_at(&result, 0, 0),
        &["a", "b", "c", "d"],
        &["ab", "bc", "cd"],
    );
}

#[test]
fn zero_hop_segment_inside_a_fixed_path_does_not_duplicate_its_boundary_node() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->(middle:PathNode)-
                           [:MISSING*0]->(end:PathNode {name: 'b'}) \
             RETURN route, middle, end",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a", "b"], &["ab"]);
    let [
        CypherValue::Path(_),
        CypherValue::Node(middle),
        CypherValue::Node(end),
    ] = result.rows[0].as_slice()
    else {
        panic!("expected a path and its two boundary-node projections")
    };
    assert_eq!(middle, end);
    assert_eq!(&path.nodes[1], middle);
}

#[test]
fn multiple_named_paths_keep_independent_ordered_entity_sequences() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH first = (:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->(middle:PathNode), \
                   second = (middle)-[:NEXT*1..2]->(end:PathNode) \
             WHERE end.name = 'd' \
             RETURN first, second",
        )
        .unwrap();

    assert_eq!(result.columns, ["first", "second"]);
    assert_eq!(result.rows.len(), 1);
    assert_path_shape(path_at(&result, 0, 0), &["a", "b"], &["ab"]);
    assert_path_shape(path_at(&result, 0, 1), &["b", "c", "d"], &["bc", "cd"]);
}

#[test]
fn diamond_paths_retain_their_distinct_ordered_intermediate_nodes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:DiamondPath {name: 'a'}), (:DiamondPath {name: 'b'}), \
                    (:DiamondPath {name: 'c'}), (:DiamondPath {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:DiamondPath {name: 'a'}), (b:DiamondPath {name: 'b'}), \
                   (c:DiamondPath {name: 'c'}), (d:DiamondPath {name: 'd'}) \
             CREATE (a)-[:LINK {name: 'ab'}]->(b), \
                    (a)-[:LINK {name: 'ac'}]->(c), \
                    (b)-[:LINK {name: 'bd'}]->(d), \
                    (c)-[:LINK {name: 'cd'}]->(d)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:DiamondPath {name: 'a'})-[:LINK*2]->
                         (:DiamondPath {name: 'd'}) \
             RETURN path",
        )
        .unwrap();

    let mut paths = result
        .rows
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let path = path_at(&result, index, 0);
            (node_names(path), relationship_names(path))
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths,
        [
            (vec!["a", "b", "d"], vec!["ab", "bd"]),
            (vec!["a", "c", "d"], vec!["ac", "cd"]),
        ]
    );
}

#[test]
fn parallel_relationship_ids_make_otherwise_identical_paths_distinct() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:ParallelPath {name: 'a'}), (:ParallelPath {name: 'b'})")
        .unwrap();
    database
        .execute(
            "MATCH (a:ParallelPath {name: 'a'}), (b:ParallelPath {name: 'b'}) \
             CREATE (a)-[:LINK {name: 'same'}]->(b), \
                    (a)-[:LINK {name: 'same'}]->(b)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:ParallelPath {name: 'a'})-[:LINK]->
                         (:ParallelPath {name: 'b'}) \
             RETURN DISTINCT path",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    let first = path_at(&result, 0, 0);
    let second = path_at(&result, 1, 0);
    assert_path_shape(first, &["a", "b"], &["same"]);
    assert_path_shape(second, &["a", "b"], &["same"]);
    assert_ne!(
        first.relationships[0].element_id,
        second.relationships[0].element_id
    );
    assert_ne!(first, second);
}

#[test]
fn a_cycle_path_may_repeat_a_node_but_not_a_relationship() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:CyclePath {name: 'a'}), (:CyclePath {name: 'b'}), \
                    (:CyclePath {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:CyclePath {name: 'a'}), (b:CyclePath {name: 'b'}), \
                   (c:CyclePath {name: 'c'}) \
             CREATE (a)-[:CYCLE {name: 'ab'}]->(b), \
                    (b)-[:CYCLE {name: 'bc'}]->(c), \
                    (c)-[:CYCLE {name: 'ca'}]->(a)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:CyclePath {name: 'a'})-[:CYCLE*3]->
                         (:CyclePath {name: 'a'}) \
             RETURN path",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a", "b", "c", "a"], &["ab", "bc", "ca"]);
    assert_eq!(path.nodes[0].element_id, path.nodes[3].element_id);
    let mut relationship_ids = path
        .relationships
        .iter()
        .map(|relationship| relationship.element_id.as_str())
        .collect::<Vec<_>>();
    relationship_ids.sort_unstable();
    relationship_ids.dedup();
    assert_eq!(relationship_ids.len(), 3);
}

#[test]
fn path_values_survive_with_aliases_without_losing_entity_types() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*1..2]->(end:PathNode) \
             WITH path AS journey, end \
             WHERE end.name = 'c' \
             RETURN journey AS path",
        )
        .unwrap();

    assert_eq!(result.columns, ["path"]);
    assert_path_shape(path_at(&result, 0, 0), &["a", "b", "c"], &["ab", "bc"]);
}

#[test]
fn union_distinct_uses_recursive_path_identity_while_union_all_keeps_duplicates() {
    let mut database = named_linear_graph();
    let branch = "MATCH path = (:PathNode {name: 'a'})-
                               [:NEXT {name: 'ab'}]->(:PathNode {name: 'b'}) \
                  RETURN path";

    let distinct = database
        .execute(&format!("{branch} UNION {branch}"))
        .unwrap();
    assert_eq!(distinct.rows.len(), 1);
    assert_path_shape(path_at(&distinct, 0, 0), &["a", "b"], &["ab"]);

    let all = database
        .execute(&format!("{branch} UNION ALL {branch}"))
        .unwrap();
    assert_eq!(all.rows.len(), 2);
    assert_eq!(all.rows[0], all.rows[1]);
}

#[test]
fn path_operations_outside_projection_are_structured_rejections() {
    let mut database = named_linear_graph();

    for query in [
        "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN length(path)",
        "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN nodes(path)",
        "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN relationships(path)",
        "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN path.missing",
    ] {
        assert!(
            matches!(
                database.execute(query),
                Err(Error::Unsupported(_) | Error::Semantic(_))
            ),
            "unsupported path operation must fail as a structured query error: {query}"
        );
    }
}

#[test]
fn optional_and_write_fed_path_bindings_remain_explicitly_unsupported() {
    let mut database = named_linear_graph();

    assert!(matches!(
        database.execute("OPTIONAL MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN path"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("MATCH path = (a:PathNode)-[edges:NEXT*1..1]->(b:PathNode) RETURN path"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute(
            "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) \
             CREATE (:PathWriteSideEffect) RETURN path"
        ),
        Err(Error::Unsupported(_))
    ));

    let side_effects = database
        .execute("MATCH (node:PathWriteSideEffect) RETURN node")
        .unwrap();
    assert!(side_effects.rows.is_empty());
}
