//! One-shot read/write/sync faults from empty files through initial commit.
//! The public opening path also covers files which do not exist at registration.

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior, ffi};

use super::{
    assert_connection_integrity, assert_integrity, io_vfs,
    migration_io::{Stored, raw_open, stored},
    pragma, seed, snapshot,
    source_read_io::{assert_files, read_only, source_cache_guard, verify_replacement},
};
use crate::{Database, Error, OpenMode, OpenOptions};
use io_vfs::{Control, Event, FileKind, Operation};

#[derive(Clone, Copy, Debug)]
enum Input {
    Missing,
    ZeroBytes,
    EmptySqlite,
}

impl Input {
    fn prepare(self, path: &Path) -> Vec<u8> {
        match self {
            Self::Missing => assert!(!path.exists()),
            Self::ZeroBytes => fs::write(path, []).unwrap(),
            Self::EmptySqlite => {
                let connection = Connection::open(path).unwrap();
                // A real SQLite header/page, with no application objects.
                connection.execute_batch("VACUUM").unwrap();
                assert_uninitialized(&connection);
                assert!(fs::metadata(path).unwrap().len() > 0);
            }
        }
        if path.exists() {
            fs::read(path).unwrap()
        } else {
            Vec::new()
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Public,
    Retained,
}

#[derive(Debug)]
struct Fault {
    index: usize,
    event: Event,
    occurrence: usize,
}

struct Trace {
    events: Vec<Event>,
    connect_end: Option<usize>,
    commit_at: Option<usize>,
    initialized: Stored,
}

impl Trace {
    fn faults(&self) -> Vec<Fault> {
        self.events
            .iter()
            .enumerate()
            .map(|(index, event)| Fault {
                index,
                event: event.clone(),
                occurrence: self.events[..index].iter().filter(|e| *e == event).count(),
            })
            .collect()
    }
}

fn options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        busy_timeout: Duration::ZERO,
    }
}

fn assert_uninitialized(connection: &Connection) {
    assert_eq!(pragma(connection, "application_id"), 0);
    assert_eq!(pragma(connection, "user_version"), 0);
    let objects: i64 = connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        objects, 0,
        "no partial schema may survive initialization failure"
    );
    assert_connection_integrity(connection);
}

fn commit_marker(connection: &Connection, control: &Control, commits: &Arc<Mutex<Vec<usize>>>) {
    let control = control.clone();
    let commits = commits.clone();
    connection
        .commit_hook(Some(move || {
            commits.lock().unwrap().push(control.event_count());
            false
        }))
        .unwrap();
}

fn assert_io_error(result: crate::Result<()>, operation: Operation) {
    let error = result.expect_err("an injected initialization fault must reject opening");
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite I/O error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::SystemIoFailure);
    assert_eq!(code.extended_code, operation.error_code());
}

fn healthy(event: &Event) -> bool {
    event.result == ffi::SQLITE_OK
        || (event.operation == Operation::Read && event.result == ffi::SQLITE_IOERR_SHORT_READ)
}

fn assert_writer_released(path: &Path, original: &[u8]) {
    let mut writer = raw_open(path, OpenFlags::SQLITE_OPEN_READ_WRITE);
    let transaction = writer
        .transaction_with_behavior(TransactionBehavior::Exclusive)
        .unwrap();
    transaction
        .execute_batch(
            "CREATE TABLE initialization_probe (value INTEGER);
        INSERT INTO initialization_probe VALUES (1);",
        )
        .unwrap();
    transaction.rollback().unwrap();
    assert_uninitialized(&writer);
    assert_eq!(fs::read(path).unwrap(), original);
}

fn verify_created(path: &Path) {
    let mut database = seed(path);
    let initial = snapshot(&mut database);
    let committed = verify_replacement(&mut database, &initial);
    let backup = path.with_file_name("backup.sqlite");
    database.backup_to(&backup).unwrap();
    let mut restored = Database::in_memory().unwrap();
    restored.restore_from(&backup).unwrap();
    assert_eq!(snapshot(&mut restored), committed);
    assert_integrity(&restored);
    drop(database);
    assert_eq!(snapshot(&mut read_only(path)), committed);
}

fn run_case(input: Input, mode: Mode, fault: Option<(&Trace, &Fault)>) -> Trace {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let original = input.prepare(&path);
    let existed = path.exists();
    let registration = io_vfs::register(&path);
    assert_eq!(
        path.exists(),
        existed,
        "registration must not create the file"
    );
    let control = &registration.control;
    let commits = Arc::new(Mutex::new(Vec::new()));
    let mut retained = None;
    if matches!(mode, Mode::Retained) {
        assert!(existed);
        let connection = registration
            .connect(
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
            )
            .unwrap();
        connection.busy_timeout(Duration::ZERO).unwrap();
        connection
            .execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_spill=OFF;")
            .unwrap();
        assert_eq!(pragma(&connection, "synchronous"), 2);
        assert_eq!(pragma(&connection, "cache_spill"), 0);
        connection.execute_batch("PRAGMA shrink_memory;").unwrap();
        commit_marker(&connection, control, &commits);
        retained = Some(connection);
    }
    control.start_reads();
    if let Some((_, fault)) = fault {
        control.fail_io(&fault.event, fault.occurrence);
    }
    let mut connect_end = None;
    let mut opened = None;
    let result = match mode {
        Mode::Retained => {
            connect_end = Some(0);
            super::super::initialize_with_mode(retained.as_mut().unwrap(), OpenMode::OpenOrCreate)
        }
        Mode::Public => Database::open_with_connector(
            &path,
            &options(OpenMode::OpenOrCreate),
            |actual, flags| {
                assert_eq!(actual, path);
                let connection = registration.connect(flags)?;
                connect_end = Some(control.event_count());
                commit_marker(&connection, control, &commits);
                Ok(connection)
            },
        )
        .map(|database| opened = Some(database)),
    };
    let events = control.stop();
    let commits = commits.lock().unwrap();
    assert!(commits.len() <= 1, "unexpected commits: {commits:?}");
    let commit_at = commits.first().copied();
    if let Some(connection) = &retained {
        connection.commit_hook(None::<fn() -> bool>).unwrap();
        assert!(connection.is_autocommit());
        assert_eq!(control.open_file_count(), 1);
    }
    if let Some((baseline, fault)) = fault {
        assert_io_error(result, fault.event.operation);
        assert!(opened.is_none());
        if matches!(mode, Mode::Public) {
            assert_eq!(control.open_file_count(), 0);
        }
        let failures: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !healthy(e))
            .collect();
        assert_eq!(
            failures.len(),
            1,
            "one-shot fault, healthy cleanup: {events:?}"
        );
        assert_eq!(failures[0].0, fault.index);
        let mut failed = fault.event.clone();
        failed.result = fault.event.operation.error_code();
        assert_eq!(*failures[0].1, failed);
        assert_eq!(&events[..fault.index], &baseline.events[..fault.index]);
        assert_eq!(
            commit_at,
            (fault.index >= baseline.commit_at.unwrap()).then_some(baseline.commit_at.unwrap())
        );
        // A failed create can leave an empty file. Do not reopen through
        // Crabnet until exact bytes and raw metadata rule out partial DDL.
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_files(directory.path(), &["graph.sqlite"]);
        assert_uninitialized(&raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY));
        assert_writer_released(&path, &original);
        if let Some(connection) = &retained {
            assert_uninitialized(connection);
        }
        drop(retained.take());
        assert_eq!(control.open_file_count(), 0);
        for mode in [OpenMode::ReadOnly, OpenMode::ReadWrite] {
            assert!(matches!(Database::open_with_options(&path, &options(mode)),
                Err(Error::Data(message)) if message.contains("uninitialized")));
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    } else {
        result.unwrap();
        assert!(events.iter().all(healthy), "{events:?}");
    }
    if let Some(database) = &opened {
        assert_eq!(pragma(&database.connection, "synchronous"), 2);
        let journal: String = database
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "delete");
    }
    drop(opened);
    drop(retained);
    assert_eq!(control.open_file_count(), 0);
    drop(registration);
    // Retry OpenOrCreate against the leftover file, not a deleted/replaced path.
    let mut database =
        Database::open_with_options(&path, &options(OpenMode::OpenOrCreate)).unwrap();
    assert_eq!(
        pragma(&database.connection, "application_id"),
        super::super::APPLICATION_ID
    );
    assert_eq!(
        pragma(&database.connection, "user_version"),
        super::super::SCHEMA_VERSION
    );
    let empty = snapshot(&mut database);
    assert!(
        empty.nodes.is_empty() && empty.relationships.is_empty() && empty.constraints.is_empty()
    );
    assert!(empty.indexes.iter().all(Vec::is_empty));
    assert_integrity(&database);
    let initialized = stored(&database.connection);
    if let Some((baseline, _)) = fault {
        assert_eq!(initialized, baseline.initialized);
    }
    drop(database);
    verify_created(&path);
    assert_files(directory.path(), &["graph.sqlite", "backup.sqlite"]);
    Trace {
        events,
        connect_end,
        commit_at,
        initialized,
    }
}

fn sweep(input: Input, mode: Mode) {
    let baseline = run_case(input, mode, None);
    let connect_end = baseline.connect_end.unwrap();
    let commit_at = baseline.commit_at.unwrap();
    assert!(commit_at > 0 && commit_at < baseline.events.len());
    assert!(
        baseline.events[..commit_at]
            .iter()
            .all(|e| e.file != FileKind::Database || e.operation != Operation::Write)
    );
    for file in [FileKind::Database, FileKind::Journal] {
        for operation in [Operation::Read, Operation::Write, Operation::Sync] {
            assert!(
                baseline
                    .events
                    .iter()
                    .any(|e| e.file == file && e.operation == operation),
                "missing {file:?} {operation:?}: {:?}",
                baseline.events
            );
        }
    }
    if matches!(mode, Mode::Public) {
        assert!(connect_end > 0);
    }
    assert!(
        baseline.events[connect_end..commit_at]
            .iter()
            .any(|e| e.operation == Operation::Read)
    );
    let faults = baseline.faults();
    eprintln!(
        "initialization {input:?} {mode:?}: {} faults (connect {connect_end}, commit {commit_at})",
        faults.len()
    );
    for fault in faults {
        eprintln!("initialization {input:?} {mode:?}: {fault:?}");
        run_case(input, mode, Some((&baseline, &fault)));
    }
}

#[test]
fn new_and_empty_file_initialization_io_failures_allow_public_retry() {
    let _cache_guard = source_cache_guard();
    for input in [Input::Missing, Input::ZeroBytes, Input::EmptySqlite] {
        sweep(input, Mode::Public);
    }
}

#[test]
fn initialization_io_failures_release_locks_before_retained_connection_close() {
    let _cache_guard = source_cache_guard();
    for input in [Input::ZeroBytes, Input::EmptySqlite] {
        sweep(input, Mode::Retained);
    }
}

#[test]
fn new_file_initialization_fault_is_path_scoped_and_consumed_once() {
    let _cache_guard = source_cache_guard();
    let baseline = run_case(Input::Missing, Mode::Public, None);
    let target = baseline
        .events
        .iter()
        .find(|event| event.file == FileKind::Database && event.operation == Operation::Sync)
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    let other_path = directory.path().join("other.sqlite");
    let registration = io_vfs::register(&path);
    let other = io_vfs::register(&other_path);
    assert!(!path.exists() && !other_path.exists());
    let control = &registration.control;
    control.start_reads();
    control.fail_io(target, 0);
    let unrelated =
        Database::open_with_connector(&other_path, &options(OpenMode::OpenOrCreate), |_, flags| {
            other.connect(flags)
        })
        .unwrap();
    assert_eq!(stored(&unrelated.connection), baseline.initialized);
    drop(unrelated);
    assert_eq!(control.event_count(), 0);
    assert!(!path.exists());
    assert_io_error(
        Database::open_with_connector(&path, &options(OpenMode::OpenOrCreate), |_, flags| {
            registration.connect(flags)
        })
        .map(drop),
        Operation::Sync,
    );
    assert_eq!(control.open_file_count(), 0);
    assert!(fs::read(&path).unwrap().is_empty());
    assert_uninitialized(&raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY));
    assert_writer_released(&path, &[]);
    assert_files(directory.path(), &["graph.sqlite", "other.sqlite"]);
    let retry_start = control.event_count();
    // Do not stop or rearm the controller: a full successful initialization
    // on this same path must follow consumption of the final-sync fault.
    let retry =
        Database::open_with_connector(&path, &options(OpenMode::OpenOrCreate), |_, flags| {
            registration.connect(flags)
        })
        .unwrap();
    let events = control.stop();
    assert_eq!(stored(&retry.connection), baseline.initialized);
    assert_integrity(&retry);
    drop(retry);
    assert_eq!(control.open_file_count(), 0);
    assert_eq!(other.control.open_file_count(), 0);
    let failures: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| !healthy(e))
        .collect();
    assert_eq!(failures.len(), 1);
    let (index, failure) = failures[0];
    let mut expected = target.clone();
    expected.result = ffi::SQLITE_IOERR_FSYNC;
    assert_eq!(*failure, expected);
    assert!(index < retry_start);
    assert!(
        events[..index]
            .iter()
            .filter(|e| e.file == FileKind::Database
                && e.operation == Operation::Write
                && e.result == ffi::SQLITE_OK)
            .count()
            > 1
    );
    assert_eq!(&events[retry_start..], &baseline.events);
    verify_created(&path);
    assert_files(
        directory.path(),
        &["graph.sqlite", "other.sqlite", "backup.sqlite"],
    );
}
