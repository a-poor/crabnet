use crabnet::{
    CancellationToken, CypherValue, Database, Error, OpenMode, OpenOptions, Parameters,
    QueryOptions, ReadMode,
};
use serde_json::json;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::{Duration, Instant},
};

fn fixture() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start {x:1})-[:LINK {x:2}]->(:End {x:3})")
        .unwrap();
    db
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

fn collect(
    db: &mut Database,
    query: &str,
    options: &QueryOptions,
) -> crabnet::Result<Vec<Vec<CypherValue>>> {
    let mut read = db.prepare_read_with_options(query, &Parameters::new(), options)?;
    assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
    read.cursor()?.collect()
}

#[test]
fn first_path_row_does_not_fetch_the_remaining_result() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start {x:1})-[:LINK {x:2}]->(:End {x:3})")
        .unwrap();
    let query = "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,200) AS i RETURN p,i";
    let options = QueryOptions {
        // One source row, two nodes, and one relationship. The suffix cannot fit.
        max_materialized_rows: Some(4),
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        let row = cursor.next().unwrap().unwrap();
        let CypherValue::Path(path) = &row[0] else {
            panic!("expected path: {row:?}");
        };
        assert_eq!(path.nodes.len(), 2);
        assert_eq!(path.relationships.len(), 1);
        assert_eq!(row[1], CypherValue::Integer(1));
    }
}

#[test]
fn sqlite_paths_lists_and_mixed_projections_preserve_typed_bags() {
    let mut db = fixture();
    db.execute("MATCH (a:Start)-[:LINK]->(b:End) CREATE (a)-[:LINK {x:4}]->(b),(b)-[:LINK {x:5}]->(a),(a)-[:LINK {x:6}]->(a)").unwrap();
    db.execute("MATCH (a:Start) SET a.data = {nested:[1,null,'é',1.2345678901234567]}")
        .unwrap();
    let parameters = Parameters::from([
        ("values".into(), json!([null,true,1,1.0,{"id":"ordinary"}])),
        ("skip".into(), json!(1)),
        ("limit".into(), json!(4)),
    ]);
    for query in [
        "MATCH p=(a:Start)-[r:LINK]->(b) RETURN p,nodes(p),relationships(p),a,r,b",
        "MATCH p=(:End)<-[:LINK]-(:Start) RETURN p",
        "MATCH p=(:Start)-[:LINK]-() RETURN p",
        "MATCH p=(:Start)-[rels:LINK*0..3]->() RETURN p,nodes(p),relationships(p),rels",
        "MATCH p=(:Start)-[:LINK]->()-[:LINK*0..2]->() RETURN p,nodes(p),relationships(p)",
        "MATCH p=(:Start)-[:LINK*0..32]->() RETURN p",
        "MATCH p=(:Start) UNWIND $values AS v RETURN p,relationships(p),v",
        "MATCH p=(:Missing)-[:LINK]->() RETURN p,nodes(p)",
        "MATCH p=(:Start)-[:LINK*0..2]->() RETURN p,length(p) AS size ORDER BY size SKIP $skip LIMIT $limit",
        "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,140) AS i RETURN p,nodes(p),relationships(p),[p,{n:nodes(p)}],i",
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
}

#[test]
fn distinct_union_and_pagination_shape_compact_and_materialized_lists() {
    let mut db = fixture();
    db.execute("MATCH (a:Start),(b:End) CREATE (a)-[:LINK {x:4}]->(b)")
        .unwrap();
    for query in [
        "MATCH p=(:Start)-[:LINK]->() UNWIND [1,1.0,2] AS i RETURN DISTINCT p,i SKIP 1 LIMIT 2",
        "MATCH p=(:Start)-[:LINK]->() RETURN nodes(p) AS x UNION MATCH (a:Start),(b:End) RETURN [a,b] AS x",
        "MATCH (a:Start),(b:End) RETURN [a,b] AS x UNION MATCH p=(:Start)-[:LINK]->() RETURN nodes(p) AS x",
        "MATCH p=(:Start)-[r:LINK]->() RETURN relationships(p) AS x UNION MATCH ()-[r:LINK]->() RETURN [r] AS x",
        "MATCH p=(:Start) RETURN relationships(p) AS x UNION RETURN [] AS x UNION RETURN null AS x",
        "MATCH p=(:Start) RETURN p AS x UNION ALL RETURN {id:'ordinary'} AS x UNION ALL RETURN null AS x",
        "MATCH p=(:Start)-[:LINK]->() RETURN DISTINCT p AS x SKIP 1 LIMIT 1 UNION MATCH p=(:Start)-[:LINK]->() RETURN p AS x",
        "MATCH p=(:Start)-[:LINK]->() RETURN nodes(p) AS x UNION ALL MATCH p=(:Start)-[:LINK]->() RETURN relationships(p) AS x",
        "MATCH p=(:Start)-[:LINK]->() RETURN p AS x UNION MATCH p=(:Start)-[:LINK]->() RETURN p AS x",
    ] {
        let expected = db.execute(query).unwrap();
        assert_eq!(
            collect(&mut db, query, &QueryOptions::default()).unwrap(),
            expected.rows,
            "{query}"
        );
    }
    assert_eq!(collect(&mut db, "MATCH p=(:Start)-[:LINK]->() RETURN nodes(p) AS x UNION MATCH (a:Start),(b:End) RETURN [a,b] AS x", &QueryOptions::default()).unwrap().len(), 1);
}

#[test]
fn entity_reads_are_deduplicated_per_batch_and_charged_again_in_later_batches() {
    let mut db = fixture();
    let query = "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,130) AS i RETURN p";
    // 130 source rows + three entity reads in each of four batches (1/64/64/1).
    let exact = QueryOptions {
        max_materialized_rows: Some(142),
        ..QueryOptions::default()
    };
    for _ in 0..2 {
        assert_eq!(collect(&mut db, query, &exact).unwrap().len(), 130);
    }
    let short = QueryOptions {
        max_materialized_rows: Some(141),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &Parameters::new(), &short)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    for _ in 0..129 {
        cursor.next().unwrap().unwrap();
    }
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    assert!(cursor.next().is_none());
}

#[test]
fn batches_load_only_needed_kinds_and_seed_from_direct_entities() {
    let mut db = fixture();
    for (projection, reads) in [
        ("nodes(p)", 7),
        ("relationships(p)", 5),
        ("rels", 5),
        ("p,a,r,b", 3),
    ] {
        let query = if projection == "rels" {
            "MATCH (:Start)-[rels:LINK*1..1]->() UNWIND range(1,3) AS i RETURN rels".to_owned()
        } else {
            format!("MATCH p=(a:Start)-[r:LINK]->(b) UNWIND range(1,3) AS i RETURN {projection}")
        };
        let exact = QueryOptions {
            max_materialized_rows: Some(reads),
            ..QueryOptions::default()
        };
        assert_eq!(collect(&mut db, &query, &exact).unwrap().len(), 3);
        let short = QueryOptions {
            max_materialized_rows: Some(reads - 1),
            ..QueryOptions::default()
        };
        assert!(
            matches!(
                collect(&mut db, &query, &short),
                Err(Error::ResourceLimit(_))
            ),
            "{query}"
        );
    }
}

#[test]
fn discarded_rows_and_zero_pages_do_not_load_entities() {
    let mut db = fixture();
    // One surviving path: all 200 source rows count, just three entity reads.
    let distinct = "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,200) AS i RETURN DISTINCT p";
    let options = QueryOptions {
        max_materialized_rows: Some(203),
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    assert_eq!(collect(&mut db, distinct, &options).unwrap().len(), 1);
    let query = "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,200) AS i RETURN DISTINCT p,i SKIP 100 LIMIT 1";
    let options = QueryOptions {
        max_materialized_rows: Some(104),
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    assert_eq!(
        collect(&mut db, query, &options).unwrap()[0][1],
        CypherValue::Integer(101)
    );
    let zero = QueryOptions {
        max_materialized_rows: Some(0),
        max_materialized_bytes: Some(0),
        max_result_rows: Some(0),
        max_result_bytes: Some(0),
        ..QueryOptions::default()
    };
    for query in [
        "MATCH p=(:Start)-[:LINK]->() RETURN DISTINCT p LIMIT 0",
        "MATCH p=(:Missing) RETURN p",
        "MATCH p=(:Missing) RETURN p AS x UNION MATCH p=(:Start) RETURN p AS x LIMIT 0",
    ] {
        assert!(
            collect(&mut db, query, &zero).unwrap().is_empty(),
            "{query}"
        );
    }
    assert!(matches!(
        db.prepare_read("MATCH p=(:Start) RETURN p AS x LIMIT 0 UNION RETURN $missing AS x"),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(
        db.prepare_read("MATCH p=(:Start) RETURN p LIMIT -1"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn result_limits_admit_each_hydrated_row_at_exact_byte_boundaries() {
    let mut db = Database::in_memory().unwrap();
    let node = db.execute("CREATE (n:N {v:1}) RETURN n").unwrap();
    let node_cost = node.rows[0][0].as_node().unwrap().element_id.len() + 50;
    for (projection, cost) in [
        ("p", node_cost + 24),
        ("nodes(p)", node_cost + 8),
        ("relationships(p)", 16),
    ] {
        let query = format!("MATCH p=(n) UNWIND range(1,3) AS i RETURN {projection}");
        let exact = QueryOptions {
            max_result_bytes: Some(cost * 3),
            ..QueryOptions::default()
        };
        assert_eq!(collect(&mut db, &query, &exact).unwrap().len(), 3);
        for options in [
            QueryOptions {
                max_result_bytes: Some(cost * 2),
                ..QueryOptions::default()
            },
            QueryOptions {
                max_result_rows: Some(2),
                ..QueryOptions::default()
            },
        ] {
            let mut read = db
                .prepare_read_with_options(&query, &Parameters::new(), &options)
                .unwrap();
            let mut cursor = read.cursor().unwrap();
            for _ in 0..2 {
                cursor.next().unwrap().unwrap();
            }
            assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
            assert!(cursor.next().is_none());
        }
        assert!(matches!(
            collect(
                &mut db,
                &query,
                &QueryOptions {
                    max_result_bytes: Some(cost - 1),
                    ..QueryOptions::default()
                }
            ),
            Err(Error::ResourceLimit(_))
        ));
    }
    let options = QueryOptions {
        max_materialized_bytes: Some(1),
        ..QueryOptions::default()
    };
    assert!(matches!(
        collect(&mut db, "MATCH p=(n) RETURN p", &options),
        Err(Error::ResourceLimit(_))
    ));
    db.execute_with(
        "MATCH (n) SET n.payload=$payload",
        &Parameters::from([("payload".into(), json!("x".repeat(2048)))]),
    )
    .unwrap();
    // Compact IDs fit; fetching the node's properties during hydration does not.
    let options = QueryOptions {
        max_materialized_bytes: Some(1024),
        ..QueryOptions::default()
    };
    assert!(matches!(
        collect(&mut db, "MATCH p=(n) RETURN p", &options),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
fn first_row_skips_unread_branches_and_batch_errors_are_terminal() {
    let mut db = fixture();
    let query = "MATCH p=(:Start) RETURN p AS x UNION ALL UNWIND [[1],'bad'] AS v RETURN v[0] AS x";
    let mut read = db.prepare_read(query).unwrap();
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert!(matches!(cursor.next(),Some(Ok(row)) if matches!(row[0], CypherValue::Path(_))));
        // The second batch encounters an error during read-ahead and is discarded.
        assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
        assert!(cursor.next().is_none());
        assert!(cursor.next().is_none());
    }
    drop(read.cursor().unwrap());
    drop(read);
    db.execute("CREATE (:AfterFailure)").unwrap();
}

#[test]
fn cancellation_deadlines_and_sqlite_work_limits_cover_cached_and_new_batches() {
    let mut db = fixture();
    let query = "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,200) AS i RETURN p";
    for consumed in [1, 2] {
        let token = CancellationToken::new();
        let options = QueryOptions {
            cancellation: Some(token.clone()),
            ..QueryOptions::default()
        };
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        for _ in 0..consumed {
            cursor.next().unwrap().unwrap();
        }
        token.cancel();
        assert!(matches!(cursor.next(), Some(Err(Error::Cancelled))));
        assert!(cursor.next().is_none());
        drop(cursor);
        assert!(matches!(read.cursor(), Err(Error::Cancelled)));
    }
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
    cursor.next().unwrap().unwrap();
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
    assert!(matches!(cursor.next(), Some(Err(Error::DeadlineExceeded))));
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(read);
    let options = QueryOptions {
        max_sqlite_steps: Some(1000),
        ..QueryOptions::default()
    };
    let query = vec!["MATCH p=(:Start)-[:LINK]->() RETURN p"; 100].join(" UNION ALL ");
    let mut read = db
        .prepare_read_with_options(&query, &Parameters::new(), &options)
        .unwrap();
    let mut counts = vec![];
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
                other => panic!("expected work limit after {count}: {other:?}"),
            }
        }
        assert!(count > 0 && count < 100);
        assert!(cursor.next().is_none());
        counts.push(count);
    }
    assert_eq!(counts[0], counts[1]);
}

fn file_options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        busy_timeout: Duration::ZERO,
    }
}

#[test]
fn snapshots_cover_all_batches_and_release_on_drop_exhaustion_error_and_panic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut reader = Database::open(&path).unwrap();
    reader
        .execute("CREATE (:Start {x:1})-[:LINK {x:2}]->(:End {x:3})")
        .unwrap();
    let mut writer =
        Database::open_with_options(&path, &file_options(OpenMode::ReadWrite)).unwrap();
    let query = "MATCH p=(:Start)-[:LINK]->() UNWIND range(1,130) AS i RETURN p";
    let mut read = reader.prepare_read(query).unwrap();
    writer.execute("MATCH (a:Start) SET a.x=4").unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(matches!(
        writer.execute("MATCH (a:Start) SET a.x=5"),
        Err(Error::Busy(_))
    ));
    let first = cursor.next().unwrap().unwrap();
    cursor.next().unwrap().unwrap(); // leave a populated batch/cache on early drop
    drop(cursor);
    writer.execute("MATCH (a:Start) SET a.x=5").unwrap();
    let CypherValue::Path(path_value) = &first[0] else {
        panic!("expected path");
    };
    assert_eq!(path_value.nodes[0].properties["x"], CypherValue::Integer(4));
    let mut cursor = read.cursor().unwrap();
    let all = cursor
        .by_ref()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(all.len(), 130);
    for row in all {
        assert_eq!(
            row[0].as_path().unwrap().nodes[0].properties["x"],
            CypherValue::Integer(5)
        );
    }
    writer.execute("MATCH (a:Start) SET a.x=6").unwrap();
    assert!(cursor.next().is_none());
    drop(cursor);
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let mut cursor = read.cursor().unwrap();
            cursor.next().unwrap().unwrap();
            cursor.next().unwrap().unwrap();
            panic!("consumer panic");
        }))
        .is_err()
    );
    writer.execute("MATCH (a:Start) SET a.x=7").unwrap();
    drop(read.cursor().unwrap());
    writer.execute("MATCH (a:Start) SET a.x=8").unwrap();
    drop(read);
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
        let mut read = reader
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        cursor.next().unwrap().unwrap();
        loop {
            match cursor.next() {
                Some(Ok(_)) => {}
                Some(Err(Error::ResourceLimit(_))) => break,
                other => panic!("expected resource failure: {other:?}"),
            }
        }
        writer.execute("MATCH (a:Start) SET a.x=9").unwrap();
        assert!(cursor.next().is_none());
    }
}

#[test]
fn read_only_hydration_cursor_recovers_after_busy_snapshot_start() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let mut setup = Database::open(&path).unwrap();
    setup.execute("CREATE (:Start)-[:LINK]->(:End)").unwrap();
    drop(setup);
    let mut reader = Database::open_with_options(&path, &file_options(OpenMode::ReadOnly)).unwrap();
    let mut read = reader
        .prepare_read("MATCH p=(:Start)-[:LINK]->() RETURN p,nodes(p),relationships(p)")
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
            .unwrap()
            .len(),
        1
    );
}
