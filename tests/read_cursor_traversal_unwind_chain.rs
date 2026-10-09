use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions, ReadMode, TraversalLimits};
use serde_json::json;

#[test]
fn dependent_expansions_resume_and_early_limit_avoids_unread_members_and_trails() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH p,$groups AS groups
        UNWIND groups AS xs WITH p,xs
        UNWIND xs AS x WITH x[0] AS v RETURN v";
    let params = Parameters::from([(
        "groups".into(),
        json!([[], [[7], [8]], null, [[9], 10], [[11]]]),
    )]);
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
        for value in [7, 8, 9] {
            assert_eq!(
                cursor.next().unwrap().unwrap(),
                [CypherValue::Integer(value)]
            );
        }
        assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
        assert!(cursor.next().is_none());
    }
    drop(read);
    let mut read = db
        .prepare_read_with_options(&format!("{query} LIMIT 3"), &params, &options)
        .unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [7, 8, 9].map(|v| vec![CypherValue::Integer(v)])
    );
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

fn fixture() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:S {x:1,tags:['a','a']}),(b {x:2,tags:[]}),(c:S {x:3}),
        (a)-[:R {open:true}]->(b),(a)-[:R {open:true}]->(b),
        (b)-[:R {open:false}]->(b),(b)-[:R {open:true}]->(c),(c)-[:R {open:true}]->(a)",
    )
    .unwrap();
    db
}

#[test]
fn chained_expansions_preserve_dependent_scalar_graph_and_nested_bags() {
    let mut db = fixture();
    let params = Parameters::from([
        (
            "groups".into(),
            json!([[],null,7,{},[null,true,false,1,1.0,-0.0,"é,]\\\"",{"x":[1,null]}],[1,1]]),
        ),
        ("open".into(), json!(true)),
    ]);
    for query in [
        "MATCH p=(a:S)-[:R*0..40]->(b) UNWIND nodes(p) AS n UNWIND n.tags AS tag RETURN p,a,b,n,tag",
        "MATCH p=(:S)-[:R*0..]->() UNWIND nodes(p) AS n UNWIND relationships(p) AS r RETURN p,n,r",
        "MATCH p=(:S)-[rs:R*0..]->() WITH p,rs UNWIND rs AS r WITH p,r UNWIND [r,r,null] AS s RETURN p,r,s,type(s)",
        "MATCH p=(:S)-[:R*]->(b) UNWIND [b,b,null] AS n UNWIND [n,n] AS m WITH p,m RETURN p,m,m.x",
        "MATCH p=(:S)-[:R*]->(b) UNWIND [[p,b],[],[{path:p,node:b},null]] AS xs UNWIND xs AS x UNWIND [x,x] AS y RETURN p,y",
        "MATCH p=(:S)-[:R*]->() UNWIND $groups AS xs WITH p,xs UNWIND xs AS x WITH x,length(p) AS hops RETURN x,hops",
        "MATCH p=(:S)-[:R* {open:$open}]->() UNWIND nodes(p) AS n WITH p,n WHERE n.x>1 UNWIND reverse(relationships(p)) AS r WITH p,n,r RETURN p,n,r",
        "MATCH p=(:S)<-[:R*]-() UNWIND nodes(p) AS n UNWIND [n.x,n.x] AS x RETURN p,x",
        "MATCH p=(:S)-[:R*]-() UNWIND relationships(p) AS r UNWIND [r,r] AS s RETURN p,s",
        "MATCH p=()-[:R*]->() UNWIND nodes(p) AS n UNWIND [n.x] AS x RETURN p,x",
        "MATCH p=(:Missing)-[:R*]->() UNWIND [1] AS x UNWIND 8[0] AS y RETURN y",
        "MATCH p=(:S)-[:R*]->() UNWIND [] AS x UNWIND 8[0] AS y RETURN y",
        "MATCH p=(:S)-[:NONE*0..]->() UNWIND nodes(p) AS n UNWIND [n,n] AS m RETURN m",
        "MATCH p=(:S)-[:R*]->() UNWIND [[],null,[],7] AS x UNWIND x AS y RETURN y",
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
fn backtracking_restores_ancestors_across_scope_drops_filters_and_four_levels() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x WITH p UNWIND nodes(p) AS n RETURN n,p",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x WITH x AS n UNWIND [n,n+10] AS x WITH n,x WHERE x>1 UNWIND [[],[x]] AS xs UNWIND xs AS v RETURN n,x,v",
        "MATCH p=(:S)-[:R*]->() UNWIND [0,1,2] AS x WITH x WHERE x>0 UNWIND range(0,x) AS y WITH x,y WHERE y>0 UNWIND [y,null] AS z WITH x,y,z WHERE z IS NOT NULL RETURN x,y,z",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x WITH p UNWIND [1,2] AS y WITH p UNWIND [1,2] AS z WITH p UNWIND [1,2] AS last RETURN length(p),last",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x UNWIND [x] AS y WITH 42 AS answer UNWIND [answer] AS z RETURN z",
    ] {
        let expected = db.execute(query).unwrap().rows;
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        let actual = read
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(bag(actual), bag(expected), "{query}");
    }
}

#[test]
fn volatile_values_survive_inner_resumption_and_refresh_at_their_own_scope() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH rand() AS trail,length(p) AS hops
        UNWIND [1,2] AS outer WITH trail,hops,outer,rand() AS middle,[rand(),rand()] AS xs
        UNWIND xs AS inner WITH trail,hops,outer,middle,inner
        UNWIND [inner,inner] AS value WITH trail,hops,outer,middle,inner,value,rand() AS leaf
        RETURN trail,hops,outer,middle,inner,value,leaf,leaf AS copy";
    let rows = db
        .prepare_read(query)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 16);
    for trail in rows.as_chunks::<8>().0 {
        assert!(
            trail
                .iter()
                .all(|r| r[0] == trail[0][0] && r[1] == trail[0][1])
        );
        for group in trail.as_chunks::<4>().0 {
            assert!(
                group
                    .iter()
                    .all(|r| r[2] == group[0][2] && r[3] == group[0][3])
            );
            for pair in group.as_chunks::<2>().0 {
                assert_eq!(pair[0][4], pair[1][4]);
                for row in pair {
                    assert_eq!(row[4], row[5]);
                    assert_eq!(row[6], row[7]);
                }
                assert_ne!(pair[0][6], pair[1][6]);
            }
            assert_ne!(group[0][4], group[2][4]);
        }
        assert_ne!(trail[0][3], trail[4][3]);
    }
    assert_ne!(rows[0][0], rows[8][0]);
}

#[test]
fn final_distinct_union_and_pages_compose_after_nested_expansion() {
    let mut db = fixture();
    let params = Parameters::from([("skip".into(), json!(2)), ("limit".into(), json!(4))]);
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n UNWIND [n,n,null] AS x RETURN x SKIP $skip LIMIT $limit",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n UNWIND [n,n,null] AS x RETURN DISTINCT x SKIP $skip LIMIT $limit",
        "MATCH p=(:S)-[:R*]->() UNWIND [p,p] AS x UNWIND [x,x] AS y RETURN DISTINCT y SKIP $skip LIMIT $limit",
        "RETURN null AS x UNION ALL MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n UNWIND [n,n] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND [[1,1.0],[],[null,2]] AS xs UNWIND xs AS x RETURN x UNION RETURN 1 AS x",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x UNWIND [x,x] AS y RETURN y SKIP 999999",
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
fn intermediate_global_stages_and_joined_producers_remain_buffered() {
    let mut db = fixture();
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH DISTINCT n UNWIND [n] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH n SKIP 0 UNWIND [n] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH n LIMIT 0 UNWIND [n] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH n ORDER BY n.x UNWIND [n] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n WITH collect(n) AS ns UNWIND ns AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n UNWIND [n] AS x RETURN count(x)",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n UNWIND [n] AS x RETURN x ORDER BY x.x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n MATCH (n)-[:R]->(b) UNWIND [b] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND nodes(p) AS n OPTIONAL MATCH (n)-[:R]->(b) UNWIND [b] AS x RETURN x",
        "MATCH p=(:S)-[:R*]->()-[:R*]->() UNWIND nodes(p) AS n UNWIND [n] AS x RETURN x",
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
fn errors_at_each_level_are_terminal_and_skip_only_guards_return() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()").unwrap();
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [[[7]],8,[[9]]] AS x UNWIND x[0] AS y RETURN y",
        "MATCH p=(:S)-[:R*]->() UNWIND [[[7]],8,[[9]]] AS x WITH x[0] AS ys UNWIND ys AS y RETURN y",
        "MATCH p=(:S)-[:R*]->() UNWIND [[[7],8]] AS xs UNWIND xs AS x WITH x[0] AS y UNWIND [y] AS z RETURN z",
        "MATCH p=(:S)-[:R*]->() UNWIND [[[7],8]] AS xs UNWIND xs AS x RETURN x[0]",
    ] {
        let mut read = db.prepare_read(query).unwrap();
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(
                cursor.next().unwrap().unwrap(),
                [CypherValue::Integer(7)],
                "{query}"
            );
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
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [[8,[7]],[[9]]] AS xs UNWIND xs AS x RETURN x[0] SKIP 1 LIMIT 1";
    assert_eq!(
        db.execute(query).unwrap().rows,
        [vec![CypherValue::Integer(7)]]
    );
    assert_eq!(
        db.prepare_read(query)
            .unwrap()
            .cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        [vec![CypherValue::Integer(7)]]
    );
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [8] AS x WITH x[0] AS xs UNWIND xs AS y RETURN y SKIP 1",
        "MATCH p=(:S)-[:R*]->() UNWIND [8] AS x UNWIND x[0] AS y RETURN y SKIP 1",
        "MATCH p=(:S)-[:R*]->() UNWIND [[8,[7]]] AS xs UNWIND xs AS x WITH x[0] AS y RETURN y SKIP 1",
    ] {
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert!(
            matches!(cursor.next(), Some(Err(Error::Type(_)))),
            "{query}"
        );
        assert!(cursor.next().is_none());
    }
}

#[test]
fn controls_account_for_every_input_stage_and_consumed_member() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()").unwrap();
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [[1,2],[],[3]] AS xs UNWIND xs AS x RETURN x";
    let params = Parameters::new();
    // Domain + neighbor + outer input + first inner input + first output = 5.
    for options in [
        QueryOptions {
            max_materialized_rows: Some(5),
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
    // An empty list still costs an input transfer; a filtered input has none.
    for (middle, limit) in [("", 7), ("WITH xs WHERE size(xs)>0", 6)] {
        let query = format!(
            "MATCH p=(:S)-[:R*]->() UNWIND [[1,2],[],[3]] AS xs {middle} UNWIND xs AS x RETURN x"
        );
        let options = QueryOptions {
            max_materialized_rows: Some(limit),
            ..QueryOptions::default()
        };
        let mut read = db
            .prepare_read_with_options(&query, &params, &options)
            .unwrap();
        let mut cursor = read.cursor().unwrap();
        cursor.next().unwrap().unwrap();
        cursor.next().unwrap().unwrap();
        assert!(matches!(cursor.next(), Some(Err(Error::ResourceLimit(_)))));
        assert!(cursor.next().is_none());
    }
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
    ] {
        let mut read = db
            .prepare_read_with_options(query, &params, &options)
            .unwrap();
        assert!(matches!(
            read.cursor().unwrap().next(),
            Some(Err(Error::ResourceLimit(_)))
        ));
    }
    let options = QueryOptions {
        max_materialized_bytes: Some(4096),
        ..QueryOptions::default()
    };
    let mut read = db
        .prepare_read_with_options(
            "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x UNWIND range(1,10000) AS y RETURN y LIMIT 1",
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
fn zero_pages_preflight_every_stage_and_union_shares_work() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
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
    let query = "MATCH p=(:S)-[:R*]->() UNWIND 8[0] AS x UNWIND x AS y RETURN y LIMIT 0";
    let mut read = db
        .prepare_read_with_options(query, &params, &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    assert!(read.cursor().unwrap().next().is_none());
    drop(read);
    for query in [
        "MATCH p=(:S)-[:R*]->() UNWIND [1] AS x UNWIND $missing AS y RETURN y LIMIT 0",
        "MATCH p=(:S)-[:R*]->() UNWIND [1] AS x WITH $missing AS xs UNWIND xs AS y RETURN y LIMIT 0",
        "RETURN 1 AS y UNION ALL MATCH p=(:S)-[:R*]->() UNWIND [1] AS x UNWIND $missing AS y RETURN y",
    ] {
        assert!(matches!(
            db.prepare_read(query),
            Err(Error::MissingParameter(_))
        ));
    }
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let branch = "MATCH p=(:S)-[:R*]->() UNWIND [1] AS x UNWIND [x,x+1] AS y RETURN y LIMIT 2";
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
fn hydration_errors_rollback_prior_writes_and_early_exit_preserves_them() {
    for action in ["error", "drop", "limit"] {
        let mut db = Database::in_memory().unwrap();
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:S)-[:R]->()").unwrap();
        let query = format!(
            "MATCH p=(:S)-[:R*]->() UNWIND [[[7]],[[8],9]] AS xs UNWIND xs AS x RETURN p,x[0]{}",
            if action == "limit" { " LIMIT 1" } else { "" }
        );
        let mut read = tx.prepare_read(&query).unwrap();
        let mut cursor = read.cursor().unwrap();
        let row = cursor.next().unwrap().unwrap();
        assert!(matches!(&row[0],CypherValue::Path(p) if p.relationships.len()==1));
        assert_eq!(row[1], CypherValue::Integer(7));
        if action == "error" {
            // Filling the next hydration batch crosses a parent-list boundary.
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
fn cancellation_deadline_and_snapshot_cleanup_cover_suspended_inner_frames() {
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
    writer.execute("CREATE (:S)-[:R]->(:End)").unwrap();
    let mut reader = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap();
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x UNWIND [x,x+10] AS y RETURN y";
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
        writer.execute("CREATE (:AfterError)").unwrap();
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
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            if action == "exhaust" {
                assert_eq!(
                    cursor
                        .by_ref()
                        .collect::<crabnet::Result<Vec<_>>>()
                        .unwrap(),
                    [11, 2, 12].map(|v| vec![CypherValue::Integer(v)])
                );
                writer.execute("CREATE (:Exhausted)").unwrap();
            }
        }
        writer.execute("CREATE (:Released)").unwrap();
    }
}
