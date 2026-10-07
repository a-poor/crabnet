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
fn direct_aggregates_preserve_paths_lists_and_bags_across_transports() {
    let mut db = graph();
    for pattern in PATTERNS {
        let expected = rows(
            &mut db,
            &format!("MATCH {pattern} RETURN p, nodes(p), relationships(p)"),
        );
        let result = rows(
            &mut db,
            &format!(
                "MATCH {pattern} UNWIND [0,1] AS copy RETURN count(p), count(ALL p), count(DISTINCT p), collect(p), collect(DISTINCT p), collect(nodes(p)), collect(DISTINCT relationships(p))"
            ),
        );
        assert_eq!(result[0][..3], [value(2), value(2), value(1)]);
        for (column, source, copies) in [(3, 0, 2), (4, 0, 1), (5, 1, 2), (6, 2, 1)] {
            assert_eq!(
                result[0][column],
                CypherValue::List(vec![expected[0][source].clone(); copies])
            );
        }
        if pattern.contains("rels:") {
            assert_eq!(
                rows(
                    &mut db,
                    &format!("MATCH {pattern} RETURN collect(rels), count(DISTINCT rels)")
                ),
                [vec![
                    CypherValue::List(vec![expected[0][2].clone()]),
                    value(1)
                ]]
            );
        }
    }
}

#[test]
fn path_grouping_preserves_aliases_functions_and_aggregate_scope() {
    let mut db = graph();
    for pattern in PATTERNS {
        let expected = rows(&mut db, &format!("MATCH {pattern} RETURN p"));
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} UNWIND [0,1] AS copy RETURN p, count(*) AS copies ORDER BY length(p)"
                )
            ),
            [vec![expected[0][0].clone(), value(2)]]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} UNWIND [0,1] AS copy WITH p AS route,count(*) AS copies WHERE length(route)=2 RETURN route,copies"
                )
            ),
            [vec![expected[0][0].clone(), value(2)]]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!("MATCH {pattern} RETURN p,length(p)+count(*) AS score")
            ),
            [vec![expected[0][0].clone(), value(3)]]
        );
    }
}

#[test]
fn grouping_path_lists_retains_types_modifiers_and_later_matches() {
    let mut db = graph();
    for pattern in PATTERNS {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} UNWIND [0,1] AS copy RETURN count(nodes(p)), count(DISTINCT nodes(p)), count(relationships(p)), count(DISTINCT relationships(p))"
                )
            ),
            [vec![value(2), value(1), value(2), value(1)]]
        );
        for list in ["nodes(p)", "relationships(p)"] {
            let expected = rows(&mut db, &format!("MATCH {pattern} RETURN {list}"));
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} UNWIND [0,1] AS copy RETURN {list},count(*) ORDER BY head({list}).rank"
                    )
                ),
                [vec![expected[0][0].clone(), value(2)]]
            );
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} UNWIND [0,1] AS copy WITH {list} AS xs,count(*) AS copies WHERE size(xs)>0 RETURN xs,copies"
                    )
                ),
                [vec![expected[0][0].clone(), value(2)]]
            );
        }
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH nodes(p) AS xs,count(*) AS copies UNWIND xs AS n MATCH (n)-[:NEXT]->(b) RETURN n.rank,b.rank,copies ORDER BY n.rank"
                )
            ),
            [
                vec![value(1), value(2), value(1)],
                vec![value(2), value(3), value(1)]
            ]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH relationships(p) AS xs,count(*) AS copies UNWIND xs AS edge MATCH (a)-[edge]->(b) RETURN a.rank,b.rank,copies ORDER BY a.rank"
                )
            ),
            [
                vec![value(1), value(2), value(1)],
                vec![value(2), value(3), value(1)]
            ]
        );
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH {pattern} WITH p, count(*) AS copies RETURN [n IN nodes(p) | n.rank], [r IN relationships(p) | r.rank], copies"
                )
            ),
            [vec![
                value(json!([1, 2, 3])),
                value(json!([10, 20])),
                value(1)
            ]]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[rels:NEXT*2]->() WITH p,rels WITH *,count(*) AS copies RETURN length(p),size(rels),copies"
        ),
        [vec![value(2), value(2), value(1)]]
    );
}

#[test]
fn distinct_paths_use_ordered_identity_direction_and_parallel_edges() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:V {k:0}), (b:V {k:0}), (a)-[:R {k:0}]->(b), (a)-[:R {k:0}]->(b), (a)-[:R {k:0}]->(a)").unwrap();
    let result = rows(
        &mut db,
        "MATCH p=(a:V)-[:R]-(b:V) UNWIND [0,1] AS copy RETURN count(p), count(DISTINCT p), count(DISTINCT nodes(p)), count(DISTINCT relationships(p)), collect(DISTINCT p)",
    );
    assert_eq!(result[0][..4], [value(10), value(5), value(3), value(3)]);
    let paths = result[0][4].as_list().unwrap();
    assert_eq!(paths.len(), 5);
    let groups = rows(
        &mut db,
        "MATCH p=(a:V)-[:R]-(b:V) UNWIND [0,1] AS copy RETURN p,count(*)",
    );
    assert_eq!(groups.len(), 5);
    assert!(groups.iter().all(|row| row[1] == value(2)));
    for path in paths {
        assert_eq!(groups.iter().filter(|row| row[0] == *path).count(), 1);
    }
    // Fixed and variable-length representations of the same route compare
    // by flattened identity, independently of segment boundaries.
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(a:V)-[:R]->(b) MATCH q=(a)-[:R*1]->(b) WHERE head(relationships(p))=head(relationships(q)) UNWIND [p,q] AS route RETURN count(route),count(DISTINCT route)"
        ),
        [vec![value(6), value(3)]]
    );
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT]->()-[:NEXT]->(b:Step {rank:3}) MATCH q=(:Step {rank:1})-[:NEXT*2]->(b) UNWIND [p,q] AS route RETURN count(route),count(DISTINCT route)"
        ),
        [vec![value(2), value(1)]]
    );
}

#[test]
fn empty_inputs_null_members_and_zero_hop_paths_keep_aggregate_semantics() {
    let mut db = graph();
    for prefix in [
        "MATCH p=(:Absent)-[:NEXT]->()",
        "MATCH p=(:Absent)-[:NEXT*]->()",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "{prefix} RETURN count(p),count(DISTINCT p),collect(p),collect(nodes(p)),collect(relationships(p))"
                )
            ),
            [vec![
                value(0),
                value(0),
                value(json!([])),
                value(json!([])),
                value(json!([]))
            ]]
        );
        assert!(rows(&mut db, &format!("{prefix} RETURN p,count(*)")).is_empty());
        assert!(
            rows(
                &mut db,
                &format!("{prefix} WITH `collect`(p) AS paths UNWIND paths AS p RETURN p")
            )
            .is_empty()
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step)-[:NEXT*0]->() UNWIND [0,1] AS copy RETURN count(p),count(DISTINCT p),count(DISTINCT nodes(p)),count(DISTINCT relationships(p)),size(collect(relationships(p)))"
        ),
        [vec![value(6), value(3), value(3), value(1), value(6)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step)-[:NEXT*0]->() RETURN relationships(p),count(*)"
        ),
        [vec![value(json!([])), value(3)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT]->() UNWIND [p,null,p] AS route RETURN count(route),count(DISTINCT route),size(collect(route)),size(collect(DISTINCT route))"
        ),
        [vec![value(2), value(1), value(2), value(1)]]
    );
    let result = rows(
        &mut db,
        "MATCH p=(:Step {rank:1})-[:NEXT]->() UNWIND [null,null] AS ignored WITH CASE WHEN false THEN p END AS route RETURN route,count(route),count(*),`collect`(route)",
    );
    assert_eq!(
        result,
        [vec![
            CypherValue::Null,
            value(0),
            value(2),
            value(json!([]))
        ]]
    );
}

#[test]
fn ordered_collections_compose_with_selection_iteration_and_conditions() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*0..2]->(b) WITH p,b ORDER BY b.rank DESC WITH collect(p) AS routes RETURN [p IN routes | length(p)],length(head(routes)),length(last(routes)),all(p IN routes WHERE length(p)>=0),[p IN reverse(routes) | head(nodes(p)).rank]"
        ),
        [vec![
            value(json!([2, 1, 0])),
            value(2),
            value(0),
            value(true),
            value(json!([1, 1, 1]))
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*0..2]->(b) WITH p,b ORDER BY b.rank DESC UNWIND [0,1] AS copy WITH p,b ORDER BY b.rank DESC RETURN [p IN collect(DISTINCT p) | length(p)]"
        ),
        [vec![value(json!([2, 1, 0]))]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*2]->() RETURN size(coalesce(collect(p),[])), CASE WHEN count(p)>0 THEN length(head(`collect`(p))) ELSE 0 END"
        ),
        [vec![value(1), value(2)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*2]->() RETURN p,count(*)+length(p),CASE WHEN count(*)>0 THEN p END"
        )[0][1],
        value(3)
    );
}

#[test]
fn grouped_paths_survive_distinct_pagination_unions_and_new_traversals() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*0..2]->() UNWIND [0,1] AS copy WITH DISTINCT p,count(*) AS copies ORDER BY length(p) DESC SKIP 1 LIMIT 1 WITH p,copies RETURN length(p),copies"
        ),
        [vec![value(1), value(2)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*0..2]->() UNWIND [0,1] AS copy RETURN DISTINCT p,count(*) AS copies,length(p) AS hops ORDER BY hops DESC LIMIT 1"
        )[0][1],
        value(2)
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[:NEXT*1..40]->(:Step {rank:3}) WITH p,count(*) AS copies WITH p, copies, head(nodes(p)) AS a MATCH q=(a)-[:NEXT*]->(:Step {rank:3}) RETURN length(p),length(q),copies,count(q)"
        ),
        [vec![value(2), value(2), value(1), value(1)]]
    );
    for (union, count) in [("UNION", 1), ("UNION ALL", 2)] {
        let query = format!(
            "MATCH p=(:Step {{rank:1}})-[:NEXT*2]->() RETURN collect(p) AS paths {union} MATCH p=(:Step {{rank:1}})-[:NEXT*1..40]->(:Step {{rank:3}}) RETURN collect(p) AS paths"
        );
        assert_eq!(rows(&mut db, &query).len(), count);
    }
}

#[test]
fn parameters_remain_plain_json_and_failures_preflight_atomically() {
    let mut db = graph();
    let parameters = Parameters::from([
        ("rank".into(), json!(1)),
        ("plain".into(), json!(["p",{"nodes":[],"relationships":[]}])),
    ]);
    let query = "MATCH p=(:Step {rank:$rank})-[:NEXT*2]->() RETURN collect(p),collect($plain)";
    let result = db.execute_with(query, &parameters).unwrap();
    assert!(result.rows[0][0].as_list().unwrap()[0].as_path().is_some());
    assert_eq!(
        result.rows[0][1],
        value(json!([["p",{"nodes":[],"relationships":[]}]]))
    );
    {
        let mut read = db.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            result.rows
        );
    }
    assert!(matches!(
        db.execute("MATCH p=(:Absent)-[:NEXT*]->() RETURN collect(p),$missing"),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(db.execute("CREATE (:Unexpected) WITH 1 AS x MATCH p=(:Absent)-[:NEXT]->() RETURN collect(p),$missing"),Err(Error::Unsupported(_))));
    assert_eq!(
        rows(&mut db, "MATCH (n:Unexpected) RETURN count(n)"),
        [vec![value(0)]]
    );
}

#[test]
fn unsupported_numeric_aggregates_invalid_scopes_and_path_writes_stay_atomic() {
    let mut db = graph();
    for query in [
        "MATCH p=(:Step)-[:NEXT]->() RETURN sum(p)",
        "MATCH p=(:Step)-[:NEXT]->() RETURN min(nodes(p))",
        "MATCH p=(:Step)-[:NEXT]->() RETURN collect(collect(p))",
        "MATCH p=(:Step)-[:NEXT]->() UNWIND collect(p) AS q RETURN q",
        "MATCH p=(a:Step)-[:NEXT]->(b) RETURN p,count(*)+b.rank",
        "MATCH p=(:Step)-[:NEXT]->() RETURN collect(p),length(p)+count(*)",
        "MATCH p=(:Step)-[:NEXT]->() WITH `collect`(p) AS paths CREATE (:Unexpected)",
        "MATCH p=(:Step)-[:NEXT]->() WITH nodes(p) AS ns,count(*) AS copies UNWIND ns AS n SET n.changed=true",
        "MATCH ()-[rels:NEXT*1]->() WITH rels,count(*) AS copies MATCH (a)-[rels*]->(b) RETURN a,b",
    ] {
        let error = db.execute(query).unwrap_err();
        assert!(
            matches!(error, Error::Unsupported(_) | Error::Semantic(_)),
            "{query}: {error:?}"
        );
    }
    {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Unexpected)").unwrap();
        assert!(
            tx.execute("MATCH p=(:Step)-[:NEXT]->() RETURN sum(p)")
                .is_err()
        );
    }
    assert_eq!(
        rows(&mut db, "MATCH (n:Unexpected) RETURN count(n)"),
        [vec![value(0)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WHERE n.changed IS NOT NULL RETURN count(n)"
        ),
        [vec![value(0)]]
    );
}

#[test]
fn aggregation_respects_resource_limits_depth_guards_and_sql_growth() {
    use crabnet::QueryOptions;
    let mut db = graph();
    let options = QueryOptions {
        max_result_rows: Some(0),
        ..QueryOptions::default()
    };
    assert!(matches!(
        db.execute_with_options(
            "MATCH p=(:Step {rank:1})-[:NEXT*2]->() RETURN collect(p)",
            &Parameters::new(),
            &options
        ),
        Err(Error::ResourceLimit(_))
    ));
    let nested = format!("{}p{}", "[".repeat(99), "]".repeat(99));
    let result = rows(
        &mut db,
        &format!("MATCH p=(:Step {{rank:1}})-[:NEXT*2]->() RETURN collect({nested})"),
    );
    let mut leaf = &result[0][0];
    for _ in 0..100 {
        leaf = &leaf.as_list().unwrap()[0];
    }
    assert!(leaf.as_path().is_some());
    let nested = format!("{}p{}", "[".repeat(100), "]".repeat(100));
    assert!(matches!(
        db.execute(&format!(
            "MATCH p=(:Step {{rank:1}})-[:NEXT*2]->() RETURN collect({nested})"
        )),
        Err(Error::Type(_))
    ));
    let mut sizes = Vec::new();
    for depth in [6, 12] {
        let expression = format!(
            "{}collect(p){}",
            "reverse(".repeat(depth),
            ")".repeat(depth)
        );
        let query = format!("MATCH p=(:Step {{rank:1}})-[:NEXT*2]->() RETURN {expression}");
        sizes.push(crabnet::explain_physical(&query).unwrap().len());
        assert_eq!(rows(&mut db, &query)[0][0].as_list().unwrap().len(), 1);
    }
    assert!(sizes[1] < sizes[0] * 3, "{sizes:?}");
}

#[test]
fn grouped_output_errors_are_terminal_for_cursors() {
    let mut db = graph();
    let params = Parameters::from([("bad".into(), json!("text"))]);
    let query = "MATCH p=(:Step {rank:1})-[:NEXT]->() WITH `collect`(p) AS routes UNWIND [0,1] AS i RETURN CASE WHEN i=0 THEN routes ELSE $bad-1 END";
    assert!(matches!(
        db.execute_with(query, &params),
        Err(Error::Type(_))
    ));
    let mut read = db.prepare_read_with(query, &params).unwrap();
    let mut cursor = read.cursor().unwrap();
    let row = cursor.next().unwrap().unwrap();
    assert!(row[0].as_list().unwrap()[0].as_path().is_some());
    assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
    assert!(cursor.next().is_none());
}
