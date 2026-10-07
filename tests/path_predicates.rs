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

const PATTERNS: &[&str] = &[
    "p = (:Step {rank:1})-[:NEXT]->()-[:NEXT]->(:Step {rank:3})",
    "p = (:Step {rank:1})-[rels:NEXT*2]->(:Step {rank:3})",
    "p = (:Step {rank:1})-[rels:NEXT*1..40]->(:Step {rank:3})",
    "p = (:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3})",
    "p = shortestPath((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
    "p = allShortestPaths((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
];

#[test]
fn direct_path_and_list_equality_agree_across_transports() {
    let mut db = graph();
    for pattern in PATTERNS {
        if !pattern.contains("shortestPath") && !pattern.contains("allShortestPaths") {
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} WHERE p=p AND head(nodes(p)) IN nodes(p) RETURN count(*)"
                    )
                ),
                [vec![value(1)]]
            );
        }
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} MATCH q=(:Step {{rank:1}})-[:NEXT]->()-[:NEXT]->(:Step {{rank:3}}) \
             RETURN p=q,p<>q,nodes(p)=nodes(q),relationships(p)=relationships(q)"
                )
            ),
            [vec![value(true), value(false), value(true), value(true)]]
        );
        if pattern.contains("rels:") {
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} WITH rels AS links,p \
                 RETURN links=relationships(p),links<>relationships(p), \
                        head(links) IN links,links IS NOT NULL"
                    )
                ),
                [vec![value(true), value(false), value(true), value(true)]]
            );
        }
    }
}

#[test]
fn direct_null_tests_and_empty_membership_preserve_three_valued_logic() {
    let mut db = graph();
    for pattern in PATTERNS {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} RETURN p IS NULL,p IS NOT NULL, \
             nodes(p) IS NULL,relationships(p) IS NOT NULL, \
             p=null,p<>null,p IN [],p IN null,p IN [null]"
                )
            ),
            [vec![
                value(false),
                value(true),
                value(false),
                value(true),
                value(json!(null)),
                value(json!(null)),
                value(false),
                value(json!(null)),
                value(json!(null))
            ]]
        );
    }
}

#[test]
fn direct_entity_membership_in_path_lists_preserves_identity() {
    let mut db = graph();
    for pattern in PATTERNS {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} MATCH (a:Step {{rank:1}})-[edge:NEXT]->() \
             RETURN a IN nodes(p),edge IN relationships(p), \
             edge IN nodes(p),a IN relationships(p),null IN relationships(p)"
                )
            ),
            [vec![
                value(true),
                value(true),
                value(false),
                value(false),
                value(json!(null))
            ]]
        );
    }
}

#[test]
fn aliases_selections_and_collections_share_comparison_semantics() {
    let mut db = graph();
    for pattern in PATTERNS {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH p AS route,nodes(p) AS ns,relationships(p) AS rs \
             WITH route AS p,ns,rs,ns AS ns2,rs AS rs2 \
             RETURN ns=ns2,rs<>rs2,p=head([p]),p IN [null,p,p], \
             ns=[n IN ns | n],rs=rs[..],head(ns) IN ns,head(rs) IN rs"
                )
            ),
            [vec![
                value(true),
                value(false),
                value(true),
                value(true),
                value(true),
                value(true),
                value(true),
                value(true)
            ]]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} RETURN nodes(p) IN [nodes(p),null], \
             relationships(p) IN [null,relationships(p)], \
             p IN [nodes(p),null],nodes(p)=relationships(p), \
             p=nodes(p),p<>42,p=p=p,p<>null=p"
                )
            ),
            [vec![
                value(true),
                value(true),
                value(json!(null)),
                value(false),
                value(false),
                value(true),
                value(true),
                value(json!(null))
            ]]
        );
    }
}

#[test]
fn ordered_identity_distinguishes_parallel_edges_and_reversed_routes() {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:N {same:1}),(b:N {same:1}), \
                (a)-[:R {same:1}]->(b),(a)-[:R {same:1}]->(b),(a)-[:R {same:1}]->(a)",
    )
    .unwrap();
    for (comparison, equal_count) in [
        ("p=q", 5),
        ("nodes(p)=nodes(q)", 9),
        ("relationships(p)=relationships(q)", 9),
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH p=(:N)-[:R]-(:N) MATCH q=(:N)-[:R]-(:N) \
             RETURN {comparison} AS same,count(*) ORDER BY same"
                )
            ),
            [
                vec![value(false), value(25 - equal_count)],
                vec![value(true), value(equal_count)]
            ]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(a:N)-[r:R]->(b:N) MATCH q=(b)-[r]-(a) \
         RETURN p=q AS same,p<>q AS different,count(*) ORDER BY same"
        ),
        [
            vec![value(false), value(true), value(2)],
            vec![value(true), value(false), value(1)]
        ]
    );
    // Traversing a self-loop in the opposite stored direction changes neither
    // node nor relationship sequence.
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(a:N)-[:R]->(a) MATCH q=(a)<-[:R]-(a) RETURN p=q"
        ),
        [vec![value(true)]]
    );
}

#[test]
fn zero_hop_and_nullable_values_keep_empty_and_unknown_distinct() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*0]->() \
         MATCH q=(:Step {rank:2})-[:NEXT*0]->() \
         RETURN p=q,nodes(p)=nodes(q),relationships(p)=relationships(q), \
         relationships(p)=[],relationships(p) IS NULL, \
         null IN relationships(p),p IN relationships(p),relationships(p) IN [[]]"
        ),
        [vec![
            value(false),
            value(false),
            value(true),
            value(true),
            value(false),
            value(false),
            value(false),
            value(true)
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT]->() UNWIND [p,null] AS q \
         RETURN q IS NULL,q IS NOT NULL,p=q,p<>q,q IN [p],q IN [], \
         nodes(q) IS NULL,relationships(q) IS NOT NULL ORDER BY q IS NULL"
        ),
        [
            vec![
                value(false),
                value(true),
                value(true),
                value(false),
                value(true),
                value(false),
                value(false),
                value(true)
            ],
            vec![
                value(true),
                value(false),
                value(json!(null)),
                value(json!(null)),
                value(json!(null)),
                value(false),
                value(true),
                value(false)
            ]
        ]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(a:Step {rank:1})-[:NEXT]->(b) \
         RETURN nodes(p)=[a,null],nodes(p)=[null,a],nodes(p)=[null], \
         nodes(p) IN [[a,null]],nodes(p) IN [[null,a],null]"
        ),
        [vec![
            value(json!(null)),
            value(false),
            value(false),
            value(json!(null)),
            value(json!(null))
        ]]
    );
}

#[test]
fn predicates_compose_with_filters_groups_quantifiers_and_unions() {
    let mut db = graph();
    for pattern in PATTERNS {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH p WHERE p=p AND nodes(p)=nodes(p) \
             UNWIND [0,1] AS copy WITH p AS route,copy WHERE route IS NOT NULL \
             RETURN route=route AS same,count(*),collect(copy) ORDER BY same"
                )
            ),
            [vec![value(true), value(2), value(json!([0, 1]))]]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} UNWIND [0,1] AS copy \
             RETURN p,CASE WHEN p=p THEN count(*) ELSE 0 END AS copies"
                )
            )[0][1],
            value(2)
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH p,collect(p) AS ps \
             WHERE p IN ps AND p=head(ps) \
             RETURN all(q IN ps WHERE q=p),single(q IN [p,null] WHERE q=p), \
             [q IN ps WHERE q=p | length(q)], \
             CASE WHEN relationships(p)=[] THEN 0 ELSE length(p) END"
                )
            ),
            [vec![
                value(true),
                value(json!(null)),
                value(json!([2])),
                value(2)
            ]]
        );
    }
    let branch = "MATCH p=(:Step {rank:1})-[:NEXT*]->() RETURN p=p AS same";
    assert_eq!(
        rows(&mut db, &format!("{branch} UNION {branch}")),
        [vec![value(true)]]
    );
    assert_eq!(
        rows(&mut db, &format!("{branch} UNION ALL {branch}")),
        vec![vec![value(true)]; 4]
    );
}

#[test]
fn parameters_cannot_forge_graph_identity_and_preflight_on_empty_inputs() {
    let mut db = graph();
    let path = rows(&mut db, "MATCH p=(:Step {rank:1})-[:NEXT]->() RETURN p")[0][0].clone();
    let path = path.as_path().unwrap();
    let node_records: Vec<_> = path
        .nodes
        .iter()
        .map(|n| json!({"id":n.element_id,"labels":n.labels,"props":{"rank":n.properties["rank"].as_i64().unwrap()}}))
        .collect();
    let rel_records: Vec<_> = path.relationships.iter().map(|r| json!({"id":r.element_id,"type":r.relationship_type,"from":r.start_node_element_id,"to":r.end_node_element_id,"props":{"rank":r.properties["rank"].as_i64().unwrap()}})).collect();
    let forged_path = json!(["p",{"nodes":node_records,"relationships":rel_records}]);
    let params = Parameters::from([
        ("path".into(), forged_path.clone()),
        ("paths".into(), json!([forged_path])),
        ("nodes".into(), json!(node_records)),
        ("rank".into(), json!(1)),
    ]);
    let query = "MATCH p=(:Step {rank:$rank})-[:NEXT]->() \
                 RETURN p=$path,p IN $paths,nodes(p)=$nodes,relationships(p)=$nodes";
    let expected = vec![vec![value(false); 4]];
    assert_eq!(db.execute_with(query, &params).unwrap().rows, expected);
    let mut read = db.prepare_read_with(query, &params).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        expected
    );
    drop(read);
    for query in [
        "MATCH p=(:Absent)-[:NEXT]->() RETURN p=$missing",
        "MATCH p=(:Absent)-[:NEXT*]->() RETURN p IN $missing",
        "MATCH p=(:Absent)-[:NEXT]->() RETURN nodes(p)=$missing",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::MissingParameter(_))),
            "{query}"
        );
        assert!(
            matches!(db.prepare_read(query), Err(Error::MissingParameter(_))),
            "{query}"
        );
    }
}

#[test]
fn invalid_membership_is_a_terminal_cursor_error() {
    let mut db = graph();
    for bad in [json!(42), json!("text"), json!({})] {
        let params = Parameters::from([("bad".into(), bad)]);
        let query = "MATCH p=(:Step {rank:1})-[:NEXT]->() RETURN p IN $bad";
        assert!(matches!(
            db.execute_with(query, &params),
            Err(Error::Type(_))
        ));
        let mut read = db.prepare_read_with(query, &params).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
        assert!(cursor.next().is_none());
    }
    assert!(matches!(
        db.execute("MATCH p=(:Step)-[:NEXT]->() RETURN p IN p"),
        Err(Error::Type(_))
    ));
    let query = "MATCH p=(:Step {rank:1})-[:NEXT]->() UNWIND [[],42] AS xs RETURN p IN xs";
    assert!(matches!(db.execute(query), Err(Error::Type(_))));
    let mut read = db.prepare_read(query).unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), vec![value(false)]);
    assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
    assert!(cursor.next().is_none());
}

#[test]
fn entity_list_predicate_writes_preserve_bags_refresh_and_rollback() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND ns AS n \
         SET n.seen=n IN ns,n.same=ns=ns RETURN n.rank,n.seen,n.same ORDER BY n.rank",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        (1..=3)
            .map(|rank| vec![value(rank), value(true), value(true)])
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        db.execute(
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND ns AS n \
         SET n.failed=ns=ns WITH ns,n SET n.invalid=ns IN 42"
        ),
        Err(Error::Type(_))
    ));
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) RETURN count(n.failed),count(n.invalid)"
        ),
        [vec![value(0), value(0)]]
    );
    for query in [
        "MATCH p=(:Step)-[:NEXT]->() RETURN p<p",
        "MATCH p=(:Step)-[:NEXT]->() RETURN nodes(p)<nodes(p)",
        "MATCH p=(:Step)-[:NEXT]->() RETURN p ORDER BY p",
        "MATCH p=(:Step)-[:NEXT]->() WHERE p=p CREATE (:Unexpected)",
        "MATCH p=(:Step)-[:NEXT]->() WITH p=p AS same CREATE (:Unexpected {same:same})",
        "CREATE (:Unexpected) WITH 1 AS x MATCH p=(:Step)-[:NEXT]->() RETURN p=p",
    ] {
        assert!(
            matches!(
                db.execute(query),
                Err(Error::Semantic(_) | Error::Unsupported(_))
            ),
            "{query}"
        );
    }
    {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Unexpected)").unwrap();
        assert!(matches!(
            tx.execute("MATCH p=(:Step)-[:NEXT]->() RETURN p IN 42"),
            Err(Error::Type(_))
        ));
    }
    assert_eq!(
        rows(&mut db, "MATCH (n:Unexpected) RETURN count(n)"),
        [vec![value(0)]]
    );
}

#[test]
fn nested_predicate_sql_growth_and_result_limits_remain_bounded() {
    let mut db = graph();
    let mut sizes = Vec::new();
    for depth in [6, 12] {
        let mut expression = "p=p".to_string();
        for _ in 0..depth {
            expression = format!("({expression})=true");
        }
        let query = format!("MATCH p=(:Step {{rank:1}})-[:NEXT]->() RETURN {expression}");
        sizes.push(crabnet::explain_physical(&query).unwrap().len());
        assert_eq!(rows(&mut db, &query), [vec![value(true)]]);
    }
    assert!(sizes[1] < sizes[0] * 3, "{sizes:?}");
    let options = crabnet::QueryOptions {
        max_result_rows: Some(0),
        ..crabnet::QueryOptions::default()
    };
    assert!(matches!(
        db.execute_with_options(
            "MATCH p=(:Step)-[:NEXT*]->() RETURN p=p",
            &Parameters::new(),
            &options
        ),
        Err(Error::ResourceLimit(_))
    ));
}
