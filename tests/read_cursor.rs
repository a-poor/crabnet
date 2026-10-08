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
fn simple_cursor_fetches_only_consumed_rows_and_preserves_duplicates() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {x: 1}), (:Item {x: 1}), (:Item {x: 2})")
        .unwrap();
    let options = QueryOptions {
        max_materialized_rows: Some(1),
        ..QueryOptions::default()
    };
    let mut read = database
        .prepare_read_with_options(
            "MATCH (n:Item) RETURN n.x AS x",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    assert_eq!(read.columns(), &["x"]);
    let mut cursor = read.cursor().unwrap();
    let first = cursor.next().unwrap().unwrap();
    assert_eq!(first.len(), 1);
    drop(cursor);
    // A fresh cursor resets counters and restarts the query.
    assert_eq!(read.cursor().unwrap().next().unwrap().unwrap(), first);
    drop(read);
    let mut read = database
        .prepare_read("MATCH (n:Item) RETURN n.x ORDER BY n.x")
        .unwrap();
    let rows = read
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(2)]
        ]
    );
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

#[test]
fn incremental_reads_match_eager_types_entities_nulls_parameters_and_ordering() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (a:Item {x: 1, data: {a: [true, null, 'é', 1.2345678901234567]}}), (b:Item {x: 2}), (a)-[:LINK {v: 3}]->(b), (a)-[:LINK {v: 3}]->(b)").unwrap();
    let parameters = Parameters::from([
        (
            "values".into(),
            json!([null, true, 1, 1.0, "'\\é", {"a":[1,null]}]),
        ),
        ("skip".into(), json!(1)),
        ("limit".into(), json!(2)),
    ]);
    for (query, ordered) in [
        ("UNWIND $values AS value RETURN value", false),
        (
            "MATCH (a)-[r:LINK]->(b) RETURN a, r, b, a.data, a.missing",
            false,
        ),
        (
            "MATCH (a:Item) OPTIONAL MATCH (a)-[r:LINK]->(b) RETURN a, r, b",
            false,
        ),
        ("MATCH (a:Item) RETURN collect(a) AS nodes", false),
        (
            "UNWIND [3, 1, 1, 2] AS x RETURN x ORDER BY x SKIP $skip LIMIT $limit",
            true,
        ),
        (
            "MATCH (a:Item) RETURN a.x AS x, count(*) AS count ORDER BY x DESC",
            true,
        ),
        ("MATCH ()-[:LINK*1..2]->(a) RETURN a", false),
        (
            "RETURN null, true, 1, 1.2345678901234567, [1, null], {a: 'é'}",
            true,
        ),
        ("MATCH (n:Missing) RETURN n", false),
    ] {
        let expected = database.execute_with(query, &parameters).unwrap();
        let mut read = database.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        assert_eq!(read.columns(), expected.columns);
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.columns(), expected.columns);
        let actual = cursor
            .by_ref()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap();
        assert!(cursor.next().is_none());
        assert!(cursor.next().is_none());
        if ordered {
            assert_eq!(actual, expected.rows, "{query}");
        } else {
            assert_eq!(bag(actual), bag(expected.rows), "{query}");
        }
    }
}

#[test]
fn buffered_plans_preserve_union_distinct_pagination_hydration_and_traversal() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (a:Start)-[:LINK]->(b:End), (a)-[:LINK]->(b)")
        .unwrap();
    for query in [
        "MATCH p = (a)-[:LINK]->(b) RETURN p, nodes(p), relationships(p)",
        "MATCH p = (a)-[:LINK]->(b) RETURN DISTINCT p SKIP 1 LIMIT 1",
        "MATCH (:Start)-[:LINK*1..]->(b) RETURN b",
        "MATCH p = shortestPath((:Start)-[:LINK*]->(:End)) RETURN p",
    ] {
        let expected = database.execute(query).unwrap();
        let mut read = database.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Buffered, "{query}");
        assert_eq!(read.columns(), expected.columns);
        let actual = read
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(bag(actual), bag(expected.rows), "{query}");
    }
    let options = QueryOptions {
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    let mut read = database
        .prepare_read_with_options(
            "MATCH (:Start)-[:LINK*1..]->(n) RETURN n AS x UNION RETURN 2 AS x",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    assert!(matches!(read.cursor(), Err(Error::ResourceLimit(_))));
    drop(read);
    database.execute("CREATE (:AfterFailure)").unwrap();
}

#[test]
fn preflight_rejects_writes_and_resolves_every_branch_before_execution() {
    let mut database = Database::in_memory().unwrap();
    for query in [
        "CREATE (:Bad)",
        "MATCH (n) SET n.x = 1",
        "MATCH (n) DETACH DELETE n",
    ] {
        assert!(
            matches!(database.prepare_read(query), Err(Error::Unsupported(_))),
            "{query}"
        );
    }
    assert!(matches!(
        database.prepare_read("RETURN $missing"),
        Err(Error::MissingParameter(_))
    ));
    let mut options = QueryOptions::default();
    options.traversal.max_states = 0;
    assert!(matches!(
        database.prepare_read_with_options(
            "MATCH ()-[:LINK*1..]->(n) RETURN n AS x UNION ALL RETURN $missing AS x",
            &Parameters::new(),
            &options
        ),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(
        database.prepare_read("RETURN 1 LIMIT -1"),
        Err(Error::Semantic(_))
    ));
    assert!(
        database
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    // The preparation owns its input values and options.
    let mut read = database
        .prepare_read_with(
            "RETURN $x",
            &Parameters::from([("x".into(), json!({"a": [1, null]}))]),
        )
        .unwrap();
    assert_eq!(read.cursor().unwrap().count(), 1);
}

#[test]
fn runtime_errors_follow_prior_rows_once_then_close_the_cursor() {
    let mut database = Database::in_memory().unwrap();
    let mut read = database
        .prepare_read("UNWIND [[1], 'wrong'] AS x RETURN x[0]")
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap(),
            vec![CypherValue::Integer(1)]
        );
        assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
        assert!(cursor.next().is_none());
        assert!(cursor.next().is_none());
    }
    drop(read);
    database.execute("CREATE (:AfterError)").unwrap();
}

#[test]
fn row_and_byte_limits_are_cumulative_exact_and_reset_for_each_cursor() {
    let mut database = Database::in_memory().unwrap();
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
        let mut read = database
            .prepare_read_with_options("UNWIND [1, 2] AS x RETURN x", &Parameters::new(), &options)
            .unwrap();
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(
                cursor.next().unwrap().unwrap(),
                vec![CypherValue::Integer(1)]
            );
            assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
            assert!(cursor.next().is_none());
        }
        drop(read);
        let mut exact = database
            .prepare_read_with_options("RETURN 1", &Parameters::new(), &options)
            .unwrap();
        assert_eq!(
            exact
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap()
                .len(),
            1
        );
    }
    let zero = QueryOptions {
        max_result_rows: Some(0),
        max_result_bytes: Some(0),
        max_materialized_rows: Some(0),
        max_materialized_bytes: Some(0),
        ..QueryOptions::default()
    };
    let mut read = database
        .prepare_read_with_options("MATCH (n:Missing) RETURN n", &Parameters::new(), &zero)
        .unwrap();
    assert!(read.cursor().unwrap().next().is_none());
    drop(read);
    let mut read = database
        .prepare_read_with_options("RETURN 1", &Parameters::new(), &zero)
        .unwrap();
    assert!(matches!(
        read.cursor().unwrap().next(),
        Some(Err(Error::ResourceLimit(_)))
    ));
}

#[test]
fn sqlite_instruction_budget_spans_cursor_advances() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("UNWIND range(1, 1000) AS x CREATE (:Item {x: x})")
        .unwrap();
    let options = QueryOptions {
        max_sqlite_steps: Some(1000),
        ..QueryOptions::default()
    };
    let mut read = database
        .prepare_read_with_options("MATCH (n) RETURN n.x", &Parameters::new(), &options)
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
                result => panic!("expected instruction exhaustion after {count} rows: {result:?}"),
            }
        }
        assert!(count > 0 && count < 1000);
        assert!(cursor.next().is_none());
        counts.push(count);
    }
    assert_eq!(counts[0], counts[1]);
    drop(read);
    database.execute("CREATE (:AfterBudget)").unwrap();
}

#[test]
fn cancellation_and_deadlines_apply_before_and_between_advances() {
    let mut database = Database::in_memory().unwrap();
    for query in [
        "UNWIND [1, 2] AS x RETURN x",
        "RETURN 1 AS x UNION ALL RETURN 2 AS x",
        "RETURN 1 AS x UNION RETURN 2 AS x",
    ] {
        let token = CancellationToken::new();
        let options = QueryOptions {
            cancellation: Some(token.clone()),
            ..QueryOptions::default()
        };
        let mut read = database
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(cursor.next().unwrap().is_ok());
        token.cancel();
        assert!(matches!(cursor.next(), Some(Err(Error::Cancelled))));
        assert!(cursor.next().is_none());
        drop(cursor);
        assert!(matches!(read.cursor(), Err(Error::Cancelled)));
        drop(read);
        assert!(matches!(
            database.prepare_read_with_options(query, &Parameters::new(), &options),
            Err(Error::Cancelled)
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let options = QueryOptions {
        deadline: Some(deadline),
        ..QueryOptions::default()
    };
    let mut read = database
        .prepare_read_with_options("UNWIND [1, 2] AS x RETURN x", &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(cursor.next().unwrap().is_ok());
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    assert!(matches!(cursor.next(), Some(Err(Error::DeadlineExceeded))));
    assert!(cursor.next().is_none());
    drop(cursor);
    assert!(matches!(read.cursor(), Err(Error::DeadlineExceeded)));
    drop(read);
    database.execute("CREATE (:AfterCancellation)").unwrap();
}

fn file_options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        busy_timeout: Duration::ZERO,
    }
}

#[test]
fn snapshot_is_pinned_at_cursor_creation_and_early_drop_releases_writer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut reader = Database::open(&path).unwrap();
    reader.execute("CREATE (:Item {x: 1})").unwrap();
    let mut writer =
        Database::open_with_options(&path, &file_options(OpenMode::ReadWrite)).unwrap();
    let mut read = reader.prepare_read("MATCH (n:Item) RETURN n.x").unwrap();
    // Preparing alone holds no read snapshot.
    writer.execute("CREATE (:Item {x: 2})").unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(matches!(
        writer.execute("CREATE (:Item {x: 3})"),
        Err(Error::Busy(_))
    ));
    let first = cursor.next().unwrap().unwrap();
    assert_eq!(first.len(), 1);
    drop(cursor);
    // Even with the prepared handle retained, dropping its cursor releases locks.
    writer.execute("CREATE (:Item {x: 4})").unwrap();
    let mut cursor = read.cursor().unwrap();
    let rows = cursor
        .by_ref()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        bag(rows),
        bag(vec![
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(2)],
            vec![CypherValue::Integer(4)]
        ])
    );
    // Exhaustion releases locks even while the exhausted cursor is retained.
    writer.execute("CREATE (:Item {x: 5})").unwrap();
    assert!(cursor.next().is_none());
    drop(cursor);
    let unconsumed = read.cursor().unwrap();
    drop(unconsumed);
    writer.execute("CREATE (:Item {x: 6})").unwrap();
}

#[test]
fn failures_panics_and_buffered_results_release_file_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut reader = Database::open(&path).unwrap();
    reader
        .execute("CREATE (:Item {x: 1}), (:Item {x: 2})")
        .unwrap();
    let mut writer =
        Database::open_with_options(&path, &file_options(OpenMode::ReadWrite)).unwrap();
    let options = QueryOptions {
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    let mut read = reader
        .prepare_read_with_options("MATCH (n) RETURN n", &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    writer.execute("CREATE (:AfterError)").unwrap();
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
    drop(read);
    let mut read = reader
        .prepare_read("MATCH (n:Item)-[:MISSING*0..]->(m) RETURN m.x AS x UNION RETURN 9 AS x")
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Buffered);
    let cursor = read.cursor().unwrap();
    writer.execute("MATCH (n:Item) DETACH DELETE n").unwrap();
    assert_eq!(
        bag(cursor.collect::<crabnet::Result<Vec<_>>>().unwrap()),
        bag(vec![
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(2)],
            vec![CypherValue::Integer(9)]
        ])
    );
}

#[test]
fn read_only_handles_and_busy_start_failures_remain_reusable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut setup = Database::open(&path).unwrap();
    setup.execute("CREATE (:Item {x: 1})").unwrap();
    drop(setup);
    let mut reader = Database::open_with_options(&path, &file_options(OpenMode::ReadOnly)).unwrap();
    let mut read = reader.prepare_read("MATCH (n) RETURN n").unwrap();
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(matches!(read.cursor(), Err(Error::Busy(_))));
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap()
            .len(),
        1
    );
    drop(read);
    assert!(matches!(
        reader.prepare_read("CREATE ()"),
        Err(Error::Unsupported(_))
    ));
}
