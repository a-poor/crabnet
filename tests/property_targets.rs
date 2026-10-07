use crabnet::{Database, Error, Parameters, QueryOptions};
use serde_json::json;

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (:Target {rank:1, old:true})-[:LINK {weight:7, old:true}]->(:Kept {rank:2})",
    )
    .unwrap();
    db
}

#[test]
fn parenthesized_nodes_and_relationships_accept_property_updates() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Target)-[r:LINK]->() SET (n).rank=3,(r).weight=9 \
         REMOVE (n).old,(r).old RETURN n.rank,r.weight,n.old,r.old",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![json!(3), json!(9), json!(null), json!(null)]]
    );
    assert_eq!(result.summary.properties_set, 4);
}

#[test]
fn list_and_nested_map_targets_update_entities_without_rebinding() {
    let mut db = graph();
    let result = db.execute(
        "MATCH (n:Target)-[r:LINK]->(m:Kept) \
         WITH collect(n) AS people,{nested:{edge:r, node:m}} AS box \
         SET (people[0]).rank=11,(box.nested.edge).weight=13 \
         REMOVE (people[-1]).old,(box.nested.edge).old \
         RETURN people[0].rank,box.nested.edge.weight,people[0].old,box.nested.edge.old,box.nested.node.rank"
    ).unwrap();
    assert_eq!(
        result.rows,
        [vec![
            json!(11),
            json!(13),
            json!(null),
            json!(null),
            json!(2)
        ]]
    );
    assert_eq!(result.summary.properties_set, 4);
}

#[test]
fn functions_conditionals_and_dynamic_selections_keep_entity_kinds() {
    for (target, key, expected) in [
        ("head([n])", "rank", 11),
        ("last([null,r])", "weight", 13),
        ("coalesce(null,{node:n}.node)", "rank", 11),
        ("CASE WHEN n.rank>0 THEN n ELSE null END", "rank", 11),
        ("CASE n WHEN n THEN r ELSE null END", "weight", 13),
        ("head([x IN [r] WHERE x.weight>=7 | x])", "weight", 13),
    ] {
        let mut db = graph();
        let query = format!(
            "MATCH (n:Target)-[r]->() SET ({target}).{key}={expected} REMOVE ({target}).old RETURN n.rank,r.weight"
        );
        let result = db
            .execute(&query)
            .unwrap_or_else(|e| panic!("{query}: {e:?}"));
        assert_eq!(result.summary.properties_set, 2, "{query}");
        assert_eq!(
            result.rows,
            [vec![
                json!(if key == "rank" { 11 } else { 1 }),
                json!(if key == "weight" { 13 } else { 7 })
            ]]
        );
    }
    let mut db = graph();
    let parameters = Parameters::from([("key".into(), json!("items"))]);
    let result = db
        .execute_with(
            "MATCH (n:Target)-[r]->() WITH n,r,{items:[n,r,null]} AS box \
         UNWIND [0,1,0,2] AS i SET (box[$key][i]).seen=i+10 \
         REMOVE (box[$key][i]).old \
         RETURN i,n.seen,r.seen,n.old,r.old ORDER BY i",
            &parameters,
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [0, 0, 1, 2].map(|i| vec![json!(i), json!(10), json!(11), json!(null), json!(null)])
    );
    assert_eq!(result.summary.properties_set, 4);
}

#[test]
fn null_missing_and_out_of_range_targets_are_noops() {
    let mut db = graph();
    let params = Parameters::from([("target".into(), json!(null)), ("index".into(), json!(99))]);
    for target in [
        "null",
        "$target",
        "[n][$index]",
        "[n][-2]",
        "[n][null]",
        "{node:n}.absent",
        "{node:n}[$target]",
        "n.absent",
    ] {
        let query = format!(
            "MATCH (n:Target) SET ({target}).rank=99 REMOVE ({target}).old RETURN n.rank,n.old"
        );
        let result = db
            .execute_with(&query, &params)
            .unwrap_or_else(|e| panic!("{query}: {e:?}"));
        assert_eq!(result.rows, [vec![json!(1), json!(true)]], "{query}");
        assert_eq!(result.summary.properties_set, 0, "{query}");
    }
    let result = db
        .execute("OPTIONAL MATCH (n:Absent) SET ([n][0]).x=1 REMOVE ({node:n}.node).x RETURN n")
        .unwrap();
    assert_eq!(result.rows, [vec![json!(null)]]);
    assert_eq!(result.summary.properties_set, 0);
    let result = db
        .execute("MATCH (n:Absent) WITH [n,42] AS xs SET (xs[1]).x=1 REMOVE (xs[1]).x")
        .unwrap();
    assert_eq!(result.summary.properties_set, 0);
}

#[test]
fn targets_and_values_are_frozen_per_item_and_duplicates_remain_updates() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:N {rank:1,pick:0}),(:N {rank:2,pick:99})")
        .unwrap();
    let result = db
        .execute(
            "MATCH (n:N) WITH n ORDER BY n.rank WITH collect(n) AS ns \
         UNWIND [1,2] AS v SET (ns[ns[0].pick]).pick=v, \
         (ns[ns[0].pick-1]).seen=ns[0].pick \
         RETURN ns[0].pick,ns[1].pick,ns[1].seen",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![json!(2), json!(99), json!(2)]; 2]);
    assert_eq!(result.summary.properties_set, 3);
    let result = db
        .execute(
            "MATCH (n:N {rank:1}) WITH n,[n] AS ns UNWIND [1,2,3] AS v \
         SET (ns[0]).pick=ns[0].pick+1, (ns[0]).seen=v \
         RETURN n.pick,ns[0].pick,ns[0].seen",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![json!(3), json!(3), json!(3)]; 3]);
    assert_eq!(result.summary.properties_set, 4);
    let result = db
        .execute(
            "MATCH (n:N) WITH n ORDER BY n.rank WITH collect(n) AS ns \
         UNWIND [0,1] AS copy \
         REMOVE (CASE WHEN ns[0].pick IS NOT NULL THEN ns[0] ELSE ns[1] END).pick \
         RETURN ns[0].pick,ns[1].pick",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![json!(null), json!(99)]; 2]);
    assert_eq!(result.summary.properties_set, 1);
}

#[test]
fn later_items_observe_updates_and_refresh_all_nested_copies() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Target)-[r]->(m:Kept) WITH n,r,[n,n] AS ns,{node:n,edge:r} AS box \
         SET (ns[0]).rank=9,(box.edge).weight=box.node.rank+1 \
         REMOVE (CASE WHEN box.edge.weight=10 THEN box.node ELSE null END).old \
         SET (box.node).rank=null \
         RETURN n,ns,box,r",
        )
        .unwrap();
    assert_eq!(result.summary.properties_set, 4);
    let row = &result.rows[0];
    assert!(row[0].as_node().unwrap().properties.is_empty());
    for n in row[1].as_list().unwrap() {
        assert_eq!(n, &row[0]);
    }
    let map = row[2].as_map().unwrap();
    assert_eq!(map["node"], row[0]);
    assert_eq!(map["edge"], row[3]);
    assert_eq!(
        row[3].as_relationship().unwrap().properties["weight"],
        json!(10)
    );
}

#[test]
fn computed_targets_work_in_create_merge_actions_and_delete_snapshots() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n) WITH n ORDER BY n.rank WITH {node:n} AS box \
         MERGE (m:Marker {key:1}) \
         ON CREATE SET (box.node).state='created',(m).fresh=true \
         ON MATCH SET (box.node).state='matched' \
         RETURN box.node.rank,box.node.state ORDER BY box.node.rank",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![json!(1), json!("created")],
            vec![json!(2), json!("matched")]
        ]
    );
    assert_eq!(result.summary.nodes_created, 1);
    let result = db
        .execute(
            "CREATE (n:Temporary {rank:1,old:true}) WITH {node:n} AS box \
         SET (box.node).rank=2 REMOVE (box.node).old \
         DELETE box.node RETURN box.node",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_created, 1);
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.properties_set, 2);
    assert_eq!(
        result.rows[0][0].as_node().unwrap().properties["rank"],
        json!(2)
    );
    assert!(
        !result.rows[0][0]
            .as_node()
            .unwrap()
            .properties
            .contains_key("old")
    );
}

fn assert_graph_intact(db: &mut Database) {
    assert_eq!(
        db.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![json!(2)]]
    );
    assert_eq!(
        db.execute("MATCH (n:Target)-[r:LINK]->(m:Kept) RETURN n.rank,n.old,r.weight,r.old,m.rank")
            .unwrap()
            .rows,
        [vec![json!(1), json!(true), json!(7), json!(true), json!(2)]]
    );
}

#[test]
fn invalid_known_targets_and_aggregates_fail_before_writes() {
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
        "missing",
    ] {
        for mutation in [
            format!("SET ({target}).rank=9"),
            format!("REMOVE ({target}).rank"),
        ] {
            let query = format!("MATCH (n:Target) CREATE (:Bad) {mutation}");
            assert!(
                matches!(crabnet::explain(&query), Err(Error::Semantic(_))),
                "{query}"
            );
            let error = db.execute(&query).unwrap_err();
            assert!(matches!(error, Error::Semantic(_)), "{query}: {error:?}");
            assert_graph_intact(&mut db);
        }
    }
    for query in [
        "MATCH p=()-[]->() SET (p).rank=1",
        "MATCH p=()-[]->() REMOVE ({path:p}.path).rank",
        "MATCH p=()-[]->() SET (nodes(p)[0]).rank=1",
        "MATCH p=()-[]->() REMOVE (relationships(p)[0]).rank",
        "MATCH (n:Target) DELETE n SET ([n][0]).rank=0",
    ] {
        assert!(
            matches!(
                db.execute(query),
                Err(Error::Semantic(_) | Error::Unsupported(_))
            ),
            "{query}"
        );
        assert_graph_intact(&mut db);
    }
}

#[test]
fn dynamic_target_and_value_errors_roll_back_previous_items_and_rows() {
    let mut db = graph();
    for bad in [
        json!(42),
        json!(true),
        json!("node"),
        json!([]),
        json!({"node":1}),
    ] {
        let params = Parameters::from([("bad".into(), bad)]);
        for mutation in ["SET (xs[i]).rank=100", "REMOVE (xs[i]).old"] {
            let query = format!(
                "MATCH (n:Target)-[r]->() SET n.rank=99,r.weight=99 \
                               CREATE (:Bad) WITH [n,$bad] AS xs UNWIND [0,1] AS i {mutation}"
            );
            let error = db.execute_with(&query, &params).unwrap_err();
            assert!(matches!(error, Error::Type(_)), "{query}: {error:?}");
            assert_graph_intact(&mut db);
        }
    }
    for query in [
        "MATCH (n:Target) CREATE (:Bad) SET (n.rank).x=1",
        "MATCH (n:Target) CREATE (:Bad) REMOVE (n.rank).x",
        "MATCH (n:Target) CREATE (:Bad) SET ([n][n.rank='text']).x=1",
        "MATCH (n:Target) CREATE (:Bad) REMOVE ({node:n}[n.rank]).x",
        "MATCH (n:Target) CREATE (:Bad) UNWIND [1,'bad'] AS v SET ([n][0]).rank=v-1",
    ] {
        let error = db.execute(query).unwrap_err();
        assert!(matches!(error, Error::Type(_)), "{query}: {error:?}");
        assert_graph_intact(&mut db);
    }
}

#[test]
fn parameters_cannot_forge_targets_and_missing_parameters_preflight() {
    let mut db = graph();
    let id = db
        .execute("MATCH (n:Target) RETURN elementId(n)")
        .unwrap()
        .rows[0][0]
        .as_str()
        .unwrap()
        .to_owned();
    let forged = json!({"id":id,"labels":["Target"],"props":{"rank":1}});
    for bad in [forged.clone(), json!(["n", forged]), json!(id)] {
        let params = Parameters::from([("bad".into(), bad)]);
        for query in [
            "MATCH (n:Target) SET ($bad).rank=0",
            "MATCH (n:Target) REMOVE ($bad).old",
            "MATCH (n:Target) WITH [n,$bad] AS xs UNWIND [0,1] AS i SET (xs[i]).rank=0",
            "MATCH (n:Target) SET n.fake=$bad REMOVE (n.fake).old",
        ] {
            assert!(
                matches!(db.execute_with(query, &params), Err(Error::Type(_))),
                "{query}"
            );
            assert_graph_intact(&mut db);
        }
    }
    for query in [
        "MATCH (n:Absent) WITH collect(n) AS ns SET (ns[$missing]).rank=0",
        "MATCH (n:Absent) WITH [n] AS ns REMOVE (ns[$missing]).rank",
        "CREATE (n:Bad) SET ([n][$missing]).rank=0",
        "CREATE (n:Bad) REMOVE ({node:n}[$missing]).rank",
        "MATCH (n:Target) SET (null).rank=$missing",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::MissingParameter(_))),
            "{query}"
        );
        assert_graph_intact(&mut db);
    }
    assert_eq!(
        db.execute("MATCH (n:Target) RETURN n.fake").unwrap().rows,
        [vec![json!(null)]]
    );
}

#[test]
fn computed_updates_maintain_uniqueness_and_property_lookup_indexes() {
    let mut db = Database::in_memory().unwrap();
    db.create_unique_constraint("Person", "email").unwrap();
    db.execute("CREATE (:Person {email:'a'}),(:Person {email:'b'})")
        .unwrap();
    assert!(matches!(
        db.execute(
            "MATCH (n:Person) WITH n ORDER BY n.email WITH collect(n) AS ns \
         SET (ns[0]).email='changed',(ns[1]).email='changed'"
        ),
        Err(Error::Constraint(_))
    ));
    for email in ["a", "b"] {
        assert_eq!(
            db.execute_with(
                "MATCH (n:Person {email:$email}) RETURN count(n)",
                &Parameters::from([("email".into(), json!(email))])
            )
            .unwrap()
            .rows,
            [vec![json!(1)]]
        );
    }
    assert_eq!(
        db.execute("MATCH (n:Person {email:'changed'}) RETURN count(n)")
            .unwrap()
            .rows,
        [vec![json!(0)]]
    );
    db.execute("MATCH (n:Person {email:'a'}) REMOVE ([n][0]).email")
        .unwrap();
    db.execute("CREATE (:Person {email:'a'})").unwrap();
    db.execute("MATCH (n:Person {email:'b'}) SET ({node:n}.node).email='changed'")
        .unwrap();
    assert_eq!(
        db.execute("MATCH (n:Person {email:'b'}) RETURN count(n)")
            .unwrap()
            .rows,
        [vec![json!(0)]]
    );
    assert_eq!(
        db.execute("MATCH (n:Person {email:'changed'}) RETURN count(n)")
            .unwrap()
            .rows,
        [vec![json!(1)]]
    );
}

#[test]
fn errors_abort_application_transactions_and_output_limits_roll_back() {
    let mut db = graph();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Earlier)").unwrap();
    let params = Parameters::from([("index".into(), json!(1))]);
    assert!(matches!(
        tx.execute_with("MATCH (n:Target) SET ([n,7][$index]).rank=0", &params),
        Err(Error::Type(_))
    ));
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert_graph_intact(&mut db);
    let mut tx = db.transaction().unwrap();
    tx.execute("MATCH (n:Target) SET ([n][0]).rank=5 REMOVE ([n][0]).old")
        .unwrap();
    assert_eq!(
        tx.execute("MATCH (n:Target) RETURN n.rank,n.old")
            .unwrap()
            .rows,
        [vec![json!(5), json!(null)]]
    );
    tx.rollback().unwrap();
    assert_graph_intact(&mut db);
    let options = QueryOptions {
        max_result_rows: Some(0),
        max_result_bytes: Some(0),
        ..QueryOptions::default()
    };
    for mutation in ["SET ([n][0]).rank=0", "REMOVE ([n][0]).old"] {
        let query = format!("MATCH (n:Target) {mutation} RETURN n");
        assert!(
            matches!(
                db.execute_with_options(&query, &Parameters::new(), &options),
                Err(Error::ResourceLimit(_))
            ),
            "{query}"
        );
        assert_graph_intact(&mut db);
    }
    let result = db
        .execute_with_options(
            "MATCH (n:Target) SET ([n][0]).rank=5 REMOVE ([n][0]).old",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    assert_eq!(result.summary.properties_set, 2);
}
