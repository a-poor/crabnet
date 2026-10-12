use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn value(value: impl Into<serde_json::Value>) -> CypherValue {
    CypherValue::try_from(value.into()).unwrap()
}

fn list(values: impl IntoIterator<Item = CypherValue>) -> CypherValue {
    CypherValue::List(values.into_iter().collect())
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
fn collected_entities_keep_order_duplicates_and_identity() {
    let mut db = graph();
    for pattern in ["(x:Step)", "()-[x:NEXT]->()"] {
        let result = rows(
            &mut db,
            &format!(
                "MATCH {pattern} WITH x ORDER BY x.rank \
             WITH collect(x) AS xs WITH xs + [head(xs)] AS xs \
             RETURN xs, head(xs), last(xs), tail(xs), reverse(xs), head(reverse(xs))"
            ),
        );
        let row = &result[0];
        let CypherValue::List(xs) = &row[0] else {
            panic!("list")
        };
        assert_eq!(row[1], xs[0]);
        assert_eq!(row[2], xs[0]);
        assert_eq!(row[3], list(xs[1..].iter().cloned()));
        assert_eq!(row[4], list(xs.iter().rev().cloned()));
        assert_eq!(row[5], xs[0]);
    }
}

#[test]
fn nested_tuples_preserve_selected_kinds_for_later_matches() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1})-[r:NEXT]->(end) \
         WITH head(tail(reverse([end, n, r]))) AS selected \
         MATCH (selected)-[r:NEXT]->(end) RETURN selected.rank, r.rank, end.rank"
        ),
        [vec![
            CypherValue::Integer(1),
            CypherValue::Integer(10),
            CypherValue::Integer(2)
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1})-[r:NEXT]->() \
         WITH last(reverse([r,n])) AS edge MATCH (a)-[edge]->(b) \
         RETURN a.rank, type(edge), b.rank"
        ),
        [vec![value(1), value("NEXT"), value(2)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH reverse(tail([n,7])) AS xs \
         RETURN head(xs)+1, last(xs)+2, size(xs), tail(xs)"
        ),
        [vec![value(8), value(9), value(1), list([])]]
    );
}

#[test]
fn path_lists_and_nested_paths_work_across_all_transports() {
    let mut db = graph();
    for pattern in [
        "p = (:Step {rank:1})-[r:NEXT]->()-[:NEXT]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*2]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*1..40]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3})",
        "p = shortestPath((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
        "p = allShortestPaths((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
    ] {
        let result = rows(
            &mut db,
            &format!(
                "MATCH {pattern} RETURN nodes(p), relationships(p), \
             head(nodes(p)), last(nodes(p)), tail(nodes(p)), reverse(nodes(p)), \
             head(relationships(p)), last(relationships(p)), tail(relationships(p)), \
             reverse(relationships(p)), p, head(reverse([p, null])), last(reverse([p,null])), \
             head(tail([null, {{path:p}}])).path, length(head([p]))"
            ),
        );
        let row = &result[0];
        let ns = row[0].as_list().unwrap();
        let rs = row[1].as_list().unwrap();
        assert_eq!(row[2], ns[0]);
        assert_eq!(row[3], ns[2]);
        assert_eq!(row[4], list(ns[1..].iter().cloned()));
        assert_eq!(row[5], list(ns.iter().rev().cloned()));
        assert_eq!(row[6], rs[0]);
        assert_eq!(row[7], rs[1]);
        assert_eq!(row[8], list([rs[1].clone()]));
        assert_eq!(row[9], list(rs.iter().rev().cloned()));
        assert_eq!(row[11], CypherValue::Null);
        assert_eq!(row[12], row[10]);
        assert_eq!(row[13], row[10]);
        assert_eq!(row[14], value(2));
        if pattern.contains("rels:") {
            assert_eq!(
                rows(
                    &mut db,
                    &format!(
                        "MATCH {pattern} RETURN [r IN reverse(rels) | r.rank], \
                 type(head(rels)), last(rels).rank, size(tail(rels))"
                    )
                ),
                [vec![
                    value(json!([20, 10])),
                    value("NEXT"),
                    value(20),
                    value(1)
                ]]
            );
        }
    }
}

#[test]
fn empty_null_and_singleton_lists_keep_their_results() {
    let mut db = graph();
    for prefix in [
        "MATCH (n:Absent) WITH collect(n) AS xs",
        "MATCH p=(:Step {rank:1})-[rs:NEXT*0]->() WITH rs AS xs",
        "MATCH (n:Step {rank:1}) WITH tail([n]) AS xs",
        "MATCH (n:Step {rank:1}) WITH [x IN [n] WHERE false | x] AS xs",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!("{prefix} RETURN head(xs), last(xs), tail(xs), reverse(xs)")
            ),
            [vec![
                CypherValue::Null,
                CypherValue::Null,
                list([]),
                list([])
            ]]
        );
    }
    for prefix in [
        "MATCH (n:Step {rank:1}) WITH [n][null..] AS xs",
        "MATCH (n:Step) WITH collect(n)[null..] AS xs",
        "MATCH (n:Step {rank:1}) WITH {node:n,missing:null} AS box WITH box.missing AS xs",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!("{prefix} RETURN head(xs), last(xs), tail(xs), reverse(xs)")
            ),
            [vec![CypherValue::Null; 4]]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "OPTIONAL MATCH (n:Absent) WITH [n] AS xs \
         RETURN head(xs), last(xs), tail(xs), reverse(xs)"
        ),
        [vec![
            CypherValue::Null,
            CypherValue::Null,
            list([]),
            list([CypherValue::Null])
        ]]
    );
    assert!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) UNWIND reverse([n][null..]) AS x RETURN x"
        )
        .is_empty()
    );
}

#[test]
fn nested_members_and_parameter_json_remain_distinct_from_graph_values() {
    let mut db = graph();
    let node = rows(&mut db, "MATCH (n:Step {rank:1}) RETURN n")[0][0].clone();
    let forged =
        json!({"id":node.as_node().unwrap().element_id,"labels":["Step"],"props":{"rank":1}});
    let params: Parameters = [(
        "plain".into(),
        json!([forged, ["n", forged], null, 1.2345678901234567]),
    )]
    .into();
    let query = "MATCH (n:Step {rank:1})-[r:NEXT]->() \
                 WITH n,r,[n,{edge:r, items:[n,null]},[r,n],$plain] AS xs \
                 RETURN n,r,xs,reverse(xs),head(xs),last(xs),tail(xs), \
                        head(reverse(xs)) = $plain";
    let result = db.execute_with(query, &params).unwrap();
    let row = &result.rows[0];
    let xs = row[2].as_list().unwrap();
    assert_eq!(row[3], list(xs.iter().rev().cloned()));
    assert_eq!(row[4], row[0]);
    assert_eq!(row[5], value(params["plain"].clone()));
    assert_eq!(row[6], list(xs[1..].iter().cloned()));
    assert_eq!(row[7], CypherValue::Null); // The compared lists contain null.
    let mut read = db.prepare_read_with(query, &params).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        result.rows
    );
    drop(read);
    assert!(matches!(
        db.execute_with(
            "MATCH (n:Step {rank:1}) WITH reverse([n,head($plain)]) AS xs \
             DETACH DELETE head(xs)",
            &params
        ),
        Err(Error::Type(_))
    ));
    assert_eq!(
        rows(&mut db, "MATCH (n:Step {rank:1}) RETURN n"),
        [vec![node]]
    );
}

#[test]
fn dynamically_selected_plain_lists_and_strings_keep_scalar_behavior() {
    let mut db = graph();
    for plain in [json!([]), json!([1,null,[2],{"key":3}]), json!(null)] {
        let params: Parameters = [("plain".into(), plain), ("i".into(), json!(1))].into();
        let result = db
            .execute_with(
                "MATCH (n:Step {rank:1}) WITH [n,$plain] AS box \
             RETURN head(box[$i]), last(box[$i]), tail(box[$i]), reverse(box[$i]), \
                    head($plain), last($plain), tail($plain), reverse($plain)",
                &params,
            )
            .unwrap();
        assert_eq!(result.rows[0][..4], result.rows[0][4..]);
    }
    for plain in [
        json!("a界🙂e\u{301}"),
        json!(12),
        json!(true),
        json!({"key":1}),
    ] {
        let valid = plain.is_string();
        let params: Parameters = [("plain".into(), plain), ("i".into(), json!(1))].into();
        let result = db.execute_with(
            "MATCH (n:Step {rank:1}) WITH [n,$plain] AS box \
             RETURN reverse(box[$i]), reverse($plain)",
            &params,
        );
        if valid {
            let result = result.unwrap();
            assert_eq!(result.rows[0][0], result.rows[0][1]);
        } else {
            assert!(matches!(result, Err(Error::Type(_))), "{result:?}");
        }
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH n, [[n], [null,n], [n,n]] AS lists \
         RETURN [xs IN lists | head(reverse(xs))], \
                all(xs IN lists WHERE last(reverse(xs)) IS NULL OR last(reverse(xs)) = n)"
        )[0][1],
        value(true)
    );
}

#[test]
fn aggregate_functions_grouping_distinct_and_unions_keep_identity() {
    let mut db = graph();
    let result = rows(
        &mut db,
        "MATCH (n:Step) WITH n ORDER BY n.rank \
         RETURN head(collect(n)).rank, last(collect(n)).rank, \
                [x IN reverse(collect(n)) | x.rank], [x IN tail(collect(n)) | x.rank]",
    );
    assert_eq!(
        result,
        [vec![
            value(1),
            value(3),
            value(json!([3, 2, 1])),
            value(json!([2, 3]))
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) UNWIND [1,2] AS copy \
         WITH reverse([n,null,n]) AS xs RETURN xs,count(*),count(DISTINCT xs)"
        )[0][1..],
        [value(2), value(1)]
    );
    let result = rows(
        &mut db,
        "MATCH (n:Step {rank:1}) UNWIND [1,2] AS copy \
         WITH DISTINCT reverse([n,null,n]) AS xs \
         RETURN head(xs)=last(xs), collect(tail(xs)), size(reverse(xs))",
    );
    assert_eq!(result[0][0], value(true));
    assert_eq!(result[0][1].as_list().unwrap().len(), 1);
    assert_eq!(result[0][2], value(3));
    for (union, expected) in [("UNION", 1), ("UNION ALL", 2)] {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH (n:Step {{rank:1}}) RETURN reverse([null,n]) AS xs {union} \
             MATCH (n:Step {{rank:1}}) RETURN [n,null] AS xs"
                )
            )
            .len(),
            expected
        );
    }
}

#[test]
fn transformed_lists_refresh_mutations_preserve_snapshots_and_write_bags() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Step {rank:1})-[r:NEXT]->() \
         WITH n,r,reverse([r,n,{node:n}]) AS xs \
         SET n.marked=true, r.marked=true SET n:Marked \
         DETACH DELETE last(xs), head(tail(xs)) RETURN n,r,xs",
        )
        .unwrap();
    let row = &result.rows[0];
    let xs = row[2].as_list().unwrap();
    assert_eq!(xs[0].as_map().unwrap()["node"], row[0]);
    assert_eq!(xs[1], row[0]);
    assert_eq!(xs[2], row[1]);
    assert_eq!(row[0].as_node().unwrap().properties["marked"], value(true));
    assert!(row[0].as_node().unwrap().labels.contains(&"Marked".into()));
    assert_eq!(
        row[1].as_relationship().unwrap().properties["marked"],
        value(true)
    );
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 1);
    let result = db
        .execute(
            "MATCH (n:Step {rank:2}) WITH reverse([n,n]) AS xs \
         UNWIND xs AS member CREATE (member)-[:VISIT]->(:Visit) \
         RETURN member.rank",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![value(2)]; 2]);
    assert_eq!(result.summary.nodes_created, 2);
    assert_eq!(result.summary.relationships_created, 2);
}

#[test]
fn invalid_inputs_and_parameters_fail_atomically_and_cursor_errors_are_terminal() {
    let mut db = graph();
    for function in ["head", "last", "tail", "reverse"] {
        for expression in ["n", "r", "p", "{node:n}"] {
            let query = format!("MATCH p=(n:Step)-[r:NEXT]->() RETURN {function}({expression})");
            assert!(
                matches!(db.execute(&query), Err(Error::Semantic(_))),
                "{query}"
            );
        }
        for args in ["", "[n], [n]"] {
            assert!(matches!(
                db.execute(&format!("MATCH (n) RETURN {function}({args})")),
                Err(Error::Semantic(_))
            ));
        }
        assert!(matches!(
            db.execute(&format!(
                "MATCH (n:Absent) CREATE (:Unexpected) RETURN {function}([n,$missing])"
            )),
            Err(Error::MissingParameter(_))
        ));
    }
    let query = "MATCH (n:Step {rank:1}) WITH [[],n] AS xs \
                 UNWIND [0,1] AS i RETURN reverse(xs[i])";
    assert!(matches!(db.execute(query), Err(Error::Type(_))));
    {
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), vec![list([])]);
        assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
        assert!(cursor.next().is_none());
    }
    let query = "MATCH (n:Step {rank:1}) SET n.changed=true \
                 CREATE (:Unexpected) WITH [[],n] AS xs \
                 UNWIND [0,1] AS i RETURN reverse(xs[i])";
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
fn reversal_keeps_depth_limits_and_generated_sql_bounded() {
    let mut db = graph();
    for depth in [1, 30, 100] {
        let nested = format!("{}n{}", "[".repeat(depth), "]".repeat(depth));
        let query = format!("MATCH (n:Step {{rank:1}}) RETURN reverse({nested})");
        crabnet::explain(&query).unwrap();
        let result = rows(&mut db, &query);
        let mut leaf = &result[0][0];
        for _ in 0..depth {
            leaf = &leaf.as_list().unwrap()[0];
        }
        assert!(leaf.as_node().is_some());
    }
    let nested = format!("{}n{}", "[".repeat(99), "]".repeat(99));
    let error = db
        .execute(&format!(
            "CREATE (n:TooDeep) WITH {{items:{nested}}} AS box RETURN reverse([box])"
        ))
        .unwrap_err();
    assert!(matches!(error, Error::Type(_)), "{error:?}");
    assert_eq!(
        rows(&mut db, "MATCH (n:TooDeep) RETURN count(n)"),
        [vec![value(0)]]
    );
    let mut lengths = Vec::new();
    for count in [8, 16] {
        let expression = format!("{}[n]{}", "reverse(".repeat(count), ")".repeat(count));
        let query = format!("MATCH (n:Step {{rank:1}}) RETURN {expression}");
        lengths.push(crabnet::explain_physical(&query).unwrap().len());
        assert_eq!(rows(&mut db, &query)[0][0].as_list().unwrap().len(), 1);
    }
    assert!(lengths[1] < lengths[0] * 3, "{lengths:?}");
}
