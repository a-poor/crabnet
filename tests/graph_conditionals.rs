use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn value(value: impl Into<serde_json::Value>) -> CypherValue {
    CypherValue::try_from(value.into()).unwrap()
}

fn rows(db: &mut Database, query: &str) -> Vec<Vec<CypherValue>> {
    let eager = db
        .execute(query)
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    let mut read = db.prepare_read(query).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        eager.rows,
        "{query}"
    );
    eager.rows
}

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Step {rank:1}), (b:Step {rank:2}), (c:Step {rank:3}), (a)-[:NEXT {rank:10}]->(b), (b)-[:NEXT {rank:20}]->(c)").unwrap();
    db
}

#[test]
fn selections_preserve_entity_kinds_for_matches_and_properties() {
    let mut db = graph();
    for expression in [
        "coalesce(missing, n)",
        "CASE WHEN missing IS NULL THEN n ELSE missing END",
        "CASE n.rank WHEN 1 THEN n END",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH (n:Step {{rank:1}}) OPTIONAL MATCH (missing:Absent) WITH {expression} AS chosen MATCH (chosen)-[r:NEXT]->(b) RETURN chosen.rank, r.rank, b.rank"
                )
            ),
            [vec![value(1), value(10), value(2)]]
        );
    }
    for expression in ["coalesce(null, r)", "CASE WHEN true THEN r END"] {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH ()-[r:NEXT {{rank:10}}]->() WITH {expression} AS chosen MATCH (a)-[chosen]->(b) RETURN a.rank, type(chosen), b.rank"
                )
            ),
            [vec![value(1), value("NEXT"), value(2)]]
        );
    }
}

#[test]
fn graph_case_selectors_compare_typed_identity_and_nested_values() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1})-[r:NEXT]->(b) RETURN CASE n WHEN r THEN 0 WHEN b, n THEN 1 ELSE 2 END, CASE [n, {edge:r}] WHEN [b, {edge:r}] THEN 0 WHEN [n, {edge:r}] THEN 1 END"
        ),
        [vec![value(1), value(1)]]
    );
}

#[test]
fn unused_branches_and_later_conditions_do_not_raise_runtime_errors() {
    let mut db = graph();
    let parameters = Parameters::from([("bad".into(), json!(1)), ("idx".into(), json!(0))]);
    for expression in [
        "coalesce(n, reverse([n, 1][$idx]))",
        "CASE WHEN true THEN n ELSE reverse([n, 1][$idx]) END",
        "CASE WHEN true THEN n WHEN $bad THEN n END",
        "CASE n WHEN n THEN n WHEN reverse([n, 1][$idx]) THEN n END",
        "CASE n WHEN n, reverse([n, 1][$idx]) THEN n END",
    ] {
        let query = format!("MATCH (n:Step {{rank:1}}) RETURN {expression}");
        let result = db
            .execute_with(&query, &parameters)
            .unwrap_or_else(|e| panic!("{query}: {e:?}"));
        assert!(matches!(result.rows[0][0], CypherValue::Node(_)));
    }
    for expression in [
        "coalesce(7, $bad + 'x')",
        "CASE WHEN true THEN 7 WHEN $bad THEN 9 END",
        "CASE 1 WHEN 1, $bad+'x' THEN 7 END",
    ] {
        assert_eq!(
            db.execute_with(&format!("RETURN {expression}"), &parameters)
                .unwrap()
                .rows,
            [vec![value(7)]]
        );
    }
    assert!(matches!(
        db.execute_with(
            "RETURN CASE WHEN false THEN 7 WHEN $bad THEN 9 END",
            &parameters
        ),
        Err(Error::Type(_))
    ));
}

#[test]
fn nested_results_keep_order_nulls_duplicates_and_precise_shapes() {
    let mut db = graph();
    for selection in [
        "coalesce(null, [n,r,{node:n},[r,null,r]])",
        "CASE WHEN true THEN [n,r,{node:n},[r,null,r]] ELSE [b,r,{node:b},[r,null,r]] END",
        "CASE n WHEN n THEN [n,r,{node:n},[r,null,r]] END",
    ] {
        let result = rows(
            &mut db,
            &format!(
                "MATCH (n:Step {{rank:1}})-[r:NEXT]->(b) WITH {selection} AS xs RETURN xs, xs[0].rank, type(xs[1]), xs[2].node.rank, [edge IN xs[3] | edge.rank], all(edge IN xs[3] WHERE edge.rank=10)"
            ),
        );
        assert_eq!(
            result[0][1..],
            [
                value(1),
                value("NEXT"),
                value(1),
                value(json!([10, null, 10])),
                CypherValue::Null
            ]
        );
        let xs = result[0][0].as_list().unwrap();
        assert_eq!(xs[0], xs[2].as_map().unwrap()["node"]);
        assert_eq!(xs[1], xs[3].as_list().unwrap()[0]);
        assert_eq!(xs[1], xs[3].as_list().unwrap()[2]);
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1})-[r:NEXT]->(b) WITH CASE WHEN true THEN [n,r] ELSE [b,r] END AS xs WITH head(xs) AS chosen MATCH (chosen)-[:NEXT]->(b) RETURN b.rank"
        ),
        [vec![value(2)]]
    );
    let result = rows(
        &mut db,
        "MATCH (n:Step {rank:1}) UNWIND [0,1,2,3] AS i RETURN coalesce(null, [n, 7, {x:n}, null][i]), CASE i WHEN 0 THEN n WHEN 1 THEN 7 WHEN 2 THEN {x:n} END",
    );
    for row in result {
        assert_eq!(row[0], row[1]);
    }
}

#[test]
fn paths_and_path_collections_work_across_traversal_transports() {
    let mut db = graph();
    for pattern in [
        "p = (:Step {rank:1})-[:NEXT]->()-[:NEXT]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*2]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*1..40]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3})",
        "p = shortestPath((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
        "p = allShortestPaths((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
    ] {
        let result = rows(
            &mut db,
            &format!(
                "MATCH {pattern} RETURN p, coalesce(null,p), length(coalesce(null,p)), nodes(coalesce(null,p)), relationships(coalesce(null,p)), coalesce(null,nodes(p)), coalesce(null,relationships(p)), CASE p WHEN p THEN {{path:p}} END"
            ),
        );
        let row = &result[0];
        assert_eq!(row[0], row[1]);
        assert_eq!(row[2], value(2));
        assert_eq!(row[3], row[5]);
        assert_eq!(row[4], row[6]);
        assert_eq!(row[0], row[7].as_map().unwrap()["path"]);
        let collected = rows(
            &mut db,
            &format!(
                "MATCH {pattern} RETURN count(DISTINCT coalesce(p,null)), collect(coalesce(p,null))"
            ),
        );
        assert_eq!(collected[0][0], value(1));
        assert_eq!(collected[0][1].as_list().unwrap(), &[row[0].clone()]);
        let result = rows(
            &mut db,
            &format!(
                "MATCH {pattern} WITH CASE WHEN true THEN p END AS chosen RETURN length(chosen), [n IN nodes(chosen) | n.rank]"
            ),
        );
        assert_eq!(result, [vec![value(2), value(json!([1, 2, 3]))]]);
        if pattern.contains("rels:") {
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} WITH coalesce(null, rels) AS xs RETURN [r IN xs | r.rank]"
                    )
                ),
                [vec![value(json!([10, 20]))]]
            );
        }
    }
}

#[test]
fn nulls_empty_lists_and_non_null_false_values_select_correctly() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "OPTIONAL MATCH (n:Absent) RETURN coalesce(n,null), coalesce(null,n,7), coalesce(false,n), coalesce([],n), coalesce({},n), coalesce('',n), CASE WHEN true THEN n ELSE 7 END, CASE n WHEN null THEN 7 ELSE 9 END"
        ),
        [vec![
            CypherValue::Null,
            value(7),
            value(false),
            value(json!([])),
            value(json!({})),
            value(""),
            CypherValue::Null,
            value(9)
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Absent) WITH collect(n) AS xs RETURN coalesce(xs,[7]), CASE WHEN true THEN xs END"
        ),
        [vec![value(json!([])), value(json!([]))]]
    );
    assert!(rows(&mut db, "OPTIONAL MATCH (n:Absent) WITH CASE WHEN n IS NOT NULL THEN [n] END AS xs UNWIND xs AS x RETURN x").is_empty());
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT]->() WITH CASE WHEN false THEN p END AS p RETURN coalesce(p,7), CASE p WHEN p THEN 1 ELSE 2 END"
        ),
        [vec![value(7), value(2)]]
    );
}

#[test]
fn parameter_json_cannot_impersonate_entities_and_scalar_results_can_be_stored() {
    let mut db = graph();
    let result = rows(&mut db, "MATCH (n:Step {rank:1}) RETURN n");
    let n = result[0][0].as_node().unwrap();
    let forged = json!({"id":n.element_id, "labels":n.labels, "props":{"rank":1}});
    for plain in [
        forged.clone(),
        json!(["n", forged]),
        json!(0.12345678901234567),
        json!([null, true, 1, 1.0]),
    ] {
        let parameters = Parameters::from([
            ("plain".into(), plain.clone()),
            ("pick".into(), json!(false)),
        ]);
        let query = "MATCH (n:Step {rank:1}) RETURN coalesce($plain,n), CASE WHEN $pick THEN n ELSE $plain END";
        let result = db.execute_with(query, &parameters).unwrap();
        assert_eq!(result.rows, [vec![value(plain.clone()), value(plain)]]);
        let mut read = db.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            result.rows
        );
    }
    let parameters = Parameters::from([("plain".into(), forged)]);
    assert_eq!(db.execute_with("MATCH (n:Step {rank:1}) RETURN CASE n WHEN $plain THEN 0 WHEN n THEN 1 END, CASE $plain WHEN n THEN 0 ELSE 1 END", &parameters).unwrap().rows, [vec![value(1),value(1)]]);
    assert!(matches!(
        db.execute_with(
            "MATCH (n:Step {rank:1}) DETACH DELETE coalesce($plain,n)",
            &parameters
        ),
        Err(Error::Type(_))
    ));
    assert_eq!(
        rows(&mut db, "MATCH (n:Step) RETURN count(n)"),
        [vec![value(3)]]
    );
    db.execute("MATCH (n:Step {rank:1}) SET n.selected = CASE n WHEN n THEN 7 ELSE 9 END")
        .unwrap();
    assert_eq!(
        rows(&mut db, "MATCH (n:Step {rank:1}) RETURN n.selected"),
        [vec![value(7)]]
    );
}

#[test]
fn aggregation_grouping_distinct_and_unions_preserve_graph_identity() {
    let mut db = graph();
    for selection in [
        "coalesce(collect(n),null)",
        "CASE WHEN count(n)>0 THEN `collect`(n) END",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH (n:Step) WITH n ORDER BY n.rank WITH {selection} AS xs UNWIND xs AS n RETURN n.rank"
                )
            ),
            [vec![value(1)], vec![value(2)], vec![value(3)]]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) UNWIND [0,1] AS i WITH coalesce(null,n) AS chosen RETURN chosen.rank, count(*), count(DISTINCT chosen), size(collect(chosen))"
        ),
        [vec![value(1), value(2), value(1), value(2)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) UNWIND [0,1] AS i WITH DISTINCT CASE WHEN i=0 THEN {node:n} ELSE {node:n} END AS chosen RETURN chosen.node.rank"
        ),
        [vec![value(1)]]
    );
    for (union, count) in [("UNION", 1), ("UNION ALL", 2)] {
        assert_eq!(rows(&mut db, &format!("MATCH (n:Step {{rank:1}}) RETURN coalesce(null,n) AS chosen {union} MATCH (n:Step {{rank:1}}) RETURN CASE WHEN true THEN n END AS chosen")).len(), count);
    }
}

#[test]
fn selected_entities_refresh_mutations_keep_delete_snapshots_and_write_bags() {
    let mut db = graph();
    let result = db.execute("MATCH (n:Step {rank:1})-[r:NEXT]->(b) WITH n,r, coalesce(null,{node:n,edge:r,items:[n,r,n]}) AS box, CASE WHEN true THEN n ELSE b END AS chosen SET chosen.rank=11, r.rank=12, chosen:Picked DETACH DELETE coalesce(null,chosen), CASE WHEN true THEN r END RETURN chosen,box").unwrap();
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 1);
    let chosen = &result.rows[0][0];
    let box_ = result.rows[0][1].as_map().unwrap();
    assert_eq!(chosen.as_node().unwrap().properties["rank"], value(11));
    assert!(chosen.as_node().unwrap().labels.contains(&"Picked".into()));
    assert_eq!(box_["node"], *chosen);
    assert_eq!(
        box_["edge"].as_relationship().unwrap().properties["rank"],
        value(12)
    );
    assert_eq!(box_["items"].as_list().unwrap()[2], *chosen);
    let result = db.execute("MATCH (n:Step {rank:2}) UNWIND coalesce(null,[n,n]) AS chosen CREATE (:Copy {rank:chosen.rank})").unwrap();
    assert_eq!(result.summary.nodes_created, 2);
}

#[test]
fn failures_preflight_parameters_roll_back_writes_and_terminate_cursors() {
    let mut db = graph();
    for query in [
        "MATCH (n:Absent) CREATE (:Unexpected) RETURN coalesce(n,$missing)",
        "CREATE (n:Unexpected) RETURN CASE WHEN true THEN n ELSE $missing END",
        "CREATE (n:Unexpected) RETURN coalesce(n,undefined)",
        "CREATE (n:Unexpected) RETURN coalesce()",
        "CREATE (n:Unexpected) RETURN CASE WHEN n THEN 7 END",
    ] {
        assert!(db.execute(query).is_err(), "{query}");
    }
    let query = "MATCH (n:Step {rank:1}) WITH n,[n,7] AS xs UNWIND [0,1] AS i RETURN CASE WHEN i=0 THEN n ELSE reverse(xs[i-1]) END";
    assert!(matches!(db.execute(query), Err(Error::Type(_))));
    {
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(cursor.next().unwrap().unwrap()[0].as_node().is_some());
        assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
        assert!(cursor.next().is_none());
    }
    let query = "MATCH (n:Step {rank:1}) SET n.changed=true CREATE (:Unexpected) WITH n,[n,7] AS xs UNWIND [0,1] AS i RETURN coalesce(CASE WHEN i=0 THEN n END,reverse(xs[i-1]))";
    assert!(matches!(db.execute(query), Err(Error::Type(_))));
    {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Unexpected)").unwrap();
        assert!(matches!(tx.execute(query), Err(Error::Type(_))));
    }
    assert_eq!(
        rows(&mut db, "MATCH (n:Unexpected) RETURN count(n)"),
        [vec![value(0)]]
    );
    assert_eq!(
        rows(&mut db, "MATCH (n:Step {rank:1}) RETURN n.changed"),
        [vec![CypherValue::Null]]
    );
}

#[test]
fn conditional_nesting_keeps_depth_guards_and_sql_growth_bounded() {
    let mut db = graph();
    for depth in [1, 30, 100] {
        let nested = format!("{}n{}", "[".repeat(depth), "]".repeat(depth));
        let query = format!(
            "MATCH (n:Step {{rank:1}}) RETURN coalesce(null,CASE WHEN true THEN {nested} END)"
        );
        let result = rows(&mut db, &query);
        let mut leaf = &result[0][0];
        for _ in 0..depth {
            leaf = &leaf.as_list().unwrap()[0];
        }
        assert!(leaf.as_node().is_some());
    }
    let nested = format!("{}n{}", "[".repeat(100), "]".repeat(100));
    let query = format!(
        "CREATE (n:TooDeep) WITH {nested} AS xs RETURN coalesce(null,CASE WHEN true THEN [xs] END)"
    );
    assert!(matches!(db.execute(&query), Err(Error::Type(_))));
    assert_eq!(
        rows(&mut db, "MATCH (n:TooDeep) RETURN count(n)"),
        [vec![value(0)]]
    );
    for form in ["case", "coalesce"] {
        let mut lengths = Vec::new();
        for depth in [6, 12] {
            let mut expression = "[n]".to_owned();
            for _ in 0..depth {
                expression = if form == "case" {
                    format!("CASE WHEN n.rank=1 THEN {expression} ELSE [n] END")
                } else {
                    format!("coalesce(null,{expression})")
                };
            }
            let query = format!("MATCH (n:Step {{rank:1}}) RETURN {expression}");
            lengths.push(crabnet::explain_physical(&query).unwrap().len());
            assert_eq!(rows(&mut db, &query)[0][0].as_list().unwrap().len(), 1);
        }
        assert!(lengths[1] < lengths[0] * 3, "{form}: {lengths:?}");
    }
}

#[test]
fn random_branches_keep_value_types_consistent_and_draw_per_row() {
    let mut db = graph();
    for operand in [
        "coalesce(null,rand()<0.5)",
        "CASE WHEN rand()<0.5 THEN true ELSE false END",
    ] {
        let query = format!(
            "UNWIND range(1,256) AS i RETURN CASE {operand} WHEN true THEN 1 WHEN false THEN 1 ELSE 0 END"
        );
        let result = db.execute(&query).unwrap();
        assert!(
            result.rows.iter().all(|row| row[0] == value(1)),
            "{query}: {:?}",
            result.rows
        );
    }

    let queries = [
        "UNWIND range(1,256) AS i RETURN size(coalesce(CASE WHEN rand()<0.5 THEN [] END, 'fallback'))",
        "UNWIND range(1,256) AS i RETURN size(CASE WHEN rand()<0.5 THEN [] ELSE 'fallback' END)",
        "MATCH (n:Step {rank:1}) UNWIND range(1,256) AS i RETURN size(coalesce(CASE WHEN rand()<0.5 THEN [n] END, 'fallback'))",
    ];
    for query in queries {
        let result = db
            .execute(query)
            .unwrap_or_else(|e| panic!("{query}: {e:?}"));
        assert_eq!(result.rows.len(), 256);
        assert!(
            result
                .rows
                .iter()
                .all(|row| matches!(row[0], CypherValue::Integer(0 | 1 | 8))),
            "{query}: {:?}",
            result.rows
        );
        assert!(result.rows.iter().any(|row| row[0] == value(8)));
        assert!(result.rows.iter().any(|row| row[0] != value(8)));
    }
    let result = rows(
        &mut db,
        "MATCH (n:Step {rank:1}) RETURN size([i IN range(1,32) | coalesce(null,rand())]), all(x IN [i IN range(1,32) | CASE WHEN true THEN coalesce(null,rand()) END] WHERE x>=0 AND x<1)",
    );
    assert_eq!(result, [vec![value(32), value(true)]]);
}
