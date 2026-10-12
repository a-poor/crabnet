use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Step {rank:1}), (b:Step {rank:2}), (c:Step {rank:3}), (a)-[:NEXT]->(b), (b)-[:NEXT]->(c)").unwrap();
    db
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

const PATTERNS: &[&str] = &[
    "p = (:Step {rank:1})-[:NEXT]->()-[:NEXT]->(:Step {rank:3})",
    "p = (:Step {rank:1})-[rels:NEXT*2]->(:Step {rank:3})",
    "p = (:Step {rank:1})-[rels:NEXT*1..40]->(:Step {rank:3})",
    "p = (:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3})",
    "p = shortestPath((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
    "p = allShortestPaths((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
];

#[test]
fn direct_path_list_sizes_and_aliases_work_across_transports() {
    let mut db = graph();
    for pattern in PATTERNS {
        assert_eq!(
            rows(
                &mut db,
                &format!("MATCH {pattern} RETURN size(nodes(p)),size(relationships(p))")
            ),
            [vec![json!(3), json!(2)]]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH nodes(p) AS ns,relationships(p) AS rs WITH ns AS points,rs AS links RETURN size(points),size(links)"
                )
            ),
            [vec![json!(3), json!(2)]]
        );
        if pattern.contains("rels:") {
            assert_eq!(
                rows(
                    &mut db,
                    &format!("MATCH {pattern} WITH rels AS links RETURN size(links)")
                ),
                [vec![json!(2)]]
            );
        }
    }
}

#[test]
fn size_composes_with_grouped_paths_and_path_list_keys() {
    let mut db = graph();
    for pattern in PATTERNS {
        let result = rows(
            &mut db,
            &format!(
                "MATCH {pattern} UNWIND [0,1] AS copy RETURN p,size(nodes(p))+count(*),size(relationships(p))+count(*)"
            ),
        );
        assert_eq!(result[0][1..], [json!(5), json!(4)]);
        for list in ["nodes(p)", "relationships(p)"] {
            let result = rows(
                &mut db,
                &format!(
                    "MATCH {pattern} UNWIND [0,1] AS copy RETURN {list},count(*) AS copies ORDER BY size({list})"
                ),
            );
            assert_eq!(result[0][1], json!(2));
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} UNWIND [0,1] AS copy WITH {list} AS xs,count(*) AS copies RETURN size(xs)+copies"
                    )
                ),
                [vec![json!(result[0][0].as_list().unwrap().len() + 2)]]
            );
        }
    }
}

#[test]
fn zero_hop_paths_have_one_node_and_no_relationships() {
    let mut db = graph();
    for pattern in [
        "p=(:Step {rank:1})",
        "p=(:Step {rank:1})-[rels:NEXT*0]->(:Step {rank:1})",
        "p=(:Step {rank:1})-[rels:NEXT*0..40]->(:Step {rank:1})",
        "p=(:Step {rank:1})-[rels:NEXT*0..]->(:Step {rank:1})",
        "p=shortestPath((:Step {rank:1})-[rels:NEXT*0..]->(:Step {rank:1}))",
        "p=allShortestPaths((:Step {rank:1})-[rels:NEXT*0..]->(:Step {rank:1}))",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!("MATCH {pattern} RETURN size(nodes(p)),size(relationships(p))")
            ),
            [vec![json!(1), json!(0)]]
        );
        let query = format!("MATCH {pattern} RETURN size(length(p))");
        assert!(matches!(db.execute(&query), Err(Error::Type(_))), "{query}");
        {
            let mut read = db.prepare_read(&query).unwrap();
            let result = read
                .cursor()
                .and_then(|cursor| cursor.collect::<crabnet::Result<Vec<_>>>());
            assert!(matches!(result, Err(Error::Type(_))), "{query}: {result:?}");
        }
        if pattern.contains("rels:") {
            assert_eq!(
                rows(&mut db, &format!("MATCH {pattern} RETURN size(rels)")),
                [vec![json!(0)]]
            );
        }
    }
}

#[test]
fn sizes_count_positions_across_segments_cycles_and_parallel_edges() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT]->()-[:NEXT*0..1]->() \
         RETURN size(nodes(p)),size(relationships(p)) ORDER BY size(nodes(p))"
        ),
        [vec![json!(2), json!(1)], vec![json!(3), json!(2)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:3})<-[:NEXT]-()<-[:NEXT]-(:Step {rank:1}) \
         RETURN size(nodes(p)),size(relationships(p))"
        ),
        [vec![json!(3), json!(2)]]
    );
    db.execute(
        "CREATE (a:Cycle),(b:End),(a)-[:R]->(a),(a)-[:R]->(b),(a)-[:R]->(b),(b)-[:BACK]->(a)",
    )
    .unwrap();
    for pattern in [
        "p=(a:Cycle)-[:R]->(a)-[:R]->(:End)",
        "p=(:Cycle)-[rs:R*2]->(:End)",
        "p=(:Cycle)-[rs:R*2..40]->(:End)",
        "p=(:Cycle)-[rs:R*2..]->(:End)",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!("MATCH {pattern} RETURN size(nodes(p)),size(relationships(p))")
            ),
            vec![vec![json!(3), json!(2)]; 2]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(a:Cycle)-[:R]->(:End)-[:BACK]->(a) \
         RETURN size(nodes(p)),size(relationships(p))"
        ),
        vec![vec![json!(3), json!(2)]; 2]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(a:Cycle)-[:R]-(a) RETURN size(nodes(p)),size(relationships(p))"
        ),
        [vec![json!(2), json!(1)]]
    );
}

#[test]
fn sizes_compose_with_filters_aggregates_pagination_and_unions() {
    let mut db = graph();
    for pattern in PATTERNS {
        if !pattern.contains("shortestPath") && !pattern.contains("allShortestPaths") {
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} WHERE size(nodes(p))=3 AND size(relationships(p))=2 RETURN count(*)"
                    )
                ),
                [vec![json!(1)]]
            );
        }
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH p WHERE size(nodes(p))=size(relationships(p))+1 \
             UNWIND [0,1] AS copy RETURN sum(size(nodes(p))),avg(size(relationships(p))), \
             min(size(nodes(p))),max(size(relationships(p))),collect(size(nodes(p)))"
                )
            ),
            [vec![
                json!(6),
                json!(2.0),
                json!(3),
                json!(2),
                json!([3, 3])
            ]]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} UNWIND [0,1] AS copy \
             RETURN size(nodes(p)) AS n,count(*) AS copies ORDER BY n"
                )
            ),
            [vec![json!(3), json!(2)]]
        );
        if pattern.contains("rels:") {
            assert_eq!(
                rows(&mut db, &format!("MATCH {pattern} RETURN sum(size(rels))")),
                [vec![json!(2)]]
            );
        }
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*0..]->() UNWIND [0,1] AS copy \
         RETURN DISTINCT size(nodes(p)) AS n ORDER BY n DESC SKIP 1 LIMIT 1"
        ),
        [vec![json!(2)]]
    );
    let branch = "MATCH p=(:Step {rank:1})-[:NEXT*]->(:Step {rank:3}) RETURN size(nodes(p)) AS n";
    assert_eq!(
        rows(&mut db, &format!("{branch} UNION {branch}")),
        [vec![json!(3)]]
    );
    assert_eq!(
        rows(&mut db, &format!("{branch} UNION ALL {branch}")),
        vec![vec![json!(3)]; 2]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Absent)-[:NEXT*]->() RETURN count(size(nodes(p))),collect(size(relationships(p)))"
        ),
        [vec![json!(0), json!([])]]
    );
}

#[test]
fn materialized_nullable_and_transformed_lists_keep_size_semantics() {
    let mut db = graph();
    for pattern in PATTERNS {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} UNWIND [p,null] AS q \
             RETURN q IS NULL AS absent,size(nodes(q)),size(relationships(q)) ORDER BY absent"
                )
            ),
            [
                vec![json!(false), json!(3), json!(2)],
                vec![json!(true), json!(null), json!(null)]
            ]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} RETURN size(tail(nodes(p))),size(reverse(relationships(p))), \
             size(nodes(p)[1..]),size(nodes(p)+[null]),size([n IN nodes(p) WHERE n.rank>1]), \
             size(coalesce(null,nodes(p))),size(CASE WHEN true THEN relationships(p) END), \
             size({{items:nodes(p)}}.items)"
                )
            ),
            [vec![
                json!(2),
                json!(2),
                json!(2),
                json!(4),
                json!(2),
                json!(3),
                json!(2),
                json!(3)
            ]]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH collect(p) AS ps \
             RETURN size(nodes(head(ps))),size(relationships(last(ps))),size(ps)"
                )
            ),
            [vec![json!(3), json!(2), json!(1)]]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT]->() WITH [nodes(p),null,[]] AS lists \
         UNWIND [0,1,2] AS i RETURN i,size(lists[i]) ORDER BY i"
        ),
        [
            vec![json!(0), json!(2)],
            vec![json!(1), json!(null)],
            vec![json!(2), json!(0)]
        ]
    );
}

#[test]
fn size_only_plans_avoid_entity_records_and_output_hydration() {
    for pattern in PATTERNS {
        for suffix in [
            "RETURN size(nodes(p)),size(relationships(p))",
            "WITH nodes(p) AS ns,relationships(p) AS rs RETURN size(ns),size(rs)",
        ] {
            let query = format!("MATCH {pattern} {suffix}");
            let plan = crabnet::explain_physical(&query).unwrap();
            assert!(!plan.contains("batch-hydrate-paths"), "{query}: {plan}");
            assert!(!plan.contains("'props',"), "{query}: {plan}");
        }
    }
}

#[test]
fn parameters_scope_errors_and_write_restrictions_remain_atomic() {
    let mut db = graph();
    let query = "MATCH p=(:Step {rank:$rank})-[:NEXT*]->() \
               WITH nodes(p) AS ns,relationships(p) AS rs WHERE size(ns)>$minimum \
               RETURN size(ns),size(rs) ORDER BY size(ns)";
    let params = Parameters::from([("rank".into(), json!(1)), ("minimum".into(), json!(1))]);
    let result = db.execute_with(query, &params).unwrap();
    assert_eq!(
        result.rows,
        [vec![json!(2), json!(1)], vec![json!(3), json!(2)]]
    );
    let mut read = db.prepare_read_with(query, &params).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        result.rows
    );
    drop(read);
    for query in [
        "MATCH p=(:Absent)-[:NEXT*]->() RETURN size(nodes(p))+$missing",
        "MATCH p=(:Absent {rank:$missing})-[:NEXT]->() RETURN size(relationships(p))",
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
    for query in [
        "MATCH p=(:Step)-[:NEXT]->() RETURN size(p)",
        "MATCH p=(:Step)-[:NEXT]->() RETURN size(nodes(p)[0])",
        "MATCH p=(:Step)-[:NEXT]->() RETURN size(relationships(p),nodes(p))",
        "MATCH p=(:Step)-[:NEXT]->() WITH nodes(p) AS ns RETURN size(nodes(p))",
        "MATCH p=(:Step)-[:NEXT]->() RETURN nodes(p),size(nodes(p))+count(*)",
        "MATCH p=(:Step)-[:NEXT]->() WITH size(nodes(p)) AS n CREATE (:Bad {n:n})",
        "CREATE (:Bad) WITH 1 AS x MATCH p=(:Step)-[:NEXT]->() RETURN size(nodes(p))",
    ] {
        let error = db.execute(query).unwrap_err();
        assert!(
            matches!(error, Error::Semantic(_) | Error::Unsupported(_)),
            "{query}: {error:?}"
        );
    }
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Bad)").unwrap();
    assert!(matches!(
        tx.execute("MATCH p=(:Step)-[:NEXT]->() RETURN size(nodes(p))+$missing"),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert_eq!(
        rows(&mut db, "MATCH (n:Bad) RETURN count(n)"),
        [vec![json!(0)]]
    );
}
