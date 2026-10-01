use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crabnet::{Database, Error, Parameters};
use serde_json::json;

#[test]
fn transaction_reads_its_writes_and_commits_multiple_parameterized_queries() {
    let mut database = Database::in_memory().unwrap();
    let mut transaction = database.transaction().unwrap();
    let parameters = Parameters::from([("name".into(), json!("Ada"))]);

    let created = transaction
        .execute_with("CREATE (:Person {name: $name})", &parameters)
        .unwrap();
    assert_eq!(created.summary.nodes_created, 1);

    let people = transaction
        .execute_with("MATCH (p:Person {name: $name}) RETURN p", &parameters)
        .unwrap();
    assert_eq!(people.rows.len(), 1);
    assert_eq!(
        people.rows[0][0].as_node().unwrap().properties["name"],
        json!("Ada")
    );

    // Application code decides what to write using the result of an earlier
    // query, without releasing the transaction between reading and writing.
    let parameters = Parameters::from([("count".into(), json!(people.rows.len()))]);
    let updated = transaction
        .execute_with(
            "MATCH (p:Person) SET p.observed = $count RETURN p.observed",
            &parameters,
        )
        .unwrap();
    assert_eq!(updated.summary.properties_set, 1);
    assert_eq!(updated.summary.nodes_created, 0);
    assert_eq!(updated.rows, [vec![json!(1)]]);
    transaction.commit().unwrap();

    let result = database
        .execute("MATCH (p:Person) RETURN p.name, p.observed")
        .unwrap();
    assert_eq!(result.rows, [vec![json!("Ada"), json!(1)]]);
}

#[test]
fn explicit_rollback_and_drop_restore_entities_labels_and_property_indexes() {
    for explicit_rollback in [true, false] {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:Saved {id: 1})-[:LINK {weight: 2}]->(:Keep {id: 2})")
            .unwrap();

        let mut transaction = database.transaction().unwrap();
        transaction
            .execute("MATCH (n:Saved {id: 1}) SET n.id = 10, n:Changed REMOVE n:Saved")
            .unwrap();
        transaction
            .execute("MATCH (n:Keep) DETACH DELETE n")
            .unwrap();
        transaction.execute("CREATE (:Temporary)").unwrap();
        assert_eq!(
            transaction
                .execute("MATCH (n:Changed {id: 10}) RETURN n.id")
                .unwrap()
                .rows,
            [vec![json!(10)]]
        );

        if explicit_rollback {
            transaction.rollback().unwrap();
        } else {
            drop(transaction);
        }

        let restored = database
            .execute(
                "MATCH (n:Saved {id: 1})-[r:LINK]->(m:Keep {id: 2}) \
                 RETURN n.id, r.weight, m.id",
            )
            .unwrap();
        assert_eq!(restored.rows, [vec![json!(1), json!(2), json!(2)]]);
        assert_eq!(
            database.execute("MATCH (n) RETURN count(n)").unwrap().rows,
            [vec![json!(2)]]
        );
        assert!(
            database
                .execute("MATCH (n:Changed {id: 10}) RETURN n")
                .unwrap()
                .rows
                .is_empty()
        );

        // Reusing the temporary row table and prepared statements after
        // rollback must not resurrect rows or retain a rolled-back layout.
        let mut transaction = database.transaction().unwrap();
        transaction
            .execute("MATCH (n:Saved {id: 1}) SET n.id = 3 RETURN n.id")
            .unwrap();
        transaction.commit().unwrap();
        assert_eq!(
            database
                .execute("MATCH (n:Saved {id: 3}) RETURN n.id")
                .unwrap()
                .rows,
            [vec![json!(3)]]
        );
    }
}

#[test]
fn query_errors_abort_prior_and_partial_writes_and_prevent_commit() {
    let cases = [
        ("", Error::EmptyQuery),
        ("RETURN (", Error::Parse(String::new())),
        ("RETURN absent", Error::Semantic(String::new())),
        (
            "WITH DISTINCT 1 AS x RETURN x",
            Error::Unsupported(String::new()),
        ),
        ("RETURN $missing", Error::MissingParameter(String::new())),
        (
            "CREATE (:Temporary) RETURN $missing",
            Error::MissingParameter(String::new()),
        ),
        (
            "MATCH (n:Saved) RETURN n.name - 1",
            Error::Type(String::new()),
        ),
        (
            "CREATE (:Temporary) RETURN range(1, 2, 0)",
            Error::Argument(String::new()),
        ),
        (
            "MATCH (n:Saved) SET n.name = 'changed' DELETE n",
            Error::Constraint(String::new()),
        ),
    ];
    for (query, expected) in cases {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:Saved {name: 'original'})-[:LINK]->(:Keep)")
            .unwrap();
        let mut transaction = database.transaction().unwrap();
        transaction.execute("CREATE (:Earlier)").unwrap();

        let error = transaction.execute(query).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&expected),
            "{query}: {error:?}"
        );
        assert!(matches!(
            transaction.execute("CREATE (:AfterFailure)"),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(
            transaction.execute_with("RETURN $missing", &Parameters::new()),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(
            transaction.commit(),
            Err(Error::TransactionAborted)
        ));

        assert_eq!(
            database.execute("MATCH (n) RETURN count(n)").unwrap().rows,
            [vec![json!(2)]],
            "{query}"
        );
        assert_eq!(
            database
                .execute("MATCH (n:Saved)-[:LINK]->(:Keep) RETURN n.name")
                .unwrap()
                .rows,
            [vec![json!("original")]],
            "{query}"
        );
        let mut transaction = database.transaction().unwrap();
        transaction
            .execute("MATCH (n:Saved) SET n.name = 'usable' RETURN n.name")
            .unwrap();
        transaction.commit().unwrap();
    }
}

#[test]
fn rollback_after_an_error_is_a_successful_no_op() {
    let mut database = Database::in_memory().unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction.execute("CREATE (:Earlier)").unwrap();
    assert!(matches!(transaction.execute(""), Err(Error::EmptyQuery)));
    transaction.rollback().unwrap();
    assert!(
        database
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn unwinding_application_code_rolls_back_the_transaction() {
    let mut database = Database::in_memory().unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut transaction = database.transaction().unwrap();
        transaction.execute("CREATE (:Temporary)").unwrap();
        panic!("application failed after its query succeeded");
    }));
    assert!(panic.is_err());
    assert!(
        database
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    database.transaction().unwrap().commit().unwrap();
}

#[test]
fn transactional_pipelines_preserve_bags_nulls_and_per_query_summaries() {
    let mut database = Database::in_memory().unwrap();
    let mut transaction = database.transaction().unwrap();
    let parameters = Parameters::from([("items".into(), json!([1, 1, null, 2]))]);
    let created = transaction
        .execute_with(
            "UNWIND $items AS value CREATE (n:Item {value: value}) \
             WITH n MATCH (m:Item) WHERE m = n \
             SET m.checked = true RETURN m.value ORDER BY m.value",
            &parameters,
        )
        .unwrap();
    assert_eq!(
        created.rows,
        [
            vec![json!(1)],
            vec![json!(1)],
            vec![json!(2)],
            vec![json!(null)]
        ]
    );
    assert_eq!(created.summary.nodes_created, 4);
    assert_eq!(created.summary.properties_set, 4);

    let merged = transaction
        .execute("MERGE (:Item {value: 1}) RETURN 1 AS value")
        .unwrap();
    assert_eq!(merged.rows, [vec![json!(1)], vec![json!(1)]]);
    assert_eq!(merged.summary.nodes_created, 0);
    assert_eq!(merged.summary.properties_set, 0);
    let read = transaction
        .execute("MATCH (n:Item) RETURN n.value ORDER BY n.value")
        .unwrap();
    assert_eq!(read.rows, created.rows);
    assert_eq!(read.summary, crabnet::QuerySummary::default());
    transaction.commit().unwrap();
    assert_eq!(
        database
            .execute("MATCH (n:Item) RETURN count(n)")
            .unwrap()
            .rows,
        [vec![json!(4)]]
    );
}

#[test]
fn hybrid_paths_unions_and_hydration_see_uncommitted_writes() {
    let mut database = Database::in_memory().unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction
        .execute("CREATE (:Step {id: 0})-[:NEXT]->(:Step {id: 1})-[:NEXT]->(:Step {id: 2})")
        .unwrap();
    transaction
        .execute("MATCH (n:Step {id: 2}) SET n.name = 'last'")
        .unwrap();
    let paths = transaction
        .execute(
            "MATCH p = (:Step {id: 0})-[:NEXT*1..]->(:Step {id: 2}) RETURN p \
             UNION ALL \
             MATCH p = shortestPath((:Step {id: 0})-[:NEXT*]->(:Step {id: 2})) RETURN p",
        )
        .unwrap();
    assert_eq!(paths.rows.len(), 2);
    assert_eq!(paths.rows[0], paths.rows[1]);
    let path = paths.rows[0][0].as_path().unwrap();
    assert_eq!(path.relationships.len(), 2);
    assert_eq!(path.nodes[2].properties["name"], json!("last"));
    transaction.rollback().unwrap();
    assert!(
        database
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn traversal_budgets_reset_per_query_and_exhaustion_aborts_prior_writes() {
    let mut database = Database::in_memory().unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction
        .execute("UNWIND range(1, 8) AS i CREATE (:Budget {limited: i < 8})")
        .unwrap();
    let loops = ["(n)-[:STEP]->(n)"; 7].join(", ");
    transaction
        .execute(&format!("MATCH (n:Budget) CREATE {loops}"))
        .unwrap();

    // Each seven-loop component explores 13,699 trails but cannot reach depth
    // eight. Seven components fit in one query's budget; two queries together
    // would not. With no emitted trails, payload size is irrelevant.
    for _ in 0..2 {
        assert_eq!(
            transaction
                .execute("MATCH (:Budget {limited: true})-[:STEP*8..]->() RETURN count(*)")
                .unwrap()
                .rows,
            [vec![json!(0)]]
        );
    }
    assert!(matches!(
        transaction.execute("MATCH (:Budget)-[:STEP*8..]->() RETURN count(*)"),
        Err(Error::ResourceLimit(_))
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
}

struct DatabaseFile(PathBuf);

impl DatabaseFile {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "crabnet-transactions-{}-{timestamp}-{sequence}.sqlite",
            std::process::id()
        )))
    }
}

impl Drop for DatabaseFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn commit_is_visible_after_reopen_and_errors_release_the_write_lock_immediately() {
    let file = DatabaseFile::new();
    let mut writer = Database::open(&file.0).unwrap();
    let mut reader = Database::open(&file.0).unwrap();
    let mut transaction = writer.transaction().unwrap();
    transaction.execute("CREATE (:Saved {id: 1})").unwrap();
    assert!(
        reader
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    transaction.commit().unwrap();
    assert_eq!(
        reader.execute("MATCH (n:Saved) RETURN n.id").unwrap().rows,
        [vec![json!(1)]]
    );

    let mut transaction = writer.transaction().unwrap();
    transaction.execute("CREATE (:Temporary)").unwrap();
    assert!(matches!(
        transaction.execute("RETURN $missing"),
        Err(Error::MissingParameter(_))
    ));
    // Keep the aborted handle alive: rollback must already have happened,
    // so another connection can write without waiting for this handle's drop.
    reader.execute("CREATE (:Saved {id: 2})").unwrap();
    transaction.rollback().unwrap();
    drop(reader);
    drop(writer);

    let mut reopened = Database::open(&file.0).unwrap();
    assert_eq!(
        reopened
            .execute("MATCH (n) RETURN n.id ORDER BY n.id")
            .unwrap()
            .rows,
        [vec![json!(1)], vec![json!(2)]]
    );
}
