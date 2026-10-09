use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions, ReadMode, TraversalLimits};
use serde_json::json;

#[test]
fn unwind_resumes_within_one_trail_and_does_not_evaluate_unread_members() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH p,$xs AS xs
        UNWIND xs AS x WITH x[0] AS v RETURN v";
    let params = Parameters::from([("xs".into(), json!([[7], [8], 9]))]);
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &params, &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(7)]);
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(8)]);
        assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
        assert!(cursor.next().is_none());
    }
    drop(read);
    let mut read = db
        .prepare_read_with_options(&format!("{query} LIMIT 2"), &params, &options)
        .unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(7)], vec![CypherValue::Integer(8)]]
    );
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

fn fixture() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:S {x:1}),(b {x:2}),(c:S {x:3}),
        (a)-[:R {open:true}]->(b),(a)-[:R {open:true}]->(b),
        (b)-[:R {open:false}]->(b),(b)-[:R {open:true}]->(c),(c)-[:R {open:true}]->(a)",
    )
    .unwrap();
    db
}

#[test]
fn unwind_preserves_scalar_graph_nested_and_path_bags() {
    let mut db = fixture();
    let params = Parameters::from([
        (
            "xs".into(),
            json!([null, true, false, 1, 1.0, -0.0, "é,]\\\"", [1,2], {"a":[1,null]}]),
        ),
        ("open".into(), json!(true)),
    ]);
    for query in [
        "MATCH p=(a:S)-[:R*0..40]->(b) UNWIND nodes(p) AS n RETURN p,a,b,n,n.x",
        "MATCH p=(a:S)-[:R*0..]->(b) UNWIND relationships(p) AS r RETURN p,r,type(r)",
        "MATCH p=(a:S)-[rs:R*0..]->(b) UNWIND rs AS r RETURN r,rs,p",
        "MATCH p=(:S)-[:R*]->(b) WITH p,b,nodes(p) AS ns UNWIND ns AS n WITH p,n WHERE n.x>1 RETURN n,p",
        "MATCH p=(:S)-[:R*]->(b) UNWIND [b,b,null] AS n RETURN n,n.x",
        "MATCH p=(:S)-[:R*]->(b) UNWIND [p,{n:b,ns:nodes(p)},null,[p,b]] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->(b) WITH p,{xs:[p,b,null]} AS box UNWIND box.xs AS x RETURN x,p",
        "MATCH p=(:S)-[:R*]->(b) WITH p,$xs AS xs UNWIND xs AS x WITH x,p RETURN x,length(p)",
        "MATCH p=(:S)-[:R* {open:$open}]->(b) WITH * UNWIND reverse(nodes(p)) AS n WITH n AS b,p RETURN b,p",
        "MATCH p=(:S)<-[:R*]-(b) UNWIND nodes(p) AS n RETURN p,n",
        "MATCH p=(:S)-[:R*]-(b) UNWIND relationships(p) AS r RETURN p,r",
        "MATCH p=()-[:R*]->(b) UNWIND [b.x,b.x] AS x RETURN x",
        "MATCH p=(:Missing)-[:R*]->() UNWIND [1,2] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND [] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND null AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND 42 AS x RETURN x",
        "MATCH p=(:S)-[:NONE*0..]->() UNWIND relationships(p) AS x RETURN x",
        "MATCH p=(:S)-[:NONE*0..]->() UNWIND nodes(p) AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH p WHERE false UNWIND [1] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND [1] AS x WITH x WHERE false RETURN x",
    ] {
        let expected = db
            .execute_with(query, &params)
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        let mut read = db
            .prepare_read_with(query, &params)
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        assert_eq!(read.columns(), expected.columns);
        for _ in 0..2 {
            let actual = read
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            assert_eq!(bag(actual), bag(expected.rows.clone()), "{query}");
        }
    }
}

#[test]
fn unwind_final_distinct_union_and_pagination_compose() {
    let mut db = fixture();
    let params = Parameters::from([("skip".into(), json!(1)), ("limit".into(), json!(3))]);
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS x RETURN x SKIP $skip LIMIT $limit",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS x WITH x WHERE x.x>1 RETURN DISTINCT x SKIP $skip LIMIT $limit",
        "MATCH p=(:S)-[:R*]->() UNWIND [p,p] AS x RETURN DISTINCT x SKIP $skip LIMIT $limit",
        "RETURN null AS x UNION ALL MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,1.0,null] AS x RETURN x UNION RETURN 1 AS x",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x RETURN x SKIP 999999",
    ] {
        let expected = db.execute_with(query, &params).unwrap().rows;
        let mut read = db.prepare_read_with(query, &params).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        assert_eq!(
            bag(read
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap()),
            bag(expected),
            "{query}"
        );
    }
}

#[test]
fn unwind_keeps_global_stages_and_joins_buffered() {
    let mut db = fixture();
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x WITH DISTINCT x UNWIND [x] AS y RETURN y",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p UNWIND nodes(p) AS n RETURN n",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH DISTINCT n RETURN n",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH n ORDER BY n.x RETURN n",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH n SKIP 0 RETURN n",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH n LIMIT 0 RETURN n",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n RETURN count(n)",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n RETURN n ORDER BY n.x",
        "UNWIND [1] AS x MATCH p=(:S)-[:R*]->() RETURN p,x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n MATCH (n)-[:R]->(m) RETURN m",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n OPTIONAL MATCH (n)-[:R]->(m) RETURN m",
    ] {
        let expected = db.execute(query).unwrap().rows;
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Buffered, "{query}");
        assert_eq!(
            bag(read
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap()),
            bag(expected)
        );
    }
}

#[test]
fn volatile_values_are_frozen_per_trail_and_evaluated_per_member() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH rand() AS x,length(p) AS hops
        UNWIND [x,x,x] AS y WITH x,y,hops,rand() AS z RETURN x,y,hops,z,z AS copy";
    let rows = db
        .prepare_read(query)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 6);
    for group in rows.as_chunks::<3>().0 {
        for row in group {
            assert_eq!(row[0], row[1]);
            assert_eq!(row[0], group[0][0]);
            assert_eq!(row[3], row[4]);
        }
        assert!(group.iter().skip(1).any(|r| r[3] != group[0][3]));
    }
    assert_ne!(rows[0][0], rows[3][0]);
    let rows = db
        .prepare_read("MATCH p=(:S)-[:R*]->() UNWIND [rand(),rand()] AS x RETURN x")
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().skip(1).any(|r| r != &rows[0]));
}

#[test]
fn skip_only_suppresses_return_and_filters_do_not_consume_the_page() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [8,[7],[9]] AS x RETURN x[0] SKIP 1 LIMIT 1";
    let expected = db.execute(query).unwrap().rows;
    let actual = db
        .prepare_read(query)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(actual, expected);
    for query in [
        "MATCH p=(:S)-[:R*]->() WITH 8[0] AS bad UNWIND [1,2] AS x RETURN bad SKIP 1 LIMIT 1",
        "MATCH p=(:S)-[:R*]->() UNWIND [8,[7],[9]] AS x WITH x[0] AS v RETURN v SKIP 1 LIMIT 1",
        "MATCH p=(:S)-[:R*]->() UNWIND 8[0] AS x RETURN x SKIP 1 LIMIT 1",
    ] {
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(
            matches!(cursor.next(), Some(Err(Error::Type(_)))),
            "{query}"
        );
        assert!(cursor.next().is_none());
    }
    let query =
        "MATCH p=(:S)-[:R*]->() UNWIND [0,1,2,3,4] AS x WITH x WHERE x>1 RETURN x SKIP 1 LIMIT 1";
    assert_eq!(
        db.prepare_read(query)
            .unwrap()
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(3)]]
    );
}

#[test]
fn zero_pages_preflight_all_parameters_and_union_work_is_cumulative() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 0,
            max_memory_bytes: 0,
            max_payload_bytes: 0,
        },
        max_materialized_rows: Some(0),
        ..QueryOptions::default()
    };
    let params = Parameters::new();
    let query = "MATCH p=(:S)-[:R*]->() WITH p,8[0] AS bad UNWIND [bad] AS x RETURN x LIMIT 0";
    assert!(
        db.prepare_read_with_options(query, &params, &options)
            .unwrap()
            .cursor()
            .unwrap()
            .next()
            .is_none()
    );
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND $missing AS x RETURN x LIMIT 0",
        "MATCH p=(:S)-[:R*]->() UNWIND [1] AS x WITH $missing AS v RETURN v LIMIT 0",
        "RETURN 1 AS x UNION ALL MATCH p=(:S)-[:R*]->() UNWIND $missing AS x RETURN x",
    ] {
        assert!(matches!(
            db.prepare_read(query),
            Err(Error::MissingParameter(_))
        ));
    }
    assert!(matches!(
        db.prepare_read("MATCH p=(:S)-[:R*]->() UNWIND [1] AS x RETURN x LIMIT -1"),
        Err(Error::Semantic(_))
    ));
    let branch = "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x RETURN x LIMIT 2";
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(&format!("{branch} UNION ALL {branch}"), &params, &options)
        .unwrap();
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
}

#[test]
fn unwind_transfer_and_output_budgets_count_each_member_and_input_row() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()").unwrap();
    let params = Parameters::new();
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [1,2,3] AS x RETURN x";
    // One start-domain row, one neighbor row, one frozen input row, then
    // one consumer row per member. The second member crosses this budget.
    for options in [
        QueryOptions {
            max_materialized_rows: Some(4),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_rows: Some(1),
            ..QueryOptions::default()
        },
    ] {
        let mut read = db
            .prepare_read_with_options(query, &params, &options)
            .unwrap();
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
            assert!(cursor.next().is_none());
        }
    }
    for options in [
        QueryOptions {
            traversal: TraversalLimits {
                max_memory_bytes: 0,
                ..TraversalLimits::default()
            },
            ..QueryOptions::default()
        },
        QueryOptions {
            traversal: TraversalLimits {
                max_payload_bytes: 0,
                ..TraversalLimits::default()
            },
            ..QueryOptions::default()
        },
        QueryOptions {
            max_materialized_bytes: Some(0),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_bytes: Some(0),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_sqlite_steps: Some(100),
            ..QueryOptions::default()
        },
    ] {
        let mut read = db
            .prepare_read_with_options(query, &params, &options)
            .unwrap();
        assert!(matches!(
            read.cursor().unwrap().next(),
            Some(Err(Error::ResourceLimit(_)))
        ));
    }
    // A large list is charged before copying the frozen input from SQLite.
    let params = Parameters::from([("xs".into(), json!(vec!["a".repeat(1024); 256]))]);
    let options = QueryOptions {
        max_materialized_bytes: Some(4096),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            "MATCH p=(:S)-[:R*]->() UNWIND $xs AS x RETURN x LIMIT 1",
            &params,
            &options,
        )
        .unwrap();
    assert!(matches!(
        read.cursor().unwrap().next(),
        Some(Err(Error::ResourceLimit(_)))
    ));
}

#[test]
fn hydration_errors_abort_prior_writes_but_early_drop_and_limit_keep_them() {
    for action in ["error", "drop", "limit"] {
        let mut db = Database::in_memory().unwrap();
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:S)-[:R]->()").unwrap();
        let query = format!(
            "MATCH p=(:S)-[:R*]->() UNWIND [[7],[8],9] AS x RETURN p,x[0]{}",
            if action == "limit" { " LIMIT 1" } else { "" }
        );
        let mut read = tx.prepare_read(&query).unwrap();
        let mut cursor = read.cursor().unwrap();
        let row = cursor.next().unwrap().unwrap();
        assert!(matches!(&row[0],CypherValue::Path(p) if p.relationships.len()==1));
        assert_eq!(row[1], CypherValue::Integer(7));
        if action == "error" {
            // The second hydration batch is discarded on the third member's error.
            assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
            assert!(cursor.next().is_none());
        } else if action == "limit" {
            assert!(cursor.next().is_none());
        }
        drop(cursor);
        if action == "error" {
            assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
        }
        drop(read);
        if action == "error" {
            assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
            assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
        } else {
            tx.execute("CREATE (:Kept)").unwrap();
            tx.commit().unwrap();
            assert_eq!(
                db.execute("MATCH (n:Kept) RETURN count(n)").unwrap().rows,
                [vec![CypherValue::Integer(1)]]
            );
        }
    }
}

#[test]
fn unwind_snapshot_and_member_state_are_released_on_drop_error_and_exhaustion() {
    use crabnet::{CancellationToken, OpenMode, OpenOptions};
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        time::Duration,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.sqlite");
    let mut writer = Database::open_with_options(
        &path,
        &OpenOptions {
            busy_timeout: Duration::ZERO,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    writer.execute("CREATE (:S)-[:R]->(:End)").unwrap();
    let mut reader = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap();
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x RETURN x";
    let token = CancellationToken::new();
    let options = QueryOptions {
        cancellation: Some(token.clone()),
        ..QueryOptions::default()
    };
    let mut read = reader
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    token.cancel();
    assert!(matches!(cursor.next(), Some(Err(Error::Cancelled))));
    writer.execute("CREATE (:AfterError)").unwrap();
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(read);
    let mut read = reader.prepare_read(query).unwrap();
    for action in ["drop", "exhaust", "panic"] {
        if action == "panic" {
            assert!(
                catch_unwind(AssertUnwindSafe(|| {
                    let mut cursor = read.cursor().unwrap();
                    cursor.next().unwrap().unwrap();
                    panic!("consumer panic");
                }))
                .is_err()
            );
        } else {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            if action == "exhaust" {
                assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
                assert!(cursor.next().is_none());
                writer.execute("CREATE (:Exhausted)").unwrap();
            }
        }
        writer.execute("CREATE (:Released)").unwrap();
    }
}
