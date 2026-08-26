use crabnet::{CypherValue, Database, Error, Parameters, QueryResult};
use serde_json::json;

fn assert_names(result: QueryResult, expected: &[&str]) {
    assert_eq!(result.columns.len(), 1);
    let actual = result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [CypherValue::String(name)] => name.as_str(),
            other => panic!("expected one string column, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

fn linear_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:TraversalNode {name: 'a'}), \
                    (:TraversalNode {name: 'b'}), \
                    (:TraversalNode {name: 'c'}), \
                    (:TraversalNode {name: 'd'}), \
                    (:TraversalNode {name: 'dead'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:TraversalNode {name: 'a'}), \
                   (b:TraversalNode {name: 'b'}), \
                   (c:TraversalNode {name: 'c'}), \
                   (d:TraversalNode {name: 'd'}), \
                   (dead:TraversalNode {name: 'dead'}) \
             CREATE (a)-[:NEXT]->(b), (b)-[:NEXT]->(c), \
                    (c)-[:NEXT]->(d), (b)-[:NEXT]->(dead)",
        )
        .unwrap();
    database
}

#[test]
fn bounded_range_returns_endpoints_at_every_allowed_depth() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:TraversalNode {name: 'a'})-[:NEXT*1..3]->(end:TraversalNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["b", "c", "d", "dead"]);
}

#[test]
fn exact_bound_returns_only_endpoints_at_that_depth() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:TraversalNode {name: 'a'})-[:NEXT*2]->(end:TraversalNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["c", "dead"]);
}

#[test]
fn omitted_lower_bound_defaults_to_one() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:TraversalNode {name: 'a'})-[:NEXT*..2]->(end:TraversalNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["b", "c", "dead"]);
}

#[test]
fn bound_literals_normalize_leading_zeroes_and_separators() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:TraversalNode {name: 'a'})-[:NEXT*01..0_02]->(end:TraversalNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["b", "c", "dead"]);
}

#[test]
fn reversed_and_omitted_lower_ranges_are_valid_empty_ranges() {
    let mut database = linear_graph();

    for query in [
        "MATCH (:TraversalNode {name: 'a'})-[:NEXT*2..1]->(end) RETURN end",
        "MATCH (:TraversalNode {name: 'a'})-[:NEXT*1..0]->(end) RETURN end",
        "MATCH (:TraversalNode {name: 'a'})-[:NEXT*..0]->(end) RETURN end",
    ] {
        let result = database.execute(query).unwrap();
        assert!(result.rows.is_empty(), "range should be empty: {query}");
    }
}

#[test]
fn zero_hop_is_an_identity_path_and_still_checks_the_endpoint() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ZeroNode {name: 'alone', eligible: true}), \
                    (:ZeroNode {name: 'other', eligible: false})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (start:ZeroNode {name: 'alone'})-[:MISSING*0..0]->
                   (end:ZeroNode {eligible: true}) \
             RETURN start.name AS start, end.name AS end",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("alone".into()),
            CypherValue::String("alone".into()),
        ]]
    );
}

#[test]
fn dead_ends_do_not_turn_mandatory_traversal_into_null_rows() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:TraversalNode {name: 'dead'})-[:NEXT*1..3]->(end) \
             RETURN end",
        )
        .unwrap();

    assert!(result.rows.is_empty());
}

#[test]
fn typed_and_untyped_traversals_filter_each_relationship() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:TypeNode {name: 'a'}), (:TypeNode {name: 'b'}), \
                    (:TypeNode {name: 'c'}), (:TypeNode {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:TypeNode {name: 'a'}), (b:TypeNode {name: 'b'}), \
                   (c:TypeNode {name: 'c'}), (d:TypeNode {name: 'd'}) \
             CREATE (a)-[:RED]->(b), (b)-[:BLUE]->(c), (a)-[:BLUE]->(d)",
        )
        .unwrap();

    let typed = database
        .execute(
            "MATCH (:TypeNode {name: 'a'})-[:RED*1..2]->(end:TypeNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();
    assert_names(typed, &["b"]);

    let untyped = database
        .execute(
            "MATCH (:TypeNode {name: 'a'})-[*1..2]->(end:TypeNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();
    assert_names(untyped, &["b", "c", "d"]);
}

#[test]
fn relationship_and_endpoint_properties_apply_to_every_candidate_path() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:PropertyNode {name: 'a'}), \
                    (:PropertyNode {name: 'b', eligible: true}), \
                    (:PropertyNode {name: 'c', eligible: true}), \
                    (:PropertyNode {name: 'd', eligible: false})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:PropertyNode {name: 'a'}), (b:PropertyNode {name: 'b'}), \
                   (c:PropertyNode {name: 'c'}), (d:PropertyNode {name: 'd'}) \
             CREATE (a)-[:LINK {enabled: true}]->(b), \
                    (b)-[:LINK {enabled: false}]->(c), \
                    (a)-[:LINK {enabled: true}]->(d)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (:PropertyNode {name: 'a'})-
                   [:LINK*1..2 {enabled: true}]->
                   (end:PropertyNode {eligible: true}) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["b"]);
}

#[test]
fn incoming_traversal_walks_against_storage_direction() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (end:TraversalNode {name: 'c'})<-[:NEXT*1..2]-
                   (start:TraversalNode) \
             RETURN start.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["a", "b"]);
}

#[test]
fn undirected_traversal_can_walk_either_storage_direction() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:TraversalNode {name: 'c'})-[:NEXT*1..2]-(end:TraversalNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["a", "b", "d", "dead"]);
}

#[test]
fn cycles_may_revisit_nodes_but_may_not_reuse_an_edge() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:CycleNode {name: 'a'}), (:CycleNode {name: 'b'}), \
                    (:CycleNode {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:CycleNode {name: 'a'}), (b:CycleNode {name: 'b'}), \
                   (c:CycleNode {name: 'c'}) \
             CREATE (a)-[:CYCLE]->(b), (b)-[:CYCLE]->(c), (c)-[:CYCLE]->(a)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (:CycleNode {name: 'a'})-[:CYCLE*1..4]->(end:CycleNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    // The depth-three path revisits `a`; there is no depth-four path because
    // that would reuse the first stored relationship.
    assert_names(result, &["a", "b", "c"]);
}

#[test]
fn a_self_loop_can_be_used_once_per_path() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (node:LoopNode {name: 'loop'})-[:LOOP]->(node)")
        .unwrap();

    let including_identity = database
        .execute(
            "MATCH (:LoopNode {name: 'loop'})-[:LOOP*0..2]->(end:LoopNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();
    assert_names(including_identity, &["loop", "loop"]);

    let reused = database
        .execute(
            "MATCH (:LoopNode {name: 'loop'})-[:LOOP*2]->(end:LoopNode) \
             RETURN end.name AS name",
        )
        .unwrap();
    assert!(reused.rows.is_empty());
}

#[test]
fn a_diamond_preserves_one_row_per_relationship_sequence() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:DiamondNode {name: 'a'}), (:DiamondNode {name: 'b'}), \
                    (:DiamondNode {name: 'c'}), (:DiamondNode {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:DiamondNode {name: 'a'}), (b:DiamondNode {name: 'b'}), \
                   (c:DiamondNode {name: 'c'}), (d:DiamondNode {name: 'd'}) \
             CREATE (a)-[:NEXT]->(b), (a)-[:NEXT]->(c), \
                    (b)-[:NEXT]->(d), (c)-[:NEXT]->(d)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (:DiamondNode {name: 'a'})-[:NEXT*2]->(end:DiamondNode) \
             RETURN end.name AS name",
        )
        .unwrap();

    assert_names(result, &["d", "d"]);
}

#[test]
fn parallel_edges_preserve_path_multiplicity_at_each_depth() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ParallelNode {name: 'a'}), (:ParallelNode {name: 'b'}), \
                    (:ParallelNode {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ParallelNode {name: 'a'}), (b:ParallelNode {name: 'b'}), \
                   (c:ParallelNode {name: 'c'}) \
             CREATE (a)-[:NEXT]->(b), (a)-[:NEXT]->(b), (b)-[:NEXT]->(c)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (:ParallelNode {name: 'a'})-[:NEXT*1..2]->(end:ParallelNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["b", "b", "c", "c"]);
}

#[test]
fn edge_identity_not_node_identity_controls_uniqueness() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:TrailNode {name: 'a'}), (:TrailNode {name: 'b'})")
        .unwrap();
    database
        .execute(
            "MATCH (a:TrailNode {name: 'a'}), (b:TrailNode {name: 'b'}) \
             CREATE (a)-[:LINK]->(b), (a)-[:LINK]->(b)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (:TrailNode {name: 'a'})-[:LINK*2]-(end:TrailNode) \
             RETURN end.name AS name",
        )
        .unwrap();

    // Each valid trail uses the two parallel stored edges in a different
    // order. Reusing either one would incorrectly produce two extra rows.
    assert_names(result, &["a", "a"]);
}

#[test]
fn fixed_and_quantified_segments_cannot_reuse_the_same_relationship() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:SharedTrailNode {name: 'a'})-[:LINK]->(:SharedTrailNode {name: 'b'})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (:SharedTrailNode {name: 'a'})-[:LINK*1]-(middle)-[:LINK]-(end) \
             RETURN end",
        )
        .unwrap();

    assert!(result.rows.is_empty());
}

#[test]
fn two_quantified_segments_share_relationship_uniqueness() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:QuantifiedTrail {name: 'a'})-[:LINK]->(:QuantifiedTrail {name: 'b'})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (:QuantifiedTrail {name: 'a'})-[:LINK*1]-(middle)-[:LINK*1]-(end) \
             RETURN end",
        )
        .unwrap();

    assert!(result.rows.is_empty());
}

#[test]
fn fixed_and_variable_segments_compose_in_one_path() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:TraversalNode {name: 'a'})-[:NEXT*1..2]->
                   (middle:TraversalNode)-[:NEXT]->(end:TraversalNode) \
             RETURN end.name AS name ORDER BY name",
        )
        .unwrap();

    assert_names(result, &["c", "d", "dead"]);
}

#[test]
fn traversal_composes_with_with_filters_and_aggregation() {
    let mut database = linear_graph();

    let projected = database
        .execute(
            "MATCH (start:TraversalNode {name: 'a'}) \
             WITH start \
             MATCH (start)-[:NEXT*1..2]->(end:TraversalNode) \
             WITH end.name AS name WHERE name <> 'b' \
             RETURN name ORDER BY name",
        )
        .unwrap();
    assert_names(projected, &["c", "dead"]);

    let aggregated = database
        .execute(
            "MATCH (:TraversalNode {name: 'a'})-[:NEXT*1..2]->(end) \
             RETURN count(*) AS paths",
        )
        .unwrap();
    assert_eq!(aggregated.rows, [vec![CypherValue::Integer(3)]]);
}

#[test]
fn relationship_property_parameters_keep_bind_positions_across_with() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:ParameterNode {name: 'a', eligible: 0}), \
                    (:ParameterNode {name: 'b', eligible: 1}), \
                    (:ParameterNode {name: 'c', eligible: 1}), \
                    (:ParameterNode {name: 'd', eligible: 1})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:ParameterNode {name: 'a'}), (b:ParameterNode {name: 'b'}), \
                   (c:ParameterNode {name: 'c'}), (d:ParameterNode {name: 'd'}) \
             CREATE (a)-[:LINK {enabled: true}]->(b), \
                    (b)-[:LINK {enabled: true}]->(c), \
                    (a)-[:LINK {enabled: false}]->(d)",
        )
        .unwrap();
    let parameters = [
        ("marker".to_owned(), json!("kept")),
        ("start".to_owned(), json!("a")),
        ("enabled".to_owned(), json!(true)),
        ("eligible".to_owned(), json!(1)),
    ]
    .into_iter()
    .collect::<Parameters>();

    let result = database
        .execute_with(
            "WITH $marker AS marker \
             MATCH (:ParameterNode {name: $start})-
                   [:LINK*1..2 {enabled: $enabled}]->
                   (end:ParameterNode {eligible: $eligible}) \
             RETURN marker, end.name AS name ORDER BY name",
            &parameters,
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("kept".into()),
                CypherValue::String("b".into()),
            ],
            vec![
                CypherValue::String("kept".into()),
                CypherValue::String("c".into()),
            ],
        ]
    );
}

#[test]
fn documented_depth_cap_is_inclusive_and_cap_plus_one_is_rejected() {
    let mut database = Database::in_memory().unwrap();
    let mut create = String::from("CREATE ");
    for depth in 0..=32 {
        if depth > 0 {
            create.push_str("-[:NEXT]->");
        }
        create.push_str(&format!("(:DepthCapNode {{depth: {depth}}})"));
    }
    database.execute(&create).unwrap();

    let at_cap = database
        .execute(
            "MATCH (:DepthCapNode {depth: 0})-[:NEXT*32]->(end:DepthCapNode) \
             RETURN end.depth AS depth",
        )
        .unwrap();
    assert_eq!(at_cap.rows, [vec![CypherValue::Integer(32)]]);

    assert!(matches!(
        database.execute(
            "MATCH (:DepthCapNode {depth: 0})-[:NEXT*33]->(end:DepthCapNode) \
             RETURN end"
        ),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn unbounded_and_broader_quantified_forms_are_structured_rejections() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "MATCH (a)-[*]->(b) RETURN b",
        "MATCH (a)-[*1..]->(b) RETURN b",
        "MATCH (a)-[*..]->(b) RETURN b",
        "MATCH (a)-[relationships:LINK*1..2]->(b) RETURN b",
        "OPTIONAL MATCH (a)-[:LINK*1..2]->(b) RETURN b",
        "MATCH (a)-[:LINK]->{1, 2}(b) RETURN b",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Unsupported(_))),
            "unsupported quantified shape should be rejected during planning: {query}"
        );
    }
}

#[test]
fn an_absurd_upper_bound_is_rejected_without_overflow_or_execution() {
    let mut database = Database::in_memory().unwrap();

    let error = database
        .execute(
            "MATCH (a)-[:LINK*1..999999999999999999999999999999999999]->(b) \
             RETURN b",
        )
        .unwrap_err();

    assert!(matches!(error, Error::Semantic(_) | Error::Unsupported(_)));
}

#[test]
fn writes_fed_by_quantified_match_are_rejected_without_side_effects() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:WriteStart)-[:LINK]->(:WriteEnd)")
        .unwrap();

    assert!(matches!(
        database.execute(
            "MATCH (:WriteStart)-[:LINK*1..2]->(:WriteEnd) \
             CREATE (:TraversalWriteSideEffect)"
        ),
        Err(Error::Unsupported(_))
    ));

    let remaining = database
        .execute("MATCH (node:TraversalWriteSideEffect) RETURN node")
        .unwrap();
    assert!(remaining.rows.is_empty());
}
