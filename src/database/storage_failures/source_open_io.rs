//! One-shot main-file reads while opening a current-version source graph.
//! Arm before SQLite connects; separate connection and Crabnet initialization
//! phases. No hot journals, migrations, empty files, or partial/persistent I/O.

use super::{
    assert_competing_writer_can_commit, assert_integrity,
    backup_io::verify_published,
    io_vfs, open_existing, pragma, snapshot,
    source_read_io::{
        Fixture, assert_files, assert_reopened, assert_source, configure_destination, read_only,
        source_cache_guard, verify_replacement,
    },
};
use crate::database::backup::{BackupIo, NativeBackupIo};
use crate::{Database, Error, OpenMode, OpenOptions, Result};
use io_vfs::{Event, FileKind, Operation, Registration};
use rusqlite::{Connection, ErrorCode, ffi};
use std::{fs, path::Path, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Connect,
    Initialize,
}

#[derive(Debug)]
struct ReadFault {
    phase: Phase,
    event: Event,
    occurrence: usize,
}

struct Trace {
    events: Vec<Event>,
    initialization_start: Option<usize>,
}

impl Trace {
    fn faults(&self) -> Vec<ReadFault> {
        let start = self.initialization_start.unwrap();
        self.events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let (phase, first) = if index < start {
                    (Phase::Connect, 0)
                } else {
                    (Phase::Initialize, start)
                };
                ReadFault {
                    phase,
                    event: event.clone(),
                    occurrence: self.events[first..index]
                        .iter()
                        .filter(|e| *e == event)
                        .count(),
                }
            })
            .collect()
    }
}

struct Opener<'a> {
    registration: Registration,
    fault: Option<&'a ReadFault>,
    trace: Option<Trace>,
    copy_started: bool,
}

impl<'a> Opener<'a> {
    fn new(path: &Path, fault: Option<&'a ReadFault>) -> Self {
        Self {
            registration: io_vfs::register(path),
            fault,
            trace: None,
            copy_started: false,
        }
    }

    fn open(&mut self, path: &Path, options: &OpenOptions) -> Result<Database> {
        assert!(self.trace.is_none());
        let control = &self.registration.control;
        control.start_reads();
        if let Some(fault) = self.fault.filter(|f| f.phase == Phase::Connect) {
            control.fail_read(&fault.event, fault.occurrence);
        }
        let mut initialization_start = None;
        let result = Database::open_with_connector(path, options, |actual, flags| {
            assert_eq!(actual, path);
            let connection = self.registration.connect(flags)?;
            initialization_start = Some(control.event_count());
            if let Some(fault) = self.fault.filter(|f| f.phase == Phase::Initialize) {
                control.fail_read(&fault.event, fault.occurrence);
            }
            Ok(connection)
        });
        // Stop before backup validation or any graph query can consume an
        // unhit opening fault and falsely pass this narrower qualification.
        self.trace = Some(Trace {
            events: control.stop(),
            initialization_start,
        });
        result
    }
}

impl BackupIo for Opener<'_> {
    fn open_source(&mut self, path: &Path, options: &OpenOptions) -> Result<Database> {
        assert_eq!(options.mode, OpenMode::ReadOnly);
        assert_eq!(options.busy_timeout, Duration::ZERO);
        self.open(path, options)
    }

    fn copy_snapshot(&mut self, source: &Connection, destination: &mut Connection) -> Result<()> {
        self.copy_started = true;
        NativeBackupIo.copy_snapshot(source, destination)
    }
}

fn assert_read_error(result: Result<()>) {
    let error = result.expect_err("an opening read fault must reject the source");
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite read error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::SystemIoFailure);
    assert_eq!(code.extended_code, ffi::SQLITE_IOERR_READ);
}

fn assert_trace<T>(result: Result<T>, trace: &Trace, fault: Option<&ReadFault>) -> Option<T> {
    assert!(!trace.events.is_empty());
    assert!(
        trace.events.iter().all(|event| {
            event.file == FileKind::Database && event.operation == Operation::Read
        }),
        "opening a current graph must not write or sync source files"
    );
    if let Some(fault) = fault {
        assert_read_error(result.map(|_| ()));
        let failed: Vec<_> = trace
            .events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.result != ffi::SQLITE_OK)
            .collect();
        assert_eq!(
            failed.len(),
            1,
            "exactly one read failure with healthy cleanup"
        );
        let (index, event) = failed[0];
        let mut expected = fault.event.clone();
        expected.result = ffi::SQLITE_IOERR_READ;
        assert_eq!(*event, expected);
        let first = match fault.phase {
            Phase::Connect => {
                assert!(trace.initialization_start.is_none());
                0
            }
            Phase::Initialize => {
                let start = trace.initialization_start.unwrap();
                assert!(start > 0 && index >= start);
                start
            }
        };
        assert_eq!(
            trace.events[first..index]
                .iter()
                .filter(|e| **e == fault.event)
                .count(),
            fault.occurrence,
            "fail the selected byte range and occurrence within its opening phase"
        );
        None
    } else {
        let database = result.unwrap();
        assert!(trace.events.iter().all(|e| e.result == ffi::SQLITE_OK));
        let start = trace.initialization_start.unwrap();
        assert!(start > 0, "observe SQLite connection header reads");
        assert!(
            trace.events.len() > start,
            "observe Crabnet initialization reads"
        );
        Some(database)
    }
}

fn options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        busy_timeout: Duration::ZERO,
    }
}

fn open_case(fixture: &Fixture, mode: OpenMode, fault: Option<&ReadFault>) -> Trace {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite");
    fs::copy(&fixture.source, &path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let mut opener = Opener::new(&path, fault);
    let result = opener.open(&path, &options(mode));
    let trace = opener.trace.take().unwrap();
    let database = assert_trace(result, &trace, fault);
    // Count wrapped handles before any reader/retry could conceal a leak.
    assert_eq!(
        opener.registration.control.open_file_count(),
        usize::from(database.is_some())
    );
    if let Some(mut database) = database {
        assert_eq!(pragma(&database.connection, "busy_timeout"), 0);
        assert_eq!(pragma(&database.connection, "foreign_keys"), 1);
        assert_eq!(
            database.connection.is_readonly("main").unwrap(),
            mode == OpenMode::ReadOnly
        );
        assert_eq!(snapshot(&mut database), fixture.replacement);
        assert_integrity(&database);
    }
    assert_eq!(opener.registration.control.open_file_count(), 0);
    assert_source(&path, &bytes, &fixture.replacement);
    assert_files(directory.path(), &["source.sqlite"]);

    // Reopen through the public API with the original access mode and a live
    // path registration. Native I/O must remain isolated from the test VFS.
    let mut retry = Database::open_with_options(&path, &options(mode)).unwrap();
    assert_eq!(snapshot(&mut retry), fixture.replacement);
    let backup = directory.path().join("backup.sqlite");
    retry.backup_to(&backup).unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
    verify_published(
        &backup,
        &fixture.replacement,
        &directory.path().join("restored.sqlite"),
    );
    if mode == OpenMode::ReadOnly {
        assert!(matches!(
            retry.execute("CREATE (:Forbidden)"),
            Err(Error::ReadOnly)
        ));
        drop(retry);
        assert_reopened(&path, &fixture.replacement);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    } else {
        let committed = verify_replacement(&mut retry, &fixture.replacement);
        drop(retry);
        assert_reopened(&path, &committed);
    }
    assert_files(
        directory.path(),
        &["source.sqlite", "backup.sqlite", "restored.sqlite"],
    );
    trace
}

fn restore_case(fixture: &Fixture, fault: Option<&ReadFault>) -> Trace {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.sqlite");
    fs::copy(&fixture.source, &source).unwrap();
    let source_bytes = fs::read(&source).unwrap();
    let destination = directory.path().join("destination.sqlite");
    fs::copy(&fixture.destination, &destination).unwrap();
    let (mut database, registration) = io_vfs::open(&destination);
    configure_destination(&database.connection);
    // Prime statement caches and mutation TEMP state before the failed restore.
    database
        .execute("MATCH (n:Incumbent {key:'kept'}) SET n.key='kept' RETURN n")
        .unwrap();
    assert_eq!(snapshot(&mut database), fixture.original);
    let destination_bytes = fs::read(&destination).unwrap();
    let mut observer = open_existing(&destination);
    assert_eq!(snapshot(&mut observer), fixture.original);
    registration.control.start(None);
    assert_eq!(registration.control.open_file_count(), 1);
    let mut opener = Opener::new(&source, fault);
    let result = database.restore_from_with_io(&source, &mut opener);
    let trace = opener.trace.take().unwrap();
    assert_trace(result, &trace, fault);
    assert_eq!(opener.registration.control.open_file_count(), 0);
    let destination_events = registration.control.stop();
    assert!(
        destination_events
            .iter()
            .all(|e| e.result == ffi::SQLITE_OK)
    );
    assert_eq!(opener.copy_started, fault.is_none());
    assert!(database.connection.is_autocommit());
    if fault.is_some() {
        assert!(
            destination_events.is_empty(),
            "opening fails before any destination write/sync"
        );
        assert_source(&destination, &destination_bytes, &fixture.original);
    } else {
        assert!(
            destination_events
                .iter()
                .any(|e| e.operation == Operation::Write)
        );
    }
    assert_source(&source, &source_bytes, &fixture.replacement);
    assert_files(directory.path(), &["source.sqlite", "destination.sqlite"]);
    if fault.is_some() {
        assert_eq!(snapshot(&mut database), fixture.original);
        assert!(matches!(
            database.execute("CREATE (:Incumbent {key:'kept'})"),
            Err(Error::Constraint(_))
        ));
        assert_competing_writer_can_commit(&mut observer, &fixture.original);
        assert_competing_writer_can_commit(&mut database, &fixture.original);
        // Public retry keeps the caller's original destination handle.
        database.restore_from(&source).unwrap();
    }
    assert!(database.connection.is_autocommit());
    assert_eq!(snapshot(&mut observer), fixture.replacement);
    let reader = read_only(&source);
    let pages = pragma(&reader.connection, "page_count");
    let page_size = pragma(&reader.connection, "page_size");
    assert_eq!(pragma(&database.connection, "page_count"), pages);
    assert_eq!(
        fs::metadata(&destination).unwrap().len(),
        u64::try_from(pages * page_size).unwrap()
    );
    drop(reader);
    let committed = verify_replacement(&mut database, &fixture.replacement);
    drop(observer);
    drop(database);
    assert_eq!(registration.control.open_file_count(), 0);
    assert_reopened(&destination, &committed);
    assert_source(&source, &source_bytes, &fixture.replacement);
    assert_files(directory.path(), &["source.sqlite", "destination.sqlite"]);
    trace
}

fn open_sweep(mode: OpenMode) {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new(false);
    let healthy = open_case(&fixture, mode, None);
    for fault in healthy.faults() {
        eprintln!("open source {mode:?}: {fault:?}");
        open_case(&fixture, mode, Some(&fault));
    }
    eprintln!(
        "open source {mode:?}: {} connection reads, {} initialization reads",
        healthy.initialization_start.unwrap(),
        healthy.events.len() - healthy.initialization_start.unwrap()
    );
}

fn restore_sweep(shrink: bool) {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new(shrink);
    let healthy = restore_case(&fixture, None);
    for fault in healthy.faults() {
        eprintln!("restore source opening shrink={shrink}: {fault:?}");
        restore_case(&fixture, Some(&fault));
    }
    eprintln!(
        "restore source opening shrink={shrink}: {} connection reads, {} initialization reads",
        healthy.initialization_start.unwrap(),
        healthy.events.len() - healthy.initialization_start.unwrap()
    );
}

#[test]
fn open_or_create_source_read_failures_preserve_graph_and_allow_public_retry() {
    open_sweep(OpenMode::OpenOrCreate);
}

#[test]
fn read_write_source_open_read_failures_preserve_graph_and_allow_public_retry() {
    open_sweep(OpenMode::ReadWrite);
}

#[test]
fn read_only_source_open_read_failures_preserve_graph_and_allow_public_retry() {
    open_sweep(OpenMode::ReadOnly);
}

#[test]
fn growing_restore_source_open_read_failures_preserve_destination_and_allow_retry() {
    restore_sweep(false);
}

#[test]
fn shrinking_restore_source_open_read_failures_preserve_destination_and_allow_retry() {
    restore_sweep(true);
}

#[test]
fn source_open_header_read_failure_is_path_scoped_and_consumed_before_initialization() {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new(false);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite");
    let other_path = directory.path().join("other.sqlite");
    fs::copy(&fixture.source, &path).unwrap();
    fs::copy(&fixture.source, &other_path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let registration = io_vfs::register(&path);
    let other = io_vfs::register(&other_path);
    registration.control.start_reads();
    registration.control.fail_read(
        &Event {
            file: FileKind::Database,
            operation: Operation::Read,
            offset: 0,
            amount: 100,
            result: ffi::SQLITE_OK,
        },
        0,
    );
    let mut unrelated =
        Database::open_with_connector(&other_path, &options(OpenMode::ReadOnly), |_, flags| {
            other.connect(flags)
        })
        .unwrap();
    assert_eq!(snapshot(&mut unrelated), fixture.replacement);
    drop(unrelated);
    assert_eq!(snapshot(&mut read_only(&path)), fixture.replacement);
    assert_eq!(
        registration.control.event_count(),
        0,
        "only the registered path through the test VFS is faulted"
    );
    assert_read_error(
        Database::open_with_connector(&path, &options(OpenMode::ReadOnly), |_, flags| {
            registration.connect(flags)
        })
        .map(drop),
    );
    assert_eq!(registration.control.open_file_count(), 0);
    assert_source(&path, &bytes, &fixture.replacement);
    // Do not stop/rearm the controller: retry through the same VFS must work
    // because the first header fault was consumed, not because it was cleared.
    let mut retry =
        Database::open_with_connector(&path, &options(OpenMode::ReadOnly), |_, flags| {
            registration.connect(flags)
        })
        .unwrap();
    assert_eq!(registration.control.open_file_count(), 1);
    assert_eq!(snapshot(&mut retry), fixture.replacement);
    drop(retry);
    assert_eq!(registration.control.open_file_count(), 0);
    assert_eq!(other.control.open_file_count(), 0);
    let events = registration.control.stop();
    assert!(events.len() > 1);
    assert_eq!(events[0].result, ffi::SQLITE_IOERR_READ);
    assert_eq!((events[0].offset, events[0].amount), (0, 100));
    assert!(events[1..].iter().all(|e| e.result == ffi::SQLITE_OK));
    assert_source(&path, &bytes, &fixture.replacement);
    assert_source(&other_path, &bytes, &fixture.replacement);
    assert_files(directory.path(), &["source.sqlite", "other.sqlite"]);
}
