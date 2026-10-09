use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions, ReadMode, TraversalLimits};
use serde_json::json;

#[test]
fn distinct_pages_count_unique_rows_and_drain_expansions_before_stopping() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->()
        UNWIND [[1],[1.0],[2],[2],[3],8] AS x
        WITH DISTINCT x[0] AS value SKIP 1 LIMIT 2
        UNWIND [value,value+10] AS member RETURN value,member";
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(query, &Parameters::new(), &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            [(2, 2), (2, 12), (3, 3), (3, 13)]
                .map(|(a, b)| vec![CypherValue::Integer(a), CypherValue::Integer(b)])
        );
    }
}

fn fixture() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:S {x:1,tags:['a','a']}),(b {x:1,tags:['a','b']}),(c {x:2}),
        (a)-[:R {open:true}]->(b),(a)-[:R {open:true}]->(b),
        (b)-[:R {open:false}]->(b),(b)-[:R {open:true}]->(c),(c)-[:R {open:true}]->(a)",
    )
    .unwrap();
    db
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

fn parity(db: &mut Database, query: &str, params: &Parameters) {
    let expected = db
        .execute_with(query, params)
        .unwrap_or_else(|e| panic!("{query}: {e}"));
    let mut read = db
        .prepare_read_with(query, params)
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

#[test]
fn scalar_keys_keep_typed_equivalence_complete_rows_and_first_representatives() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let params = Parameters::from([(
        "values".into(),
        json!([
            null,null,true,1.0,1,false,-0.0,0,"1","é\u{0000}",
            9007199254740992_i64,9007199254740992.0,9007199254740993_i64,
            i64::MAX,i64::MAX as f64,i64::MIN,i64::MIN as f64,
            [1,null],[1.0,null],{"a":[1,null],"b":false},{"b":false,"a":[1.0,null]},[],{}
        ]),
    )]);
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND $values AS x WITH DISTINCT x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND $values AS x WITH DISTINCT x,x IS NULL AS absent RETURN x,absent",
        "MATCH p=(:S)-[:R*]->() UNWIND $values AS x WITH DISTINCT x,length(p) AS hops RETURN x,hops",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,1,2] AS x UNWIND [true,false,null,null] AS y WITH DISTINCT x,y RETURN x,y",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,1,2] AS x WITH x,2 AS y WITH DISTINCT y AS x,x AS y RETURN x,y",
    ] {
        parity(&mut db, query, &params);
    }
    let rows = db
        .prepare_read("MATCH p=(:S)-[:R*]->() UNWIND [1.0,1,2,2.0] AS x WITH DISTINCT x RETURN x")
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        rows,
        [vec![CypherValue::Float(1.0)], vec![CypherValue::Integer(2)]]
    );
}

#[test]
fn graph_keys_compare_projected_identities_including_shared_path_columns() {
    let mut db = fixture();
    let params = Parameters::from([("open".into(), json!(true))]);
    for query in [
        "MATCH p=(a:S)-[rs:R*0..]->(b) WITH DISTINCT * RETURN p,a,b,rs",
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT b RETURN b",
        "MATCH p=(:S)-[:R*]->(b) UNWIND [b,b,null] AS n WITH DISTINCT n RETURN n,n.x",
        "MATCH p=(:S)-[:R*]->() UNWIND relationships(p) AS r WITH DISTINCT r RETURN r",
        "MATCH p=(:S)-[:R*0..]->() WITH DISTINCT nodes(p) AS ns RETURN ns",
        "MATCH p=(:S)-[:R*0..]->() WITH DISTINCT relationships(p) AS rs RETURN rs",
        "MATCH p=(:S)-[:R*0..]->() WITH DISTINCT nodes(p) AS ns,relationships(p) AS rs RETURN ns,rs",
        "MATCH p=(:S)-[:R*0..]->() WITH DISTINCT p,nodes(p) AS ns,relationships(p) AS rs RETURN p,ns,rs",
        "MATCH p=(:S)-[:R*0..]->() UNWIND [p,p] AS q WITH DISTINCT q RETURN q",
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT [b,b,null] AS ns RETURN ns",
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT {paths:[p,p],node:b,values:[1,null]} AS box RETURN box",
        "MATCH p=(:S)-[rs:R*]->() WITH DISTINCT rs RETURN rs",
        "MATCH p=(:S)-[:R* {open:$open}]->() WITH DISTINCT nodes(p) AS ns UNWIND ns AS n WITH DISTINCT n RETURN n",
        "MATCH p=(:S)<-[:R*]-() WITH DISTINCT p,relationships(p) AS rs RETURN p,rs",
        "MATCH p=(:S)-[:R*]-() WITH DISTINCT relationships(p) AS rs RETURN rs",
        "MATCH p=()-[:R*]->() WITH DISTINCT nodes(p) AS ns RETURN ns",
        "MATCH p=(:S)-[:NONE*0..]->() WITH DISTINCT relationships(p) AS rs RETURN rs",
        "MATCH p=(:Missing)-[:R*]->() WITH DISTINCT p RETURN p",
    ] {
        parity(&mut db, query, &params);
    }
    // Parallel edges yield distinct paths, but the same projected node list.
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:S)-[:R]->(b),(a)-[:R]->(b)").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH DISTINCT nodes(p) AS ns UNWIND ns AS n RETURN n";
    let rows = db
        .prepare_read(query)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn distinct_state_spans_parent_lists_and_each_stage_has_its_own_scope() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [[1,1,2],[],[2,3],null,[1,3,4]] AS xs UNWIND xs AS x WITH DISTINCT x UNWIND [x,x] AS y RETURN y",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT 1 AS x UNWIND [x,x+1] AS y WITH DISTINCT y AS x UNWIND [x,x] AS y WITH DISTINCT y RETURN y",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [1,1,2,3,2] AS x WITH DISTINCT x SKIP 1 LIMIT 2 UNWIND [x,x+10] AS y WITH DISTINCT y SKIP 1 LIMIT 2 RETURN y",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p WITH p SKIP 0 UNWIND [p,p] AS q WITH DISTINCT q RETURN q",
        "MATCH (:S)-[:R*]->() WITH DISTINCT * RETURN 1",
        "MATCH (:S)-[:R*]->() WITH DISTINCT * SKIP 1 RETURN 1",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p UNWIND [] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND [] AS x WITH DISTINCT x RETURN x",
    ] {
        parity(&mut db, query, &Parameters::new());
    }
}

#[test]
fn filters_use_incoming_scope_but_only_projected_values_enter_seen_sets() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [0,1,2] AS x WITH DISTINCT 1 AS value WHERE x>0 RETURN value",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2,3,4] AS x WITH DISTINCT x%2 AS value WHERE x>1 RETURN value",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2,3,4] AS x WITH DISTINCT x%2 AS value WHERE value=1 RETURN value",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT 1 AS value WHERE length(p)>1 RETURN value",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT length(p) AS hops WHERE hops>1 RETURN hops",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p WHERE all(n IN nodes(p) WHERE n IS NOT NULL) RETURN p",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p WHERE false RETURN p",
    ] {
        parity(&mut db, query, &Parameters::new());
    }
}

#[test]
fn projected_volatile_values_are_frozen_and_reused_after_deduplication() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [1,1,2] AS x
        WITH DISTINCT x,rand() AS draw WHERE draw=draw
        UNWIND [draw,draw] AS copy WITH DISTINCT x,draw,copy
        UNWIND [copy,copy] AS member RETURN x,draw,copy,member,rand() AS leaf";
    let rows = db
        .prepare_read(query)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 6);
    for pair in rows.as_chunks::<2>().0 {
        assert_eq!(pair[0][1], pair[1][1]);
        for row in pair {
            assert_eq!(row[1], row[2]);
            assert_eq!(row[1], row[3]);
        }
        assert_ne!(pair[0][4], pair[1][4]);
    }
    assert_ne!(rows[0][1], rows[2][1]);
    assert_ne!(rows[2][1], rows[4][1]);
}

#[test]
fn distinct_projection_errors_are_not_suppressed_by_skip_and_are_terminal() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()").unwrap();
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [8,[7]] AS x WITH DISTINCT x[0] AS value SKIP 1 LIMIT 1 RETURN value",
        "MATCH p=(:S)-[:R*]->() UNWIND [1] AS x WITH DISTINCT x WHERE 8[0]=1 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT 8[0] AS x RETURN x SKIP 1",
    ] {
        assert!(matches!(db.execute(query), Err(Error::Type(_))), "{query}");
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(
            matches!(cursor.next(), Some(Err(Error::Type(_)))),
            "{query}"
        );
        assert!(cursor.next().is_none());
    }
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [[7],[7],8] AS x WITH DISTINCT x[0] AS value RETURN value",
        "MATCH p=(:S)-[:R*]->() UNWIND [[7],[7],8] AS x WITH DISTINCT x RETURN x[0]",
        "MATCH p=(:S)-[:R*]->() UNWIND [[7],[7],8] AS x WITH DISTINCT x UNWIND [x[0]] AS y RETURN y",
    ] {
        let mut read = db.prepare_read(query).unwrap();
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(7)]);
            assert!(
                matches!(cursor.next(), Some(Err(Error::Type(_)))),
                "{query}"
            );
            assert!(cursor.next().is_none());
        }
        drop(read);
        assert_eq!(
            db.prepare_read(&format!("{query} LIMIT 1"))
                .unwrap()
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            [vec![CypherValue::Integer(7)]]
        );
    }
}

#[test]
fn final_shaping_and_union_keep_distinct_sets_local_to_each_stage_and_branch() {
    let mut db = fixture();
    for query in [
        "MATCH p=(:S)-[:R*]->(b) WITH DISTINCT b UNWIND [b,b,null] AS n RETURN DISTINCT n",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT length(p)%2 AS x RETURN x UNION ALL RETURN 1 AS x",
        "RETURN 1 AS x UNION MATCH p=(:S)-[:R*]->() WITH DISTINCT length(p)%2 AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p UNWIND [p,p] AS x RETURN DISTINCT x",
    ] {
        parity(&mut db, query, &Parameters::new());
    }
    let branch = "MATCH p=(:S)-[:R*]->() UNWIND [1,1,2] AS x WITH DISTINCT x RETURN x";
    let rows = db
        .prepare_read(&format!("{branch} UNION ALL {branch}"))
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows, [1, 2, 1, 2].map(|x| vec![CypherValue::Integer(x)]));
    // Eager window ranking may choose another unordered subset. Compare page
    // composition to this cursor's complete stream, whose bag was checked above.
    let query = "MATCH p=(:S)-[:R*]->() WITH DISTINCT p UNWIND [p,p] AS x RETURN DISTINCT x";
    let all = db
        .prepare_read(query)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    let page = db
        .prepare_read(&format!("{query} SKIP 1 LIMIT 2"))
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(page, all.into_iter().skip(1).take(2).collect::<Vec<_>>());
}

#[test]
fn zero_pages_preflight_all_stages_and_exhausted_distinct_limits_stop_ancestors() {
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
    for query in [
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT 8[0] AS x LIMIT 0 RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND 8[0] AS x WITH DISTINCT x LIMIT 0 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT 8[0] AS x WITH x LIMIT 0 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT 8[0] AS x RETURN x LIMIT 0",
    ] {
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        assert!(read.cursor().unwrap().next().is_none());
    }
    for query in [
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT $missing AS x LIMIT 0 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p LIMIT 0 WITH DISTINCT p SKIP $missing RETURN p",
        "MATCH p=(:S)-[:R*]->() WITH DISTINCT p LIMIT 0 UNWIND $missing AS x RETURN x",
        "RETURN 1 AS x UNION ALL MATCH p=(:S)-[:R*]->() WITH DISTINCT p LIMIT 0 RETURN $missing AS x",
    ] {
        assert!(
            matches!(db.prepare_read(query), Err(Error::MissingParameter(_))),
            "{query}"
        );
    }
    for value in [
        json!(-1),
        json!(1.5),
        json!(null),
        json!(true),
        json!("2"),
        json!([]),
    ] {
        for page in ["SKIP $page", "LIMIT $page"] {
            let query = format!(
                "MATCH p=(:S)-[:R*]->() WITH DISTINCT p LIMIT 0 WITH DISTINCT p {page} RETURN p"
            );
            assert!(
                db.prepare_read_with(&query, &Parameters::from([("page".into(), value.clone())]))
                    .is_err()
            );
        }
    }
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    for suffix in [
        "UNWIND [] AS x RETURN x",
        "WITH p WHERE false RETURN p",
        "WITH p SKIP 1 RETURN p",
    ] {
        let query = format!("MATCH p=(:S)-[:R*]->() WITH DISTINCT p LIMIT 1 {suffix}");
        let mut read = db
            .prepare_read_with_options(&query, &Parameters::new(), &options)
            .unwrap();
        assert!(read.cursor().unwrap().next().is_none(), "{query}");
    }
}

#[test]
fn duplicate_and_skipped_inputs_spend_transfer_budgets_and_union_work_is_cumulative() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query =
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [1,1,2] AS x WITH DISTINCT x RETURN x";
    // Domain + neighbor + page + UNWIND + three DISTINCT inputs + two outputs.
    for (limit, success) in [(8, false), (9, true)] {
        let options = QueryOptions {
            max_materialized_rows: Some(limit),
            ..QueryOptions::default()
        };
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            if success {
                assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(2)]);
            } else {
                assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
            }
            assert!(cursor.next().is_none());
        }
    }
    let query =
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [1,1,2] AS x WITH DISTINCT x SKIP 1 RETURN x";
    let options = QueryOptions {
        max_materialized_rows: Some(8),
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    assert_eq!(
        db.prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap()
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(2)]]
    );
    for options in [
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
        QueryOptions {
            traversal: TraversalLimits {
                max_payload_bytes: 0,
                ..TraversalLimits::default()
            },
            ..QueryOptions::default()
        },
    ] {
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        assert!(matches!(
            read.cursor().unwrap().next(),
            Some(Err(Error::ResourceLimit(_)))
        ));
    }
    let branch = "MATCH p=(:S)-[:R*]->() WITH DISTINCT 1 AS x LIMIT 1 RETURN x";
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            &format!("{branch} UNION ALL {branch}"),
            &Parameters::new(),
            &options,
        )
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
    assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
    assert!(cursor.next().is_none());
}

#[test]
fn hydration_errors_roll_back_prior_writes_while_drop_and_limit_preserve_them() {
    for action in ["error", "drop", "limit"] {
        let mut db = Database::in_memory().unwrap();
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
        let query = format!(
            "MATCH p=(:S)-[:R*]->() WITH DISTINCT p LIMIT 1
            UNWIND [[7],[7],[8],9] AS x WITH DISTINCT p,x {} RETURN p,x[0]",
            if action == "limit" { "LIMIT 2" } else { "" }
        );
        let mut read = tx.prepare_read(&query).unwrap();
        let mut cursor = read.cursor().unwrap();
        let row = cursor.next().unwrap().unwrap();
        assert!(matches!(&row[0],CypherValue::Path(p) if p.relationships.len()==1));
        assert_eq!(row[1], CypherValue::Integer(7));
        if action == "error" {
            assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
            assert!(cursor.next().is_none());
        } else if action == "limit" {
            assert_eq!(cursor.next().unwrap().unwrap()[1], CypherValue::Integer(8));
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
fn retained_distinct_sets_share_snapshot_and_control_cleanup_rules() {
    use crabnet::{CancellationToken, OpenMode, OpenOptions};
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        time::{Duration, Instant},
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
    writer.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let mut reader = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH DISTINCT p LIMIT 1 UNWIND [1,1,2] AS x WITH DISTINCT x UNWIND [x,x+10] AS y RETURN y";
    for deadline in [false, true] {
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
        let mut read = reader
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
        assert!(matches!(
            writer.execute("CREATE (:Blocked)"),
            Err(Error::Busy(_))
        ));
        if deadline {
            std::thread::sleep(expires.saturating_duration_since(Instant::now()));
        } else {
            token.cancel();
        }
        assert!(matches!(
            cursor.next(),
            Some(Err(Error::Cancelled | Error::DeadlineExceeded))
        ));
        writer.execute("CREATE (:Released)").unwrap();
        assert!(cursor.next().is_none());
    }
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
            cursor.next().unwrap().unwrap();
            if action == "exhaust" {
                assert_eq!(
                    cursor
                        .by_ref()
                        .collect::<crabnet::Result<Vec<_>>>()
                        .unwrap(),
                    [11, 2, 12].map(|x| vec![CypherValue::Integer(x)])
                );
                writer.execute("CREATE (:Exhausted)").unwrap();
            }
        }
        writer.execute("CREATE (:Released)").unwrap();
    }
}
