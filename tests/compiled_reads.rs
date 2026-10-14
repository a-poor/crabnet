use std::time::{Duration, Instant};

use crabnet::{
    CancellationToken, CypherValue, Database, Error, OpenMode, OpenOptions, Parameters,
    QueryOptions, compile_read, compile_read_with_options,
};
use serde_json::json;

fn parameters(value: serde_json::Value) -> Parameters {
    Parameters::from([("value".into(), value)])
}

#[test]
fn compiled_reads_own_the_plan_and_rebind_every_execution() {
    let plan = {
        let source = String::from("RETURN $value AS value, [$value, $value] AS copies");
        compile_read(&source).unwrap()
    };
    assert_eq!(plan.columns(), ["value", "copies"]);
    let mut db = Database::in_memory().unwrap();
    for value in [
        json!(null),
        json!(true),
        json!(1),
        json!(1.5),
        json!("x' $other"),
        json!([1, null]),
        json!({"nested": [false]}),
    ] {
        let expected = CypherValue::try_from(value.clone()).unwrap();
        let result = db.execute_compiled_with(&plan, &parameters(value)).unwrap();
        assert_eq!(result.columns, plan.columns());
        assert_eq!(
            result.rows,
            [vec![
                expected.clone(),
                CypherValue::List(vec![expected.clone(), expected])
            ]]
        );
        assert_eq!(result.summary, Default::default());
    }
    db.execute("CREATE (:AfterReads)").unwrap();
    let other_plan = compile_read("MATCH (n) RETURN count(n) AS count").unwrap();
    assert_eq!(
        db.execute_compiled(&other_plan).unwrap().rows,
        [vec![CypherValue::Integer(1)]]
    );
    let mut other = Database::in_memory().unwrap();
    assert_eq!(
        other.execute_compiled(&other_plan).unwrap().rows,
        [vec![CypherValue::Integer(0)]]
    );
    assert_eq!(
        other
            .execute_compiled_with(&plan.clone(), &parameters(json!(2)))
            .unwrap()
            .rows[0][0],
        CypherValue::Integer(2)
    );
}

#[test]
fn compilation_rejects_mutations_and_static_errors_and_honors_controls() {
    for query in [
        "CREATE ()",
        "MATCH (n) SET n.x = 1 RETURN n",
        "MATCH (n) DELETE n",
        "MERGE (:X)",
    ] {
        assert!(
            matches!(compile_read(query), Err(Error::Unsupported(_))),
            "{query}"
        );
    }
    assert!(matches!(
        compile_read("RETURN unknown"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        compile_read("RETURN sum('bad')"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(compile_read("RETURN ("), Err(Error::Parse(_))));
    assert!(matches!(compile_read(""), Err(Error::EmptyQuery)));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = QueryOptions {
        cancellation: Some(cancellation),
        ..QueryOptions::default()
    };
    let expired = QueryOptions {
        deadline: Some(Instant::now()),
        ..QueryOptions::default()
    };
    assert!(matches!(
        compile_read_with_options("RETURN (", &cancelled),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        compile_read_with_options("RETURN (", &expired),
        Err(Error::DeadlineExceeded)
    ));
    // Compilation does no query work and retains none of these controls.
    let token = CancellationToken::new();
    let plan = compile_read_with_options(
        "RETURN $value",
        &QueryOptions {
            cancellation: Some(token.clone()),
            max_result_rows: Some(0),
            max_sqlite_steps: Some(0),
            ..QueryOptions::default()
        },
    )
    .unwrap();
    token.cancel();
    let mut db = Database::in_memory().unwrap();
    assert_eq!(
        db.execute_compiled_with(&plan, &parameters(json!(1)))
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn every_execution_preflights_all_bindings_and_recovers_after_errors() {
    let mut db = Database::in_memory().unwrap();
    let plan =
        compile_read("RETURN abs($value) AS value UNION ALL RETURN $later AS value").unwrap();
    // The later missing binding wins over the first branch's runtime error.
    assert!(
        matches!(db.execute_compiled_with(&plan, &parameters(json!("bad"))), Err(Error::MissingParameter(name)) if name == "later")
    );
    let mut params = parameters(json!("bad"));
    params.insert("later".into(), json!(1));
    assert!(matches!(
        db.execute_compiled_with(&plan, &params),
        Err(Error::Type(_))
    ));
    params.insert("value".into(), json!(3));
    assert_eq!(
        db.execute_compiled_with(&plan, &params).unwrap().rows,
        [vec![CypherValue::Integer(3)], vec![CypherValue::Integer(1)]]
    );
    params.insert("later".into(), json!(u64::MAX));
    assert!(matches!(
        db.execute_compiled_with(&plan, &params),
        Err(Error::Semantic(_))
    ));
    let page = compile_read("UNWIND [1, 1, 2] AS x RETURN x SKIP $skip LIMIT $limit").unwrap();
    for (skip, limit, expected) in [(0, 2, vec![1, 1]), (1, 2, vec![1, 2]), (0, 0, vec![])] {
        let params =
            Parameters::from([("skip".into(), json!(skip)), ("limit".into(), json!(limit))]);
        assert_eq!(
            db.execute_compiled_with(&page, &params).unwrap().rows,
            expected
                .into_iter()
                .map(|v| vec![CypherValue::Integer(v)])
                .collect::<Vec<_>>()
        );
    }
    let invalid = Parameters::from([("skip".into(), json!(-1)), ("limit".into(), json!(1))]);
    assert!(matches!(
        db.execute_compiled_with(&page, &invalid),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn compiled_hybrid_union_distinct_and_hydrated_results_match_ordinary_reads() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Step {key:0})-[:R {weight:1}]->(b:Step {key:1})-[:R {weight:2}]->(c:Step {key:2}), (a)-[:R {weight:3}]->(b)").unwrap();
    for query in [
        "MATCH p=(:Step {key:$value})-[:R*0..2]->(n) RETURN p, nodes(p), relationships(p) ORDER BY n.key, length(p)",
        "MATCH p=(:Step {key:$value})-[:R*0..40]->(n) RETURN p, nodes(p), relationships(p) ORDER BY n.key, length(p)",
        "MATCH p=allShortestPaths((:Step {key:$value})-[:R*0..]->(:Step {key:2})) RETURN p, nodes(p)",
        "MATCH p=(:Step {key:$value})-[:R*0..40]->(n) RETURN DISTINCT n.key AS k UNION RETURN $value AS k",
        "MATCH p=(:Step {key:$value})-[:R*0..40]->(n) WITH n.key AS key, count(*) AS copies RETURN key, copies ORDER BY key",
        "MATCH p=(:Step {key:$value})-[:R*0..40]->(n) WITH p UNWIND nodes(p) AS m WITH DISTINCT m LIMIT 2 RETURN m.key ORDER BY m.key",
    ] {
        let plan = compile_read(query).unwrap();
        for value in [0, 1, 2, 9, 0] {
            let params = parameters(json!(value));
            assert_eq!(
                db.execute_compiled_with(&plan, &params).unwrap(),
                db.execute_with(query, &params).unwrap(),
                "{query}: {value}"
            );
        }
    }
}

#[test]
fn execution_controls_are_fresh_and_failures_leave_the_plan_reusable() {
    let mut db = Database::in_memory().unwrap();
    let plan = compile_read("UNWIND $value AS x RETURN x").unwrap();
    let params = parameters(json!([1, 2]));
    let exact = QueryOptions {
        max_result_rows: Some(2),
        max_result_bytes: Some(32),
        ..QueryOptions::default()
    };
    for _ in 0..3 {
        assert_eq!(
            db.execute_compiled_with_options(&plan, &params, &exact)
                .unwrap()
                .rows
                .len(),
            2
        );
    }
    for options in [
        QueryOptions {
            max_result_rows: Some(1),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_result_bytes: Some(31),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_materialized_rows: Some(0),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_materialized_bytes: Some(0),
            ..QueryOptions::default()
        },
        QueryOptions {
            max_sqlite_steps: Some(0),
            ..QueryOptions::default()
        },
    ] {
        assert!(
            matches!(
                db.execute_compiled_with_options(&plan, &params, &options),
                Err(Error::ResourceLimit(_))
            ),
            "{options:?}"
        );
        assert_eq!(
            db.execute_compiled_with(&plan, &params).unwrap().rows.len(),
            2
        );
    }
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        db.execute_compiled_with_options(
            &plan,
            &params,
            &QueryOptions {
                cancellation: Some(token),
                ..QueryOptions::default()
            }
        ),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        db.execute_compiled_with_options(
            &plan,
            &params,
            &QueryOptions {
                deadline: Some(Instant::now()),
                ..QueryOptions::default()
            }
        ),
        Err(Error::DeadlineExceeded)
    ));
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let traversal = compile_read("MATCH p=(:Start)-[:R*1..40]->() RETURN p").unwrap();
    let mut options = QueryOptions::default();
    options.traversal.max_states = 0;
    assert!(matches!(
        db.execute_compiled_with_options(&traversal, &Parameters::new(), &options),
        Err(Error::ResourceLimit(_))
    ));
    options.traversal.max_states = 1;
    for _ in 0..2 {
        assert_eq!(
            db.execute_compiled_with_options(&traversal, &Parameters::new(), &options)
                .unwrap()
                .rows
                .len(),
            1
        );
    }
    let union = compile_read("MATCH p=(:Start)-[:R*1..40]->() RETURN p UNION ALL MATCH p=(:Start)-[:R*1..40]->() RETURN p").unwrap();
    assert!(matches!(
        db.execute_compiled_with_options(&union, &Parameters::new(), &options),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
fn plans_see_fresh_data_across_connections_constraints_reopen_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compiled.sqlite");
    let backup = dir.path().join("before.sqlite");
    let plan = compile_read("MATCH (n:Saved {key:$value}) RETURN n").unwrap();
    let params = parameters(json!(1));
    let mut writer = Database::open(&path).unwrap();
    writer
        .execute("CREATE (:Saved {key:1, generation:1})")
        .unwrap();
    writer.backup_to(&backup).unwrap();
    let mut reader = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap();
    let before = reader.execute_compiled_with(&plan, &params).unwrap();
    writer.create_unique_constraint("Saved", "key").unwrap();
    writer
        .execute("MATCH (n:Saved) SET n.generation = 2")
        .unwrap();
    let after = reader.execute_compiled_with(&plan, &params).unwrap();
    assert_ne!(before, after);
    assert_eq!(after, writer.execute_compiled_with(&plan, &params).unwrap());
    writer.drop_unique_constraint("Saved", "key").unwrap();
    drop(reader);
    writer.restore_from(&backup).unwrap();
    assert_eq!(
        writer.execute_compiled_with(&plan, &params).unwrap(),
        before
    );
    drop(writer);
    let mut reopened = Database::open(&path).unwrap();
    assert_eq!(
        reopened.execute_compiled_with(&plan, &params).unwrap(),
        before
    );
    // Retaining the compiled handle and returned rows retains no read locks.
    let mut writer = Database::open_with_options(
        &path,
        &OpenOptions {
            busy_timeout: Duration::ZERO,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    writer.execute("CREATE (:AfterReads)").unwrap();
}

#[test]
fn owned_plans_can_be_shared_between_independent_connections() {
    let read = std::sync::Arc::new(compile_read("RETURN $value").unwrap());
    let threads = [1, 2].map(|value| {
        let read = read.clone();
        std::thread::spawn(move || {
            let mut db = Database::in_memory().unwrap();
            db.execute_compiled_with(&read, &parameters(json!(value)))
                .unwrap()
        })
    });
    for (index, thread) in threads.into_iter().enumerate() {
        assert_eq!(
            thread.join().unwrap().rows,
            [vec![CypherValue::Integer(index as i64 + 1)]]
        );
    }
}

#[test]
fn transaction_execution_sees_writes_and_every_failure_aborts_and_releases_locks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transactions.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE (:Saved {key:1})").unwrap();
    let saved = db.execute("MATCH (n) RETURN n").unwrap();
    let read = compile_read("MATCH (n) RETURN n").unwrap();
    let numeric = compile_read("RETURN abs($value)").unwrap();
    let mut tx = db.transaction().unwrap();
    tx.execute("CREATE (:Temporary)").unwrap();
    assert_eq!(tx.execute_compiled(&read).unwrap().rows.len(), 2);
    for v in [1, 2] {
        assert_eq!(
            tx.execute_compiled_with(&numeric, &parameters(json!(v)))
                .unwrap()
                .rows,
            [vec![CypherValue::Integer(v)]]
        );
    }
    tx.rollback().unwrap();
    for failure in ["missing", "type", "limit", "cancelled", "deadline"] {
        let mut tx = db.transaction().unwrap();
        tx.execute("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary)")
            .unwrap();
        tx.create_unique_constraint("Temporary", "key").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let result = match failure {
            "missing" => tx.execute_compiled(&numeric),
            "type" => tx.execute_compiled_with(&numeric, &parameters(json!("bad"))),
            "limit" => tx.execute_compiled_with_options(
                &read,
                &Parameters::new(),
                &QueryOptions {
                    max_result_rows: Some(1),
                    ..QueryOptions::default()
                },
            ),
            "cancelled" => tx.execute_compiled_with_options(
                &read,
                &Parameters::new(),
                &QueryOptions {
                    cancellation: Some(token),
                    ..QueryOptions::default()
                },
            ),
            _ => tx.execute_compiled_with_options(
                &read,
                &Parameters::new(),
                &QueryOptions {
                    deadline: Some(Instant::now()),
                    ..QueryOptions::default()
                },
            ),
        };
        assert!(
            matches!(
                (&result, failure),
                (Err(Error::MissingParameter(_)), "missing")
                    | (Err(Error::Type(_)), "type")
                    | (Err(Error::ResourceLimit(_)), "limit")
                    | (Err(Error::Cancelled), "cancelled")
                    | (Err(Error::DeadlineExceeded), "deadline")
            ),
            "{failure}: {result:?}"
        );
        assert!(matches!(
            tx.execute_compiled(&read),
            Err(Error::TransactionAborted)
        ));
        let mut other = Database::open_with_options(
            &path,
            &OpenOptions {
                busy_timeout: Duration::ZERO,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(other.execute_compiled(&read).unwrap(), saved);
        assert!(other.unique_constraints().unwrap().is_empty());
        other.execute("CREATE (n:LockProbe) DELETE n").unwrap();
        assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
    }
    assert_eq!(db.execute_compiled(&read).unwrap(), saved);
}
