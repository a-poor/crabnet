use crabnet::{
    CancellationToken, CypherValue, Database, Error, OpenMode, OpenOptions, Parameters,
    QueryOptions, ReadMode,
};
use serde_json::json;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

#[test]
fn distinct_and_union_stream_first_representatives() {
    let mut db = Database::in_memory().unwrap();
    for query in [
        "UNWIND [1,1.0,2,null,null] AS x RETURN DISTINCT x",
        "RETURN 1 AS x UNION RETURN 1.0 AS x UNION RETURN 2 AS x UNION RETURN null AS x",
        "RETURN 1 AS x UNION DISTINCT RETURN 1.0 AS x UNION DISTINCT RETURN 2 AS x UNION DISTINCT RETURN null AS x",
    ] {
        let expected = db.execute(query).unwrap();
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.columns(), &["x"]);
            assert_eq!(
                cursor
                    .by_ref()
                    .collect::<crabnet::Result<Vec<_>>>()
                    .unwrap(),
                expected.rows
            );
            assert!(cursor.next().is_none());
        }
    }
}

#[test]
fn pagination_counts_unique_rows_and_does_not_evaluate_the_unread_suffix() {
    let mut db = Database::in_memory().unwrap();
    let mut read = db
        .prepare_read("UNWIND [[1],[1],[2],'wrong'] AS v RETURN DISTINCT v[0] AS x SKIP 1 LIMIT 1")
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
    assert!(cursor.next().is_none());
    assert!(cursor.next().is_none());
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

#[test]
fn typed_scalar_equivalence_and_first_representatives_match_eager() {
    let mut db = Database::in_memory().unwrap();
    let parameters = Parameters::from([(
        "values".into(),
        json!([
            null,null,true,1,1.0,false,0,-0.0,"1","é\u{0000}",
            9007199254740992_i64,9007199254740992.0,9007199254740993_i64,
            i64::MAX,i64::MAX as f64,i64::MIN,i64::MIN as f64,
            [1,null],[1.0,null],{"a":[1,null],"b":false},{"b":false,"a":[1.0,null]},[],{}
        ]),
    )]);
    for query in [
        "UNWIND $values AS x RETURN DISTINCT x",
        "UNWIND $values AS x RETURN x UNION UNWIND $values AS x RETURN x",
        "UNWIND $values AS x RETURN DISTINCT x, x IS NULL AS absent",
    ] {
        let expected = db.execute_with(query, &parameters).unwrap();
        let mut read = db.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            expected.rows,
            "{query}"
        );
    }
}

#[test]
fn graph_entities_nested_paths_and_ordinary_maps_keep_identity_rules() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Item {x:1}),(b:Item {x:1}),(a)-[:R {x:1}]->(b),(a)-[:R {x:1}]->(b),(a)-[:R]->(a)").unwrap();
    for query in [
        "MATCH (n:Item) UNWIND [0,1] AS copy RETURN DISTINCT n",
        "MATCH ()-[r:R]->() RETURN r AS x UNION MATCH ()-[r:R]->() RETURN r AS x",
        "MATCH (n:Item) RETURN n AS x UNION MATCH (n:Item) RETURN properties(n) AS x UNION RETURN null AS x",
        "MATCH p=()-[:R]->() UNWIND [0,1] AS copy RETURN DISTINCT [p,{ns:nodes(p),rs:relationships(p)}] AS x",
        "MATCH p=()-[:R]->() RETURN [p] AS x UNION MATCH p=()<-[:R]-() RETURN [p] AS x",
        "MATCH (n:Item) RETURN collect(n) AS x UNION MATCH (n:Item) RETURN collect(n) AS x",
        "MATCH (n:Item) OPTIONAL MATCH (n)-[r:R]->() RETURN DISTINCT n,r",
    ] {
        let expected = db.execute(query).unwrap();
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
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
fn branch_local_distinct_and_pages_run_before_the_union_policy() {
    let mut db = Database::in_memory().unwrap();
    for (query, expected) in [
        (
            "UNWIND [1,1,2,3] AS x RETURN DISTINCT x SKIP 1 LIMIT 1 UNION ALL UNWIND [2,2,4] AS x RETURN DISTINCT x",
            vec![2, 2, 4],
        ),
        (
            "UNWIND [1,1,2,3] AS x RETURN DISTINCT x SKIP 1 LIMIT 1 UNION UNWIND [2,2,4] AS x RETURN DISTINCT x",
            vec![2, 4],
        ),
        (
            "UNWIND [3,1,2,1] AS x RETURN DISTINCT x ORDER BY x DESC SKIP 1 LIMIT 2 UNION RETURN 4 AS x",
            vec![2, 1, 4],
        ),
        (
            "UNWIND [1,1,2] AS x RETURN DISTINCT x SKIP 99 UNION RETURN 4 AS x",
            vec![4],
        ),
        (
            "UNWIND ['bad'] AS v RETURN DISTINCT v[0] AS x LIMIT 0 UNION RETURN 4 AS x",
            vec![4],
        ),
        (
            "MATCH (n:Missing) RETURN DISTINCT n AS x UNION MATCH (n:Missing) RETURN n AS x",
            vec![],
        ),
    ] {
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        for _ in 0..2 {
            assert_eq!(
                read.cursor()
                    .unwrap()
                    .collect::<crabnet::Result<Vec<_>>>()
                    .unwrap(),
                expected
                    .iter()
                    .map(|x| vec![CypherValue::Integer(*x)])
                    .collect::<Vec<_>>(),
                "{query}"
            );
        }
    }
    // A long left-associated union chain must not keep a seen set per prefix.
    let query = (0..100)
        .map(|i| format!("RETURN {} AS x", i % 3))
        .collect::<Vec<_>>()
        .join(" UNION ");
    let mut read = db.prepare_read(&query).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [0, 1, 2].map(|x| vec![CypherValue::Integer(x)])
    );
}

#[test]
fn budgets_count_all_inputs_but_only_surviving_results() {
    let mut db = Database::in_memory().unwrap();
    for options in [
        QueryOptions {
            max_materialized_rows: Some(2),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_materialized_bytes: Some(2),
            ..QueryOptions::default()
        },
    ] {
        for query in [
            "UNWIND [1,1,2] AS x RETURN DISTINCT x",
            "RETURN 1 AS x UNION RETURN 1 AS x UNION RETURN 2 AS x",
        ] {
            let mut read = db
                .prepare_read_with_options(query, &Parameters::new(), &options)
                .unwrap();
            for _ in 0..2 {
                let mut cursor = read.cursor().unwrap();
                assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
                assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
                assert!(cursor.next().is_none());
            }
        }
    }
    for options in [
        QueryOptions {
            max_result_rows: Some(1),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_bytes: Some(16),
            ..QueryOptions::default()
        },
    ] {
        for query in [
            "UNWIND [1,1,1] AS x RETURN DISTINCT x",
            "RETURN 1 AS x UNION RETURN 1 AS x",
            "UNWIND [1,1,2,2] AS x RETURN DISTINCT x SKIP 1",
        ] {
            let mut read = db
                .prepare_read_with_options(query, &Parameters::new(), &options)
                .unwrap();
            assert_eq!(
                read.cursor()
                    .unwrap()
                    .collect::<crabnet::Result<Vec<_>>>()
                    .unwrap()
                    .len(),
                1
            );
        }
        let mut read = db
            .prepare_read_with_options(
                "RETURN 1 AS x UNION RETURN 2 AS x",
                &Parameters::new(),
                &options,
            )
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        cursor.next().unwrap().unwrap();
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
    let zero = QueryOptions {
        max_materialized_rows: Some(0),
        max_materialized_bytes: Some(0),
        max_result_rows: Some(0),
        max_result_bytes: Some(0),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            "UNWIND ['bad'] AS v RETURN DISTINCT v[0] LIMIT 0",
            &Parameters::new(),
            &zero,
        )
        .unwrap();
    assert!(read.cursor().unwrap().next().is_none());
}

#[test]
fn duplicates_do_not_hide_later_errors_and_early_drop_skips_them() {
    let mut db = Database::in_memory().unwrap();
    for query in [
        "UNWIND [[1],[1],'bad'] AS v RETURN DISTINCT v[0] AS x",
        "RETURN 1 AS x UNION RETURN 1.0 AS x UNION UNWIND ['bad'] AS v RETURN v[0] AS x",
    ] {
        let mut read = db.prepare_read(query).unwrap();
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            drop(cursor);
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
            assert!(cursor.next().is_none());
            assert!(cursor.next().is_none());
        }
    }
}

#[test]
fn preparation_checks_even_unread_branches_and_zero_pages() {
    let mut db = Database::in_memory().unwrap();
    for query in [
        "RETURN DISTINCT $missing LIMIT 0",
        "RETURN 1 AS x UNION RETURN DISTINCT $missing AS x LIMIT 0",
        "MATCH (n:Missing) RETURN DISTINCT n SKIP $missing",
    ] {
        assert!(
            matches!(db.prepare_read(query), Err(Error::MissingParameter(_))),
            "{query}"
        );
    }
    for query in [
        "RETURN DISTINCT 1 LIMIT -1",
        "RETURN 1 AS x UNION RETURN 2 AS x SKIP -1",
        "RETURN 1 AS x UNION RETURN 2 AS y",
    ] {
        assert!(
            matches!(db.prepare_read(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    assert!(matches!(
        db.prepare_read("CREATE (:Bad) RETURN DISTINCT 1"),
        Err(Error::Unsupported(_))
    ));
    let parameters = Parameters::from([("skip".into(), json!(1)), ("limit".into(), json!(1))]);
    let mut read = db
        .prepare_read_with(
            "UNWIND [1,1,2,3] AS x RETURN DISTINCT x SKIP $skip LIMIT $limit",
            &parameters,
        )
        .unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(2)]]
    );
}

#[test]
fn work_limits_and_cancellation_apply_while_searching_for_a_new_row() {
    let mut db = Database::in_memory().unwrap();
    let query = vec!["RETURN 1 AS x"; 100].join(" UNION ");
    let options = QueryOptions {
        max_sqlite_steps: Some(1000),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(&query, &Parameters::new(), &options)
        .unwrap();
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        cursor.next().unwrap().unwrap();
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
    drop(read);
    let token = CancellationToken::new();
    let options = QueryOptions {
        cancellation: Some(token.clone()),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(&query, &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    token.cancel();
    assert!(matches!(cursor.next(), Some(Err(Error::Cancelled))));
    assert!(cursor.next().is_none());
    drop(cursor);
    assert!(matches!(read.cursor(), Err(Error::Cancelled)));
    drop(read);
    let deadline = Instant::now() + Duration::from_secs(1);
    let options = QueryOptions {
        deadline: Some(deadline),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(&query, &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    assert!(matches!(cursor.next(), Some(Err(Error::DeadlineExceeded))));
    assert!(cursor.next().is_none());
}

fn file_options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        busy_timeout: Duration::ZERO,
    }
}

#[test]
fn snapshot_spans_distinct_branches_and_reuse_sees_committed_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut reader = Database::open(&path).unwrap();
    reader
        .execute("CREATE (:Item {x:1}),(:Item {x:1})")
        .unwrap();
    let mut writer =
        Database::open_with_options(&path, &file_options(OpenMode::ReadWrite)).unwrap();
    let mut read = reader
        .prepare_read("RETURN 0 AS x UNION MATCH (n:Item) RETURN DISTINCT n.x AS x ORDER BY x")
        .unwrap();
    writer.execute("CREATE (:Item {x:2})").unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(0)]);
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
    drop(cursor);
    writer.execute("CREATE (:Item {x:3})").unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(
        cursor
            .by_ref()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [0, 1, 2, 3].map(|x| vec![CypherValue::Integer(x)])
    );
    writer.execute("CREATE (:Item {x:4})").unwrap();
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(read.cursor().unwrap());
    writer.execute("CREATE (:AfterUnconsumedDrop)").unwrap();
}

#[test]
fn errors_panics_and_completed_pages_release_statements_and_seen_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut reader = Database::open(&path).unwrap();
    let mut writer =
        Database::open_with_options(&path, &file_options(OpenMode::ReadWrite)).unwrap();
    for (query, options) in [
        (
            "RETURN 1 AS x UNION UNWIND ['bad'] AS v RETURN v[0] AS x",
            QueryOptions::default(),
        ),
        (
            "RETURN 1 AS x UNION RETURN 2 AS x",
            QueryOptions {
                max_result_rows: Some(1),
                ..QueryOptions::default()
            },
        ),
    ] {
        let mut read = reader
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        cursor.next().unwrap().unwrap();
        assert!(cursor.next().unwrap().is_err());
        writer.execute("CREATE (:AfterError)").unwrap();
        assert!(cursor.next().is_none());
    }
    let mut read = reader
        .prepare_read("UNWIND [1,1,2] AS x RETURN DISTINCT x LIMIT 1")
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    assert!(cursor.next().is_none());
    writer.execute("CREATE (:AfterPage)").unwrap();
    drop(cursor);
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let mut cursor = read.cursor().unwrap();
            cursor.next().unwrap().unwrap();
            panic!("consumer panic");
        }))
        .is_err()
    );
    writer.execute("CREATE (:AfterPanic)").unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn read_only_and_busy_start_retain_distinct_cursor_reusability() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    drop(Database::open(&path).unwrap());
    let mut reader = Database::open_with_options(&path, &file_options(OpenMode::ReadOnly)).unwrap();
    let mut read = reader
        .prepare_read("RETURN 1 AS x UNION RETURN 1.0 AS x")
        .unwrap();
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(matches!(read.cursor(), Err(Error::Busy(_))));
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(1)]]
    );
}
