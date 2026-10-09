use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

use crabnet::{
    CancellationToken, CypherValue, Database, Error, OpenMode, OpenOptions, Parameters,
    QueryOptions, ReadMode,
};
use serde_json::json;

#[test]
fn union_all_streams_branches_and_preserves_duplicate_rows() {
    let mut db = Database::in_memory().unwrap();
    let query = "RETURN 1 AS x UNION ALL MATCH (n:Missing) RETURN n AS x \
                 UNION ALL UNWIND [1,2] AS x RETURN x";
    let expected = db.execute(query).unwrap();
    let mut read = db.prepare_read(query).unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    assert_eq!(read.columns(), &["x"]);
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
        assert!(cursor.next().is_none());
    }
}

#[test]
fn early_drop_does_not_evaluate_or_charge_later_branches() {
    let mut db = Database::in_memory().unwrap();
    let options = QueryOptions {
        max_materialized_rows: Some(1),
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            "RETURN 1 AS x UNION ALL UNWIND ['wrong'] AS x RETURN x[0] AS x",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
        drop(cursor);
    }
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

#[test]
fn branch_shapes_parameters_and_sql_modifiers_match_eager_results() {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Item {x:1}),(b:Item {x:2}),(a)-[:LINK {x:3}]->(b),(a)-[:LINK {x:3}]->(b)",
    )
    .unwrap();
    let parameters = Parameters::from([
        (
            "first".into(),
            json!([null, true, 1, 1.0, "é", {"a":[1,null]}]),
        ),
        ("second".into(), json!({"id":"ordinary", "labels":["Item"]})),
        ("skip".into(), json!(1)),
        ("limit".into(), json!(2)),
    ]);
    for query in [
        "UNWIND $first AS v RETURN v AS x UNION ALL RETURN $second AS x",
        "MATCH (n:Item) RETURN n AS x UNION ALL MATCH ()-[r:LINK]->() RETURN r AS x UNION ALL RETURN null AS x",
        "MATCH (n:Item) OPTIONAL MATCH (n)-[r:LINK]->() RETURN r AS x UNION ALL RETURN {a:[1,null]} AS x",
        "MATCH (n:Item) RETURN n AS x,n.x AS y UNION ALL MATCH ()-[r:LINK]->(n) RETURN r.x AS x,n AS y",
        "MATCH (n:Item) RETURN collect(n) AS x UNION ALL MATCH p=()-[:LINK]->() RETURN [p,{nodes:nodes(p)}] AS x",
        "UNWIND [3,1,1,2] AS x RETURN x ORDER BY x SKIP $skip LIMIT $limit UNION ALL UNWIND [2,2,4] AS x RETURN x ORDER BY x DESC",
        "MATCH (n:Item) RETURN n.x AS x,count(*) AS c UNION ALL RETURN 9 AS x,0 AS c",
        "MATCH ()-[:LINK*1..2]->(n) RETURN n AS x UNION ALL MATCH (n:Item) RETURN n AS x",
        "MATCH (n:Missing) RETURN n AS x UNION ALL MATCH (n:Missing) RETURN n AS x",
        "UNWIND [1,1,2] AS x RETURN DISTINCT x SKIP 1 LIMIT 1 UNION ALL RETURN 2 AS x",
    ] {
        let expected = db.execute_with(query, &parameters).unwrap();
        let mut read = db.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        assert_eq!(read.columns(), expected.columns);
        let actual = read
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(bag(actual), bag(expected.rows), "{query}");
    }
    let mut read = db.prepare_read_with(
        "UNWIND [3,1,1,2] AS x RETURN x ORDER BY x SKIP $skip LIMIT $limit UNION ALL UNWIND [2,2,4] AS x RETURN x ORDER BY x DESC",
        &parameters,
    ).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [1, 2, 4, 2, 2].map(|x| vec![CypherValue::Integer(x)])
    );
}

#[test]
fn any_buffered_branch_keeps_the_entire_union_buffered() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Start)-[:LINK]->(b:End),(a)-[:LINK]->(b)")
        .unwrap();
    for branch in [
        "MATCH p=(:Start)-[:LINK*1..40]->() WITH * RETURN p AS x",
        "MATCH p=(:Start)-[:LINK*1..40]->() WITH * RETURN nodes(p) AS x",
        "MATCH p=(:Start)-[:LINK*1..40]->() WITH * RETURN DISTINCT p AS x SKIP 1 LIMIT 1",
        "MATCH (:Start)-[:LINK*1..40]->(n) WITH * RETURN n AS x",
        "MATCH (:Start)-[:LINK*]->(n) WITH * RETURN n AS x",
        "MATCH p=shortestPath((:Start)-[:LINK*]->(:End)) RETURN length(p) AS x",
    ] {
        for query in [
            format!("RETURN null AS x UNION ALL {branch}"),
            format!("{branch} UNION ALL RETURN null AS x"),
            format!("RETURN null AS x UNION {branch}"),
            format!("{branch} UNION RETURN null AS x"),
        ] {
            let expected = db.execute(&query).unwrap();
            let mut read = db.prepare_read(&query).unwrap();
            assert_eq!(read.mode(), ReadMode::Buffered, "{query}");
            assert_eq!(
                bag(read
                    .cursor()
                    .unwrap()
                    .collect::<crabnet::Result<Vec<_>>>()
                    .unwrap()),
                bag(expected.rows)
            );
        }
    }
    for query in [
        "RETURN 1 AS x UNION RETURN 1 AS x",
        "RETURN 1 AS x UNION DISTINCT RETURN 1 AS x",
    ] {
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        assert_eq!(read.cursor().unwrap().count(), 1);
    }
}

#[test]
fn all_branches_preflight_and_later_runtime_errors_are_terminal() {
    let mut db = Database::in_memory().unwrap();
    assert!(matches!(
        db.prepare_read("RETURN 1 AS x UNION ALL RETURN $missing AS x"),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(
        db.prepare_read("RETURN 1 AS x UNION ALL RETURN 2 AS x LIMIT -1"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        db.prepare_read("RETURN 1 AS x UNION ALL RETURN 2 AS y"),
        Err(Error::Semantic(_))
    ));
    let mut read = db.prepare_read(
        "RETURN 1 AS x UNION ALL UNWIND [[2],'wrong'] AS v RETURN v[0] AS x UNION ALL RETURN 3 AS x"
    ).unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
        assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
        assert!(cursor.next().is_none());
        assert!(cursor.next().is_none());
    }
    drop(read);
    db.execute("CREATE (:AfterError)").unwrap();
}

#[test]
fn row_and_byte_limits_span_branches_and_reset_on_reuse() {
    let mut db = Database::in_memory().unwrap();
    for options in [
        QueryOptions {
            max_materialized_rows: Some(1),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_materialized_bytes: Some(1),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_rows: Some(1),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_bytes: Some(16),
            ..QueryOptions::default()
        },
    ] {
        let mut read = db
            .prepare_read_with_options(
                "RETURN 1 AS x UNION ALL MATCH (n:Missing) RETURN n AS x UNION ALL RETURN 1 AS x",
                &Parameters::new(),
                &options,
            )
            .unwrap();
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
            assert!(cursor.next().is_none());
        }
        drop(read);
        let mut read = db.prepare_read_with_options(
            "MATCH (n:Missing) RETURN n AS x UNION ALL RETURN 1 AS x UNION ALL MATCH (n:Missing) RETURN n AS x",
            &Parameters::new(), &options,
        ).unwrap();
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            [vec![CypherValue::Integer(1)]]
        );
    }
    let zero = QueryOptions {
        max_result_rows: Some(0),
        max_result_bytes: Some(0),
        max_materialized_rows: Some(0),
        max_materialized_bytes: Some(0),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            "MATCH (n:Missing) RETURN n AS x UNION ALL MATCH (n:Missing) RETURN n AS x",
            &Parameters::new(),
            &zero,
        )
        .unwrap();
    assert!(read.cursor().unwrap().next().is_none());
}

#[test]
fn sqlite_work_budget_is_shared_even_by_small_branches() {
    let mut db = Database::in_memory().unwrap();
    let options = QueryOptions {
        max_sqlite_steps: Some(1000),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options("RETURN 1 AS x", &Parameters::new(), &options)
        .unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap()
            .len(),
        1
    );
    drop(read);
    let query = vec!["RETURN 1 AS x"; 100].join(" UNION ALL ");
    let mut read = db
        .prepare_read_with_options(&query, &Parameters::new(), &options)
        .unwrap();
    let mut counts = Vec::new();
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        let mut count = 0;
        loop {
            match cursor.next() {
                Some(Ok(_)) => count += 1,
                Some(Err(Error::ResourceLimit(message))) => {
                    assert!(message.contains("SQLite instruction"));
                    break;
                }
                result => panic!("expected shared budget failure after {count} rows: {result:?}"),
            }
        }
        assert!(count > 0 && count < 100);
        assert!(cursor.next().is_none());
        counts.push(count);
    }
    assert_eq!(counts[0], counts[1]);
}

#[test]
fn cancellation_and_deadline_stop_before_the_next_branch() {
    let mut db = Database::in_memory().unwrap();
    // The second branch would raise a type error if it were evaluated.
    let query = "RETURN 1 AS x UNION ALL UNWIND ['wrong'] AS v RETURN v[0] AS x";
    let token = CancellationToken::new();
    let options = QueryOptions {
        cancellation: Some(token.clone()),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &Parameters::new(), &options)
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
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    assert!(matches!(cursor.next(), Some(Err(Error::DeadlineExceeded))));
    assert!(cursor.next().is_none());
    drop(cursor);
    assert!(matches!(read.cursor(), Err(Error::DeadlineExceeded)));
    drop(read);
    db.execute("CREATE (:AfterCancellation)").unwrap();
}

fn file_options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        busy_timeout: Duration::ZERO,
    }
}

#[test]
fn snapshot_spans_branches_and_releases_on_drop_or_exhaustion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut reader = Database::open(&path).unwrap();
    reader.execute("CREATE (:Item {x:1})").unwrap();
    let mut writer =
        Database::open_with_options(&path, &file_options(OpenMode::ReadWrite)).unwrap();
    let mut read = reader
        .prepare_read("RETURN 0 AS x UNION ALL MATCH (n:Item) RETURN n.x AS x ORDER BY x")
        .unwrap();
    // Neither preparing all statements nor an unconsumed, dropped cursor holds a lock.
    writer.execute("CREATE (:Item {x:2})").unwrap();
    drop(read.cursor().unwrap());
    writer.execute("CREATE (:Other)").unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(0)]);
    // The first branch never accesses graph data. The snapshot must already be pinned.
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
    drop(cursor); // Do not request None after the last row.
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
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    drop(cursor); // Drop while later branches are still unexecuted.
    writer.execute("CREATE (:Item {x:5})").unwrap();
}

#[test]
fn errors_cancellation_and_panics_reset_every_statement_before_unlocking() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut reader = Database::open(&path).unwrap();
    let mut writer =
        Database::open_with_options(&path, &file_options(OpenMode::ReadWrite)).unwrap();
    let token = CancellationToken::new();
    for (query, options) in [
        (
            "RETURN 1 AS x UNION ALL UNWIND ['wrong'] AS v RETURN v[0] AS x UNION ALL RETURN 3 AS x",
            QueryOptions::default(),
        ),
        (
            "RETURN 1 AS x UNION ALL RETURN 2 AS x UNION ALL RETURN 3 AS x",
            QueryOptions {
                max_result_rows: Some(1),
                ..QueryOptions::default()
            },
        ),
        (
            "RETURN 1 AS x UNION ALL RETURN 2 AS x UNION ALL RETURN 3 AS x",
            QueryOptions {
                cancellation: Some(token.clone()),
                ..QueryOptions::default()
            },
        ),
    ] {
        let mut read = reader
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        cursor.next().unwrap().unwrap();
        if options.cancellation.is_some() {
            token.cancel();
        }
        assert!(cursor.next().unwrap().is_err());
        writer.execute("CREATE (:AfterError)").unwrap();
        assert!(cursor.next().is_none());
    }
    let mut read = reader
        .prepare_read("RETURN 0 AS x UNION ALL MATCH (n) RETURN n AS x UNION ALL RETURN 2 AS x")
        .unwrap();
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let mut cursor = read.cursor().unwrap();
            cursor.next().unwrap().unwrap();
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
            .unwrap()
            .len(),
        6
    );
}

#[test]
fn read_only_union_cursor_reuses_statements_after_a_busy_start() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut setup = Database::open(&path).unwrap();
    setup.execute("CREATE (:Item {x:1})").unwrap();
    drop(setup);
    let mut reader = Database::open_with_options(&path, &file_options(OpenMode::ReadOnly)).unwrap();
    let mut read = reader
        .prepare_read("RETURN 0 AS x UNION ALL MATCH (n) RETURN n.x AS x")
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(matches!(read.cursor(), Err(Error::Busy(_))));
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [0, 1].map(|x| vec![CypherValue::Integer(x)])
    );
}
