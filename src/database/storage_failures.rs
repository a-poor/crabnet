//! Allocation-failure tests using the real SQLite pager, through the public API.
//! Private connection access is limited to fault setup, observation, and checks.
//! Child modules also inject one-shot write/sync failures at commit and migration.
//! Neither model simulates filesystem ENOSPC or power loss.

mod commit_io;
mod io_vfs;
mod migration_io;

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rusqlite::{Connection, ErrorCode, hooks::Action, types::Value as SqlValue};
use serde_json::json;

use crate::{CypherValue, Database, Error, OpenMode, OpenOptions, Parameters, UniqueConstraint};

const MATCH: &str = "
    MATCH (a:Saved {key:'a'})-[r:LINK]->(b:Saved {key:'b'})";
const MUTATE: &str = "
    SET a.key='changed', a.number=11, a:Pending, r.weight=9
    REMOVE a:Original
    CREATE (p:Transient {id:1}), (p)-[:TEMP]->(b)";

fn open_existing(path: &Path) -> Database {
    Database::open_with_options(
        path,
        &OpenOptions {
            mode: OpenMode::ReadWrite,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap()
}

fn seed(path: &Path) -> Database {
    let mut database = Database::open(path).unwrap();
    database.create_unique_constraint("Saved", "key").unwrap();
    database
        .create_unique_constraint("Saved", "number")
        .unwrap();
    database
        .execute(
            "CREATE (a:Saved:Original {key:'a', number:1,
                nested:[true, null, {precise:1.25}], big:9007199254740993}),
             (b:Saved {key:'b', number:2.0}), (:Saved {key:'gone', number:3}),
             (a)-[:LINK {weight:1}]->(b), (a)-[:LINK {weight:1}]->(b)",
        )
        .unwrap();
    let journal: String = database
        .connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    database
}

fn pragma(connection: &Connection, name: &str) -> i64 {
    connection
        .pragma_query_value(None, name, |row| row.get(0))
        .unwrap()
}

struct GrowthLimit {
    original: i64,
    parameters: Parameters,
    payload_bytes: usize,
}

impl GrowthLimit {
    fn install(database: &Database) -> Self {
        let connection = &database.connection;
        let original = pragma(connection, "max_page_count");
        // Leave room for earlier mutations and constraint changes. The payload
        // alone exceeds the TOTAL capped file size, including any free pages.
        let limit = pragma(connection, "page_count") + 32;
        let payload_bytes =
            usize::try_from((limit + 16) * pragma(connection, "page_size")).unwrap();
        assert!(limit < original);
        connection
            .pragma_update(None, "max_page_count", limit)
            .unwrap();
        assert_eq!(pragma(connection, "max_page_count"), limit);
        Self {
            original,
            parameters: Parameters::from([("payload".into(), json!("x".repeat(payload_bytes)))]),
            payload_bytes,
        }
    }

    fn lift(&self, database: &Database) {
        database
            .connection
            .pragma_update(None, "max_page_count", self.original)
            .unwrap();
        assert_eq!(
            pragma(&database.connection, "max_page_count"),
            self.original
        );
    }
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    nodes: Vec<Vec<CypherValue>>,
    relationships: Vec<Vec<CypherValue>>,
    constraints: Vec<UniqueConstraint>,
    indexes: Vec<Vec<Vec<SqlValue>>>,
}

fn snapshot(database: &mut Database) -> Snapshot {
    let nodes = database
        .execute("MATCH (n) RETURN n ORDER BY elementId(n)")
        .unwrap()
        .rows;
    let relationships = database
        .execute("MATCH ()-[r]->() RETURN r ORDER BY elementId(r)")
        .unwrap()
        .rows;
    let constraints = database.unique_constraints().unwrap();
    // Include the exact stored types and canonical unique-key BLOBs, rather
    // than relying on graph scans that could hide stale derived entries.
    let indexes = [
        "SELECT * FROM node_labels ORDER BY label, node_id",
        "SELECT * FROM node_properties ORDER BY node_id, key",
        "SELECT * FROM node_unique_values ORDER BY label, property, node_id",
    ]
    .into_iter()
    .map(|sql| {
        let mut statement = database.connection.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| (0..columns).map(|index| row.get(index)).collect())
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect();
    Snapshot {
        nodes,
        relationships,
        constraints,
        indexes,
    }
}

fn assert_integrity(database: &Database) {
    assert_connection_integrity(&database.connection);
}

fn assert_connection_integrity(connection: &Connection) {
    let mut statement = connection.prepare("PRAGMA integrity_check").unwrap();
    let messages = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(messages, ["ok"]);
    assert!(
        !connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap()
    );
}

fn assert_full(error: Error) {
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected the original SQLITE_FULL error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::DiskFull);
    assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_FULL);
}

fn observe_writes(database: &Database) -> Arc<AtomicUsize> {
    let observed = Arc::new(AtomicUsize::new(0));
    let callback = Arc::clone(&observed);
    database
        .connection
        .update_hook(Some(move |action, schema: &str, table: &str, _| {
            if schema != "main" {
                return;
            }
            let bit = match (table, action) {
                ("nodes", Action::SQLITE_INSERT) => 1,
                ("nodes", Action::SQLITE_UPDATE) => 2,
                ("nodes", Action::SQLITE_DELETE) => 4,
                ("edges", Action::SQLITE_INSERT) => 8,
                ("edges", Action::SQLITE_UPDATE) => 16,
                _ => 0,
            };
            callback.fetch_or(bit, Ordering::Relaxed);
        }))
        .unwrap();
    observed
}

fn assert_competing_writer_can_commit(observer: &mut Database, before: &Snapshot) {
    // The zero busy timeout makes leaked locks fail immediately. The caller
    // keeps the original Database (and, where applicable, aborted handle) alive.
    assert_eq!(snapshot(observer), *before);
    observer.execute("CREATE (:LockProbe {id:1})").unwrap();
    observer.execute("MATCH (n:LockProbe) DELETE n").unwrap();
    assert_eq!(snapshot(observer), *before);
}

fn assert_saved_constraint(database: &mut Database) {
    assert!(matches!(
        database.execute("CREATE (:Saved {key:'a'})"),
        Err(Error::Constraint(_))
    ));
}

#[test]
fn standalone_disk_full_rolls_back_partial_node_and_relationship_writes() {
    for (mutation, verification, count) in [
        (
            "CREATE (:Payload {payload:$payload})",
            "MATCH (n:Payload) RETURN size(n.payload)",
            2,
        ),
        (
            "CREATE (a)-[:PAYLOAD {payload:$payload}]->(b)",
            "MATCH ()-[r:PAYLOAD]->() RETURN size(r.payload)",
            2,
        ),
        (
            "SET a.payload=$payload",
            "MATCH (n:Saved {key:'changed'}) RETURN size(n.payload)",
            1,
        ),
        (
            "SET r.payload=$payload",
            "MATCH ()-[r:LINK]->() RETURN size(r.payload)",
            2,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.sqlite");
        let mut database = seed(&path);
        let before = snapshot(&mut database);
        let fault = GrowthLimit::install(&database);
        let observed = observe_writes(&database);
        let query = format!("{MATCH} {MUTATE} {mutation}");
        assert_full(
            database
                .execute_with(&query, &fault.parameters)
                .expect_err("the growth limit must reject this otherwise valid query"),
        );
        // Earlier CREATE, SET, and relationship writes really ran;
        // a preflight/planning failure would not be an atomicity test.
        assert_eq!(observed.load(Ordering::Relaxed), 27, "{mutation}");
        assert!(database.connection.is_autocommit());
        assert_eq!(snapshot(&mut database), before, "{mutation}");
        assert_integrity(&database);
        assert_saved_constraint(&mut database);
        assert_competing_writer_can_commit(&mut open_existing(&path), &before);
        // Check persistence before retrying so a successful retry cannot hide
        // damage from the failed attempt. The original connection stays alive.
        assert_eq!(snapshot(&mut open_existing(&path)), before);

        fault.lift(&database);
        database.execute_with(&query, &fault.parameters).unwrap();
        assert_eq!(
            database.execute(verification).unwrap().rows,
            vec![vec![json!(fault.payload_bytes)]; count],
            "{mutation}"
        );
        let committed = snapshot(&mut database);
        assert_integrity(&database);
        drop(database);
        let mut reopened = open_existing(&path);
        assert_eq!(snapshot(&mut reopened), committed);
        assert_integrity(&reopened);
    }
}

#[test]
fn transaction_disk_full_aborts_prior_queries_and_releases_locks_before_drop() {
    for finish in ["commit", "rollback", "drop"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.sqlite");
        let mut database = seed(&path);
        let mut observer = open_existing(&path);
        let before = snapshot(&mut database);
        let fault = GrowthLimit::install(&database);
        let observed = observe_writes(&database);
        let mut transaction = database.transaction().unwrap();
        assert!(transaction.drop_unique_constraint("Saved", "key").unwrap());
        assert!(
            transaction
                .create_unique_constraint("Transient", "id")
                .unwrap()
        );
        // A single bound row avoids creating duplicate Transient IDs; the
        // standalone matrix separately covers the parallel-relationship bag.
        transaction
            .execute(&format!("{MATCH} WITH a, r, b LIMIT 1 {MUTATE}"))
            .unwrap();
        transaction
            .execute("MATCH (n:Saved {key:'gone'}) DELETE n")
            .unwrap();
        assert_eq!(
            transaction
                .execute("MATCH (n:Pending {number:11}) RETURN n.key")
                .unwrap()
                .rows,
            [vec![json!("changed")]]
        );
        assert_full(
            transaction
                .execute_with("CREATE (:Payload {payload:$payload})", &fault.parameters)
                .unwrap_err(),
        );
        assert_eq!(observed.load(Ordering::Relaxed), 31);
        assert!(matches!(
            transaction.execute("RETURN 1"),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(
            transaction.unique_constraints(),
            Err(Error::TransactionAborted)
        ));
        assert_competing_writer_can_commit(&mut observer, &before);
        match finish {
            "commit" => assert!(matches!(
                transaction.commit(),
                Err(Error::TransactionAborted)
            )),
            "rollback" => transaction.rollback().unwrap(),
            "drop" => drop(transaction),
            _ => unreachable!(),
        }
        assert!(database.connection.is_autocommit());
        assert_eq!(snapshot(&mut database), before);
        assert_integrity(&database);
        assert_saved_constraint(&mut database);
        assert_eq!(snapshot(&mut open_existing(&path)), before);

        fault.lift(&database);
        let mut transaction = database.transaction().unwrap();
        transaction
            .execute_with("CREATE (:Payload {payload:$payload})", &fault.parameters)
            .unwrap();
        transaction.commit().unwrap();
        assert_eq!(
            database
                .execute("MATCH (n:Payload) RETURN size(n.payload)")
                .unwrap()
                .rows,
            [vec![json!(fault.payload_bytes)]]
        );
        let committed = snapshot(&mut database);
        drop(database);
        assert_eq!(snapshot(&mut open_existing(&path)), committed);
        assert_integrity(&observer);
    }
}
