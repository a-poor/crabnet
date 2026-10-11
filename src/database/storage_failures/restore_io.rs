//! Fail every destination write/sync while replacing a smaller or larger graph.
//! Source reads, truncation, and journal deletion are outside this fault model.

use super::{
    Snapshot, assert_competing_writer_can_commit, assert_integrity, assert_saved_constraint,
    io_vfs, open_existing, pragma, seed, snapshot,
};
use crate::{Database, Error, OpenMode, OpenOptions, Parameters};
use io_vfs::{Event, FileKind, Operation};
use rusqlite::{Connection, ErrorCode, OpenFlags, ffi};
use serde_json::json;
use std::{fs, path::PathBuf, time::Duration};

#[derive(Clone, Copy, Debug)]
enum SizeChange {
    Grow,
    Shrink,
}

struct Fixture {
    _directory: tempfile::TempDir,
    source: PathBuf,
    destination: PathBuf,
    source_bytes: Vec<u8>,
    original: Snapshot,
    replacement: Snapshot,
    original_pages: i64,
    replacement_pages: i64,
    page_size: i64,
}

impl Fixture {
    fn new(change: SizeChange) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("destination.sqlite");
        let mut original = seed(&destination);
        let mut replacement = Database::in_memory().unwrap();
        replacement
            .create_unique_constraint("Replacement", "key")
            .unwrap();
        replacement
            .execute(
                "CREATE (a:Replacement:New {key:'x', number:1.0,
                    nested:[null, false, {precise:2.5}], big:9007199254740995}),
                 (b:Replacement {key:'y', number:2}),
                 (a)-[:REPLACED {weight:3}]->(b), (a)-[:REPLACED {weight:3}]->(b)",
            )
            .unwrap();
        let page_size = pragma(&original.connection, "page_size");
        assert_eq!(pragma(&replacement.connection, "page_size"), page_size);
        let larger = match change {
            SizeChange::Grow => &mut replacement,
            SizeChange::Shrink => &mut original,
        };
        larger
            .execute_with(
                "CREATE (:Payload {body:$body})",
                &Parameters::from([(
                    "body".into(),
                    json!("x".repeat(usize::try_from(page_size * 4).unwrap())),
                )]),
            )
            .unwrap();
        let source = directory.path().join("backup.sqlite");
        replacement.backup_to(&source).unwrap();
        let original_pages = pragma(&original.connection, "page_count");
        let replacement_pages = pragma(&replacement.connection, "page_count");
        match change {
            SizeChange::Grow => assert!(replacement_pages > original_pages),
            SizeChange::Shrink => assert!(replacement_pages < original_pages),
        }
        Self {
            source_bytes: fs::read(&source).unwrap(),
            original: snapshot(&mut original),
            replacement: snapshot(&mut replacement),
            original_pages,
            replacement_pages,
            page_size,
            source,
            destination,
            _directory: directory,
        }
    }

    fn assert_source_unchanged_and_unlocked(&self) {
        assert_eq!(fs::read(&self.source).unwrap(), self.source_bytes);
        // No data is written: an exclusive lock proves restore released its
        // source read transaction without changing the backup's byte oracle.
        let probe = Connection::open_with_flags(
            &self.source,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        )
        .unwrap();
        probe.busy_timeout(Duration::ZERO).unwrap();
        probe.execute_batch("BEGIN EXCLUSIVE; COMMIT").unwrap();
        drop(probe);
        let mut reader = Database::open_with_options(
            &self.source,
            &OpenOptions {
                mode: OpenMode::ReadOnly,
                busy_timeout: Duration::ZERO,
            },
        )
        .unwrap();
        assert_eq!(snapshot(&mut reader), self.replacement);
        assert_integrity(&reader);
        assert_eq!(fs::read(&self.source).unwrap(), self.source_bytes);
    }
}

fn configure(database: &Database) {
    let connection = &database.connection;
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
        .pragma_update(None, "synchronous", "FULL")
        .unwrap();
    connection
        .pragma_update(None, "cache_spill", false)
        .unwrap();
    assert_eq!(pragma(connection, "synchronous"), 2);
    assert_eq!(pragma(connection, "cache_spill"), 0);
    let journal: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
}

fn assert_failed(result: crate::Result<()>, events: &[Event], index: usize, expected: &Event) {
    let error = result.expect_err("injection must reject an otherwise successful restore");
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite I/O error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::SystemIoFailure);
    assert_eq!(code.extended_code, expected.operation.error_code());
    let failed: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.result != ffi::SQLITE_OK)
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "one-shot fault, healthy rollback: {events:?}"
    );
    let mut target = expected.clone();
    target.result = expected.operation.error_code();
    assert_eq!(failed, [(index, &target)]);
    if expected.file == FileKind::Database {
        assert!(
            events[index + 1..].iter().any(|event| {
                event.file == FileKind::Database
                    && event.operation == Operation::Write
                    && event.result == ffi::SQLITE_OK
            }),
            "rollback must write original database pages: {events:?}"
        );
    }
}

fn run_case(fixture: &Fixture, fault: Option<(usize, &Event)>) -> Vec<Event> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("live.sqlite");
    // Copy a closed fixture so every trace starts with identical pages/IDs.
    fs::copy(&fixture.destination, &path).unwrap();
    let (mut database, registration) = io_vfs::open(&path);
    configure(&database);
    // Prime the prepared statements and mutation TEMP relation before restore.
    database
        .execute("MATCH (n:Saved {key:'a'}) SET n.number=1 RETURN n")
        .unwrap();
    assert_eq!(snapshot(&mut database), fixture.original);
    let before_bytes = fs::read(&path).unwrap();
    let mut observer = open_existing(&path);
    assert_eq!(snapshot(&mut observer), fixture.original);
    let control = &registration.control;
    control.start(fault.map(|(index, _)| index));
    let result = database.restore_from(&fixture.source);
    let events = control.stop();
    assert!(!events.is_empty(), "must intercept restore I/O");

    if let Some((index, expected)) = fault {
        assert_failed(result, &events, index, expected);
        // Before reusing either connection (or reopening for recovery), the
        // destination file must already contain precisely its original pages.
        assert_eq!(fs::read(&path).unwrap(), before_bytes);
        assert!(database.connection.is_autocommit());
        fixture.assert_source_unchanged_and_unlocked();
        assert_competing_writer_can_commit(&mut observer, &fixture.original);
        assert_eq!(snapshot(&mut database), fixture.original);
        assert_eq!(
            pragma(&database.connection, "page_count"),
            fixture.original_pages
        );
        assert_integrity(&database);
        assert_saved_constraint(&mut database);
        let mut reopened = open_existing(&path);
        assert_eq!(snapshot(&mut reopened), fixture.original);
        assert_integrity(&reopened);
        drop(reopened);
        // The original handle can still read/write before retrying the same
        // restore. The observer's earlier commit did not require this cleanup.
        assert_competing_writer_can_commit(&mut database, &fixture.original);
        control.start(None);
        database.restore_from(&fixture.source).unwrap();
        assert!(
            control
                .stop()
                .iter()
                .all(|event| event.result == ffi::SQLITE_OK)
        );
    } else {
        result.unwrap();
        assert!(events.iter().all(|event| event.result == ffi::SQLITE_OK));
    }

    assert_eq!(snapshot(&mut database), fixture.replacement);
    assert_eq!(snapshot(&mut observer), fixture.replacement);
    assert_eq!(
        pragma(&database.connection, "page_count"),
        fixture.replacement_pages
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        u64::try_from(fixture.page_size * fixture.replacement_pages).unwrap()
    );
    assert_integrity(&database);
    fixture.assert_source_unchanged_and_unlocked();
    assert!(matches!(
        database.execute("CREATE (:Replacement {key:'x'})"),
        Err(Error::Constraint(_))
    ));
    // Exercise mutation TEMP state after replacement and persist a fresh write.
    database
        .execute("MATCH (n:Replacement {key:'x'}) SET n.restored=true RETURN n")
        .unwrap();
    assert_eq!(
        observer
            .execute("MATCH (n:Replacement {key:'x'}) RETURN n.restored")
            .unwrap()
            .rows,
        [vec![json!(true)]]
    );
    let committed = snapshot(&mut database);
    drop(observer);
    drop(database);
    drop(registration);
    let mut reopened = open_existing(&path);
    assert_eq!(snapshot(&mut reopened), committed);
    assert_integrity(&reopened);
    fixture.assert_source_unchanged_and_unlocked();
    events
}

fn sweep(change: SizeChange) {
    let fixture = Fixture::new(change);
    let healthy = run_case(&fixture, None);
    for file in [FileKind::Journal, FileKind::Database] {
        for operation in [Operation::Write, Operation::Sync] {
            assert!(
                healthy
                    .iter()
                    .any(|event| event.file == file && event.operation == operation)
            );
        }
    }
    assert!(
        healthy
            .iter()
            .filter(|event| event.file == FileKind::Database && event.operation == Operation::Write)
            .count()
            > 1,
        "must cover failure after earlier database pages were replaced"
    );
    for (index, expected) in healthy.iter().enumerate() {
        eprintln!("restore {change:?}, fault {index}: {expected:?}");
        let events = run_case(&fixture, Some((index, expected)));
        assert_eq!(
            events[..index],
            healthy[..index],
            "{change:?}, fault {index}"
        );
    }
    eprintln!(
        "restore {change:?}: {} destination write/sync fault points",
        healthy.len()
    );
}

#[test]
fn larger_restore_io_failures_preserve_destination_and_allow_retry() {
    sweep(SizeChange::Grow);
}

#[test]
fn smaller_restore_io_failures_preserve_destination_and_allow_retry() {
    sweep(SizeChange::Shrink);
}
