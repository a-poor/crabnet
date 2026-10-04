use std::time::{Duration, Instant};

use crabnet::{CancellationToken, Database, Error, Parameters, QueryOptions, Value};

#[test]
fn traversal_options_apply_per_query_and_across_union_branches() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Start)-[:STEP]->(:End)").unwrap();
    let query = "MATCH (:Start)-[:STEP*1..]->(n) RETURN n";
    let mut options = QueryOptions::default();
    options.traversal.max_states = 0;
    assert!(matches!(
        database.execute_with_options(query, &Parameters::new(), &options),
        Err(Error::ResourceLimit(_))
    ));
    options.traversal.max_states = 1;
    for _ in 0..2 {
        assert_eq!(
            database
                .execute_with_options(query, &Parameters::new(), &options)
                .unwrap()
                .rows
                .len(),
            1
        );
    }
    assert!(matches!(
        database.execute_with_options(
            &format!("{query} UNION ALL {query}"),
            &Parameters::new(),
            &options
        ),
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(database.execute(query).unwrap().rows.len(), 1);
}

#[test]
fn traversal_payload_and_memory_limits_are_configurable() {
    let mut database = Database::in_memory().unwrap();
    let empty = "MATCH ()-[:STEP*1..]->(n) RETURN n";
    let mut options = QueryOptions::default();
    options.traversal.max_payload_bytes = 2; // The empty JSON relation is [].
    assert!(
        database
            .execute_with_options(empty, &Parameters::new(), &options)
            .unwrap()
            .rows
            .is_empty()
    );
    options.traversal.max_payload_bytes = 1;
    assert!(matches!(
        database.execute_with_options(empty, &Parameters::new(), &options),
        Err(Error::ResourceLimit(_))
    ));
    options.traversal.max_payload_bytes = 2;
    assert!(matches!(
        database.execute_with_options(
            &format!("{empty} UNION ALL {empty}"),
            &Parameters::new(),
            &options
        ),
        Err(Error::ResourceLimit(_))
    ));

    database.execute("CREATE (:Start)-[:STEP]->(:End)").unwrap();
    options = QueryOptions::default();
    options.traversal.max_memory_bytes = 0;
    for query in [
        "MATCH ()-[:STEP*1..]->(n) RETURN n",
        "MATCH p = shortestPath((:Start)-[:STEP*]->(:End)) RETURN p",
    ] {
        assert!(matches!(
            database.execute_with_options(query, &Parameters::new(), &options),
            Err(Error::ResourceLimit(_))
        ));
        assert_eq!(database.execute(query).unwrap().rows.len(), 1);
    }
    // The traversal guard only counts Rust producers, not bounded SQLite CTEs.
    options.traversal.max_states = 0;
    assert_eq!(
        database
            .execute_with_options(
                "MATCH ()-[:STEP*1..2]->(n) RETURN n",
                &Parameters::new(),
                &options
            )
            .unwrap()
            .rows
            .len(),
        1
    );
}

#[test]
fn cancellation_and_deadline_failures_do_not_leak_to_later_queries() {
    let mut database = Database::in_memory().unwrap();
    let cancellation = CancellationToken::new();
    let clone = cancellation.clone();
    clone.cancel();
    assert!(cancellation.is_cancelled());
    let cancelled = QueryOptions {
        cancellation: Some(cancellation),
        ..QueryOptions::default()
    };
    let expired = QueryOptions {
        deadline: Some(Instant::now()),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options("CREATE (:Bad)", &Parameters::new(), &cancelled),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        database.execute_with_options("CREATE (:Bad)", &Parameters::new(), &expired),
        Err(Error::DeadlineExceeded)
    ));
    let mut transaction = database.transaction().unwrap();
    transaction.execute("CREATE (:Earlier)").unwrap();
    assert!(matches!(
        transaction.execute_with_options("RETURN 1", &Parameters::new(), &cancelled),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        transaction.commit(),
        Err(Error::TransactionAborted)
    ));
    assert!(
        database
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    database.execute("CREATE (:Usable)").unwrap();
}

#[test]
fn sqlite_work_can_be_interrupted_even_when_no_rows_are_returned() {
    let mut database = Database::in_memory().unwrap();
    let query = "UNWIND range(1, 10000) AS x UNWIND range(1, 10000) AS y RETURN count(*)";
    let limited = QueryOptions {
        max_sqlite_steps: Some(100),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options(query, &Parameters::new(), &limited),
        Err(Error::ResourceLimit(_))
    ));
    let timed = QueryOptions {
        deadline: Some(Instant::now() + Duration::from_millis(30)),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options(query, &Parameters::new(), &timed),
        Err(Error::DeadlineExceeded)
    ));
    assert_eq!(
        database.execute("RETURN 42").unwrap().rows[0][0],
        Value::from(42)
    );
}

#[test]
fn result_limits_follow_distinct_pagination_and_union_semantics() {
    let mut database = Database::in_memory().unwrap();
    let options = QueryOptions {
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    for query in [
        "UNWIND [1, 1, 2] AS x RETURN DISTINCT x LIMIT 1",
        "RETURN 1 AS x UNION RETURN 1 AS x",
    ] {
        assert_eq!(
            database
                .execute_with_options(query, &Parameters::new(), &options)
                .unwrap()
                .rows
                .len(),
            1
        );
    }
    for query in [
        "UNWIND [1, 1] AS x RETURN x",
        "RETURN 1 AS x UNION ALL RETURN 1 AS x",
    ] {
        assert!(matches!(
            database.execute_with_options(query, &Parameters::new(), &options),
            Err(Error::ResourceLimit(_))
        ));
    }
    let zero = QueryOptions {
        max_result_rows: Some(0),
        max_result_bytes: Some(0),
        ..QueryOptions::default()
    };
    assert!(
        database
            .execute_with_options("UNWIND [] AS x RETURN x", &Parameters::new(), &zero)
            .unwrap()
            .rows
            .is_empty()
    );
    database
        .execute_with_options("CREATE (:NoReturn)", &Parameters::new(), &zero)
        .unwrap();
}

#[test]
fn result_bytes_count_utf8_and_nested_values_at_exact_boundaries() {
    let mut database = Database::in_memory().unwrap();
    let parameters = Parameters::from([("value".into(), serde_json::json!(["é", {"k": null}]))]);
    // Row 8 + list 8 + string (8+2) + map (8+key 1+null 8).
    let mut options = QueryOptions {
        max_result_bytes: Some(43),
        ..QueryOptions::default()
    };
    database
        .execute_with_options("RETURN $value", &parameters, &options)
        .unwrap();
    options.max_result_bytes = Some(42);
    assert!(matches!(
        database.execute_with_options("RETURN $value", &parameters, &options),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
fn materialization_budgets_cover_branches_and_precede_final_row_shaping() {
    let mut database = Database::in_memory().unwrap();
    let mut options = QueryOptions {
        max_materialized_rows: Some(2),
        ..QueryOptions::default()
    };
    database
        .execute_with_options(
            "UNWIND [1, 1] AS x RETURN DISTINCT x",
            &Parameters::new(),
            &options,
        )
        .unwrap();
    options.max_materialized_rows = Some(1);
    for query in [
        "UNWIND [1, 1] AS x RETURN DISTINCT x",
        "RETURN 1 AS x UNION RETURN 1 AS x",
    ] {
        assert!(matches!(
            database.execute_with_options(query, &Parameters::new(), &options),
            Err(Error::ResourceLimit(_))
        ));
    }
    // The encoded JSON string includes quotes, so 'é' takes four UTF-8 bytes.
    options = QueryOptions {
        max_materialized_bytes: Some(4),
        ..QueryOptions::default()
    };
    database
        .execute_with_options("RETURN 'é'", &Parameters::new(), &options)
        .unwrap();
    options.max_materialized_bytes = Some(3);
    assert!(matches!(
        database.execute_with_options("RETURN 'é'", &Parameters::new(), &options),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
fn hydration_counts_materialized_entities_and_expanded_result_bytes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute_with(
            "CREATE (:Start {data: $data})-[:STEP]->(:End)",
            &Parameters::from([("data".into(), Value::from("x".repeat(2048)))]),
        )
        .unwrap();
    let query = "MATCH p = (:Start)-[:STEP]->(:End) RETURN p";
    let materialized = QueryOptions {
        max_materialized_rows: Some(1),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options(query, &Parameters::new(), &materialized),
        Err(Error::ResourceLimit(_))
    ));
    let output = QueryOptions {
        max_result_bytes: Some(1024),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options(query, &Parameters::new(), &output),
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(database.execute(query).unwrap().rows.len(), 1);
}

#[test]
fn late_resource_failure_rolls_back_writes_indexes_and_explicit_transactions() {
    let mut database = Database::in_memory().unwrap();
    database.create_unique_constraint("Item", "key").unwrap();
    database.execute("CREATE (:Item {key: 'saved'})").unwrap();
    let options = QueryOptions {
        max_result_rows: Some(0),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options(
            "CREATE (n:Item {key: 'new'}) RETURN n",
            &Parameters::new(),
            &options
        ),
        Err(Error::ResourceLimit(_))
    ));
    let mut transaction = database.transaction().unwrap();
    transaction
        .execute("MATCH (n:Item) SET n.key = 'changed'")
        .unwrap();
    transaction
        .execute("CREATE (:Item {key: 'earlier'})")
        .unwrap();
    assert!(matches!(
        transaction.execute_with_options(
            "CREATE (n:Item {key: 'new'}) RETURN n",
            &Parameters::new(),
            &options
        ),
        Err(Error::ResourceLimit(_))
    ));
    assert!(matches!(
        transaction.execute("RETURN 1"),
        Err(Error::TransactionAborted)
    ));
    assert!(matches!(
        transaction.commit(),
        Err(Error::TransactionAborted)
    ));
    assert_eq!(
        database
            .execute("MATCH (n:Item {key: 'saved'}) RETURN n.key")
            .unwrap()
            .rows,
        [vec![Value::from("saved")]]
    );
    // Restored uniqueness and property indexes must permit these keys again.
    database
        .execute("CREATE (:Item {key: 'new'}), (:Item {key: 'earlier'}), (:Item {key: 'changed'})")
        .unwrap();
}

#[test]
fn mutation_internal_reads_share_budgets_but_not_final_result_limits() {
    let mut database = Database::in_memory().unwrap();
    let query = "UNWIND [1, 2] AS x CREATE (n:Item {key: x}) RETURN count(*)";
    let output = QueryOptions {
        max_result_rows: Some(1),
        ..QueryOptions::default()
    };
    assert_eq!(
        database
            .execute_with_options(query, &Parameters::new(), &output)
            .unwrap()
            .rows[0][0],
        Value::from(2)
    );
    let limited = QueryOptions {
        max_materialized_rows: Some(2),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options(query, &Parameters::new(), &limited),
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(
        database
            .execute("MATCH (n:Item) RETURN count(*)")
            .unwrap()
            .rows[0][0],
        Value::from(2)
    );

    let mut transaction = database.transaction().unwrap();
    let once = QueryOptions {
        max_materialized_rows: Some(1),
        ..QueryOptions::default()
    };
    for _ in 0..2 {
        transaction
            .execute_with_options("RETURN 1", &Parameters::new(), &once)
            .unwrap();
    }
    transaction.commit().unwrap();
}

#[test]
fn traversal_inputs_count_even_when_the_query_emits_no_rows() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Start)-[:STEP]->(:End)").unwrap();
    let options = QueryOptions {
        max_materialized_rows: Some(0),
        ..QueryOptions::default()
    };
    assert!(matches!(
        database.execute_with_options(
            "MATCH (:Start)-[:STEP*9..]->(n) RETURN n",
            &Parameters::new(),
            &options
        ),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
fn entity_and_path_result_costs_agree_across_transports() {
    let mut database = Database::in_memory().unwrap();
    let node = database.execute("CREATE (n:N {v: 1}) RETURN n").unwrap();
    let id_bytes = node.rows[0][0].as_node().unwrap().element_id.len();
    // Row 8, node 8, ID, label list (8+8+1), property map (8+1+8).
    let node_result_bytes = id_bytes + 50;
    for (query, cost) in [
        ("MATCH (n) RETURN n", node_result_bytes),
        ("MATCH (n) RETURN collect(n)", node_result_bytes + 8),
        ("MATCH p = (n) RETURN p", node_result_bytes + 24),
        ("MATCH p = (n) RETURN nodes(p)", node_result_bytes + 8),
        ("MATCH p = (n) RETURN relationships(p)", 16),
    ] {
        let exact = QueryOptions {
            max_result_bytes: Some(cost),
            ..QueryOptions::default()
        };
        database
            .execute_with_options(query, &Parameters::new(), &exact)
            .unwrap();
        let short = QueryOptions {
            max_result_bytes: Some(cost - 1),
            ..QueryOptions::default()
        };
        assert!(
            matches!(
                database.execute_with_options(query, &Parameters::new(), &short),
                Err(Error::ResourceLimit(_))
            ),
            "{query}"
        );
    }
}
