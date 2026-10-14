//! Exercise each write/sync in a healthy commit trace, one failure at a time.
//! Arming after successful transaction queries (or in the standalone commit
//! hook) excludes statement-time failures. Cache spill is disabled explicitly.

use super::{
    MATCH, MUTATE, assert_competing_writer_can_commit, assert_integrity, assert_saved_constraint,
    io_vfs, open_existing, pragma, seed, snapshot,
};
use crate::{Database, Error, Parameters, QueryResult};
use io_vfs::{Control, Event, FileKind, Operation};
use rusqlite::{ErrorCode, ffi};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub(super) enum Mode {
    Standalone,
    Transaction,
}

pub(super) fn configure(database: &Database) {
    let connection = &database.connection;
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

fn apply(
    database: &mut Database,
    mode: Mode,
    control: &Control,
    fail_at: Option<usize>,
) -> crate::Result<QueryResult> {
    let control = control.clone();
    apply_armed(database, mode, move || control.start(fail_at))
}

pub(super) fn apply_armed(
    database: &mut Database,
    mode: Mode,
    arm: impl Fn() + Send + 'static,
) -> crate::Result<QueryResult> {
    let parameters = Parameters::from([(
        "extra".into(),
        json!([null, true, {"precise": 9007199254740993_i64}]),
    )]);
    let tail = "SET b.extra=$extra RETURN a.key, b.extra";
    match mode {
        Mode::Standalone => {
            // SQLite invokes this before flushing the journal/database. Every
            // Cypher row has already run; all injected I/O is in implicit commit.
            let armed = Arc::new(AtomicUsize::new(0));
            let callback_armed = armed.clone();
            database
                .connection
                .commit_hook(Some(move || {
                    callback_armed.fetch_add(1, Ordering::Relaxed);
                    arm();
                    false
                }))
                .unwrap();
            let result = database.execute_with(&format!("{MATCH} {MUTATE} {tail}"), &parameters);
            database
                .connection
                .commit_hook(None::<fn() -> bool>)
                .unwrap();
            assert_eq!(armed.load(Ordering::Relaxed), 1);
            result
        }
        Mode::Transaction => {
            let mut transaction = database.transaction().unwrap();
            assert!(transaction.drop_unique_constraint("Saved", "key").unwrap());
            assert!(
                transaction
                    .create_unique_constraint("Transient", "id")
                    .unwrap()
            );
            let provisional = transaction
                .execute_with(
                    &format!("{MATCH} WITH a, r, b LIMIT 1 {MUTATE} {tail}"),
                    &parameters,
                )
                .unwrap();
            transaction
                .execute("MATCH (n:Saved {key:'gone'}) DELETE n")
                .unwrap();
            // Both the mutation result and a later read succeeded before arm.
            assert_eq!(
                provisional.rows,
                [vec![json!("changed"), parameters["extra"].clone()]]
            );
            assert_eq!(
                transaction
                    .execute("MATCH (n:Pending) RETURN n.key")
                    .unwrap()
                    .rows,
                [vec![json!("changed")]]
            );
            arm();
            transaction.commit()?;
            Ok(provisional)
        }
    }
}

fn assert_io_error(error: Error, operation: Operation) {
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite I/O error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::SystemIoFailure);
    assert_eq!(code.extended_code, operation.error_code());
}

pub(super) fn assert_committed(database: &mut Database, mode: Mode, result: QueryResult) {
    let copies = match mode {
        Mode::Standalone => 2,
        Mode::Transaction => 1,
    };
    assert_eq!(
        result.rows,
        vec![
            vec![
                json!("changed"),
                json!([null, true, {"precise": 9007199254740993_i64}])
            ];
            copies
        ]
    );
    assert_eq!(
        database
            .execute("MATCH (n:Transient) RETURN n.id")
            .unwrap()
            .rows,
        vec![vec![json!(1)]; copies]
    );
    assert_eq!(
        database
            .execute("MATCH ()-[r:TEMP]->() RETURN count(r)")
            .unwrap()
            .rows,
        [vec![json!(copies)]]
    );
    assert_eq!(
        database
            .execute("MATCH ()-[r:LINK]->() RETURN r.weight ORDER BY r.weight")
            .unwrap()
            .rows,
        match mode {
            Mode::Standalone => vec![vec![json!(9)], vec![json!(9)]],
            Mode::Transaction => vec![vec![json!(1)], vec![json!(9)]],
        }
    );
    if matches!(mode, Mode::Transaction) {
        assert!(
            database
                .execute("MATCH (n:Saved {key:'gone'}) RETURN n")
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(matches!(
            database.execute("CREATE (:Transient {id:1})"),
            Err(Error::Constraint(_))
        ));
        let constraints = database.unique_constraints().unwrap();
        assert_eq!(constraints.len(), 2);
        assert!(
            constraints
                .iter()
                .any(|constraint| constraint.label == "Transient" && constraint.property == "id")
        );
        assert!(
            !constraints
                .iter()
                .any(|constraint| constraint.label == "Saved" && constraint.property == "key")
        );
    }
}

fn run_case(mode: Mode, fault: Option<(usize, &Event)>) -> Vec<Event> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    drop(seed(&path));
    let (mut database, registration) = io_vfs::open(&path);
    configure(&database);
    let before = snapshot(&mut database);
    let mut observer = open_existing(&path);
    let control = &registration.control;
    let result = apply(&mut database, mode, control, fault.map(|(index, _)| index));
    let events = control.stop();
    assert!(
        !events.is_empty(),
        "must actually intercept commit I/O: {mode:?}"
    );
    if let Some((index, expected)) = fault {
        assert_io_error(
            result.expect_err("injection must reject an otherwise successful commit"),
            expected.operation,
        );
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
        assert_eq!(failed[0].0, index);
        assert_eq!(failed[0].1.file, expected.file);
        assert_eq!(failed[0].1.operation, expected.operation);
        assert_eq!(failed[0].1.offset, expected.offset);
        assert_eq!(failed[0].1.amount, expected.amount);
        assert_eq!(failed[0].1.result, expected.operation.error_code());
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
        // Check the observer FIRST: reading on the original handle must not be
        // needed to clean up a failed commit or release its locks.
        assert!(database.connection.is_autocommit());
        assert_competing_writer_can_commit(&mut observer, &before);
        assert_eq!(snapshot(&mut database), before);
        assert_integrity(&database);
        assert_saved_constraint(&mut database);
        let mut reopened = open_existing(&path);
        assert_eq!(snapshot(&mut reopened), before);
        assert_integrity(&reopened);
        drop(reopened);

        // Same connection, exact same logical operation, no injected fault.
        let retry = apply(&mut database, mode, control, None).unwrap();
        let retry_events = control.stop();
        assert!(
            retry_events
                .iter()
                .all(|event| event.result == ffi::SQLITE_OK)
        );
        assert_committed(&mut database, mode, retry);
    } else {
        assert!(events.iter().all(|event| event.result == ffi::SQLITE_OK));
        assert_committed(&mut database, mode, result.unwrap());
    }
    let committed = snapshot(&mut database);
    assert_ne!(committed, before);
    assert_integrity(&database);
    drop(database);
    drop(registration);
    let mut reopened = open_existing(&path);
    assert_eq!(snapshot(&mut reopened), committed);
    assert_eq!(snapshot(&mut observer), committed);
    assert_integrity(&reopened);
    events
}

fn sweep(mode: Mode) {
    // Discover the healthy trace on this native VFS instead of pinning a
    // platform-dependent syscall count. Cover every intercepted commit event,
    // including later writes after earlier main-database page writes succeeded.
    let events = run_case(mode, None);
    for file in [FileKind::Journal, FileKind::Database] {
        for operation in [Operation::Write, Operation::Sync] {
            assert!(
                events
                    .iter()
                    .any(|event| event.file == file && event.operation == operation)
            );
        }
    }
    assert!(
        events
            .iter()
            .filter(|event| event.file == FileKind::Database && event.operation == Operation::Write)
            .count()
            > 1
    );
    for (index, event) in events.iter().enumerate() {
        eprintln!("{mode:?} commit fault {index}: {event:?}");
        let failed = run_case(mode, Some((index, event)));
        assert_eq!(
            &failed[..index],
            &events[..index],
            "healthy I/O prefix: {mode:?}, fault {index}"
        );
    }
}

#[test]
fn standalone_commit_io_failures_roll_back_and_allow_retry() {
    sweep(Mode::Standalone);
}

#[test]
fn transaction_commit_io_failures_roll_back_queries_and_constraints() {
    sweep(Mode::Transaction);
}

#[test]
fn io_faults_are_scoped_to_one_database_and_consumed_once() {
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first.sqlite");
    let second_path = directory.path().join("second.sqlite");
    drop(seed(&first_path));
    drop(seed(&second_path));
    let (mut first, first_registration) = io_vfs::open(&first_path);
    let (mut second, second_registration) = io_vfs::open(&second_path);
    configure(&first);
    configure(&second);

    let mut transaction = first.transaction().unwrap();
    transaction.execute("CREATE (:Probe)").unwrap();
    first_registration.control.start(Some(0));
    second_registration.control.start(None);
    second.execute("CREATE (:Probe)").unwrap();
    let other_events = second_registration.control.stop();
    assert!(!other_events.is_empty());
    assert!(
        other_events
            .iter()
            .all(|event| event.result == ffi::SQLITE_OK)
    );
    let error = transaction.commit().unwrap_err();
    // Leave the first controller recording: retry succeeds without disarming
    // or resetting it, proving the failure consumed itself before rollback.
    first.execute("CREATE (:Probe)").unwrap();
    let events = first_registration.control.stop();
    assert_io_error(error, events[0].operation);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.result != ffi::SQLITE_OK)
            .count(),
        1
    );
    for database in [&mut first, &mut second] {
        assert_eq!(
            database
                .execute("MATCH (n:Probe) RETURN count(n)")
                .unwrap()
                .rows,
            [vec![json!(1)]]
        );
        assert_integrity(database);
    }
}
