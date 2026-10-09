use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

use crabnet::{
    CancellationToken, CypherValue, Database, Error, OpenMode, OpenOptions, Parameters,
    QueryOptions, ReadMode, Transaction, TraversalLimits,
};
use serde_json::json;

fn assert_aborted(tx: &mut Transaction<'_>) {
    assert!(matches!(
        tx.execute("RETURN 1"),
        Err(Error::TransactionAborted)
    ));
    assert!(matches!(
        tx.prepare_read("RETURN 1"),
        Err(Error::TransactionAborted)
    ));
    assert!(matches!(
        tx.create_unique_constraint("N", "key"),
        Err(Error::TransactionAborted)
    ));
    assert!(matches!(
        tx.drop_unique_constraint("N", "key"),
        Err(Error::TransactionAborted)
    ));
    assert!(matches!(
        tx.unique_constraints(),
        Err(Error::TransactionAborted)
    ));
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

#[test]
fn cursor_reads_prior_writes_and_early_drop_keeps_transaction_open() {
    let mut db = Database::in_memory().unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Item {x:1}),(:Item {x:2})").unwrap();
    {
        let mut read = tx
            .prepare_read("MATCH (n:Item) RETURN n.x ORDER BY n.x")
            .unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
        }
    }
    tx.execute("CREATE (:Item {x:3})").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.execute("MATCH (n:Item) RETURN count(n)").unwrap().rows,
        [vec![CypherValue::Integer(3)]]
    );
}

#[test]
fn a_partial_cursor_error_rolls_back_prior_writes_and_prevents_reuse() {
    let mut db = Database::in_memory().unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Earlier)").unwrap();
    {
        let mut read = tx
            .prepare_read("UNWIND [[1],'bad'] AS v RETURN v[0]")
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
        assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
        assert!(cursor.next().is_none());
        drop(cursor);
        assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
    }
    assert!(matches!(
        tx.execute("RETURN 1"),
        Err(Error::TransactionAborted)
    ));
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
}

#[test]
fn both_modes_match_eager_transaction_results_including_uncommitted_graph_values() {
    let mut db = Database::in_memory().unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (a:Start {x:1,data:{v:['é',null,1.2345678901234567]}}),(b:End {x:2}),(a)-[:LINK {x:3}]->(b),(a)-[:LINK {x:4}]->(b),(b)-[:LINK]->(a)").unwrap();
    let parameters = Parameters::from([
        ("values".into(), json!([null,true,1,1.0,{"id":"ordinary"}])),
        ("skip".into(), json!(1)),
        ("limit".into(), json!(2)),
    ]);
    for (query, mode) in [
        (
            "UNWIND $values AS x RETURN DISTINCT x SKIP $skip LIMIT $limit",
            ReadMode::Incremental,
        ),
        (
            "MATCH (n:Start) RETURN n AS x UNION ALL RETURN null AS x",
            ReadMode::Incremental,
        ),
        (
            "MATCH p=(:Start)-[r:LINK*0..2]->() RETURN p,nodes(p),relationships(p),r",
            ReadMode::Incremental,
        ),
        (
            "MATCH p=(a:Start)-[r:LINK]->(b) RETURN p,a,r,b,[p,{nodes:nodes(p)}]",
            ReadMode::Incremental,
        ),
        (
            "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,130) AS i RETURN DISTINCT p,i",
            ReadMode::Incremental,
        ),
        (
            "MATCH p=(:Start)-[:LINK]->() RETURN nodes(p) AS x UNION MATCH (a:Start),(b:End) RETURN [a,b] AS x",
            ReadMode::Incremental,
        ),
        (
            "MATCH p=(:Start)-[:LINK*1..40]->() RETURN p,nodes(p),relationships(p)",
            ReadMode::Incremental,
        ),
        (
            "MATCH p=(:Start)-[:LINK*]->() RETURN DISTINCT p SKIP $skip LIMIT $limit",
            ReadMode::Incremental,
        ),
        (
            "MATCH p=shortestPath((:Start)-[:LINK*]->(:End)) RETURN p AS x UNION ALL RETURN null AS x",
            ReadMode::Buffered,
        ),
    ] {
        let expected = tx.execute_with(query, &parameters).unwrap();
        let mut read = tx.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(read.mode(), mode, "{query}");
        assert_eq!(read.columns(), expected.columns);
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.columns(), expected.columns);
            let actual = cursor
                .by_ref()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(bag(actual), bag(expected.rows.clone()), "{query}");
            assert!(cursor.next().is_none());
        }
    }
    tx.rollback().unwrap();
    assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
}

#[test]
fn parameters_are_owned_and_budgets_reset_for_each_cursor() {
    let mut db = Database::in_memory().unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Earlier)").unwrap();
    let mut read = {
        let query = "UNWIND $xs AS x RETURN x".to_owned();
        let parameters = Parameters::from([("xs".into(), json!([1, 2]))]);
        let options = QueryOptions {
            max_result_rows: Some(1),
            max_materialized_rows: Some(1),
            max_result_bytes: Some(16),
            max_materialized_bytes: Some(1),
            ..QueryOptions::default()
        };
        tx.prepare_read_with_options(&query, &parameters, &options)
            .unwrap()
    };
    for _ in 0..3 {
        assert_eq!(
            read.cursor().unwrap().next().unwrap().unwrap(),
            [CypherValue::Integer(1)]
        );
    }
    drop(read);
    tx.execute("CREATE (:AfterRead)").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![CypherValue::Integer(2)]]
    );
}

#[test]
fn preparation_errors_reject_writes_and_abort_the_whole_transaction() {
    let mut db = Database::in_memory().unwrap();
    for query in [
        "CREATE (:Forbidden)",
        "MATCH (n) DELETE n",
        "RETURN missing",
        "RETURN (",
        "RETURN 1 AS x UNION ALL RETURN $missing AS x",
        "RETURN 1 LIMIT -1",
    ] {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Earlier)").unwrap();
        let error = match tx.prepare_read(query) {
            Err(error) => error,
            Ok(_) => panic!("expected rejection: {query}"),
        };
        assert!(
            !matches!(error, Error::TransactionAborted),
            "{query}: {error}"
        );
        if query.starts_with("CREATE") || query.contains("DELETE") {
            assert!(matches!(error, Error::Unsupported(_)));
        }
        if query.contains("$missing") {
            assert!(matches!(error, Error::MissingParameter(_)));
        }
        assert_aborted(&mut tx);
        tx.rollback().unwrap();
        assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
    }
    for expired in [false, true] {
        let token = CancellationToken::new();
        token.cancel();
        let options = if expired {
            QueryOptions {
                deadline: Some(Instant::now()),
                ..QueryOptions::default()
            }
        } else {
            QueryOptions {
                cancellation: Some(token),
                ..QueryOptions::default()
            }
        };
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Earlier)").unwrap();
        let error = match tx.prepare_read_with_options("RETURN 1", &Parameters::new(), &options) {
            Err(error) => error,
            Ok(_) => panic!("expected control error"),
        };
        assert!(matches!(error, Error::Cancelled | Error::DeadlineExceeded));
        assert_aborted(&mut tx);
        assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
        assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
    }
}

#[test]
fn errors_starting_buffered_or_cancelled_cursors_abort_before_returning() {
    let mut db = Database::in_memory().unwrap();
    for (query, options) in [
        (
            "MATCH p=(:Start)-[:LINK*1..40]->() WITH DISTINCT * ORDER BY 1 RETURN p",
            QueryOptions {
                max_result_rows: Some(0),
                ..QueryOptions::default()
            },
        ),
        (
            "MATCH p=(:Start)-[:LINK*1..40]->() WITH DISTINCT * ORDER BY 1 RETURN p",
            QueryOptions {
                traversal: TraversalLimits {
                    max_states: 0,
                    ..TraversalLimits::default()
                },
                ..QueryOptions::default()
            },
        ),
        (
            "MATCH p=(:Start)-[:LINK*1..40]->() WITH DISTINCT p ORDER BY 1 UNWIND ['bad'] AS v RETURN p,v[0]",
            QueryOptions::default(),
        ),
    ] {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Start)-[:LINK]->(:End)").unwrap();
        let mut read = tx
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        assert_eq!(read.mode(), ReadMode::Buffered);
        assert!(matches!(
            read.cursor(),
            Err(Error::ResourceLimit(_) | Error::Type(_))
        ));
        assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
        drop(read);
        assert_aborted(&mut tx);
        tx.rollback().unwrap();
        assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
    }
    let token = CancellationToken::new();
    let options = QueryOptions {
        cancellation: Some(token.clone()),
        ..QueryOptions::default()
    };
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Earlier)").unwrap();
    let mut read = tx
        .prepare_read_with_options("RETURN 1", &Parameters::new(), &options)
        .unwrap();
    token.cancel();
    assert!(matches!(read.cursor(), Err(Error::Cancelled)));
    assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
    drop(read);
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
}

#[test]
fn cumulative_row_and_byte_failures_abort_after_admitted_rows() {
    let mut db = Database::in_memory().unwrap();
    for options in [
        QueryOptions {
            max_result_rows: Some(2),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_bytes: Some(32),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_materialized_rows: Some(2),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_materialized_bytes: Some(2),
            ..QueryOptions::default()
        },
    ] {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Earlier)").unwrap();
        let mut read = tx
            .prepare_read_with_options(
                "RETURN 1 AS x UNION ALL RETURN 2 AS x UNION ALL RETURN 3 AS x",
                &Parameters::new(),
                &options,
            )
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        for value in [1, 2] {
            assert_eq!(
                cursor.next().unwrap().unwrap(),
                [CypherValue::Integer(value)]
            );
        }
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
        drop(cursor);
        assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
        drop(read);
        assert_aborted(&mut tx);
        tx.rollback().unwrap();
        assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
    }
}

#[test]
fn controls_abort_both_buffered_rows_and_hydration_batches() {
    let mut db = Database::in_memory().unwrap();
    for runtime in [false, true] {
        for deadline in [false, true] {
            let mut tx = db.transaction().unwrap();
            tx.execute("CREATE (:Start)-[:LINK]->(:End)").unwrap();
            let query = format!(
                "MATCH p=(:Start)-[:LINK{}]->() UNWIND range(1,130) AS i RETURN p",
                if runtime { "*1..40" } else { "" }
            );
            let token = CancellationToken::new();
            let expires = Instant::now() + Duration::from_secs(1);
            let options = if deadline {
                QueryOptions {
                    deadline: Some(expires),
                    ..QueryOptions::default()
                }
            } else {
                QueryOptions {
                    cancellation: Some(token.clone()),
                    ..QueryOptions::default()
                }
            };
            let mut read = tx
                .prepare_read_with_options(&query, &Parameters::new(), &options)
                .unwrap();
            let mut cursor = read.cursor().unwrap();
            cursor.next().unwrap().unwrap();
            cursor.next().unwrap().unwrap(); // hydration mode now holds a continuation batch
            if deadline {
                std::thread::sleep(expires.saturating_duration_since(Instant::now()));
            } else {
                token.cancel();
            }
            let error = cursor.next().unwrap().unwrap_err();
            assert!(matches!(error, Error::Cancelled | Error::DeadlineExceeded));
            assert!(cursor.next().is_none());
            drop(cursor);
            assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
            drop(read);
            assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
            assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
        }
    }
}

#[test]
fn sqlite_instruction_failure_aborts_prior_writes_and_cleans_up_progress_handler() {
    let mut db = Database::in_memory().unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Earlier)").unwrap();
    let query = vec!["RETURN 1 AS x"; 100].join(" UNION ALL ");
    let options = QueryOptions {
        max_sqlite_steps: Some(1000),
        ..QueryOptions::default()
    };
    let mut read = tx
        .prepare_read_with_options(&query, &Parameters::new(), &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    let mut rows = 0;
    loop {
        match cursor.next() {
            Some(Ok(_)) => rows += 1,
            Some(Err(Error::ResourceLimit(message))) => {
                assert!(message.contains("SQLite instruction"));
                break;
            }
            other => panic!("expected step limit: {other:?}"),
        }
    }
    assert!(rows > 0 && rows < 100);
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(read);
    tx.rollback().unwrap();
    assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
    db.execute("CREATE (:Usable)").unwrap();
}

fn file_options() -> OpenOptions {
    OpenOptions {
        mode: OpenMode::ReadWrite,
        busy_timeout: Duration::ZERO,
    }
}

#[test]
fn successful_cursor_cleanup_retains_the_parent_write_lock_until_commit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut db = Database::open(&path).unwrap();
    let mut observer = Database::open_with_options(&path, &file_options()).unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Start)-[:LINK]->(:End)").unwrap();
    for query in [
        "MATCH p=(:Start)-[:LINK]->() RETURN p",
        "MATCH p=(:Start)-[:LINK*1..40]->() RETURN p",
        "MATCH (n:Missing) RETURN n",
    ] {
        let mut read = tx.prepare_read(query).unwrap();
        drop(read.cursor().unwrap());
        assert!(matches!(
            observer.execute("CREATE (:Blocked)"),
            Err(Error::Busy(_))
        ));
        let mut cursor = read.cursor().unwrap();
        for row in cursor.by_ref() {
            row.unwrap();
        }
        assert!(matches!(
            observer.execute("CREATE (:Blocked)"),
            Err(Error::Busy(_))
        ));
        assert!(
            observer
                .execute("MATCH (n) RETURN n")
                .unwrap()
                .rows
                .is_empty()
        );
        drop(cursor);
        drop(read);
        assert!(matches!(
            observer.execute("CREATE (:Blocked)"),
            Err(Error::Busy(_))
        ));
    }
    tx.commit().unwrap();
    observer.execute("CREATE (:AfterCommit)").unwrap();
    assert_eq!(
        observer.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![CypherValue::Integer(3)]]
    );
}

#[test]
fn cursor_failure_releases_write_lock_with_live_handles_and_restores_graph_and_constraints() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE (:Saved {key:1})-[:LINK {v:2}]->(:Keep)")
        .unwrap();
    let original = db.execute("MATCH p=()-[:LINK]->() RETURN p").unwrap().rows;
    let mut observer = Database::open_with_options(&path, &file_options()).unwrap();
    let mut tx = db.transaction().unwrap();
    tx.create_unique_constraint("Saved", "key").unwrap();
    tx.execute("MATCH (n:Saved) SET n.key=9,n:Changed REMOVE n:Saved")
        .unwrap();
    tx.execute("MATCH (n:Keep) DETACH DELETE n").unwrap();
    tx.execute("CREATE (:Temporary)").unwrap();
    let mut read = tx
        .prepare_read(
            "MATCH p=(:Changed) RETURN p AS x UNION ALL UNWIND ['bad'] AS v RETURN v[0] AS x",
        )
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    let provisional = cursor.next().unwrap().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
    // Keep cursor, prepared read, and transaction alive to prove immediate rollback.
    observer.execute("CREATE (:AfterAbort)").unwrap();
    assert_eq!(
        observer
            .execute("MATCH p=()-[:LINK]->() RETURN p")
            .unwrap()
            .rows,
        original
    );
    assert!(observer.unique_constraints().unwrap().is_empty());
    assert_eq!(
        observer
            .execute("MATCH (n:Saved {key:1}) RETURN count(n)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1)]]
    );
    assert!(
        observer
            .execute("MATCH (n:Changed) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(
        provisional[0].as_path().unwrap().nodes[0].properties["key"],
        CypherValue::Integer(9)
    );
    assert!(cursor.next().is_none());
    drop(cursor);
    assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
    drop(read);
    assert_aborted(&mut tx);
    assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    db.execute("CREATE (:Reused)").unwrap();
}

#[test]
fn preparation_and_cursor_start_failures_release_locks_immediately() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut db = Database::open(&path).unwrap();
    let mut writer = Database::open_with_options(&path, &file_options()).unwrap();
    {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Earlier)").unwrap();
        assert!(matches!(
            tx.prepare_read("RETURN $missing"),
            Err(Error::MissingParameter(_))
        ));
        writer.execute("CREATE (:AfterAbort)").unwrap();
        assert_aborted(&mut tx);
    }
    for buffered in [false, true] {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Earlier)-[:LINK]->(:End)").unwrap();
        let token = CancellationToken::new();
        let options = if buffered {
            QueryOptions {
                max_result_rows: Some(0),
                ..QueryOptions::default()
            }
        } else {
            QueryOptions {
                cancellation: Some(token.clone()),
                ..QueryOptions::default()
            }
        };
        let query = if buffered {
            "MATCH p=(:Earlier)-[:LINK*1..40]->() WITH DISTINCT * ORDER BY 1 RETURN p"
        } else {
            "RETURN 1"
        };
        let mut read = tx
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        if !buffered {
            token.cancel();
        }
        assert!(matches!(
            read.cursor(),
            Err(Error::Cancelled | Error::ResourceLimit(_))
        ));
        writer.execute("CREATE (:AfterAbort)").unwrap();
        assert!(
            writer
                .execute("MATCH (n:Earlier) RETURN n")
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
        drop(read);
        tx.rollback().unwrap();
    }
}

#[test]
fn hydration_resource_errors_rollback_even_with_cached_provisional_rows() {
    let mut db = Database::in_memory().unwrap();
    for options in [
        QueryOptions {
            max_materialized_rows: Some(4),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_rows: Some(2),
            ..QueryOptions::default()
        },
    ] {
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:Start {value:'uncommitted'})-[:LINK]->(:End)")
            .unwrap();
        let mut read = tx
            .prepare_read_with_options(
                "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,130) AS i RETURN DISTINCT p,i",
                &Parameters::new(),
                &options,
            )
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        let first = cursor.next().unwrap().unwrap();
        loop {
            match cursor.next() {
                Some(Ok(_)) => {}
                Some(Err(Error::ResourceLimit(_))) => break,
                other => panic!("expected hydration/output failure: {other:?}"),
            }
        }
        assert!(cursor.next().is_none());
        drop(cursor);
        assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
        drop(read);
        assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
        assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
        assert_eq!(
            first[0].as_path().unwrap().nodes[0].properties["value"],
            CypherValue::String("uncommitted".into())
        );
    }
}

#[test]
fn unobserved_suffix_errors_and_consumer_panics_do_not_abort_successful_reads() {
    let mut db = Database::in_memory().unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Earlier)").unwrap();
    for query in [
        "RETURN 1 AS x UNION ALL UNWIND ['bad'] AS v RETURN v[0] AS x",
        "UNWIND [[1],'bad'] AS v RETURN DISTINCT v[0] LIMIT 1",
    ] {
        let mut read = tx.prepare_read(query).unwrap();
        assert_eq!(
            read.cursor().unwrap().next().unwrap().unwrap(),
            [CypherValue::Integer(1)]
        );
    }
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let mut read = tx
                .prepare_read("RETURN 1 AS x UNION ALL RETURN 2 AS x")
                .unwrap();
            let mut cursor = read.cursor().unwrap();
            cursor.next().unwrap().unwrap();
            panic!("consumer panic with parent outside catch");
        }))
        .is_err()
    );
    tx.execute("CREATE (:AfterRead)").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![CypherValue::Integer(2)]]
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let mut tx = db.transaction().unwrap();
            tx.execute("CREATE (:RolledBack)").unwrap();
            let mut read = tx.prepare_read("RETURN 1").unwrap();
            let _cursor = read.cursor().unwrap();
            panic!("parent unwinds too");
        }))
        .is_err()
    );
    assert!(
        db.execute("MATCH (n:RolledBack) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
}
