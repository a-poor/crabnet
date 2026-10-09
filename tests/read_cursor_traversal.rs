use crabnet::{Database, Error, Parameters, QueryOptions, ReadMode, TraversalLimits};

#[test]
fn first_trail_and_limit_do_not_enumerate_the_unread_suffix() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (a:Start)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let query = "MATCH p=(:Start)-[:R*]->() RETURN length(p)";
    assert!(matches!(
        database.execute_with_options(query, &Parameters::new(), &options),
        Err(Error::ResourceLimit(_))
    ));
    let mut read = database
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap()[0], serde_json::json!(1));
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    assert!(cursor.next().is_none());
    drop(cursor);
    assert_eq!(
        read.cursor().unwrap().next().unwrap().unwrap()[0],
        serde_json::json!(1)
    );
    drop(read);
    let mut read = database
        .prepare_read_with_options(&format!("{query} LIMIT 1"), &Parameters::new(), &options)
        .unwrap();
    let rows = read
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn constrained_cursors_do_not_load_unrelated_edges_or_the_unread_neighborhood() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:Frontier)").unwrap();
    db.execute("UNWIND range(1, 200) AS i CREATE (n:Unrelated)-[:R]->(n)")
        .unwrap();
    let query = "MATCH p=(:Start)-[:R*]->() RETURN length(p)";
    // One seed, one neighbor edge, one consuming result. Unrelated edges
    // must not use transfer or retained-memory budgets, even at exhaustion.
    let options = QueryOptions {
        max_materialized_rows: Some(3),
        traversal: TraversalLimits {
            max_memory_bytes: 2_000,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let rows = db
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows, [vec![crabnet::CypherValue::Integer(1)]]);

    db.execute("MATCH (f:Frontier) UNWIND range(1, 200) AS i CREATE (f)-[:R]->()")
        .unwrap();
    let mut read = db
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap()[0], serde_json::json!(1));
        // Only advancing into the high-degree frontier pays for its neighbors.
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
    drop(read);
    let mut read = db
        .prepare_read_with_options(&format!("{query} LIMIT 1"), &Parameters::new(), &options)
        .unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        rows
    );
}

#[test]
fn indexed_cursor_neighbors_keep_typed_filters_and_report_both_execution_sources() {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Start), (b:End), (a)-[:R {open:true}]->(b),
        (a)-[:R {open:true}]->(b), (a)-[:R {open:false}]->(b),
        (a)-[:R {open:1}]->(b), (a)-[:R]->(b), (a)-[:OTHER {open:true}]->(b)",
    )
    .unwrap();
    for (value, expected) in [
        (serde_json::json!(true), 2),
        (serde_json::json!(false), 1),
        (serde_json::json!(1), 1),
        (serde_json::json!(null), 0),
        (serde_json::json!("true"), 0),
    ] {
        let parameters = Parameters::from([("open".into(), value)]);
        for pattern in [
            "(:Start)-[:R* {open:$open}]->(:End)",
            "(:End)<-[:R* {open:$open}]-(:Start)",
            "(:Start)-[:R* {open:$open}]-(:End)",
        ] {
            let query = format!("MATCH p={pattern} RETURN length(p)");
            let rows = db
                .prepare_read_with(&query, &parameters)
                .unwrap()
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(rows.len(), expected, "{query}");
            // Typed filters and parallel relationships retain the eager bag.
            assert_eq!(
                bag(rows),
                bag(db.execute_with(&query, &parameters).unwrap().rows)
            );
        }
    }
    let physical = crabnet::explain_physical("MATCH p=(:Start)-[*]->() RETURN length(p)").unwrap();
    assert!(physical.contains("edge-source:"));
    assert!(physical.contains("cursor-neighbor-source:"));
    assert!(physical.contains("?1 current-node"));
    let physical = crabnet::explain_physical("MATCH p=(:Start)-[*]->() RETURN count(*)").unwrap();
    assert!(physical.contains("edge-source:"));
    assert!(!physical.contains("neighbor-source:"));
}

fn bag(mut rows: Vec<Vec<crabnet::CypherValue>>) -> Vec<Vec<crabnet::CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

fn fixture() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:S {x:1, data:{v:[true,null,'é']}}),(b {x:2}),(c {x:3}),(d:S {x:4}), (a)-[:R {open:true}]->(b),(a)-[:R {open:true}]->(b),(b)-[:R {open:true}]->(c),(c)-[:R {open:true}]->(a),(b)-[:R {open:false}]->(b),(d)-[:OTHER]->(a)").unwrap();
    db
}

#[test]
fn traversal_cursors_preserve_typed_bags_paths_directions_and_predicates() {
    let mut db = fixture();
    let parameters = Parameters::from([
        ("open".into(), serde_json::json!(true)),
        ("min".into(), serde_json::json!(1)),
        ("value".into(), serde_json::json!([1,1.0,null,{"v":true}])),
    ]);
    for query in [
        "MATCH p=(a:S)-[rs:R*0..40]->(b) RETURN a,b,p,nodes(p),relationships(p),rs,a.data,b.missing,$value",
        "MATCH p=(a:S)<-[:R*]-(b) RETURN a.x,b.x,length(p)",
        "MATCH p=(a:S)-[:R*]-(b) WHERE length(p)>$min RETURN b.x,p",
        "MATCH p=(:S)-[:R* {open:$open}]->(b) WHERE b.x IN [1,3] RETURN b,p,[p,{nodes:nodes(p)}]",
        "MATCH p=(a:S)-[:R*0..]->(a) RETURN p,length(p)",
        "MATCH p=(:Missing)-[:R*]->() RETURN p",
        "MATCH p=(:S)-[:MISSING*0..]->(b) RETURN p,b",
        "MATCH p=(:S)-[:R*0..40]->(b:S) RETURN b,length(p)",
        "MATCH p=(:S)-[:R*]->(b) RETURN DISTINCT b",
        "MATCH p=(:S)-[:R*]->(b) RETURN b AS x UNION ALL RETURN null AS x",
        "RETURN null AS x UNION MATCH p=(:S)-[:R*]->(b) RETURN b AS x",
    ] {
        let expected = db.execute_with(query, &parameters).unwrap();
        let mut read = db.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        assert_eq!(read.columns(), expected.columns);
        for _ in 0..2 {
            let actual = read
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(bag(actual), bag(expected.rows.clone()), "{query}");
        }
    }
}

#[test]
fn global_and_joined_traversal_shapes_keep_explicit_buffering() {
    let mut db = fixture();
    for query in [
        "MATCH p=(:S)-[:R*]->(b) RETURN count(*)",
        "MATCH p=(:S)-[:R*]->(b) RETURN b.x,collect(p)",
        "MATCH p=(:S)-[:R*]->(b) RETURN p ORDER BY length(p) LIMIT 1",
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT p,b RETURN p",
        "MATCH p=(:S)-[:R*]->(b), (c) RETURN b,c",
        "MATCH p=(:S)-[:R*]->(b)-[:R]->(c) RETURN p",
        "MATCH p=(:S)-[:R*]->(b)-[:R*]->(c) RETURN p",
        "MATCH p=(:S)-[:R*]->(b) UNWIND nodes(p) AS n WITH DISTINCT n UNWIND [n] AS m RETURN m",
        "MATCH p=shortestPath((:S)-[:R*]->(b)) RETURN p",
        "RETURN null AS x UNION ALL MATCH p=(:S)-[:R*]->() RETURN count(*) AS x",
    ] {
        let expected = db.execute(query).unwrap();
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Buffered, "{query}");
        assert_eq!(
            bag(read
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap()),
            bag(expected.rows),
            "{query}"
        );
    }
}

#[test]
fn pagination_and_union_share_counters_and_release_completed_producers() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let params = Parameters::from([
        ("skip".into(), serde_json::json!(1)),
        ("limit".into(), serde_json::json!(1)),
    ]);
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 2,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let query = "MATCH p=(:S)-[:R*]->() RETURN length(p) AS x SKIP $skip LIMIT $limit";
    let mut read = db
        .prepare_read_with_options(query, &params, &options)
        .unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![crabnet::CypherValue::Integer(2)]]
    );
    drop(read);
    let branch = "MATCH p=(:S)-[:R*]->() RETURN length(p) AS x LIMIT 1";
    let mut read = db
        .prepare_read_with_options(
            &format!("{branch} UNION ALL {branch} UNION ALL {branch}"),
            &Parameters::new(),
            &options,
        )
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    for _ in 0..2 {
        assert_eq!(cursor.next().unwrap().unwrap()[0], serde_json::json!(1));
    }
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    assert!(cursor.next().is_none());
}

#[test]
fn zero_limit_is_lazy_but_all_parameters_are_checked_up_front() {
    let mut db = fixture();
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 0,
            max_memory_bytes: 0,
            max_payload_bytes: 0,
        },
        max_materialized_rows: Some(0),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            "MATCH p=(:S)-[:R*]->() RETURN p LIMIT 0",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    assert!(read.cursor().unwrap().next().is_none());
    drop(read);
    assert!(matches!(
        db.prepare_read("MATCH p=(:S)-[:R*]->() RETURN p SKIP $missing LIMIT 0"),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(
        db.prepare_read_with(
            "MATCH p=(:S)-[:R*]->() RETURN p LIMIT $limit",
            &Parameters::from([("limit".into(), serde_json::json!(-1))])
        ),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        db.prepare_read(
            "RETURN 1 AS x UNION ALL MATCH p=(:S)-[:R* {v:$missing}]->() RETURN p AS x LIMIT 0"
        ),
        Err(Error::MissingParameter(_))
    ));
}

#[test]
fn runtime_errors_and_hydration_read_ahead_are_terminal_after_yielded_rows() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let params = Parameters::from([("values".into(), serde_json::json!([[7], 8, [9]]))]);
    let mut read = db
        .prepare_read_with(
            "MATCH p=(:S)-[:R*]->() RETURN $values[length(p)-1][0]",
            &params,
        )
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap()[0], serde_json::json!(7));
    assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(read);
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 2,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            "MATCH p=(:S)-[:R*]->() RETURN p",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    let first = cursor.next().unwrap().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    assert!(cursor.next().is_none());
    assert!(first[0].as_path().is_some());
}

#[test]
fn traversal_failures_abort_transaction_writes_and_successful_early_drop_keeps_them() {
    let mut db = Database::in_memory().unwrap();
    db.create_unique_constraint("S", "key").unwrap();
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:S {key:'a'})-[:R]->()-[:R]->()")
        .unwrap();
    let mut read = tx
        .prepare_read_with_options(
            "MATCH p=(:S)-[:R*]->() RETURN p",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    let provisional = cursor.next().unwrap().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    drop(cursor);
    assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
    drop(read);
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert!(provisional[0].as_path().is_some());
    assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:S {key:'a'})-[:R]->()-[:R]->()")
        .unwrap();
    let mut read = tx
        .prepare_read_with_options(
            "MATCH p=(:S)-[:R*]->() RETURN length(p)",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    read.cursor().unwrap().next().unwrap().unwrap();
    drop(read);
    tx.commit().unwrap();
    assert_eq!(
        db.execute("MATCH (n:S {key:'a'}) RETURN count(*)")
            .unwrap()
            .rows[0][0],
        serde_json::json!(1)
    );
}

#[test]
fn graph_input_result_and_payload_limits_remain_cumulative_and_reusable() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let query = "MATCH p=(:S)-[:R*]->() RETURN length(p)";
    // One start ID, one neighbor edge, then one consuming SQL result.
    // Advancing fetches the next neighborhood against the same transfer budget.
    let options = QueryOptions {
        max_materialized_rows: Some(3),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    drop(cursor);
    drop(read);
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
                max_payload_bytes: 1,
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
            max_result_rows: Some(0),
            ..QueryOptions::default()
        },
    ] {
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
    assert_eq!(
        db.prepare_read(query)
            .unwrap()
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn snapshots_pin_before_enumeration_and_release_on_drop_error_and_panic() {
    use crabnet::{OpenMode, OpenOptions};
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        time::Duration,
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
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
    let query = "MATCH p=(:S)-[:R*]->() RETURN length(p)";
    let mut read = reader.prepare_read(query).unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    cursor.next().unwrap().unwrap();
    drop(cursor);
    writer.execute("MATCH (e:End) CREATE (e)-[:R]->()").unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap()
            .len(),
        2
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let mut cursor = read.cursor().unwrap();
            cursor.next().unwrap().unwrap();
            panic!("consumer panic");
        }))
        .is_err()
    );
    writer.execute("CREATE (:AfterPanic)").unwrap();
    drop(read);
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut read = reader
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    // A failed cursor releases its snapshot with both handles still alive.
    writer.execute("CREATE (:AfterError)").unwrap();
}

#[test]
fn cancellation_deadlines_and_sqlite_work_limits_clean_up_traversal_state() {
    use crabnet::CancellationToken;
    use std::time::{Duration, Instant};
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() RETURN length(p)";
    for deadline in [false, true] {
        let token = CancellationToken::new();
        let expires = Instant::now() + Duration::from_secs(1);
        let options = QueryOptions {
            cancellation: Some(token.clone()),
            deadline: deadline.then_some(expires),
            ..QueryOptions::default()
        };
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        cursor.next().unwrap().unwrap();
        if deadline {
            std::thread::sleep(expires.saturating_duration_since(Instant::now()));
        } else {
            token.cancel();
        }
        assert!(matches!(
            cursor.next(),
            Some(Err(Error::Cancelled | Error::DeadlineExceeded))
        ));
        assert!(cursor.next().is_none());
    }
    let options = QueryOptions {
        max_sqlite_steps: Some(100),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    assert!(matches!(
        read.cursor().unwrap().collect::<crabnet::Result<Vec<_>>>(),
        Err(Error::ResourceLimit(_))
    ));
    drop(read);
    assert_eq!(
        db.prepare_read(query)
            .unwrap()
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn ordinary_skip_does_not_evaluate_discarded_projection_expressions() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let params = Parameters::from([("values".into(), serde_json::json!([8, [7]]))]);
    let query = "MATCH p=(:S)-[:R*]->() RETURN $values[length(p)-1][0] SKIP 1 LIMIT 1";
    let expected = db.execute_with(query, &params).unwrap();
    let mut read = db.prepare_read_with(query, &params).unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        expected.rows
    );
}

#[test]
fn volatile_projections_keep_their_per_row_sql_sources() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() RETURN length(p),rand(),rand()+1 SKIP 1";
    let mut read = db.prepare_read(query).unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    let rows = read
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], serde_json::json!(2));
    assert!((0.0..1.0).contains(&rows[0][1].as_f64().unwrap()));
    assert!((1.0..2.0).contains(&rows[0][2].as_f64().unwrap()));
}
