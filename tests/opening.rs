use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crabnet::{Database, Error, OpenMode, OpenOptions, Parameters};
use rusqlite::Connection;
use serde_json::json;

struct DatabaseFile(PathBuf);

impl DatabaseFile {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "crabnet-opening-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        Self(directory.join("graph.sqlite"))
    }
}

impl Drop for DatabaseFile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0.parent().unwrap());
    }
}

fn options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        ..OpenOptions::default()
    }
}

#[test]
fn open_existing_rejects_an_empty_file_without_initializing_it() {
    let file = DatabaseFile::new();
    fs::write(&file.0, []).unwrap();
    for mode in [OpenMode::ReadWrite, OpenMode::ReadOnly] {
        assert!(matches!(
            Database::open_with_options(&file.0, &options(mode)),
            Err(Error::Data(_))
        ));
        assert!(fs::read(&file.0).unwrap().is_empty());
    }
    Database::open(&file.0).unwrap();
    Database::open_with_options(&file.0, &options(OpenMode::ReadWrite)).unwrap();
}

#[test]
fn only_create_mode_creates_missing_files_and_defaults_preserve_existing_data() {
    let file = DatabaseFile::new();
    for mode in [OpenMode::ReadWrite, OpenMode::ReadOnly] {
        assert!(matches!(
            Database::open_with_options(&file.0, &options(mode)),
            Err(Error::Sqlite(_))
        ));
        assert!(!file.0.exists());
    }
    let mut database = Database::open_with_options(&file.0, &OpenOptions::default()).unwrap();
    database
        .execute("CREATE (:Saved {value: 1}), (:Saved {value: 1})")
        .unwrap();
    drop(database);
    let mut database = Database::open(&file.0).unwrap();
    assert_eq!(
        database.execute("MATCH (n) RETURN n.value").unwrap().rows,
        [vec![json!(1)], vec![json!(1)]]
    );
    database.execute("CREATE (:Saved {value: 2})").unwrap();
}

#[test]
fn invalid_options_fail_before_creating_a_file() {
    let file = DatabaseFile::new();
    for timeout in [
        Duration::MAX,
        Duration::from_millis(i32::MAX as u64) + Duration::from_nanos(1),
    ] {
        let options = OpenOptions {
            busy_timeout: timeout,
            ..OpenOptions::default()
        };
        assert!(matches!(
            Database::open_with_options(&file.0, &options),
            Err(Error::Argument(_))
        ));
        assert!(!file.0.exists());
    }
    for path in ["", ":memory:", "file:ignored?mode=memory&cache=shared"] {
        assert!(matches!(Database::open(path), Err(Error::Argument(_))));
    }
}

#[test]
fn read_only_supports_hybrid_reads_and_rejects_every_write_entry_point() {
    let file = DatabaseFile::new();
    let mut database = Database::open(&file.0).unwrap();
    database.create_unique_constraint("Node", "id").unwrap();
    database
        .execute("CREATE (a:Node {id: 1})-[:LINK]->(b:Node {id: 2}), (a)-[:LINK]->(b)")
        .unwrap();
    let query = "MATCH p=(a:Node {id: $id})-[:LINK*1..]->(b) RETURN p, b.id";
    let parameters = Parameters::from([("id".into(), json!(1))]);
    let expected = database.execute_with(query, &parameters).unwrap();
    assert_eq!(expected.rows.len(), 2);
    drop(database);
    let bytes = fs::read(&file.0).unwrap();
    let mut reader = Database::open_with_options(&file.0, &options(OpenMode::ReadOnly)).unwrap();
    let mut actual = reader.execute_with(query, &parameters).unwrap().rows;
    // Paths differ by relationship identity; compare as a bag.
    for row in expected.rows {
        let index = actual
            .iter()
            .position(|candidate| *candidate == row)
            .unwrap();
        actual.remove(index);
    }
    assert!(actual.is_empty());
    assert_eq!(reader.unique_constraints().unwrap().len(), 1);
    for query in [
        "CREATE (:Node)",
        "MERGE (:Node {id: 1})",
        "MATCH (n:Missing) SET n.value = 1",
        "MATCH (n:Missing) REMOVE n.value",
        "MATCH (n:Missing) DELETE n",
        "MATCH (n) DETACH DELETE n",
    ] {
        assert!(
            matches!(reader.execute(query), Err(Error::ReadOnly)),
            "{query}"
        );
    }
    assert!(matches!(reader.transaction(), Err(Error::ReadOnly)));
    assert!(matches!(
        reader.create_unique_constraint("Node", "id"),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        reader.drop_unique_constraint("Missing", "id"),
        Err(Error::ReadOnly)
    ));
    assert_eq!(
        reader.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![json!(2)]]
    );
    drop(reader);
    assert_eq!(fs::read(&file.0).unwrap(), bytes);
}

#[test]
fn readers_see_commits_and_failed_writers_can_be_reused() {
    let file = DatabaseFile::new();
    let mut writer = Database::open(&file.0).unwrap();
    writer.execute("CREATE (:Saved {id: 1})").unwrap();
    let no_wait = OpenOptions {
        mode: OpenMode::ReadWrite,
        busy_timeout: Duration::ZERO,
    };
    let mut contender = Database::open_with_options(&file.0, &no_wait).unwrap();
    let mut transaction = writer.transaction().unwrap();
    transaction.execute("CREATE (:Saved {id: 2})").unwrap();
    // Current-schema opening and reading do not reserve the writer.
    let mut reader = Database::open_with_options(&file.0, &options(OpenMode::ReadOnly)).unwrap();
    assert_eq!(
        reader.execute("MATCH (n) RETURN n.id").unwrap().rows,
        [vec![json!(1)]]
    );
    assert!(matches!(
        contender.execute("CREATE (:Lost)"),
        Err(Error::Busy(_))
    ));
    assert!(matches!(contender.transaction(), Err(Error::Busy(_))));
    assert!(matches!(
        contender.create_unique_constraint("Saved", "id"),
        Err(Error::Busy(_))
    ));
    assert!(matches!(
        contender.drop_unique_constraint("Saved", "id"),
        Err(Error::Busy(_))
    ));
    transaction.commit().unwrap();
    assert_eq!(
        reader
            .execute("MATCH (n) RETURN n.id ORDER BY n.id")
            .unwrap()
            .rows,
        [vec![json!(1)], vec![json!(2)]]
    );
    let mut transaction = writer.transaction().unwrap();
    transaction.execute("CREATE (:RolledBack)").unwrap();
    assert!(matches!(
        transaction.execute("RETURN $missing"),
        Err(Error::MissingParameter(_))
    ));
    // Even with the aborted handle alive, the other writer can proceed.
    contender.execute("CREATE (:Saved {id: 3})").unwrap();
    assert!(matches!(
        transaction.commit(),
        Err(Error::TransactionAborted)
    ));
    assert_eq!(
        reader
            .execute("MATCH (n) RETURN n.id ORDER BY n.id")
            .unwrap()
            .rows,
        [vec![json!(1)], vec![json!(2)], vec![json!(3)]]
    );
}

#[test]
fn configured_timeout_applies_to_lock_waits_and_preserves_the_sqlite_error() {
    use std::error::Error as _;
    let file = DatabaseFile::new();
    let mut writer = Database::open(&file.0).unwrap();
    let mut contender = Database::open_with_options(
        &file.0,
        &OpenOptions {
            mode: OpenMode::ReadWrite,
            busy_timeout: Duration::from_millis(60),
        },
    )
    .unwrap();
    let transaction = writer.transaction().unwrap();
    let started = Instant::now();
    let error = contender.execute("CREATE (:Lost)").unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(40));
    let Error::Busy(sqlite) = &error else {
        panic!("{error:?}")
    };
    assert_eq!(
        sqlite.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert!(error.source().is_some());
    transaction.rollback().unwrap();
    contender.execute("CREATE (:Saved)").unwrap();
}

#[test]
fn waiting_writer_succeeds_after_the_other_connection_releases_its_lock() {
    let file = DatabaseFile::new();
    let mut writer = Database::open(&file.0).unwrap();
    let mut contender = Database::open_with_options(
        &file.0,
        &OpenOptions {
            mode: OpenMode::ReadWrite,
            busy_timeout: Duration::from_secs(5),
        },
    )
    .unwrap();
    let mut transaction = writer.transaction().unwrap();
    transaction.execute("CREATE (:Saved {id: 1})").unwrap();
    let (started, ready) = mpsc::channel();
    let worker = thread::spawn(move || {
        started.send(()).unwrap();
        contender.execute("CREATE (:Saved {id: 2})").unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    thread::sleep(Duration::from_millis(50));
    transaction.commit().unwrap();
    worker.join().unwrap();
    assert_eq!(
        writer
            .execute("MATCH (n) RETURN n.id ORDER BY n.id")
            .unwrap()
            .rows,
        [vec![json!(1)], vec![json!(2)]]
    );
}

#[test]
fn reader_blocked_commit_rolls_back_standalone_and_multi_query_writes() {
    let file = DatabaseFile::new();
    let mut writer = Database::open_with_options(
        &file.0,
        &OpenOptions {
            busy_timeout: Duration::ZERO,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    writer.execute("CREATE (:Saved {id: 1})").unwrap();
    // A raw read transaction pins a snapshot longer than the eager public API
    // allows. This fault injector forces contention specifically at COMMIT.
    let reader = Connection::open(&file.0).unwrap();
    reader.execute_batch("BEGIN; SELECT * FROM nodes;").unwrap();
    assert!(matches!(
        writer.execute("CREATE (:Lost)"),
        Err(Error::Busy(_))
    ));
    let mut transaction = writer.transaction().unwrap();
    transaction.create_unique_constraint("Saved", "id").unwrap();
    transaction.execute("MATCH (n:Saved) SET n.id = 2").unwrap();
    transaction.execute("CREATE (:Lost)").unwrap();
    assert!(matches!(transaction.commit(), Err(Error::Busy(_))));
    reader.execute_batch("ROLLBACK").unwrap();
    assert!(writer.unique_constraints().unwrap().is_empty());
    assert_eq!(
        writer.execute("MATCH (n) RETURN n.id").unwrap().rows,
        [vec![json!(1)]]
    );
    writer.execute("CREATE (:Saved {id: 3})").unwrap();
    drop(writer);
    let mut reopened = Database::open(&file.0).unwrap();
    assert_eq!(
        reopened
            .execute("MATCH (n) RETURN n.id ORDER BY n.id")
            .unwrap()
            .rows,
        [vec![json!(1)], vec![json!(3)]]
    );
}

#[test]
fn opening_and_reads_report_exclusive_lock_contention_without_poisoning_handles() {
    let file = DatabaseFile::new();
    let mut database = Database::open_with_options(
        &file.0,
        &OpenOptions {
            busy_timeout: Duration::ZERO,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let blocker = Connection::open(&file.0).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    for mode in [
        OpenMode::OpenOrCreate,
        OpenMode::ReadWrite,
        OpenMode::ReadOnly,
    ] {
        assert!(matches!(
            Database::open_with_options(
                &file.0,
                &OpenOptions {
                    mode,
                    busy_timeout: Duration::ZERO
                }
            ),
            Err(Error::Busy(_))
        ));
    }
    assert!(matches!(
        database.execute("MATCH (n) RETURN n"),
        Err(Error::Busy(_))
    ));
    blocker.execute_batch("ROLLBACK").unwrap();
    assert!(
        database
            .execute("MATCH (n) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    Database::open_with_options(&file.0, &options(OpenMode::ReadOnly)).unwrap();
}

#[test]
fn unknown_and_foreign_files_are_rejected_without_modification() {
    for setup in [
        "CREATE TABLE unrelated (id INTEGER)",
        "PRAGMA application_id = 42",
        "PRAGMA application_id = 1129464130; PRAGMA user_version = 999",
    ] {
        let file = DatabaseFile::new();
        Connection::open(&file.0)
            .unwrap()
            .execute_batch(setup)
            .unwrap();
        let bytes = fs::read(&file.0).unwrap();
        for mode in [
            OpenMode::OpenOrCreate,
            OpenMode::ReadWrite,
            OpenMode::ReadOnly,
        ] {
            assert!(matches!(
                Database::open_with_options(&file.0, &options(mode)),
                Err(Error::Data(_))
            ));
            assert_eq!(fs::read(&file.0).unwrap(), bytes);
        }
    }
}

fn legacy_file(file: &DatabaseFile) {
    Connection::open(&file.0).unwrap().execute_batch(r#"
        CREATE TABLE nodes (id TEXT PRIMARY KEY NOT NULL, label TEXT, props TEXT NOT NULL DEFAULT '{}') STRICT;
        CREATE TABLE edges (id TEXT PRIMARY KEY NOT NULL, edge_type TEXT NOT NULL,
            from_node TEXT NOT NULL REFERENCES nodes(id), to_node TEXT NOT NULL REFERENCES nodes(id),
            props TEXT NOT NULL DEFAULT '{}') STRICT;
        CREATE INDEX nodes_by_label ON nodes(label);
        INSERT INTO nodes VALUES ('saved', 'Saved', '{"id":1}');
        PRAGMA application_id = 1129464130;
        PRAGMA user_version = 1;
    "#).unwrap();
}

#[test]
fn read_only_requires_migration_by_a_writable_opener() {
    let file = DatabaseFile::new();
    legacy_file(&file);
    let bytes = fs::read(&file.0).unwrap();
    assert!(
        matches!(Database::open_with_options(&file.0, &options(OpenMode::ReadOnly)), Err(Error::Data(message)) if message.contains("requires migration"))
    );
    assert_eq!(fs::read(&file.0).unwrap(), bytes);
    Database::open_with_options(&file.0, &options(OpenMode::ReadWrite)).unwrap();
    let mut reader = Database::open_with_options(&file.0, &options(OpenMode::ReadOnly)).unwrap();
    assert_eq!(
        reader
            .execute("MATCH (n:Saved) RETURN elementId(n), n.id")
            .unwrap()
            .rows,
        [vec![json!("saved"), json!(1)]]
    );
}

#[test]
fn concurrent_openers_initialize_or_migrate_once_and_keep_all_committed_writes() {
    for legacy in [false, true] {
        let file = DatabaseFile::new();
        if legacy {
            legacy_file(&file);
        }
        let barrier = Arc::new(Barrier::new(4));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let barrier = Arc::clone(&barrier);
            let path = file.0.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                let mut database = Database::open(path).unwrap();
                database.execute("CREATE (:Concurrent)").unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let mut database = Database::open(&file.0).unwrap();
        assert_eq!(
            database
                .execute("MATCH (n:Concurrent) RETURN count(n)")
                .unwrap()
                .rows,
            [vec![json!(4)]]
        );
        assert_eq!(
            database
                .execute("MATCH (n:Saved) RETURN count(n)")
                .unwrap()
                .rows,
            [vec![json!(usize::from(legacy))]]
        );
    }
}
