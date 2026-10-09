use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions, ReadMode, TraversalLimits};
use serde_json::json;

#[test]
fn intermediate_pages_count_across_parents_and_drain_admitted_expansions() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S)-[:R]->()-[:R]->()-[:R]->()")
        .unwrap();
    let query = "MATCH p=(:S)-[:R*]->()
        WITH length(p) AS hops SKIP $skip LIMIT $limit
        UNWIND [1,2,3] AS x WITH hops,x SKIP 2 LIMIT 3
        UNWIND [x,x+10] AS y RETURN hops,x,y";
    let params = Parameters::from([("skip".into(), json!(1)), ("limit".into(), json!(2))]);
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 3,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let expected = [
        (2, 3, 3),
        (2, 3, 13),
        (3, 1, 1),
        (3, 1, 11),
        (3, 2, 2),
        (3, 2, 12),
    ]
    .map(|(h, x, y)| {
        vec![
            CypherValue::Integer(h),
            CypherValue::Integer(x),
            CypherValue::Integer(y),
        ]
    });
    assert_eq!(db.execute_with(query, &params).unwrap().rows, expected);
    let mut read = db
        .prepare_read_with_options(query, &params, &options)
        .unwrap();
    assert_eq!(read.mode(), ReadMode::Incremental);
    for _ in 0..2 {
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            expected
        );
    }
}

fn chain() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:S {x:0})-[:R {x:1}]->({x:1})-[:R {x:2}]->({x:2})-[:R {x:3}]->({x:3})")
        .unwrap();
    db
}

#[test]
fn successive_filtered_pages_match_an_independent_sequence_model() {
    let mut db = chain();
    let query = "MATCH p=(:S)-[:R*]->() UNWIND [0,1,2] AS x
        WITH length(p) AS hops,x SKIP $s1 LIMIT $l1 WHERE x>0
        UNWIND range(0,x) AS y WITH hops,x,y SKIP $s2 LIMIT $l2 WHERE y>0
        UNWIND [y,y+10] AS z RETURN hops,x,z";
    for (s1, l1) in [(0, 0), (0, 1), (0, 99), (1, 3), (4, 5), (99, 1)] {
        for (s2, l2) in [(0, 0), (0, 1), (0, 99), (1, 3), (4, 5), (99, 1)] {
            let expected = (1..=3)
                .flat_map(|h| (1..=2).map(move |x| (h, x)))
                .skip(s1)
                .take(l1)
                .flat_map(|(h, x)| (1..=x).map(move |y| (h, x, y)))
                .skip(s2)
                .take(l2)
                .flat_map(|(h, x, y)| {
                    [y, y + 10].map(|z| {
                        vec![
                            CypherValue::Integer(h),
                            CypherValue::Integer(x),
                            CypherValue::Integer(z),
                        ]
                    })
                })
                .collect::<Vec<_>>();
            let params = Parameters::from([
                ("s1".into(), json!(s1)),
                ("l1".into(), json!(l1)),
                ("s2".into(), json!(s2)),
                ("l2".into(), json!(l2)),
            ]);
            let rows = db
                .prepare_read_with(query, &params)
                .unwrap()
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(rows, expected, "{params:?}");
        }
    }
}

fn bag(mut rows: Vec<Vec<CypherValue>>) -> Vec<Vec<CypherValue>> {
    rows.sort_by_cached_key(|row| format!("{row:?}"));
    rows
}

#[test]
fn pages_preserve_typed_bags_filters_scope_changes_and_empty_inputs() {
    let mut db = chain();
    let params = Parameters::from([("skip".into(), json!(1)), ("limit".into(), json!(2))]);
    for query in [
        "MATCH p=(a:S)-[rs:R*0..]->(b) WITH * SKIP $skip LIMIT $limit RETURN p,a,b,rs,nodes(p),relationships(p)",
        "MATCH p=(:S)-[:R*]->(b) WITH p,b SKIP 0 RETURN p,b",
        "MATCH p=(:S)-[:R*]->(b) WITH p,b LIMIT 999 RETURN p,b",
        "MATCH p=(:S)-[:R*]->(b) WITH p,b SKIP 999 RETURN p,b",
        "MATCH p=(:S)-[:R*]->(b) WITH b.x AS x SKIP 1 LIMIT 1 WHERE b.x>1 RETURN x",
        "MATCH p=(:S)-[:R*]->(b) WITH b.x AS x SKIP 1 LIMIT 1 WHERE x>1 RETURN x",
        "MATCH p=(:S)-[:R*]->(b) WITH CASE WHEN b.x=2 THEN null ELSE b END AS n SKIP 1 LIMIT 2 RETURN n,n.x",
        "MATCH p=(:S)-[:R*]->(b) WITH {path:p,nodes:[b,b,null],values:[1,1.0,null]} AS box SKIP 1 RETURN box",
        "MATCH p=(:S)-[:R*]->(b) WITH p,b SKIP 1 WITH p AS q,b LIMIT 1 WITH q,b RETURN q,b",
        "MATCH p=(:S)-[:R*]->() UNWIND [1,2] AS x WITH p SKIP 1 LIMIT 3 UNWIND nodes(p) AS n RETURN n,p",
        "MATCH p=(:S)-[:R*]->() UNWIND [[1,1,null],[],[1.0,2],null] AS xs WITH xs SKIP 1 LIMIT 5 UNWIND xs AS x WITH x SKIP 1 LIMIT 3 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 2 UNWIND nodes(p) AS n WITH n SKIP 1 LIMIT 3 UNWIND [n,n,null] AS m WITH m SKIP 2 LIMIT 4 RETURN m",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [[],null,7] AS xs WITH xs LIMIT 2 UNWIND xs AS x RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH p SKIP 1 LIMIT 1 WITH 42 AS x SKIP 0 WITH x AS y LIMIT 2 RETURN y",
        "MATCH (:S)-[:R*]->() WITH * SKIP 1 LIMIT 1 RETURN 1",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 WITH p LIMIT 3 RETURN p",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 3 WITH p LIMIT 1 RETURN p",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 2 WITH p WHERE false RETURN p",
        "MATCH p=(:Missing)-[:R*]->() WITH 8[0] AS x SKIP 1 LIMIT 1 RETURN x",
        "MATCH p=(:S)-[:NONE*0..]->() WITH p LIMIT 1 UNWIND nodes(p) AS n RETURN p,n",
        "MATCH p=()-[:R*]->() WITH p SKIP 1 LIMIT 3 RETURN p",
        "MATCH p=(:S)<-[:R*0..]-() WITH p SKIP 0 LIMIT 2 RETURN p",
        "MATCH p=(:S)-[:R*0..]-() WITH p SKIP 1 LIMIT 3 RETURN p",
    ] {
        let expected = db
            .execute_with(query, &params)
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        let mut read = db.prepare_read_with(query, &params).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental, "{query}");
        assert_eq!(read.columns(), expected.columns);
        for _ in 0..2 {
            let rows = read
                .cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap_or_else(|e| panic!("{query}: {e}"));
            assert_eq!(bag(rows), bag(expected.rows.clone()), "{query}");
        }
    }
}

#[test]
fn page_transfers_and_union_branches_share_resource_budgets() {
    let mut db = chain();
    let query =
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [1,2,3] AS x WITH x SKIP 1 LIMIT 2 RETURN x";
    // Domain + neighbor + page input + UNWIND input + skipped page input +
    // admitted page input + output = seven transfers through the first row.
    for options in [
        QueryOptions {
            max_materialized_rows: Some(7),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_rows: Some(1),
            ..QueryOptions::default()
        },
    ] {
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
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
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    let branch = "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [1,2] AS x WITH x LIMIT 1 RETURN x";
    let mut read = db
        .prepare_read_with_options(
            &format!("{branch} UNION ALL {branch}"),
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
}

#[test]
fn page_hydration_errors_abort_prior_writes_and_early_exit_keeps_them() {
    for action in ["error", "drop", "limit"] {
        let mut db = Database::in_memory().unwrap();
        let mut tx = db.transaction().unwrap();
        tx.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
        let query = format!(
            "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1
            UNWIND [[7],[8],9] AS x WITH p,x LIMIT {} RETURN p,x[0]",
            if action == "limit" { 2 } else { 3 }
        );
        let mut read = tx.prepare_read(&query).unwrap();
        let mut cursor = read.cursor().unwrap();
        let row = cursor.next().unwrap().unwrap();
        assert!(matches!(&row[0], CypherValue::Path(p) if p.relationships.len()==1));
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
fn suspended_pages_release_snapshots_on_cancel_drop_exhaustion_and_panic() {
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
    writer.execute("CREATE (:S)-[:R]->()-[:R]->()").unwrap();
    let mut reader = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap();
    let query = "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [1,2] AS x WITH x LIMIT 2 UNWIND [x,x+10] AS y RETURN y";
    let token = CancellationToken::new();
    let mut read = reader
        .prepare_read_with_options(
            query,
            &Parameters::new(),
            &QueryOptions {
                cancellation: Some(token.clone()),
                ..QueryOptions::default()
            },
        )
        .unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
    assert!(matches!(
        writer.execute("CREATE (:Blocked)"),
        Err(Error::Busy(_))
    ));
    token.cancel();
    assert!(matches!(cursor.next(), Some(Err(Error::Cancelled))));
    writer.execute("CREATE (:Released)").unwrap();
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

#[test]
fn exhausted_pages_stop_ancestors_after_empty_or_filtered_descendants() {
    let mut db = chain();
    let options = QueryOptions {
        traversal: TraversalLimits {
            max_states: 1,
            ..TraversalLimits::default()
        },
        ..QueryOptions::default()
    };
    for (suffix, expected) in [
        ("WITH p LIMIT 1 UNWIND [1,2,3] AS x RETURN x", vec![1, 2, 3]),
        ("UNWIND [1,2,3,8[0]] AS x WITH x LIMIT 1 RETURN x", vec![]),
        (
            "UNWIND [[1],[2],8] AS xs WITH xs LIMIT 2 UNWIND xs AS x RETURN x",
            vec![1, 2],
        ),
        (
            "UNWIND [1,2,3] AS x WITH x LIMIT 2 UNWIND [] AS y RETURN y",
            vec![],
        ),
        ("WITH p LIMIT 1 WITH p WHERE false RETURN 1", vec![]),
        (
            "UNWIND [1,2,3] AS x WITH x LIMIT 2 WITH x WHERE false RETURN x",
            vec![],
        ),
        ("WITH p LIMIT 1 WITH p SKIP 1 RETURN 1", vec![]),
    ] {
        let query = format!("MATCH p=(:S)-[:R*]->() {suffix}");
        let mut read = db
            .prepare_read_with_options(&query, &Parameters::new(), &options)
            .unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        let result = read.cursor().unwrap().collect::<crabnet::Result<Vec<_>>>();
        if suffix.contains("8[0]") {
            // A list is still evaluated in full before its members are paged.
            assert!(matches!(result, Err(Error::Type(_))));
        } else {
            assert_eq!(
                result.unwrap(),
                expected
                    .into_iter()
                    .map(|v| vec![CypherValue::Integer(v)])
                    .collect::<Vec<_>>(),
                "{query}"
            );
        }
    }
}

#[test]
fn skip_guards_its_projection_but_still_evaluates_filters_and_prior_stages() {
    let mut db = chain();
    for query in [
        "MATCH p=(:S)-[:R*]->() WITH [8,[7],9][length(p)-1][0] AS x SKIP 1 LIMIT 1 RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND [8,[7],9] AS x WITH x[0] AS y SKIP 1 LIMIT 1 RETURN y",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [8,[7],9] AS x WITH x SKIP 1 LIMIT 1 RETURN x[0]",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [8,[7],9] AS x WITH x SKIP 0 LIMIT 2 RETURN x[0] SKIP 1",
    ] {
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
            [vec![CypherValue::Integer(7)]],
            "{query}"
        );
    }
    for query in [
        "MATCH p=(:S)-[:R*]->() WITH 8[0] AS x WITH x SKIP 1 LIMIT 1 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH p SKIP 1 LIMIT 1 WHERE 8[0]=1 RETURN p",
        "MATCH p=(:S)-[:R*]->() WITH 8[0] AS x SKIP 1 LIMIT 1 WHERE x=1 RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND 8[0] AS x WITH x SKIP 1 LIMIT 1 RETURN x",
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
    let query = "MATCH p=(:S)-[:R*]->() WITH p LIMIT 1 UNWIND [[7],8,[9]] AS x WITH x[0] AS y LIMIT 3 RETURN y";
    let mut read = db.prepare_read(query).unwrap();
    let mut cursor = read.cursor().unwrap();
    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(7)]);
    assert!(matches!(cursor.next(), Some(Err(Error::Type(_)))));
    assert!(cursor.next().is_none());
}

#[test]
fn zero_pages_preflight_all_parameters_without_starting_any_stage() {
    let mut db = chain();
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
        "MATCH p=(:S)-[:R*]->() WITH 8[0] AS x LIMIT 0 RETURN x",
        "MATCH p=(:S)-[:R*]->() UNWIND 8[0] AS x WITH x LIMIT 0 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH 8[0] AS x LIMIT 1 WITH x SKIP 999 LIMIT 0 UNWIND 8[0] AS y RETURN y",
        "MATCH p=(:S)-[:R*]->() WITH 8[0] AS x LIMIT 1 RETURN x LIMIT 0",
    ] {
        let mut read = db
            .prepare_read_with_options(query, &Parameters::new(), &options)
            .unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        assert!(read.cursor().unwrap().next().is_none());
    }
    for query in [
        "MATCH p=(:S)-[:R*]->() WITH $missing AS x LIMIT 0 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 0 WITH p SKIP $missing RETURN p",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 0 UNWIND $missing AS x RETURN x",
        "RETURN 1 AS x UNION ALL MATCH p=(:S)-[:R*]->() WITH p LIMIT 0 RETURN $missing AS x",
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
            let query = format!("MATCH p=(:S)-[:R*]->() WITH p LIMIT 0 WITH p {page} RETURN p");
            let params = Parameters::from([("page".into(), value.clone())]);
            assert!(
                db.prepare_read_with(&query, &params).is_err(),
                "{query}: {value}"
            );
        }
    }
}

#[test]
fn pages_keep_volatile_aliases_stable_through_downstream_expansions() {
    let mut db = chain();
    let query = "MATCH p=(:S)-[:R*]->() WITH rand() AS trail,p SKIP 1 LIMIT 2
        UNWIND [1,2] AS x WITH trail,x,rand() AS member SKIP 1 LIMIT 2
        UNWIND [member,member] AS y WITH trail,x,member,y,rand() AS leaf LIMIT 4
        RETURN trail,x,member,y,leaf,leaf AS copy";
    let rows = db
        .prepare_read(query)
        .unwrap()
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 4);
    for pair in rows.as_chunks::<2>().0 {
        for row in pair {
            assert_eq!(row[2], row[3]);
            assert_eq!(row[4], row[5]);
            assert_eq!(row[0], pair[0][0]);
            assert_eq!(row[2], pair[0][2]);
        }
        assert_ne!(pair[0][4], pair[1][4]);
    }
    assert_ne!(rows[0][0], rows[2][0]);
    assert_ne!(rows[0][2], rows[2][2]);
}

#[test]
fn final_distinct_union_and_pagination_compose_with_stage_pages() {
    let mut db = chain();
    for query in [
        "MATCH p=(:S)-[:R*]->(n) WITH n LIMIT 2 UNWIND [n,n,null] AS x WITH x SKIP 1 LIMIT 4 RETURN DISTINCT x SKIP 1 LIMIT 2",
        "MATCH p=(:S)-[:R*]->() WITH p LIMIT 2 UNWIND [p,p] AS x WITH x LIMIT 3 RETURN DISTINCT x",
        "MATCH p=(:S)-[:R*]->() WITH length(p) AS x SKIP 1 LIMIT 1 RETURN x UNION ALL RETURN 2 AS x",
        "RETURN 2 AS x UNION MATCH p=(:S)-[:R*]->() WITH length(p) AS x SKIP 1 LIMIT 1 RETURN x",
        "MATCH p=(:S)-[:R*]->() WITH length(p) AS x LIMIT 2 RETURN x SKIP 1 LIMIT 1",
    ] {
        let expected = db.execute(query).unwrap().rows;
        let mut read = db.prepare_read(query).unwrap();
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
