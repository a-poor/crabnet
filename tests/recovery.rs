use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use crabnet::{Database, Error, OpenMode, OpenOptions, Parameters};
use rusqlite::Connection;
use serde_json::json;

const STAGE: &str = "CRABNET_RECOVERY_STAGE";
const DATABASE_PATH: &str = "CRABNET_RECOVERY_PATH";
const TEST_NAME: &str = "process_death_recovers_spilled_writes_and_keeps_completed_commits";

// Terminate/reap even if a parent assertion or readiness timeout fails.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child_writes(path: &Path, stage: &str) -> ! {
    let mut database = Database::open_with_options(
        path,
        &OpenOptions {
            mode: OpenMode::ReadWrite,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction.drop_unique_constraint("Saved", "key").unwrap();
    transaction
        .create_unique_constraint("Pending", "id")
        .unwrap();
    transaction
        .execute("MATCH (n:Saved) SET n.key = 'changed', n:Changed REMOVE n:Original")
        .unwrap();
    // Exceed SQLite's default page cache so uncommitted pages really reach
    // the database file. The parent checks both a hot journal header and a
    // changed main file before killing us; this is not merely rollback-on-drop.
    let parameters = Parameters::from([("payload".into(), json!("x".repeat(64 * 1024)))]);
    transaction
        .execute_with(
            "UNWIND range(1, 96) AS i CREATE (:Pending {id: i, payload: $payload})",
            &parameters,
        )
        .unwrap();
    let _pending_transaction = if stage == "committed" {
        transaction.commit().unwrap();
        None
    } else {
        assert_eq!(stage, "uncommitted");
        // Keep the live handle in scope while blocked on stdin.
        Some(transaction)
    };
    println!("CRABNET_READY");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    panic!("parent should terminate this process without running destructors");
}

fn ready_child(path: &Path, stage: &str) -> ChildGuard {
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(STAGE, stage)
            .env(DATABASE_PATH, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.trim() == "CRABNET_READY" => {
                    let _ = send.send(true);
                    return;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = send.send(false);
    });
    assert_eq!(
        receive.recv_timeout(Duration::from_secs(45)),
        Ok(true),
        "child failed to reach {stage}"
    );
    child
}

fn check_storage(path: &Path) {
    let connection = Connection::open(path).unwrap();
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    assert!(
        !connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap()
    );
    for (index, expected) in [
        (
            "SELECT label, node_id FROM node_labels",
            "SELECT value, nodes.id FROM nodes, json_each(nodes.labels)",
        ),
        (
            "SELECT node_id, key, value FROM node_properties",
            "SELECT nodes.id, p.key, p.value FROM nodes, json_each(nodes.props) AS p WHERE p.type <> 'null'",
        ),
    ] {
        // EXCEPT in both directions checks the full materialized indexes.
        for (left, right) in [(index, expected), (expected, index)] {
            assert!(
                !connection
                    .prepare(&format!("{left} EXCEPT {right}"))
                    .unwrap()
                    .exists([])
                    .unwrap()
            );
        }
    }
}

#[test]
fn process_death_recovers_spilled_writes_and_keeps_completed_commits() {
    // The same real test binary acts as the controlled writer in the child.
    // Every normal test run executes the parent and both interruption stages.
    if let Ok(stage) = std::env::var(STAGE) {
        child_writes(Path::new(&std::env::var_os(DATABASE_PATH).unwrap()), &stage);
    }
    for stage in ["uncommitted", "committed"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.sqlite");
        let journal = directory.path().join("live.sqlite-journal");
        let mut database = Database::open(&path).unwrap();
        database.create_unique_constraint("Saved", "key").unwrap();
        database
            .execute("CREATE (a:Saved:Original {key: 'a'})-[:LINK {weight: 1}]->(:Other)")
            .unwrap();
        let before = database
            .execute("MATCH (n) RETURN n ORDER BY elementId(n)")
            .unwrap()
            .rows;
        let edges = database.execute("MATCH ()-[r]->() RETURN r").unwrap().rows;
        drop(database);
        let original_bytes = fs::read(&path).unwrap();
        let mut child = ready_child(&path, stage);
        if stage == "uncommitted" {
            let header = fs::read(&journal).unwrap();
            assert!(header.len() > 512);
            assert_eq!(
                &header[..8],
                &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]
            );
            assert!(
                fs::read(&path).unwrap() != original_bytes,
                "write must have spilled before interruption"
            );
        } else {
            assert!(
                !journal.exists(),
                "successful DELETE-journal commit removes its journal"
            );
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());

        if stage == "uncommitted" {
            let read_only = Database::open_with_options(
                &path,
                &OpenOptions {
                    mode: OpenMode::ReadOnly,
                    ..OpenOptions::default()
                },
            );
            assert!(
                matches!(read_only, Err(Error::Sqlite(error)) if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ReadOnly))
            );
        }
        // Writable opening lets SQLite recover a hot journal before Crabnet
        // inspects application/schema metadata. No application replay is needed.
        let mut recovered = Database::open_with_options(
            &path,
            &OpenOptions {
                mode: OpenMode::ReadWrite,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(recovered.unique_constraints().unwrap().len(), 1);
        assert_eq!(
            recovered.execute("MATCH ()-[r]->() RETURN r").unwrap().rows,
            edges
        );
        if stage == "uncommitted" {
            assert_eq!(
                recovered
                    .execute("MATCH (n) RETURN n ORDER BY elementId(n)")
                    .unwrap()
                    .rows,
                before
            );
            assert_eq!(recovered.unique_constraints().unwrap()[0].label, "Saved");
            assert_eq!(
                recovered
                    .execute("MATCH (n:Saved:Original {key: 'a'}) RETURN count(n)")
                    .unwrap()
                    .rows,
                [vec![json!(1)]]
            );
            assert!(matches!(
                recovered.execute("CREATE (:Saved {key: 'a'})"),
                Err(Error::Constraint(_))
            ));
        } else {
            assert_eq!(recovered.unique_constraints().unwrap()[0].label, "Pending");
            assert_eq!(
                recovered
                    .execute("MATCH (n:Pending) RETURN count(n)")
                    .unwrap()
                    .rows,
                [vec![json!(96)]]
            );
            assert_eq!(
                recovered
                    .execute("MATCH (n:Saved:Changed {key: 'changed'}) RETURN count(n)")
                    .unwrap()
                    .rows,
                [vec![json!(1)]]
            );
            assert!(matches!(
                recovered.execute("CREATE (:Pending {id: 1})"),
                Err(Error::Constraint(_))
            ));
        }
        check_storage(&path);
        recovered.execute("CREATE (:AfterRecovery)").unwrap();
        let backup = directory.path().join("recovered-backup.sqlite");
        recovered.backup_to(&backup).unwrap();
        let mut restored = Database::in_memory().unwrap();
        restored.restore_from(&backup).unwrap();
        assert_eq!(
            restored.execute("MATCH (n) RETURN count(n)").unwrap().rows,
            recovered.execute("MATCH (n) RETURN count(n)").unwrap().rows
        );
        drop(recovered);
        Database::open_with_options(
            &path,
            &OpenOptions {
                mode: OpenMode::ReadOnly,
                ..OpenOptions::default()
            },
        )
        .unwrap();
    }
}
