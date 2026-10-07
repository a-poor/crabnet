use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions};
use serde_json::json;

#[test]
fn indexed_nodes_and_map_relationships_are_deleted_with_snapshots() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Target {name:'gone'})-[:LINK {weight:7}]->(:Kept)")
        .unwrap();
    let result = db
        .execute(
            "MATCH (n:Target)-[r:LINK]->() \
             WITH collect(n) AS nodes, {edge:r} AS box \
             DELETE nodes[0], box.edge \
             RETURN nodes[0], box.edge",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 1);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0][0].as_node().unwrap().properties["name"],
        CypherValue::String("gone".into())
    );
    assert_eq!(
        result.rows[0][1].as_relationship().unwrap().properties["weight"],
        CypherValue::Integer(7)
    );
    assert_eq!(
        db.execute("MATCH (n) RETURN labels(n)").unwrap().rows,
        [vec![json!(["Kept"])]]
    );
}

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Target {rank:1})-[:LINK {weight:7}]->(:Kept {rank:2})")
        .unwrap();
    db
}

fn assert_graph_intact(db: &mut Database) {
    assert_eq!(
        db.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![json!(2)]]
    );
    assert_eq!(
        db.execute("MATCH (n:Target)-[r:LINK]->(m:Kept) RETURN n.rank, r.weight, m.rank")
            .unwrap()
            .rows,
        [vec![json!(1), json!(7), json!(2)]]
    );
}

#[test]
fn mixed_dynamic_targets_deduplicate_across_columns_and_the_input_bag() {
    let mut db = graph();
    let params = Parameters::from([("key".into(), json!("items"))]);
    let result = db
        .execute_with(
            "MATCH (n:Target)-[r]->() \
         WITH n, r, {items:[n,r,null]} AS box \
         UNWIND [0,1,0,2,1] AS i \
         DELETE n, box[$key][i], r, box.items[0] \
         RETURN i, box.items[0].rank, box.items[1].weight ORDER BY i",
            &params,
        )
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 1);
    assert_eq!(
        result.rows,
        [0, 0, 1, 1, 2].map(|i| vec![json!(i), json!(1), json!(7)])
    );
    assert_eq!(
        db.execute("MATCH (n) RETURN n.rank").unwrap().rows,
        [vec![json!(2)]]
    );
}

#[test]
fn detach_targets_include_shared_edges_self_loops_and_explicit_relationships_once() {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Target {rank:1}), (b:Target {rank:2}), (k:Kept), \
         (a)-[:LINK]->(b), (b)-[:LINK]->(a), (a)-[:LOOP]->(a), \
         (k)-[:LINK]->(a), (b)-[:LINK]->(k)",
    )
    .unwrap();
    let result = db
        .execute(
            "MATCH (n:Target)-[r]->() \
         WITH {nodes:collect(n), edges:collect(r)} AS box \
         UNWIND box.nodes AS n \
         DETACH DELETE {victim:n}.victim, box.edges[0] \
         RETURN n.rank AS rank ORDER BY rank",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 2);
    assert_eq!(result.summary.relationships_deleted, 5);
    assert_eq!(
        result.rows,
        [
            vec![json!(1)],
            vec![json!(1)],
            vec![json!(2)],
            vec![json!(2)]
        ]
    );
    assert_eq!(
        db.execute("MATCH (n) RETURN labels(n)").unwrap().rows,
        [vec![json!(["Kept"])]]
    );
}

#[test]
fn null_missing_and_out_of_range_targets_are_noops() {
    let mut db = graph();
    let params = Parameters::from([("target".into(), json!(null)), ("index".into(), json!(99))]);
    for query in [
        "MATCH (n:Target) WITH [n] AS ns DELETE ns[$index], ns[-2], ns[null]",
        "MATCH (n:Target) WITH {node:n} AS box DELETE box.absent, box[$target]",
        "OPTIONAL MATCH (n:Absent) DELETE [n][0], {node:n}.node RETURN n",
        "WITH null AS target DELETE target, null, [null][0], $target",
        "MATCH (n:Target) DELETE n.absent",
        "MATCH (n:Absent) WITH [n,42] AS values DELETE values[1]",
    ] {
        let result = db
            .execute_with(query, &params)
            .unwrap_or_else(|e| panic!("{query}: {e:?}"));
        assert_eq!(result.summary.nodes_deleted, 0, "{query}");
        assert_eq!(result.summary.relationships_deleted, 0, "{query}");
    }
    assert_graph_intact(&mut db);
    let result = db
        .execute("MATCH (n:Target) WITH collect(n) AS ns DETACH DELETE ns[-1] RETURN ns[-1].rank")
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.rows, [vec![json!(1)]]);
}

#[test]
fn target_expressions_see_prior_mutations_and_keep_refreshed_snapshots() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Target)-[r]->(m:Kept) \
         WITH n,r,{nested:{nodes:[n,m], edge:r}} AS box \
         SET n.pick=0, n.rank=9, n:Changed, r.weight=11 \
         CREATE (:Created) \
         DELETE box.nested.nodes[n.pick], box.nested.edge \
         RETURN box.nested.nodes[0], box.nested.edge, box.nested.nodes[1].rank",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_created, 1);
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 1);
    let node = result.rows[0][0].as_node().unwrap();
    assert_eq!(node.properties["rank"], json!(9));
    assert!(node.labels.contains(&"Changed".into()));
    assert_eq!(
        result.rows[0][1].as_relationship().unwrap().properties["weight"],
        json!(11)
    );
    assert_eq!(result.rows[0][2], json!(2));
    assert_eq!(
        db.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![json!(2)]]
    );
    let created = db
        .execute("CREATE (fresh:Transient {rank:3}) DELETE [fresh][0] RETURN fresh.rank")
        .unwrap();
    assert_eq!(created.summary.nodes_created, 1);
    assert_eq!(created.summary.nodes_deleted, 1);
    assert_eq!(created.rows, [vec![json!(3)]]);
}

#[test]
fn all_targets_are_evaluated_before_relationships_are_removed() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Target)-[r]->() \
         DELETE r, [x IN ([n]+[n])[..1] WHERE (x)-->() | x][0] \
         RETURN n.rank",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 1);
    assert_eq!(result.rows, [vec![json!(1)]]);
}

#[test]
fn invalid_known_targets_and_aggregates_fail_during_planning() {
    let mut db = graph();
    for target in [
        "42",
        "1+1",
        "true",
        "'node'",
        "[n]",
        "{node:n}",
        "collect(n)[0]",
        "count(n)",
    ] {
        let query = format!("MATCH (n:Target) CREATE (:Bad) DELETE {target}");
        assert!(
            matches!(crabnet::explain(&query), Err(Error::Semantic(_))),
            "{query}"
        );
        assert!(
            matches!(db.execute(&query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    for query in [
        "MATCH (n:Target) DELETE missing",
        "MATCH p=()-[]->() DELETE p",
        "MATCH p=()-[]->() DELETE {path:p}.path",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    assert!(matches!(
        db.execute("MATCH (n) DELETE [n][0] SET n.rank=0"),
        Err(Error::Unsupported(_))
    ));
    assert_graph_intact(&mut db);
}

#[test]
fn runtime_target_errors_roll_back_all_rows_and_earlier_writes() {
    let mut db = graph();
    for value in [
        json!(42),
        json!(true),
        json!("node"),
        json!([]),
        json!({"node":1}),
    ] {
        let params = Parameters::from([("bad".into(), value)]);
        // The first row and first target are valid, but the second row must
        // abort the whole statement, including preceding CREATE and SET.
        let query = "MATCH (n:Target)-[r]->() \
                     SET n.rank=99, r.weight=99 CREATE (:Bad) \
                     WITH n,r,[n,$bad] AS values UNWIND [0,1] AS i \
                     DETACH DELETE {edge:r}.edge, values[i]";
        let error = db.execute_with(query, &params).unwrap_err();
        assert!(matches!(error, Error::Type(_)), "{params:?}: {error:?}");
        assert_graph_intact(&mut db);
    }
    for query in [
        "MATCH (n:Target) CREATE (:Bad) DELETE n.rank",
        "MATCH (n:Target) CREATE (:Bad) WITH n,[n] AS ns DELETE ns[n.rank='wrong']",
        "MATCH (n:Target) CREATE (:Bad) WITH n,{node:n} AS box DELETE box[n.rank]",
    ] {
        assert!(matches!(db.execute(query), Err(Error::Type(_))), "{query}");
        assert_graph_intact(&mut db);
    }
    let error = db
        .execute("MATCH (n:Target) SET n.rank=99 CREATE (:Bad) DELETE [n][0]")
        .unwrap_err();
    assert!(matches!(error, Error::Constraint(_)));
    assert_graph_intact(&mut db);
}

#[test]
fn property_json_cannot_impersonate_a_deletion_target() {
    let mut db = graph();
    let id = db
        .execute("MATCH (n:Target) RETURN elementId(n)")
        .unwrap()
        .rows[0][0]
        .as_str()
        .unwrap()
        .to_owned();
    let forged = json!({"id":id,"labels":["Target"],"props":{"rank":1}});
    for value in [forged.clone(), json!(["n", forged]), json!(id)] {
        let params = Parameters::from([("forged".into(), value)]);
        for query in [
            "MATCH (n:Target) DETACH DELETE $forged",
            "MATCH (n:Target) WITH [n,$forged] AS values UNWIND [0,1] AS i DETACH DELETE values[i]",
            "MATCH (n:Target) SET n.fake=$forged DETACH DELETE n.fake",
        ] {
            assert!(
                matches!(db.execute_with(query, &params), Err(Error::Type(_))),
                "{query}"
            );
            assert_graph_intact(&mut db);
        }
    }
    assert_eq!(
        db.execute("MATCH (n:Target) RETURN n.fake").unwrap().rows,
        [vec![json!(null)]]
    );
}

#[test]
fn target_parameters_are_preflighted_and_errors_abort_application_transactions() {
    let mut db = graph();
    for query in [
        "MATCH (n:Absent) WITH collect(n) AS ns DELETE ns[$missing]",
        "CREATE (n:Bad) DELETE [n][$missing]",
        "MATCH (n:Target) DETACH DELETE {node:n}[$missing]",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::MissingParameter(_))),
            "{query}"
        );
        assert_graph_intact(&mut db);
    }
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Earlier)").unwrap();
    let params = Parameters::from([("index".into(), json!(1))]);
    assert!(matches!(
        tx.execute_with(
            "MATCH (n:Target) WITH [n,7] AS values DETACH DELETE values[$index]",
            &params
        ),
        Err(Error::Type(_))
    ));
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert_graph_intact(&mut db);
    let mut tx = db.transaction().unwrap();
    let result = tx
        .execute("MATCH (n:Target) DETACH DELETE {node:n}.node")
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 1);
    tx.rollback().unwrap();
    assert_graph_intact(&mut db);
}

#[test]
fn internal_targets_do_not_consume_result_budgets_and_output_errors_roll_back_deletion() {
    let mut db = graph();
    let options = QueryOptions {
        max_result_rows: Some(0),
        max_result_bytes: Some(0),
        ..QueryOptions::default()
    };
    let query = "MATCH (n:Target) DETACH DELETE [n][0] RETURN n";
    assert!(matches!(
        db.execute_with_options(query, &Parameters::new(), &options),
        Err(Error::ResourceLimit(_))
    ));
    assert_graph_intact(&mut db);
    let result = db
        .execute_with_options(
            "MATCH (n:Target) DETACH DELETE [n][0]",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 1);
}
