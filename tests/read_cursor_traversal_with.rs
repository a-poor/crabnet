use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions, ReadMode, TraversalLimits};
use serde_json::json;

#[test]
fn with_filters_and_projections_stop_at_the_requested_row() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->({x:1})-[:R]->({x:2})-[:R]->({x:3})")
        .unwrap();
    let query = "MATCH p=(:Start)-[:R*]->(n)
        WITH p,n WHERE n.x >= $min
        WITH length(p) AS hops,n.x AS x RETURN hops,x LIMIT 1";
    let parameters = Parameters::from([("min".into(), json!(2))]);
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 2,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    assert!(matches!(
        db.execute_with_options(query, &parameters, &options),
        Err(Error::ResourceLimit(_))
    ));
    let mut read = db
        .prepare_read_with_options(query, &parameters, &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        let rows = read
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            [vec![CypherValue::Integer(2), CypherValue::Integer(2)]]
        );
    }
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
fn with_chains_preserve_typed_bags_scopes_paths_and_empty_inputs() {
    let mut db = fixture();
    let params = Parameters::from([
        ("min".into(), json!(1)),
        ("open".into(), json!(true)),
        ("value".into(), json!([null,1,1.0,{"x":"é"}])),
    ]);
    for query in [
        "MATCH p=(a:S)-[rs:R*0..40]->(b) WITH * WITH p AS q,a,b,rs RETURN q,a,b,rs,nodes(q),relationships(q),$value",
        "MATCH p=(a:S)-[:R*]->(b) WITH b.x AS x WHERE a.x=1 WITH x+1 AS x RETURN x",
        "MATCH p=(:S)-[:R*]->(b) WITH length(p) AS hops WHERE hops >= $min RETURN hops",
        "MATCH p=(:S)-[:R* {open:$open}]->(b) WITH p,b WHERE length(p)>$min WITH p AS q,b RETURN q,b",
        "MATCH p=(:S)-[:R*]->(b) WITH b WHERE (b)-[:R]->() RETURN b",
        "MATCH p=(:S)-[:R*]->(b) WITH p WHERE all(n IN nodes(p) WHERE n.x>0) RETURN p",
        "MATCH p=(:S)<-[:R*]-(b) WITH p,b WITH b AS n,p AS q RETURN n,q",
        "MATCH p=(:S)-[:R*]-(b) WITH p,b WHERE b.x IN [1,3] RETURN p,b",
        "MATCH p=(:S)-[:R*0..]->(b) WITH {paths:[p,null],nodes:nodes(p),edge:head(relationships(p))} AS box RETURN box,box.nodes[-1]",
        "MATCH p=(:S)-[:R*]->(b) WITH CASE WHEN b.x=2 THEN b ELSE null END AS n RETURN n,n.x",
        "MATCH p=(:S)-[:R*]->(b) WITH length(p) AS x,null AS missing WITH x,missing WHERE missing IS NULL RETURN x,missing",
        "MATCH p=(:Missing)-[:R*]->(b) WITH p,b RETURN p,b",
        "MATCH p=(:S)-[:NONE*0..]->(b) WITH p,b RETURN p,b",
        "MATCH p=(:S)-[:R*]->(b) WITH p,b WHERE false RETURN p,b",
        "MATCH p=()-[:R*]->(b) WITH p,b RETURN p,b",
    ] {
        let expected = db.execute_with(query, &params).unwrap();
        let mut read = db.prepare_read_with(query, &params).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        assert_eq!(read.columns(), expected.columns);
        for _ in 0..2 {
            let rows = read
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(bag(rows), bag(expected.rows.clone()), "{query}");
        }
    }
}

#[test]
fn non_rowwise_stages_and_other_producers_remain_buffered() {
    let mut db = fixture();
    for query in [
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT p,b RETURN p,b",
        "MATCH p=(:S)-[:R*]->(b) WITH length(p) AS x ORDER BY x RETURN x",
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT p,b SKIP 0 RETURN p,b",
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT p,b LIMIT 0 RETURN p,b",
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT p,b SKIP 1 LIMIT 2 WITH b RETURN b",
        "MATCH p=(:S)-[:R*]->(b) WITH count(*) AS x WITH x+1 AS y RETURN y",
        "MATCH p=(:S)-[:R*]->(b) WITH head(collect(b)) AS n WITH n RETURN n",
        "MATCH p=(:S)-[:R*]->(b) WITH b RETURN count(*)",
        "MATCH p=(:S)-[:R*]->(b) WITH b RETURN b ORDER BY b.x",
        "MATCH p=(:S)-[:R*]->(b) WITH b MATCH (n) RETURN n,b",
        "MATCH p=(:S)-[:R*]->(b) WITH b OPTIONAL MATCH (b)-[:R]->(c) RETURN b,c",
        "MATCH p=(:S)-[:R*]->(b) WITH p UNWIND nodes(p) AS n WITH DISTINCT n UNWIND [n] AS m RETURN m",
        "WITH 1 AS x MATCH p=(:S)-[:R*]->(b) WITH p,x RETURN p,x",
        "MATCH p=(:S)-[:R*]->(b)-[:R*]->(c) WITH p RETURN p",
        "MATCH p=shortestPath((:S)-[:R*]->(b)) WITH p RETURN p",
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
    let query = "RETURN 1 AS x UNION ALL MATCH p=(:S)-[:R*]->(b) WITH count(*) AS x RETURN x";
    assert_eq!(db.prepare_read(query).unwrap().mode(), ReadMode::Buffered);
}

#[test]
fn final_distinct_union_and_pages_compose_after_with_filters() {
    let mut db = fixture();
    let params = Parameters::from([("skip".into(), json!(1)), ("limit".into(), json!(2))]);
    for query in [
        "MATCH p=(:S)-[:R*]->(b) WITH b WHERE b.x>1 RETURN DISTINCT b SKIP $skip LIMIT $limit",
        "MATCH p=(:S)-[:R*]->(b) WITH length(p) AS x RETURN x SKIP $skip LIMIT $limit",
        "MATCH p=(:S)-[:R*]->(b) WITH b AS x RETURN x UNION ALL RETURN null AS x",
        "RETURN null AS x UNION MATCH p=(:S)-[:R*]->(b) WITH b AS x RETURN x",
        "MATCH p=(:S)-[:R*]->(b) WITH p AS x RETURN DISTINCT x SKIP 1 LIMIT 2",
    ] {
        let expected = db.execute_with(query, &params).unwrap();
        let mut read = db.prepare_read_with(query, &params).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
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
fn with_evaluation_errors_remain_terminal_and_final_skip_only_guards_return() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let params = Parameters::from([("values".into(), json!([[7], 8, [9]]))]);
    let query = "MATCH p=(:S)-[:R*]->() WITH length(p) AS hops
        WITH $values[hops-1][0] AS value RETURN value";
    let mut read = db.prepare_read_with(query, &params).unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(7)]);
    assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(read);
    assert_eq!(
        db.prepare_read_with(&format!("{query} LIMIT 1"), &params)
            .unwrap()
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(7)]]
    );
    let params = Parameters::from([("values".into(), json!([8, [7], [9]]))]);
    let query =
        "MATCH p=(:S)-[:R*]->() WITH length(p) AS hops RETURN $values[hops-1][0] SKIP 1 LIMIT 1";
    let expected = db.execute_with(query, &params).unwrap().rows;
    assert_eq!(
        db.prepare_read_with(query, &params)
            .unwrap()
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        expected
    );
    let query =
        "MATCH p=(:S)-[:R*]->() WITH $values[length(p)-1][0] AS value RETURN value SKIP 1 LIMIT 1";
    assert!(matches!(
        db.execute_with(query, &params),
        Err(Error::Type(_))
    ));
    let mut read = db.prepare_read_with(query, &params).unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
    assert!(cursor.next().is_none());
}

#[test]
fn volatile_with_aliases_are_evaluated_once_per_row() {
    let mut db = fixture();
    let query = "MATCH p=(:S)-[:R*]->() WITH rand() AS x,length(p) AS hops
        WITH x AS y,[x,x] AS pair,hops WHERE x=x RETURN y,pair,y+0,hops SKIP 1 LIMIT 8";
    let mut read = db.prepare_read(query).unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    let rows = read
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 8);
    for row in &rows {
        assert_eq!(row[0], row[2]);
        assert_eq!(
            row[1],
            CypherValue::List(vec![row[0].clone(), row[0].clone()])
        );
    }
    assert!(rows.iter().skip(1).any(|row| row[0] != rows[0][0]));
}

#[test]
fn zero_pages_preflight_and_union_budgets_cover_every_with_stage() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let params = Parameters::new();
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 0,
            max_memory_bytes: 0,
            max_payload_bytes: 0,
        },
        max_materialized_rows: Some(0),
        ..QueryOptions::default()
    };
    let query = "MATCH p=(:S)-[:R*]->() WITH p,8[0] AS bad RETURN bad LIMIT 0";
    let mut read = db
        .prepare_read_with_options(query, &params, &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    assert!(read.cursor().unwrap().next().is_none());
    drop(read);
    assert!(matches!(
        db.prepare_read("MATCH p=(:S)-[:R*]->() WITH p,$missing AS x RETURN p LIMIT 0"),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(
        db.prepare_read(
            "RETURN 1 AS x UNION ALL MATCH p=(:S)-[:R*]->() WITH $missing AS x RETURN x"
        ),
        Err(Error::MissingParameter(_))
    ));
    let branch = "MATCH p=(:S)-[:R*]->() WITH length(p) AS x WHERE length(p)>1 RETURN x LIMIT 1";
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 3,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(&format!("{branch} UNION ALL {branch}"), &params, &options)
        .unwrap();
    for _ in 0..2 {
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
}

#[test]
fn with_cursors_enforce_transfer_memory_payload_output_and_control_budgets() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH length(p) AS x WITH x+1 AS y RETURN y";
    let params = Parameters::new();
    // The consumer sees one row per admitted trail, despite two WITH stages.
    for options in [
        QueryOptions {
            max_materialized_rows: Some(3),
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
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
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
            max_sqlite_steps: Some(100),
            ..QueryOptions::default()
        },
    ] {
        let mut read = db
            .prepare_read_with_options(query, &params, &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
    let token = crabnet::CancellationToken::new();
    let options = QueryOptions {
        cancellation: Some(token.clone()),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &params, &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    token.cancel();
    assert!(matches!(cursor.next(), Some(Err(Error::Cancelled))));
    assert!(cursor.next().is_none());
    drop(cursor);
    drop(read);
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
fn with_hydration_errors_roll_back_prior_writes_and_early_drop_keeps_them() {
    for observe_error in [false, true] {
        let mut db = Database::in_memory().unwrap();
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
            .unwrap();
        let params = Parameters::from([("values".into(), json!([[7], 8, [9]]))]);
        let query = "MATCH p=(:S)-[:R*]->() WITH p,$values[length(p)-1][0] AS value RETURN p,value";
        let mut read = tx.prepare_read_with(query, &params).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        let mut cursor = read.cursor().unwrap();
        let row = cursor.next().unwrap().unwrap();
        assert!(matches!(&row[0], CypherValue::Path(path) if path.relationships.len()==1));
        assert_eq!(row[1], CypherValue::Integer(7));
        if observe_error {
            assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
            assert!(cursor.next().is_none());
        }
        drop(cursor);
        if observe_error {
            assert!(matches!(read.cursor(), Err(Error::TransactionAborted)));
        }
        drop(read);
        if observe_error {
            assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
            assert!(db.execute("MATCH (n) RETURN n").unwrap().rows.is_empty());
        } else {
            tx.execute("CREATE (:Kept)").unwrap();
            tx.commit().unwrap();
            assert_eq!(
                db.execute("MATCH (n:S) RETURN count(n)").unwrap().rows,
                [vec![CypherValue::Integer(1)]]
            );
        }
    }
}

#[test]
fn with_cursors_pin_snapshots_and_release_locks_on_exhaustion_error_drop_and_panic() {
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
    let query = "MATCH p=(:S)-[:R*]->() WITH p WITH length(p) AS x RETURN x";
    let mut read = reader.prepare_read(query).unwrap();
    let mut cursor = read.cursor().unwrap();
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
    drop(cursor);
    writer.execute("MATCH (e:End) CREATE (e)-[:R]->()").unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(
        cursor
            .by_ref()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(1)], vec![CypherValue::Integer(2)]]
    );
    writer.execute("CREATE (:AfterExhaustion)").unwrap();
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
    let params = Parameters::new();
    let options = QueryOptions {
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    let mut read = reader
        .prepare_read_with_options(query, &params, &options)
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    cursor.next().unwrap().unwrap();
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    writer.execute("CREATE (:AfterFailure)").unwrap();
    assert!(cursor.next().is_none());
}
