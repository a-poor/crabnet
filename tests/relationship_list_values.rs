use crabnet::{
    CypherValue, Database, Error, PathValue, QueryResult, RelationshipValue, explain_physical,
};

fn text(value: &str) -> CypherValue {
    CypherValue::String(value.into())
}

fn relationship_name(relationship: &RelationshipValue) -> &str {
    relationship
        .properties
        .get("name")
        .and_then(CypherValue::as_str)
        .expect("relationship must have a string name")
}

fn relationship_list(value: &CypherValue) -> Vec<&RelationshipValue> {
    value
        .as_list()
        .expect("expected a relationship list")
        .iter()
        .map(|value| {
            value
                .as_relationship()
                .expect("relationship-list member must be a relationship")
        })
        .collect()
}

fn relationship_list_names(value: &CypherValue) -> Vec<&str> {
    relationship_list(value)
        .into_iter()
        .map(relationship_name)
        .collect()
}

fn path_at(result: &QueryResult, row: usize, column: usize) -> &PathValue {
    result.rows[row][column]
        .as_path()
        .unwrap_or_else(|| panic!("expected a path at row {row}, column {column}"))
}

fn assert_list_matches_path(path: &PathValue, list: &CypherValue) {
    let relationships = relationship_list(list);
    assert_eq!(relationships.len(), path.relationships.len());
    for (from_list, from_path) in relationships.iter().zip(&path.relationships) {
        assert_eq!(from_list.element_id, from_path.element_id);
    }
}

fn linear_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:RelListNode {name: 'a'}), (:RelListNode {name: 'b'}), \
                    (:RelListNode {name: 'c'}), (:RelListNode {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:RelListNode {name: 'a'}), (b:RelListNode {name: 'b'}), \
                   (c:RelListNode {name: 'c'}), (d:RelListNode {name: 'd'}) \
             CREATE (a)-[:NEXT {name: 'ab'}]->(b), \
                    (b)-[:NEXT {name: 'bc'}]->(c), \
                    (c)-[:NEXT {name: 'cd'}]->(d)",
        )
        .unwrap();
    database
}

#[test]
fn exact_relationship_list_coexists_with_its_named_path() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH path = (:RelListNode {name: 'a'})-[rels:NEXT*2]->
                         (:RelListNode {name: 'c'}) \
             RETURN path, rels",
        )
        .unwrap();

    assert_eq!(result.columns, ["path", "rels"]);
    assert_eq!(result.rows.len(), 1);
    let path = path_at(&result, 0, 0);
    assert_eq!(
        path.relationships
            .iter()
            .map(relationship_name)
            .collect::<Vec<_>>(),
        ["ab", "bc"]
    );
    assert_list_matches_path(path, &result.rows[0][1]);
    for (index, relationship) in relationship_list(&result.rows[0][1])
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            relationship.start_node_element_id,
            path.nodes[index].element_id
        );
        assert_eq!(
            relationship.end_node_element_id,
            path.nodes[index + 1].element_id
        );
    }
}

#[test]
fn multi_segment_relationship_lists_share_the_named_path_and_edge_uniqueness() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH path = (:RelListNode {name: 'a'})-[first:NEXT*1..2]->
                          (middle:RelListNode {name: 'c'})-[second:NEXT*1..2]->
                          (:RelListNode {name: 'd'}) \
             RETURN path, first, second",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    let path = path_at(&result, 0, 0);
    assert_eq!(
        path.relationships
            .iter()
            .map(relationship_name)
            .collect::<Vec<_>>(),
        ["ab", "bc", "cd"]
    );
    assert_eq!(relationship_list_names(&result.rows[0][1]), ["ab", "bc"]);
    assert_eq!(relationship_list_names(&result.rows[0][2]), ["cd"]);

    let reused = database
        .execute(
            "MATCH path = (:RelListNode {name: 'a'})-[first:NEXT*1]-
                          (:RelListNode {name: 'b'})-[second:NEXT*1]-
                          (:RelListNode {name: 'a'}) \
             RETURN path, first, second",
        )
        .unwrap();
    assert!(reused.rows.is_empty());
}

#[test]
fn bounded_range_returns_one_ordered_relationship_list_per_candidate() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1..3]->
                   (end:RelListNode) \
             RETURN rels, end.name AS endpoint ORDER BY endpoint",
        )
        .unwrap();

    assert_eq!(result.columns, ["rels", "endpoint"]);
    assert_eq!(result.rows.len(), 3);
    assert_eq!(relationship_list_names(&result.rows[0][0]), ["ab"]);
    assert_eq!(relationship_list_names(&result.rows[1][0]), ["ab", "bc"]);
    assert_eq!(
        relationship_list_names(&result.rows[2][0]),
        ["ab", "bc", "cd"]
    );
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| row[1].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["b", "c", "d"]
    );
}

#[test]
fn zero_hop_relationship_binding_is_an_empty_typed_list() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:MISSING*0]->
                   (:RelListNode {name: 'a'}) \
             RETURN rels",
        )
        .unwrap();

    assert_eq!(result.columns, ["rels"]);
    assert_eq!(result.rows, [vec![CypherValue::List(Vec::new())]]);
}

#[test]
fn incoming_relationship_list_follows_traversal_order_but_keeps_storage_direction() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH path = (:RelListNode {name: 'c'})<-[rels:NEXT*2]-
                         (:RelListNode {name: 'a'}) \
             RETURN path, rels",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    let path = path_at(&result, 0, 0);
    assert_eq!(relationship_list_names(&result.rows[0][1]), ["bc", "ab"]);
    assert_list_matches_path(path, &result.rows[0][1]);
    for (index, relationship) in relationship_list(&result.rows[0][1])
        .into_iter()
        .enumerate()
    {
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
fn undirected_relationship_list_uses_the_chosen_traversal_orientation() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH path = (:RelListNode {name: 'c'})-[rels:NEXT*2]-
                         (:RelListNode {name: 'a'}) \
             RETURN path, rels",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    let path = path_at(&result, 0, 0);
    assert_eq!(relationship_list_names(&result.rows[0][1]), ["bc", "ab"]);
    assert_list_matches_path(path, &result.rows[0][1]);
    for (index, relationship) in relationship_list(&result.rows[0][1])
        .into_iter()
        .enumerate()
    {
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
fn direct_relationship_list_values_survive_with_aliases() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*2]->
                   (end:RelListNode {name: 'c'}) \
             WITH rels AS links, end \
             RETURN links, end.name AS endpoint",
        )
        .unwrap();

    assert_eq!(result.columns, ["links", "endpoint"]);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(relationship_list_names(&result.rows[0][0]), ["ab", "bc"]);
    assert_eq!(result.rows[0][1], CypherValue::String("c".into()));
}

#[test]
fn distinct_and_pagination_shape_relationship_list_identities() {
    let mut database = linear_graph();
    database
        .execute("CREATE (:RelListMarker), (:RelListMarker)")
        .unwrap();

    let result = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*0..3]->
                   (end:RelListNode), (:RelListMarker) \
             RETURN DISTINCT rels, end.name AS endpoint \
             ORDER BY endpoint SKIP 1 LIMIT 2",
        )
        .unwrap();

    assert_eq!(result.columns, ["rels", "endpoint"]);
    assert_eq!(result.rows.len(), 2);
    assert_eq!(relationship_list_names(&result.rows[0][0]), ["ab"]);
    assert_eq!(relationship_list_names(&result.rows[1][0]), ["ab", "bc"]);
    assert_eq!(result.rows[0][1], CypherValue::String("b".into()));
    assert_eq!(result.rows[1][1], CypherValue::String("c".into()));
}

#[test]
fn physical_plan_shapes_relationship_list_identities_before_one_batch_hydration() {
    let explanation = explain_physical(
        "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*0..3]->
               (end:RelListNode) \
         RETURN DISTINCT rels, end.name AS endpoint \
         ORDER BY endpoint SKIP 1 LIMIT 2",
    )
    .unwrap();

    let distinct = explanation.find(": semantic-distinct").unwrap();
    let pagination = explanation.find(": pagination").unwrap();
    let hydration = explanation.find(": batch-hydrate-paths").unwrap();
    assert!(distinct < pagination && pagination < hydration);
    assert_eq!(
        explanation.matches("batch-hydrate-paths").count(),
        1,
        "relationship-list identities must be shaped before one shared hydration stage:\n{explanation}"
    );
}

#[test]
fn union_uses_relationship_list_identity_while_union_all_keeps_bag_multiplicity() {
    let mut database = linear_graph();
    let branch = "MATCH (:RelListNode {name: 'a'})-
                               [rels:NEXT*2]->(:RelListNode {name: 'c'})
                  RETURN rels AS links";

    let distinct = database
        .execute(&format!("{branch} UNION {branch}"))
        .unwrap();
    assert_eq!(distinct.columns, ["links"]);
    assert_eq!(distinct.rows.len(), 1);
    assert_eq!(relationship_list_names(&distinct.rows[0][0]), ["ab", "bc"]);

    let all = database
        .execute(&format!("{branch} UNION ALL {branch}"))
        .unwrap();
    assert_eq!(all.rows.len(), 2);
    assert_eq!(all.rows[0], all.rows[1]);
}

#[test]
fn union_deduplicates_pattern_and_path_function_relationship_lists() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*2]->
                   (:RelListNode {name: 'c'}) \
             RETURN rels AS links \
             UNION \
             MATCH path = (:RelListNode {name: 'a'})-[:NEXT*2]->
                          (:RelListNode {name: 'c'}) \
             RETURN relationships(path) AS links",
        )
        .unwrap();

    assert_eq!(result.columns, ["links"]);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(relationship_list_names(&result.rows[0][0]), ["ab", "bc"]);
}

#[test]
fn zero_hop_relationship_list_union_literal_empty_list_deduplicates() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:MISSING*0]->
                   (:RelListNode {name: 'a'}) \
             RETURN rels AS links \
             UNION \
             RETURN [] AS links",
        )
        .unwrap();

    assert_eq!(result.columns, ["links"]);
    assert_eq!(result.rows, [vec![CypherValue::List(Vec::new())]]);
}

#[test]
fn distinct_multiple_zero_hop_relationship_bindings_is_one_empty_list() {
    let mut database = linear_graph();

    let result = database
        .execute(
            "MATCH (:RelListNode)-[rels:MISSING*0]->(:RelListNode) \
             RETURN DISTINCT rels",
        )
        .unwrap();

    assert_eq!(result.columns, ["rels"]);
    assert_eq!(result.rows, [vec![CypherValue::List(Vec::new())]]);
}

#[test]
fn parallel_edges_produce_distinct_single_relationship_lists() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:RelListParallel {name: 'a'}), \
                    (:RelListParallel {name: 'b'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:RelListParallel {name: 'a'}), \
                   (b:RelListParallel {name: 'b'}) \
             CREATE (a)-[:LINK {name: 'same'}]->(b), \
                    (a)-[:LINK {name: 'same'}]->(b)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (:RelListParallel {name: 'a'})-[rels:LINK*1]->
                   (:RelListParallel {name: 'b'}) \
             RETURN DISTINCT rels",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    let ids = result
        .rows
        .iter()
        .map(|row| relationship_list(&row[0])[0].element_id.as_str())
        .collect::<Vec<_>>();
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn relationship_list_scope_drops_at_a_with_boundary() {
    let mut database = linear_graph();

    let error = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(end:RelListNode) \
             WITH end \
             RETURN rels",
        )
        .unwrap_err();

    assert!(matches!(error, Error::Semantic(_)));
}

#[test]
fn dropped_relationship_list_name_can_be_rebound_as_node_or_fresh_list() {
    let mut database = linear_graph();

    let node = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->
                   (end:RelListNode {name: 'b'}) \
             WITH end \
             MATCH (rels:RelListNode {name: 'd'}) \
             RETURN rels.name AS name",
        )
        .unwrap();
    assert_eq!(node.rows, [vec![CypherValue::String("d".into())]]);

    let list = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->
                   (end:RelListNode {name: 'b'}) \
             WITH end \
             MATCH (end)-[rels:NEXT*1]->
                   (finish:RelListNode {name: 'c'}) \
             RETURN rels, finish.name AS name",
        )
        .unwrap();
    assert_eq!(list.rows.len(), 1);
    assert_eq!(relationship_list_names(&list.rows[0][0]), ["bc"]);
    assert_eq!(list.rows[0][1], CypherValue::String("c".into()));
}

#[test]
fn relationship_list_bindings_reject_prebound_wrong_kinds() {
    let mut database = linear_graph();

    for query in [
        "WITH 1 AS rels \
         MATCH (:RelListNode)-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels",
        "MATCH (:RelListNode {name: 'a'})-[rels:NEXT]->(:RelListNode {name: 'b'}) \
         WITH rels \
         MATCH (:RelListNode)-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels",
        "MATCH (rels:RelListNode {name: 'a'})-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels",
        "MATCH path = (:RelListNode {name: 'a'})-[:NEXT]->(:RelListNode) \
         WITH path AS rels \
         MATCH (:RelListNode)-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "wrong-kind relationship-list binding must fail semantically: {query}"
        );
    }
}

#[test]
fn relationship_list_binding_name_collisions_are_semantic() {
    let mut database = linear_graph();

    for query in [
        "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(rels) \
         RETURN rels",
        "MATCH rels = (:RelListNode {name: 'a'})-[rels:NEXT*1]->
                      (:RelListNode {name: 'b'}) \
         RETURN rels",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "relationship-list name collisions must fail semantically: {query}"
        );
    }
}

#[test]
fn same_clause_rematch_denotes_the_same_relationships_without_a_uniqueness_conflict() {
    let mut database = linear_graph();

    // The second occurrence of `rels` must resolve to the same sequence, so
    // `other` always equals `end`. If the re-match took part in relationship
    // uniqueness the query would return no rows at all.
    let result = database
        .execute(
            "MATCH (start:RelListNode)-[rels:NEXT*1..2]->(end), \
                   (start)-[rels:NEXT*1..2]->(other) \
             RETURN start.name AS start, end.name AS end, other.name AS other \
             ORDER BY start, end",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![text("a"), text("b"), text("b")],
            vec![text("a"), text("c"), text("c")],
            vec![text("b"), text("c"), text("c")],
            vec![text("b"), text("d"), text("d")],
            vec![text("c"), text("d"), text("d")],
        ]
    );

    // A third, fresh traversal in the same clause still cannot reuse the
    // relationships the bound list already holds.
    let result = database
        .execute(
            "MATCH (start:RelListNode {name: 'a'})-[rels:NEXT*1]->(end), \
                   (start)-[rels:NEXT*1]->(other), \
                   (other)-[more:NEXT*1..2]->(tail) \
             RETURN end.name AS end, tail.name AS tail ORDER BY tail",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![text("b"), text("c")], vec![text("b"), text("d")]]
    );
}

#[test]
fn prebound_relationship_lists_match_exactly_after_with() {
    let mut database = linear_graph();
    let bound = "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1..2]->() WITH rels ";

    // Both the SQLite-recursive and Rust-backed forms accept the bound list
    // and match only the endpoints of that exact sequence.
    for pattern in ["(x)-[rels:NEXT*1..3]->(y)", "(x)-[rels:NEXT*]->(y)"] {
        let result = database
            .execute(&format!(
                "{bound} MATCH {pattern} RETURN x.name AS x, y.name AS y ORDER BY y"
            ))
            .unwrap();
        assert_eq!(
            result.rows,
            [vec![text("a"), text("b")], vec![text("a"), text("c")]],
            "{pattern}"
        );
    }

    // The re-matched pattern keeps its own direction, bounds, and type filters.
    let reversed = database
        .execute(&format!(
            "{bound} MATCH (x)<-[rels:NEXT*]-(y) RETURN x.name AS x, y.name AS y"
        ))
        .unwrap();
    assert_eq!(reversed.rows, [vec![text("b"), text("a")]]);
    let single_hop = database
        .execute(&format!(
            "{bound} MATCH (x)-[rels:NEXT*1]->(y) RETURN x.name AS x, y.name AS y"
        ))
        .unwrap();
    assert_eq!(single_hop.rows, [vec![text("a"), text("b")]]);
    let wrong_type = database
        .execute(&format!(
            "{bound} MATCH (x)-[rels:OTHER*]->(y) RETURN x.name AS x, y.name AS y"
        ))
        .unwrap();
    assert!(wrong_type.rows.is_empty());

    // The variable still projects the original list after the re-match.
    let projected = database
        .execute(&format!(
            "{bound} MATCH ()-[rels:NEXT*]->(y) RETURN rels ORDER BY y.name"
        ))
        .unwrap();
    assert_eq!(
        projected
            .rows
            .iter()
            .map(|row| relationship_list_names(&row[0]))
            .collect::<Vec<_>>(),
        [vec!["ab"], vec!["ab", "bc"]]
    );
}

#[test]
fn prebound_lists_from_path_functions_and_shortest_forms_match_as_one_sequence() {
    let mut database = linear_graph();

    // A list projected from a multi-segment path compares as the
    // concatenation of its segments' relationships.
    let multi_segment = database
        .execute(
            "MATCH route = (:RelListNode {name: 'a'})-[:NEXT]->()-[:NEXT]->() \
             WITH relationships(route) AS rels \
             MATCH (x)-[rels*]->(y) RETURN x.name AS x, y.name AS y",
        )
        .unwrap();
    assert_eq!(multi_segment.rows, [vec![text("a"), text("c")]]);

    let shortest = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1..2]->() WITH rels \
             MATCH shortestPath((x)-[rels:NEXT*]->(y)) \
             RETURN x.name AS x, y.name AS y ORDER BY y",
        )
        .unwrap();
    assert_eq!(
        shortest.rows,
        [vec![text("a"), text("b")], vec![text("a"), text("c")]]
    );

    // A re-matched list bound by an earlier clause still takes part in
    // relationship uniqueness with the other traversals of its own clause.
    let with_other = database
        .execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1..2]->() WITH rels \
             MATCH (x)-[rels:NEXT*]->(y)-[other:NEXT*1..3]->(z) \
             RETURN y.name AS y, z.name AS z ORDER BY y, z",
        )
        .unwrap();
    assert_eq!(
        with_other.rows,
        [
            vec![text("b"), text("c")],
            vec![text("b"), text("d")],
            vec![text("c"), text("d")],
        ]
    );
}

#[test]
fn prebound_relationship_list_rematches_keep_the_existing_rejection_boundary() {
    let mut database = linear_graph();

    assert!(matches!(
        database.execute(
            "MATCH (start:RelListNode)-[rels:NEXT*1]->() \
             OPTIONAL MATCH (start)-[rels:NEXT*1]->(other) RETURN other"
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute(
            "MATCH (start:RelListNode)-[rels:NEXT*1]->(), (start)-[rels:NEXT*1]->(other) \
             SET other.seen = true RETURN other"
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(
        database
            .execute("MATCH (node:RelListNode) WHERE node.seen = true RETURN node")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn relationship_list_bindings_cannot_change_to_an_incompatible_kind() {
    let mut database = linear_graph();

    let query = "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(:RelListNode) \
         WITH rels \
         MATCH (rels) \
         RETURN rels";

    assert!(
        matches!(database.execute(query), Err(Error::Semantic(_))),
        "relationship-list kind changes must fail semantically: {query}"
    );
}

#[test]
fn relationship_lists_remain_rejected_in_unimplemented_value_contexts() {
    let mut database = linear_graph();

    for query in [
        "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels = [] AS same",
        "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels AS links ORDER BY links",
        "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels, count(*) AS paths",
        "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(:RelListNode) \
         RETURN collect(rels) AS lists",
        "OPTIONAL MATCH (:RelListNode)-[rels:NEXT*1]->(:RelListNode) \
         RETURN rels",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Unsupported(_))),
            "deferred relationship-list context must remain unsupported: {query}"
        );
    }
}

#[test]
fn writes_fed_by_relationship_list_bindings_are_rejected_without_side_effects() {
    let mut database = linear_graph();

    assert!(matches!(
        database.execute(
            "MATCH (:RelListNode {name: 'a'})-[rels:NEXT*1]->(:RelListNode) \
             CREATE (:RelListWriteSideEffect) \
             RETURN rels"
        ),
        Err(Error::Unsupported(_))
    ));

    let side_effects = database
        .execute("MATCH (node:RelListWriteSideEffect) RETURN node")
        .unwrap();
    assert!(side_effects.rows.is_empty());
}
