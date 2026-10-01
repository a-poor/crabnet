use crabnet::{CypherValue, Database, Error};
use serde_json::json;

fn linear_graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Step {rank: 1}), (b:Step {rank: 2}), (c:Step {rank: 3}), \
         (a)-[:NEXT {rank: 1}]->(b), (b)-[:NEXT {rank: 2}]->(c)",
    )
    .unwrap();
    db
}

#[test]
fn path_nodes_unwind_as_entities_without_duplicating_segment_junctions() {
    let mut db = linear_graph();
    let result = db
        .execute(
            "MATCH p = (a:Step {rank: 1})-[:NEXT]->()-[:NEXT]->() \
             UNWIND nodes(p) AS n MATCH (n) \
             RETURN n, a.rank, length(p) ORDER BY n.rank",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 3);
    for (index, row) in result.rows.iter().enumerate() {
        assert_eq!(
            row[0].as_node().unwrap().properties["rank"],
            CypherValue::Integer(index as i64 + 1)
        );
        assert_eq!(row[1], CypherValue::Integer(1));
        assert_eq!(row[2], CypherValue::Integer(2));
    }
}

#[test]
fn path_relationships_unwind_through_aliases_with_stored_endpoints() {
    let mut db = linear_graph();
    let result = db
        .execute(
            "MATCH p = (:Step {rank: 3})<-[:NEXT]-()<-[:NEXT]-() \
             WITH relationships(p) AS edges WITH edges AS links \
             UNWIND links AS edge MATCH (a)-[edge]->(b) \
             RETURN edge, a, b ORDER BY edge.rank",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    for (index, row) in result.rows.iter().enumerate() {
        let edge = row[0].as_relationship().unwrap();
        let source = row[1].as_node().unwrap();
        let target = row[2].as_node().unwrap();
        assert_eq!(edge.start_node_element_id, source.element_id);
        assert_eq!(edge.end_node_element_id, target.element_id);
        assert_eq!(
            source.properties["rank"],
            CypherValue::Integer(index as i64 + 1)
        );
        assert_eq!(
            target.properties["rank"],
            CypherValue::Integer(index as i64 + 2)
        );
    }
}

#[test]
fn zero_hop_segments_keep_one_junction_and_empty_relationship_lists_emit_no_rows() {
    let mut db = linear_graph();
    for pattern in [
        "(:Step {rank: 1})",
        "(:Step {rank: 1})-[:NEXT*0]->()",
        "(:Step {rank: 1})-[:NEXT*0..0]->()-[:NEXT*0]->()",
    ] {
        let query = format!("MATCH p = {pattern} UNWIND nodes(p) AS n RETURN n.rank");
        assert_eq!(
            db.execute(&query).unwrap().rows,
            [vec![CypherValue::Integer(1)]]
        );
        let query = format!("MATCH p = {pattern} UNWIND relationships(p) AS r RETURN r");
        assert!(db.execute(&query).unwrap().rows.is_empty());
    }
    for function in ["nodes", "relationships"] {
        let query = format!(
            "MATCH p = (:Missing)-[:NEXT]->() UNWIND {function}(p) AS member RETURN member"
        );
        assert!(db.execute(&query).unwrap().rows.is_empty());
    }
    let result = db.execute(
        "MATCH p = (:Step {rank: 1})-[:NEXT*0]->()-[:NEXT]->()-[:NEXT*0]->()-[:NEXT]->()-[:NEXT*0]->() \
         UNWIND nodes(p) AS n RETURN n.rank ORDER BY n.rank",
    ).unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(2)],
            vec![CypherValue::Integer(3)]
        ]
    );
}

#[test]
fn cycles_self_loops_and_parallel_edges_preserve_bag_multiplicity_and_identity() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Start {rank: 1}), (b {rank: 2}), (a)-[:LINK]->(b), (a)-[:LINK]->(b), (b)-[:BACK]->(a), (a)-[:LOOP]->(a)").unwrap();
    let result = db
        .execute(
            "UNWIND [1,1] AS duplicate MATCH p = (a:Start)-[:LINK]->()-[:BACK]->(a) \
         UNWIND nodes(p) AS n RETURN n.rank, count(*) ORDER BY n.rank",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(1), CypherValue::Integer(8)],
            vec![CypherValue::Integer(2), CypherValue::Integer(4)]
        ]
    );
    let result = db
        .execute("MATCH p = (:Start)-[:LOOP]->() UNWIND nodes(p) AS n RETURN n")
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0], result.rows[1]);
    let result = db.execute(
        "MATCH p = (:Start)-[:LINK]->() UNWIND relationships(p) AS r RETURN collect(r), collect(DISTINCT r)",
    ).unwrap();
    for value in &result.rows[0] {
        let CypherValue::List(edges) = value else {
            panic!("expected list")
        };
        assert_eq!(edges.len(), 2);
        assert_ne!(
            edges[0].as_relationship().unwrap().element_id,
            edges[1].as_relationship().unwrap().element_id
        );
    }
}

#[test]
fn bounded_runtime_and_shortest_producers_support_both_functions_and_legacy_lists() {
    let mut db = linear_graph();
    for pattern in [
        "p = (:Step {rank: 1})-[rels:NEXT*2]->(:Step {rank: 3})",
        "p = (:Step {rank: 1})-[rels:NEXT*1..40]->(:Step {rank: 3})",
        "p = (:Step {rank: 1})-[rels:NEXT*]->(:Step {rank: 3})",
        "p = shortestPath((:Step {rank: 1})-[rels:NEXT*]->(:Step {rank: 3}))",
        "p = allShortestPaths((:Step {rank: 1})-[rels:NEXT*]->(:Step {rank: 3}))",
        "p = (:Step {rank: 1})-[:NEXT]->()-[rels:NEXT*]->(:Step {rank: 3})",
    ] {
        let query = format!(
            "MATCH {pattern} WITH p AS route, nodes(p) AS ns, relationships(p) AS rs, rels \
             WITH * UNWIND ns AS n RETURN n.rank ORDER BY n.rank"
        );
        assert_eq!(
            db.execute(&query).unwrap().rows,
            [
                vec![CypherValue::Integer(1)],
                vec![CypherValue::Integer(2)],
                vec![CypherValue::Integer(3)]
            ],
            "{pattern}"
        );
        let query =
            format!("MATCH {pattern} UNWIND relationships(p) AS r RETURN r.rank ORDER BY r.rank");
        assert_eq!(
            db.execute(&query).unwrap().rows,
            [vec![CypherValue::Integer(1)], vec![CypherValue::Integer(2)]],
            "{pattern}"
        );
        let query = format!(
            "MATCH {pattern} WITH rels AS links UNWIND links AS r RETURN r.rank ORDER BY r.rank"
        );
        let expected = if pattern.contains("-[:NEXT]->()") {
            vec![vec![CypherValue::Integer(2)]]
        } else {
            vec![vec![CypherValue::Integer(1)], vec![CypherValue::Integer(2)]]
        };
        assert_eq!(db.execute(&query).unwrap().rows, expected, "{pattern}");
    }
}

#[test]
fn unwound_entities_feed_parameterized_optional_and_runtime_matches() {
    let mut db = linear_graph();
    let query = "MATCH p = (:Step {rank: $start})-[:NEXT*2]->() \
                 UNWIND nodes(p) AS n OPTIONAL MATCH (n)-[r:NEXT]->(next) \
                 RETURN n.rank, next.rank, r.rank ORDER BY n.rank";
    assert!(matches!(db.execute(query), Err(Error::MissingParameter(_))));
    let result = db
        .execute_with(query, &[("start".into(), json!(1))].into())
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::Integer(1),
                CypherValue::Integer(2),
                CypherValue::Integer(1)
            ],
            vec![
                CypherValue::Integer(2),
                CypherValue::Integer(3),
                CypherValue::Integer(2)
            ],
            vec![
                CypherValue::Integer(3),
                CypherValue::Null,
                CypherValue::Null
            ],
        ]
    );
    let result = db
        .execute(
            "MATCH p = (:Step {rank: 1})-[:NEXT*2]->() UNWIND nodes(p) AS n \
         MATCH (n)-[:NEXT*1..]->(target) RETURN n.rank, target.rank ORDER BY n.rank, target.rank",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(1), CypherValue::Integer(2)],
            vec![CypherValue::Integer(1), CypherValue::Integer(3)],
            vec![CypherValue::Integer(2), CypherValue::Integer(3)],
        ]
    );
    let result = db
        .execute(
            "MATCH p = (:Step {rank: 1})-[:NEXT*2]->() \
         UNWIND nodes(p) AS n UNWIND relationships(p) AS r \
         WITH n, collect(DISTINCT r) AS edges UNWIND edges AS edge \
         RETURN n.rank, edge.rank ORDER BY n.rank, edge.rank",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 6);
    for (index, row) in result.rows.iter().enumerate() {
        assert_eq!(
            row,
            &[
                CypherValue::Integer(index as i64 / 2 + 1),
                CypherValue::Integer(index as i64 % 2 + 1)
            ]
        );
    }
}

#[test]
fn distinct_pagination_union_and_path_hydration_compose_with_unwind() {
    let mut db = linear_graph();
    let result = db
        .execute(
            "UNWIND [1,1] AS duplicate MATCH p = (:Step {rank: 1})-[:NEXT*2]->() \
         WITH DISTINCT nodes(p) AS ns SKIP 0 LIMIT 1 UNWIND ns AS n \
         UNWIND [1,1] AS copy WITH DISTINCT n ORDER BY n.rank SKIP 1 LIMIT 1 RETURN n.rank",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![CypherValue::Integer(2)]]);
    let query = "MATCH p = (:Step {rank: 1})-[:NEXT*2]->() UNWIND nodes(p) AS n RETURN n \
                 UNION MATCH (n:Step) RETURN n";
    assert_eq!(db.execute(query).unwrap().rows.len(), 3);
    assert_eq!(
        db.execute(&query.replace("UNION", "UNION ALL"))
            .unwrap()
            .rows
            .len(),
        6
    );
    let query = "MATCH p = (:Step {rank: 1})-[:NEXT*]->(:Step {rank: 3}) \
                 UNWIND nodes(p) AS n RETURN p, nodes(p), relationships(p), n ORDER BY n.rank";
    let result = db.execute(query).unwrap();
    assert_eq!(result.rows.len(), 3);
    for (index, row) in result.rows.iter().enumerate() {
        let path = row[0].as_path().unwrap();
        assert_eq!(row[3].as_node().unwrap(), &path.nodes[index]);
        assert_eq!(
            row[1],
            CypherValue::List(path.nodes.iter().cloned().map(CypherValue::Node).collect())
        );
        assert_eq!(
            row[2],
            CypherValue::List(
                path.relationships
                    .iter()
                    .cloned()
                    .map(CypherValue::Relationship)
                    .collect()
            )
        );
    }
    let explanation = crabnet::explain_physical(query).unwrap();
    assert_eq!(explanation.matches("batch-hydrate-paths").count(), 1);
}

#[test]
fn path_unwind_keeps_scope_kind_and_write_rejections_without_side_effects() {
    let mut db = linear_graph();
    for query in [
        "MATCH p = ()-[:NEXT]->() UNWIND nodes(p) AS p RETURN p",
        "MATCH p = ()-[:NEXT]->() WITH nodes(p) AS ns UNWIND nodes(p) AS n RETURN n",
        "MATCH (n) UNWIND nodes(n) AS member RETURN member",
        "MATCH p = ()-[:NEXT]->() UNWIND relationships(p) AS r MATCH (r) RETURN r",
        "MATCH p = ()-[:NEXT]->() UNWIND nodes(p) AS n RETURN type(n)",
        "MATCH p = ()-[:NEXT]->() UNWIND nodes(p)[0] AS n RETURN n",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    for query in [
        "MATCH p = ()-[:NEXT]->() UNWIND nodes(p) AS n SET n.changed = true RETURN n",
        "CREATE (:Failed) WITH 1 AS seed MATCH p = ()-[:NEXT]->() UNWIND relationships(p) AS r DELETE r",
        "OPTIONAL MATCH p = (:Missing)-[:NEXT]->() UNWIND nodes(p) AS n RETURN n",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::Unsupported(_))),
            "{query}"
        );
    }
    assert_eq!(
        db.execute("MATCH (n:Step) RETURN count(n), count(n.changed)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(3), CypherValue::Integer(0)]]
    );
    assert_eq!(
        db.execute("MATCH (n:Failed) RETURN count(n)").unwrap().rows,
        [vec![CypherValue::Integer(0)]]
    );
    assert_eq!(
        db.execute("MATCH ()-[r:NEXT]->() RETURN count(r)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(2)]]
    );
}
