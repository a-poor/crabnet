use crabnet::{
    CypherValue, Database, Error, Parameters, PathValue, RelationshipValue, explain,
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

fn relationship_names<'a>(relationships: &[&'a RelationshipValue]) -> Vec<&'a str> {
    relationships
        .iter()
        .map(|relationship| property_string(&relationship.properties, "name"))
        .collect()
}

fn assert_list_matches_path(path: &PathValue, value: &CypherValue) {
    let relationships = relationship_list(value);
    assert_eq!(relationships.len(), path.relationships.len());
    for (from_list, from_path) in relationships.iter().zip(&path.relationships) {
        assert_eq!(from_list.element_id, from_path.element_id);
    }
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

fn linear_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:UnboundedLinear {name: 'a', depth: 0}), \
                    (:UnboundedLinear {name: 'b', depth: 1}), \
                    (:UnboundedLinear {name: 'c', depth: 2}), \
                    (:UnboundedLinear {name: 'd', depth: 3})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:UnboundedLinear {name: 'a'}), \
                   (b:UnboundedLinear {name: 'b'}), \
                   (c:UnboundedLinear {name: 'c'}), \
                   (d:UnboundedLinear {name: 'd'}) \
             CREATE (a)-[:NEXT {name: 'ab'}]->(b), \
                    (b)-[:NEXT {name: 'bc'}]->(c), \
                    (c)-[:NEXT {name: 'cd'}]->(d)",
        )
        .unwrap();
    database
}

fn deep_graph(maximum: usize) -> Database {
    let mut database = Database::in_memory().unwrap();
    let mut create = String::from("CREATE ");
    for depth in 0..=maximum {
        if depth > 0 {
            create.push_str("-[:NEXT]->");
        }
        create.push_str(&format!("(:DeepHybridNode {{depth: {depth}}})"));
    }
    database.execute(&create).unwrap();
    database
}

fn diamond_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:HybridDiamond {name: 'a'}), (:HybridDiamond {name: 'b'}), \
                    (:HybridDiamond {name: 'c'}), (:HybridDiamond {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:HybridDiamond {name: 'a'}), (b:HybridDiamond {name: 'b'}), \
                   (c:HybridDiamond {name: 'c'}), (d:HybridDiamond {name: 'd'}) \
             CREATE (a)-[:NEXT]->(b), (a)-[:NEXT]->(c), \
                    (b)-[:NEXT]->(d), (c)-[:NEXT]->(d)",
        )
        .unwrap();
    database
}

#[test]
fn deep_range_crosses_the_sql_threshold_without_losing_paths_or_lists() {
    let mut database = deep_graph(40);

    let result = database
        .execute(
            "MATCH route = (:DeepHybridNode {depth: 0})-[rels:NEXT*31..34]->
                           (end:DeepHybridNode) \
             RETURN end.depth AS depth, length(route) AS hops, route, rels \
             ORDER BY depth",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 4);
    for (index, row) in result.rows.iter().enumerate() {
        let expected_depth = 31 + index as i64;
        assert_eq!(integer(&row[0]), expected_depth);
        assert_eq!(integer(&row[1]), expected_depth);

        let route = path(&row[2]);
        assert_eq!(route.nodes.len(), expected_depth as usize + 1);
        assert_eq!(route.relationships.len(), expected_depth as usize);
        assert_list_matches_path(route, &row[3]);
    }
}

#[test]
fn legacy_unbounded_lower_bounds_are_exact() {
    let mut database = linear_graph();

    for (quantifier, expected) in [
        ("*", vec![1, 2, 3]),
        ("*..", vec![1, 2, 3]),
        ("*2..", vec![2, 3]),
        ("*0..", vec![0, 1, 2, 3]),
    ] {
        let query = format!(
            "MATCH (:UnboundedLinear {{depth: 0}})-[:NEXT{quantifier}]->
                   (end:UnboundedLinear) \
             RETURN end.depth AS depth ORDER BY depth"
        );
        let result = database.execute(&query).unwrap();
        let actual = result
            .rows
            .iter()
            .map(|row| integer(&row[0]))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "wrong result for {quantifier}");
    }
}

#[test]
fn unbounded_cycle_revisits_nodes_but_never_reuses_an_edge() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:HybridCycle {name: 'a'}), (:HybridCycle {name: 'b'}), \
                    (:HybridCycle {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:HybridCycle {name: 'a'}), (b:HybridCycle {name: 'b'}), \
                   (c:HybridCycle {name: 'c'}) \
             CREATE (a)-[:CYCLE]->(b), (b)-[:CYCLE]->(c), (c)-[:CYCLE]->(a)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH route = (:HybridCycle {name: 'a'})-[:CYCLE*]->(end) \
             RETURN end.name AS name, length(route) AS hops ORDER BY hops",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("b".into()), CypherValue::Integer(1)],
            vec![CypherValue::String("c".into()), CypherValue::Integer(2)],
            vec![CypherValue::String("a".into()), CypherValue::Integer(3)],
        ]
    );
}

#[test]
fn undirected_unbounded_self_loop_is_used_once() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (node:HybridLoop {name: 'loop'})-
                    [:LOOP {name: 'self'}]->(node)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH route = (:HybridLoop)-[:LOOP*0..]-(:HybridLoop) \
             RETURN length(route) AS hops ORDER BY hops",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(0)], vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn unbounded_diamond_preserves_one_row_per_relationship_sequence() {
    let mut database = diamond_graph();

    let result = database
        .execute(
            "MATCH (:HybridDiamond {name: 'a'})-[:NEXT*2..]->
                   (end:HybridDiamond {name: 'd'}) \
             RETURN end.name AS name",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("d".into())],
            vec![CypherValue::String("d".into())],
        ]
    );
}

#[test]
fn parallel_edges_remain_distinct_path_and_relationship_list_identities() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:HybridParallel {name: 'a'}), (:HybridParallel {name: 'b'}), \
                    (:HybridParallel {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:HybridParallel {name: 'a'}), (b:HybridParallel {name: 'b'}), \
                   (c:HybridParallel {name: 'c'}) \
             CREATE (a)-[:NEXT {name: 'ab1'}]->(b), \
                    (a)-[:NEXT {name: 'ab2'}]->(b), \
                    (b)-[:NEXT {name: 'bc'}]->(c)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH route = (:HybridParallel {name: 'a'})-[rels:NEXT*2..]->
                           (:HybridParallel {name: 'c'}) \
             RETURN route, rels",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    let mut sequences = Vec::new();
    for row in &result.rows {
        let route = path(&row[0]);
        assert_eq!(node_names(route), ["a", "b", "c"]);
        assert_list_matches_path(route, &row[1]);
        sequences.push(relationship_names(&relationship_list(&row[1])).join(","));
    }
    sequences.sort();
    assert_eq!(sequences, ["ab1,bc", "ab2,bc"]);
}

#[test]
fn incoming_unbounded_values_follow_traversal_order_and_keep_storage_direction() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH route = (:UnboundedLinear {name: 'd'})<-[rels:NEXT*]-(start) \
             RETURN start.name AS name, length(route) AS hops, route, rels \
             ORDER BY hops",
        )
        .unwrap();

    let expected_names = ["c", "b", "a"];
    let expected_paths = [
        vec!["d", "c"],
        vec!["d", "c", "b"],
        vec!["d", "c", "b", "a"],
    ];
    assert_eq!(result.rows.len(), 3);
    for (index, row) in result.rows.iter().enumerate() {
        assert_eq!(string(&row[0]), expected_names[index]);
        assert_eq!(integer(&row[1]), index as i64 + 1);
        let route = path(&row[2]);
        assert_eq!(node_names(route), expected_paths[index]);
        assert_list_matches_path(route, &row[3]);
        for (offset, relationship) in route.relationships.iter().enumerate() {
            assert_eq!(
                relationship.start_node_element_id,
                route.nodes[offset + 1].element_id
            );
            assert_eq!(
                relationship.end_node_element_id,
                route.nodes[offset].element_id
            );
        }
    }
}

#[test]
fn undirected_unbounded_path_keeps_reverse_storage_direction() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH route = (:UnboundedLinear {name: 'd'})-[rels:NEXT*]-
                           (:UnboundedLinear {name: 'a'}) \
             RETURN route, rels",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    let route = path(&result.rows[0][0]);
    assert_eq!(node_names(route), ["d", "c", "b", "a"]);
    let relationships = relationship_list(&result.rows[0][1]);
    assert_eq!(relationship_names(&relationships), ["cd", "bc", "ab"]);
    assert_list_matches_path(route, &result.rows[0][1]);
    for (offset, relationship) in route.relationships.iter().enumerate() {
        assert_eq!(
            relationship.start_node_element_id,
            route.nodes[offset + 1].element_id
        );
        assert_eq!(
            relationship.end_node_element_id,
            route.nodes[offset].element_id
        );
    }
}

#[test]
fn unbounded_type_property_and_parameter_filters_prune_each_hop() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:HybridProperty {name: 'a', eligible: false}), \
                    (:HybridProperty {name: 'b', eligible: true}), \
                    (:HybridProperty {name: 'c', eligible: true}), \
                    (:HybridProperty {name: 'd', eligible: true}), \
                    (:HybridProperty {name: 'e', eligible: true})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:HybridProperty {name: 'a'}), (b:HybridProperty {name: 'b'}), \
                   (c:HybridProperty {name: 'c'}), (d:HybridProperty {name: 'd'}), \
                   (e:HybridProperty {name: 'e'}) \
             CREATE (a)-[:NEXT {name: 'ab', enabled: true}]->(b), \
                    (b)-[:NEXT {name: 'bc', enabled: true}]->(c), \
                    (a)-[:NEXT {name: 'ad', enabled: false}]->(d), \
                    (a)-[:OTHER {name: 'ae', enabled: true}]->(e)",
        )
        .unwrap();
    let parameters = [
        ("start".to_owned(), json!("a")),
        ("enabled".to_owned(), json!(true)),
        ("eligible".to_owned(), json!(true)),
    ]
    .into_iter()
    .collect::<Parameters>();

    let result = database
        .execute_with(
            "MATCH (:HybridProperty {name: $start})-
                   [rels:NEXT* {enabled: $enabled}]->
                   (end:HybridProperty {eligible: $eligible}) \
             RETURN end.name AS name, rels ORDER BY name",
            &parameters,
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    assert_eq!(string(&result.rows[0][0]), "b");
    assert_eq!(
        relationship_names(&relationship_list(&result.rows[0][1])),
        ["ab"]
    );
    assert_eq!(string(&result.rows[1][0]), "c");
    assert_eq!(
        relationship_names(&relationship_list(&result.rows[1][1])),
        ["ab", "bc"]
    );
}

#[test]
fn unbounded_paths_and_relationship_lists_survive_with_aliases() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH route = (:UnboundedLinear {name: 'a'})-[rels:NEXT*]->(end) \
             WITH route AS path_alias, rels AS links \
             RETURN path_alias, links, length(path_alias) AS hops ORDER BY hops",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 3);
    for (index, row) in result.rows.iter().enumerate() {
        let expected_hops = index as i64 + 1;
        let route = path(&row[0]);
        assert_eq!(route.relationships.len(), expected_hops as usize);
        assert_list_matches_path(route, &row[1]);
        assert_eq!(integer(&row[2]), expected_hops);
    }
}

#[test]
fn sql_rows_flow_through_rust_traversal_and_back_to_sql() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (start:UnboundedLinear {name: 'a'}) \
             WITH start \
             MATCH (start)-[:NEXT*]->(middle) \
             WITH middle \
             MATCH (middle)-[:NEXT]->(end) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::String("c".into())],
            vec![CypherValue::String("d".into())],
        ]
    );
}

#[test]
fn unbounded_bag_rows_feed_filtering_and_aggregation() {
    let mut database = diamond_graph();

    let result = database
        .execute(
            "MATCH (:HybridDiamond {name: 'a'})-[:NEXT*]->(end) \
             WITH end.name AS name WHERE name = 'd' \
             RETURN name, count(*) AS paths",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("d".into()),
            CypherValue::Integer(2),
        ]]
    );
}

#[test]
fn mixed_and_multiple_hybrid_segments_compose_with_shared_edge_uniqueness() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:HybridMixed {name: 'a'}), (:HybridMixed {name: 'b'}), \
                    (:HybridMixed {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:HybridMixed {name: 'a'}), (b:HybridMixed {name: 'b'}), \
                   (c:HybridMixed {name: 'c'}) \
             CREATE (a)-[:LINK]->(b), (b)-[:LINK]->(c)",
        )
        .unwrap();

    for query in [
        "MATCH (:HybridMixed {name: 'a'})-[:LINK*]->(middle)-[:LINK]->(end) \
         RETURN end.name AS name",
        "MATCH (:HybridMixed {name: 'a'})-[:LINK*]->(middle)-[:LINK*]->(end) \
         RETURN end.name AS name",
        "MATCH (:HybridMixed {name: 'a'})-[:LINK*]->(middle)-[:LINK*1..1]->(end) RETURN end.name AS name",
    ] {
        let result = database.execute(query).unwrap();
        assert_eq!(
            result.rows,
            [vec![CypherValue::String("c".into())]],
            "unexpected mixed-segment result for {query}"
        );
    }

    let mut single_edge = Database::in_memory().unwrap();
    single_edge
        .execute(
            "CREATE (:HybridReuse {name: 'a'})-[:ONLY]->
                    (:HybridReuse {name: 'b'})",
        )
        .unwrap();
    let reused = single_edge
        .execute(
            "MATCH (:HybridReuse {name: 'a'})-[:ONLY*]-(middle)-[:ONLY]-(end) \
             RETURN end",
        )
        .unwrap();
    assert!(reused.rows.is_empty());
}

#[test]
fn relationship_uniqueness_spans_one_match_but_not_separate_match_clauses() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:HybridClause {name: 'a'})-[:ONLY]->(:HybridClause {name: 'b'})")
        .unwrap();

    for query in [
        "MATCH (a:HybridClause {name: 'a'})-[:ONLY*]->(b), (a)-[:ONLY]->(b) RETURN b",
        "MATCH (a:HybridClause {name: 'a'})-[:ONLY*]->(b), (a)-[:ONLY*]->(b) RETURN b",
    ] {
        let result = database.execute(query).unwrap();
        assert!(
            result.rows.is_empty(),
            "one MATCH reused a relationship across path patterns: {query}"
        );
    }

    let separate_clauses = database
        .execute(
            "MATCH (a:HybridClause {name: 'a'})-[:ONLY*]->(b) MATCH (a)-[:ONLY]->(b) RETURN b.name AS name",
        )
        .unwrap();
    assert_eq!(
        separate_clauses.rows,
        [vec![CypherValue::String("b".into())]]
    );

    database
        .execute(
            "MATCH (a:HybridClause {name: 'a'}), (b:HybridClause {name: 'b'}) CREATE (a)-[:ONLY]->(b)",
        )
        .unwrap();
    let parallel_edges = database
        .execute(
            "MATCH (a:HybridClause {name: 'a'})-[:ONLY*]->(b), (a)-[:ONLY]->(b) RETURN count(*) AS paths",
        )
        .unwrap();
    assert_eq!(parallel_edges.rows, [vec![CypherValue::Integer(2)]]);
}

#[test]
fn union_shapes_hybrid_path_identities_before_shared_hydration() {
    let mut database = linear_graph();
    let distinct_query = "MATCH route = (:UnboundedLinear {name: 'a'})-[:NEXT*]->(end) \
         RETURN route, length(route) AS hops \
         UNION \
         MATCH route = (:UnboundedLinear {name: 'a'})-[:NEXT*1..3]->(end) \
         RETURN route, length(route) AS hops";
    let all_query = distinct_query.replacen(" UNION ", " UNION ALL ", 1);

    let distinct = database.execute(distinct_query).unwrap();
    let all = database.execute(&all_query).unwrap();
    assert_eq!(distinct.rows.len(), 3);
    assert_eq!(all.rows.len(), 6);

    let mut distinct_hops = distinct
        .rows
        .iter()
        .map(|row| integer(&row[1]))
        .collect::<Vec<_>>();
    distinct_hops.sort();
    assert_eq!(distinct_hops, [1, 2, 3]);
    assert!(
        distinct
            .rows
            .iter()
            .all(|row| path(&row[0]).relationships.len() == integer(&row[1]) as usize)
    );

    let physical = explain_physical(distinct_query).unwrap();
    assert_eq!(physical.matches("trail-expand").count(), 1);
    assert_eq!(physical.matches("batch-hydrate-paths").count(), 1);
    assert_stage_precedes(&physical, "trail-expand", "union: semantic-distinct");
    assert_stage_precedes(&physical, "union: semantic-distinct", "batch-hydrate-paths");
}

#[test]
fn logical_and_physical_explain_show_the_sql_rust_strategy_boundary() {
    let bounded = "MATCH (start)-[:NEXT*32]->(end) RETURN end";
    let deep = "MATCH (start)-[:NEXT*33]->(end) RETURN end";
    let unbounded = "MATCH route = (start)-[:NEXT*]->(end) \
         RETURN DISTINCT route, length(route) AS hops ORDER BY hops SKIP 1 LIMIT 1";

    let bounded_logical = explain(bounded).unwrap();
    assert!(bounded_logical.contains("expand-variable"));
    assert!(!bounded_logical.contains("expand-hybrid-variable"));

    let deep_logical = explain(deep).unwrap();
    assert!(deep_logical.contains("expand-hybrid-variable executor=rust"));
    assert!(deep_logical.contains("bounds=33..33"));

    let unbounded_logical = explain(unbounded).unwrap();
    assert!(unbounded_logical.contains("expand-hybrid-variable executor=rust"));
    assert!(unbounded_logical.contains("bounds=1..unbounded"));

    let bounded_physical = explain_physical(bounded).unwrap();
    assert!(bounded_physical.contains("_crabnet_walk_"));
    assert!(!bounded_physical.contains("trail-expand"));

    let deep_physical = explain_physical(deep).unwrap();
    assert!(deep_physical.contains("rust-stage: trail-expand"));
    assert!(!deep_physical.contains("_crabnet_walk_"));

    let unbounded_physical = explain_physical(unbounded).unwrap();
    assert_eq!(unbounded_physical.matches("trail-expand").count(), 1);
    assert_eq!(unbounded_physical.matches("batch-hydrate-paths").count(), 1);
    assert_stage_precedes(&unbounded_physical, "trail-expand", "semantic-distinct");
    assert_stage_precedes(&unbounded_physical, "semantic-distinct", "pagination");
    assert_stage_precedes(&unbounded_physical, "pagination", "batch-hydrate-paths");
}

#[test]
fn neighboring_unimplemented_traversal_forms_remain_rejected() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "OPTIONAL MATCH (a)-[:NEXT*]->(b) RETURN b",
        "MATCH (a)-[:NEXT]->{1, 2}(b) RETURN b",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Unsupported(_))),
            "expected a structured unsupported error for {query}"
        );
    }
}

#[test]
fn writes_fed_by_unbounded_traversal_are_rejected_without_side_effects() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:HybridWriteStart)-[:LINK]->(:HybridWriteEnd)")
        .unwrap();

    assert!(matches!(
        database.execute(
            "MATCH (:HybridWriteStart)-[:LINK*]->(:HybridWriteEnd) \
             CREATE (:HybridTraversalSideEffect)"
        ),
        Err(Error::Unsupported(_))
    ));

    let remaining = database
        .execute("MATCH (node:HybridTraversalSideEffect) RETURN node")
        .unwrap();
    assert!(remaining.rows.is_empty());
}
