//! One-shot source xRead errors during snapshot validation and copying.
//! Opening/initialization finishes before arming; native retries keep the same
//! caller-owned handle. Cold source caches force reads in both phases.

use super::{
    Snapshot, assert_competing_writer_can_commit, assert_integrity, assert_saved_constraint,
    io_vfs, open_existing, pragma, seed, snapshot,
};
use crate::database::backup::{BackupIo, NativeBackupIo};
use crate::{Database, Error, OpenMode, OpenOptions, Parameters};
use io_vfs::{Control, Event, FileKind, Operation, Registration};
use rusqlite::{Connection, ErrorCode, OpenFlags, ffi};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

pub(super) fn source_cache_guard() -> MutexGuard<'static, ()> {
    // Bundled SQLite enables MEMORY_MANAGEMENT, which shares the page-cache
    // eviction group across connections even with PRIVATE_CACHE. One test's
    // shrink_memory can therefore add reads to another test's healthy trace.
    // Serialize these deliberate cache resets and the operations they measure;
    // opening read sweeps also join this guard to keep their traces stable.
    // Ordinary database/VFS tests continue to run in parallel.
    static CACHE_RESETS: Mutex<()> = Mutex::new(());
    CACHE_RESETS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

pub(super) struct Fixture {
    _directory: tempfile::TempDir,
    pub(super) source: PathBuf,
    pub(super) destination: PathBuf,
    pub(super) replacement: Snapshot,
    pub(super) original: Snapshot,
}

impl Fixture {
    pub(super) fn new(shrink: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let mut replacement = seed(&source);
        add_payload(&mut replacement, 4);
        let destination = directory.path().join("destination.sqlite");
        let mut original = Database::open(&destination).unwrap();
        original
            .create_unique_constraint("Incumbent", "key")
            .unwrap();
        original
            .execute("CREATE (:Incumbent {key:'kept'})")
            .unwrap();
        if shrink {
            add_payload(&mut original, 12);
        }
        let original_pages = pragma(&original.connection, "page_count");
        let replacement_pages = pragma(&replacement.connection, "page_count");
        assert_eq!(original_pages > replacement_pages, shrink);
        assert_ne!(original_pages, replacement_pages);
        Self {
            source,
            destination,
            replacement: snapshot(&mut replacement),
            original: snapshot(&mut original),
            _directory: directory,
        }
    }
}

fn add_payload(database: &mut Database, pages: i64) {
    let size = usize::try_from(pragma(&database.connection, "page_size") * pages).unwrap();
    database
        .execute_with(
            "CREATE (:Payload {body:$body})",
            &Parameters::from([("body".into(), json!("x".repeat(size)))]),
        )
        .unwrap();
}

pub(super) fn read_only(path: &Path) -> Database {
    Database::open_with_options(
        path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap()
}

fn source_connection(path: &Path, mode: OpenMode) -> (Database, Registration) {
    let flags = match mode {
        OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        OpenMode::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
        OpenMode::OpenOrCreate => unreachable!(),
    };
    let (mut connection, registration) = io_vfs::connect_with_flags(path, flags);
    connection.busy_timeout(Duration::ZERO).unwrap();
    // Use the public opener's initialization and function registration. Opening
    // errors are outside this sweep; only the following snapshot is faulted.
    crate::database::initialize_with_mode(&mut connection, mode).unwrap();
    crate::database::register_functions(&connection).unwrap();
    assert_eq!(
        connection.is_readonly("main").unwrap(),
        mode == OpenMode::ReadOnly
    );
    (Database { connection }, registration)
}

fn cold_source(database: &Database) {
    let connection = &database.connection;
    connection.pragma_update(None, "cache_size", 128).unwrap();
    connection.execute_batch("PRAGMA shrink_memory").unwrap();
    assert_eq!(pragma(connection, "cache_size"), 128);
    assert!(pragma(connection, "page_count") < 128);
    // The page-count check may read a page; discard it too before recording.
    connection.execute_batch("PRAGMA shrink_memory").unwrap();
}

pub(super) fn configure_destination(connection: &Connection) {
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
        .pragma_update(None, "synchronous", "FULL")
        .unwrap();
    connection
        .pragma_update(None, "cache_spill", false)
        .unwrap();
    let journal: String = connection
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    assert_eq!(pragma(connection, "synchronous"), 2);
    assert_eq!(pragma(connection, "cache_spill"), 0);
}

#[derive(Debug)]
struct Trace {
    events: Vec<Event>,
    copy_start: Option<usize>,
    destination_writes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Validation,
    Copy,
}

#[derive(Debug)]
struct ReadFault {
    phase: Phase,
    event: Event,
    occurrence: usize,
}

impl Trace {
    fn faults(&self) -> Vec<ReadFault> {
        let start = self.copy_start.unwrap();
        self.events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let (phase, first) = if index < start {
                    (Phase::Validation, 0)
                } else {
                    (Phase::Copy, start)
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

struct ReadIo<'a> {
    fault: Option<&'a ReadFault>,
    source: Option<Registration>,
    copy_start: Option<usize>,
    stage: Option<PathBuf>,
    destination: Option<Registration>,
    finished: bool,
}

impl<'a> ReadIo<'a> {
    fn new(fault: Option<&'a ReadFault>, source: Option<Registration>) -> Self {
        Self {
            fault,
            source,
            copy_start: None,
            stage: None,
            destination: None,
            finished: false,
        }
    }

    fn control(&self) -> &Control {
        &self.source.as_ref().unwrap().control
    }

    fn arm(&self, phase: Phase) {
        if let Some(fault) = self.fault.filter(|f| f.phase == phase) {
            self.control().fail_read(&fault.event, fault.occurrence);
        }
    }

    fn trace(&self) -> Trace {
        Trace {
            events: self.control().stop(),
            copy_start: self.copy_start,
            destination_writes: 0,
        }
    }
}

impl BackupIo for ReadIo<'_> {
    fn open_source(&mut self, path: &Path, options: &OpenOptions) -> crate::Result<Database> {
        assert_eq!(options.mode, OpenMode::ReadOnly);
        assert_eq!(options.busy_timeout, Duration::ZERO);
        assert!(self.source.is_none());
        let (database, registration) = source_connection(path, options.mode);
        cold_source(&database);
        registration.control.start_reads();
        self.source = Some(registration);
        self.arm(Phase::Validation);
        Ok(database)
    }

    fn open_destination(&mut self, path: &Path) -> crate::Result<Connection> {
        self.stage = Some(path.into());
        let (connection, registration) = io_vfs::connect(path);
        configure_destination(&connection);
        registration.control.start(None);
        self.destination = Some(registration);
        Ok(connection)
    }

    fn copy_snapshot(
        &mut self,
        source: &Connection,
        destination: &mut Connection,
    ) -> crate::Result<()> {
        // Discard unpinned validation pages without ending the read snapshot.
        // This forces disk reads during copy even after a full validation scan.
        source.execute_batch("PRAGMA shrink_memory")?;
        self.copy_start = Some(self.control().event_count());
        self.arm(Phase::Copy);
        NativeBackupIo.copy_snapshot(source, destination)
    }

    fn sync(&mut self, file: &fs::File) -> std::io::Result<()> {
        self.finished = true;
        NativeBackupIo.sync(file)
    }
}

fn assert_trace(result: crate::Result<()>, trace: &Trace, fault: Option<&ReadFault>) {
    assert!(!trace.events.is_empty(), "must observe actual source reads");
    assert!(
        trace
            .events
            .iter()
            .all(|e| e.file == FileKind::Database && e.operation == Operation::Read)
    );
    if let Some(fault) = fault {
        let error = result.expect_err("source read fault must reject the operation");
        match error {
            Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) => {
                assert_eq!(code.code, ErrorCode::SystemIoFailure);
                assert_eq!(code.extended_code, ffi::SQLITE_IOERR_READ);
            }
            Error::Data(message) => {
                // SQLite's b-tree integrity checker embeds some read errors in
                // result rows instead of returning them from sqlite3_step.
                // Preserve its diagnostic, without parsing text into an error
                // code in production or allowing arbitrary validation errors.
                assert_eq!(fault.phase, Phase::Validation);
                assert!(message.starts_with("backup integrity check failed: "));
                assert!(
                    message.contains("unable to get the page. error code=266")
                        || message.contains("failed to get page"),
                    "unexpected integrity diagnostic: {message}"
                );
            }
            error => panic!("expected source read error or integrity diagnostic, got {error:?}"),
        }
        let mut expected = fault.event.clone();
        expected.result = ffi::SQLITE_IOERR_READ;
        let failed: Vec<_> = trace
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| e.result != ffi::SQLITE_OK)
            .collect();
        assert_eq!(failed.len(), 1, "one-shot error with healthy cleanup");
        let (index, event) = failed[0];
        assert_eq!(*event, expected);
        let first = if fault.phase == Phase::Validation {
            assert_eq!(
                trace.copy_start, None,
                "validation failure must prevent copy"
            );
            0
        } else {
            let start = trace.copy_start.unwrap();
            assert!(start > 0);
            assert!(index >= start);
            start
        };
        assert_eq!(
            trace.events[first..index]
                .iter()
                .filter(|e| **e == fault.event)
                .count(),
            fault.occurrence,
            "must fail the selected occurrence of the exact byte range in this phase"
        );
    } else {
        result.unwrap();
        assert!(trace.events.iter().all(|e| e.result == ffi::SQLITE_OK));
        let start = trace.copy_start.unwrap();
        assert!(start > 0, "validation must read the source");
        assert!(
            trace.events.len() > start + 1,
            "copy must read multiple uncached source pages"
        );
    }
}

pub(super) fn assert_source(path: &Path, bytes: &[u8], expected: &Snapshot) {
    assert_eq!(fs::read(path).unwrap(), bytes);
    // Check lock release before reusing the failed caller or opening a reader.
    let probe = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    probe.busy_timeout(Duration::ZERO).unwrap();
    probe.execute_batch("BEGIN EXCLUSIVE; COMMIT").unwrap();
    drop(probe);
    let mut reader = read_only(path);
    assert_eq!(snapshot(&mut reader), *expected);
    assert_integrity(&reader);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

pub(super) fn assert_files(directory: &Path, expected: &[&str]) {
    let mut actual: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    actual.sort();
    let mut expected: Vec<_> = expected.iter().map(std::ffi::OsString::from).collect();
    expected.sort();
    assert_eq!(actual, expected, "no staging files or sidecars left behind");
}

pub(super) fn verify_replacement(database: &mut Database, expected: &Snapshot) -> Snapshot {
    assert_eq!(snapshot(database), *expected);
    assert_integrity(database);
    assert_saved_constraint(database);
    database
        .execute("MATCH (n:Saved {key:'a'}) SET n.restored=true RETURN n")
        .unwrap();
    snapshot(database)
}

pub(super) fn assert_reopened(path: &Path, expected: &Snapshot) {
    let mut reopened = read_only(path);
    assert_eq!(snapshot(&mut reopened), *expected);
    assert_integrity(&reopened);
}

fn backup_case(
    fixture: &Fixture,
    mode: OpenMode,
    existing: bool,
    fault: Option<&ReadFault>,
) -> Trace {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.sqlite");
    fs::copy(&fixture.source, &source_path).unwrap();
    let source_bytes = fs::read(&source_path).unwrap();
    let (mut source, registration) = source_connection(&source_path, mode);
    assert_eq!(snapshot(&mut source), fixture.replacement);
    cold_source(&source);
    let output = directory.path().join("output");
    fs::create_dir(&output).unwrap();
    let destination = output.join("backup.sqlite");
    let protected_bytes = fs::read(&fixture.destination).unwrap();
    if existing {
        fs::copy(&fixture.destination, &destination).unwrap();
    }
    let mut io = ReadIo::new(fault, Some(registration));
    io.control().start_reads();
    io.arm(Phase::Validation);
    let result = source.backup_to_with_io(&destination, &mut io);
    let mut trace = io.trace();
    assert_trace(result, &trace, fault);
    if let Some(registration) = &io.destination {
        let events = registration.control.stop();
        assert!(events.iter().all(|e| e.result == ffi::SQLITE_OK));
        trace.destination_writes = events
            .iter()
            .filter(|e| e.operation == Operation::Write)
            .count();
    }
    if let Some(stage) = &io.stage {
        assert!(!stage.exists());
        assert!(!stage.parent().unwrap().exists());
    }
    assert_eq!(io.stage.is_some(), trace.copy_start.is_some());
    assert_eq!(io.finished, fault.is_none());
    assert_files(
        &output,
        if existing || fault.is_none() {
            &["backup.sqlite"]
        } else {
            &[]
        },
    );
    if existing {
        assert_eq!(fs::read(&destination).unwrap(), protected_bytes);
        assert_eq!(snapshot(&mut read_only(&destination)), fixture.original);
    }
    assert!(source.connection.is_autocommit());
    assert_source(&source_path, &source_bytes, &fixture.replacement);
    assert_eq!(snapshot(&mut source), fixture.replacement);
    assert_integrity(&source);
    if mode == OpenMode::ReadOnly {
        assert!(matches!(
            source.execute("CREATE (:Forbidden)"),
            Err(Error::ReadOnly)
        ));
    } else {
        assert_competing_writer_can_commit(&mut source, &fixture.replacement);
        assert_saved_constraint(&mut source);
    }
    let published = if fault.is_some() {
        let retry = if existing {
            output.join("retry.sqlite")
        } else {
            destination.clone()
        };
        let before = fs::read(&source_path).unwrap();
        // Public retry, same handle and live registration, with the consumed
        // fault cleared. Continue observing reads to prove native success.
        io.control().start_reads();
        source.backup_to(&retry).unwrap();
        assert!(source.connection.is_autocommit());
        assert!(
            io.control()
                .stop()
                .iter()
                .all(|e| e.result == ffi::SQLITE_OK)
        );
        assert_source(&source_path, &before, &fixture.replacement);
        retry
    } else {
        destination.clone()
    };
    assert_eq!(snapshot(&mut read_only(&published)), fixture.replacement);
    let published_bytes = fs::read(&published).unwrap();
    let restored_path = directory.path().join("restored.sqlite");
    fs::copy(&fixture.destination, &restored_path).unwrap();
    let mut restored = open_existing(&restored_path);
    restored.restore_from(&published).unwrap();
    let committed = verify_replacement(&mut restored, &fixture.replacement);
    drop(restored);
    assert_reopened(&restored_path, &committed);
    assert_eq!(fs::read(&published).unwrap(), published_bytes);
    assert_files(
        &output,
        if existing {
            &["backup.sqlite", "retry.sqlite"]
        } else {
            &["backup.sqlite"]
        },
    );
    if existing {
        assert_eq!(fs::read(&destination).unwrap(), protected_bytes);
    }
    trace
}

fn restore_case(fixture: &Fixture, fault: Option<&ReadFault>) -> Trace {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.sqlite");
    fs::copy(&fixture.source, &source_path).unwrap();
    let source_bytes = fs::read(&source_path).unwrap();
    let path = directory.path().join("destination.sqlite");
    fs::copy(&fixture.destination, &path).unwrap();
    let (mut database, destination_registration) = io_vfs::open(&path);
    configure_destination(&database.connection);
    database
        .execute("MATCH (n:Incumbent {key:'kept'}) SET n.key='kept' RETURN n")
        .unwrap();
    assert_eq!(snapshot(&mut database), fixture.original);
    let before = fs::read(&path).unwrap();
    let mut observer = open_existing(&path);
    assert_eq!(snapshot(&mut observer), fixture.original);
    destination_registration.control.start(None);
    let mut io = ReadIo::new(fault, None);
    let result = database.restore_from_with_io(&source_path, &mut io);
    let mut trace = io.trace();
    assert_trace(result, &trace, fault);
    let destination_events = destination_registration.control.stop();
    trace.destination_writes = destination_events
        .iter()
        .filter(|e| e.operation == Operation::Write)
        .count();
    assert!(
        destination_events
            .iter()
            .all(|e| e.result == ffi::SQLITE_OK)
    );
    assert!(database.connection.is_autocommit());
    if fault.is_some() {
        // Byte equality BEFORE any reuse or reopen could recover the file.
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    assert_source(&source_path, &source_bytes, &fixture.replacement);
    if fault.is_some() {
        if trace.copy_start.is_none() {
            assert!(destination_events.is_empty());
        }
        assert_competing_writer_can_commit(&mut observer, &fixture.original);
        assert_eq!(snapshot(&mut database), fixture.original);
        assert_integrity(&database);
        assert!(matches!(
            database.execute("CREATE (:Incumbent {key:'kept'})"),
            Err(Error::Constraint(_))
        ));
        assert_eq!(snapshot(&mut read_only(&path)), fixture.original);
        assert_competing_writer_can_commit(&mut database, &fixture.original);
        // Retry through the public method, which reopens the source normally.
        database.restore_from(&source_path).unwrap();
    }
    assert!(database.connection.is_autocommit());
    assert_eq!(snapshot(&mut observer), fixture.replacement);
    let source = read_only(&source_path);
    let pages = pragma(&source.connection, "page_count");
    let page_size = pragma(&source.connection, "page_size");
    assert_eq!(pragma(&database.connection, "page_count"), pages);
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        u64::try_from(pages * page_size).unwrap()
    );
    drop(source);
    let committed = verify_replacement(&mut database, &fixture.replacement);
    drop(observer);
    drop(database);
    assert_reopened(&path, &committed);
    assert_source(&source_path, &source_bytes, &fixture.replacement);
    assert_files(directory.path(), &["destination.sqlite", "source.sqlite"]);
    trace
}

fn backup_sweep(mode: OpenMode) {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new(false);
    let healthy = backup_case(&fixture, mode, false, None);
    let mut after_writes = 0;
    for fault in healthy.faults() {
        for existing in [false, true] {
            eprintln!("backup source {mode:?}, existing={existing}, {fault:?}");
            let trace = backup_case(&fixture, mode, existing, Some(&fault));
            if fault.phase == Phase::Copy && trace.destination_writes > 0 {
                after_writes += 1;
            }
        }
    }
    assert!(
        after_writes > 0,
        "must fail source reads after staging writes have started"
    );
    eprintln!(
        "backup source {mode:?}: {} validation reads, {} copy reads, two target states",
        healthy.copy_start.unwrap(),
        healthy.events.len() - healthy.copy_start.unwrap()
    );
}

fn restore_sweep(shrink: bool) {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new(shrink);
    let healthy = restore_case(&fixture, None);
    let mut after_writes = 0;
    for fault in healthy.faults() {
        eprintln!("restore source shrink={shrink}, {fault:?}");
        let trace = restore_case(&fixture, Some(&fault));
        if fault.phase == Phase::Copy && trace.destination_writes > 0 {
            after_writes += 1;
        }
    }
    assert!(
        after_writes > 0,
        "must fail source reads after replacement writes have started"
    );
    eprintln!(
        "restore source shrink={shrink}: {} validation reads, {} copy reads",
        healthy.copy_start.unwrap(),
        healthy.events.len() - healthy.copy_start.unwrap()
    );
}

#[test]
fn writable_backup_source_read_failures_clean_staging_and_allow_retry() {
    backup_sweep(OpenMode::ReadWrite);
}

#[test]
fn read_only_backup_source_read_failures_clean_staging_and_allow_retry() {
    backup_sweep(OpenMode::ReadOnly);
}

#[test]
fn growing_restore_source_read_failures_preserve_destination_and_allow_retry() {
    restore_sweep(false);
}

#[test]
fn shrinking_restore_source_read_failures_preserve_destination_and_allow_retry() {
    restore_sweep(true);
}

#[test]
fn source_read_faults_are_path_scoped_and_consumed_once() {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new(false);
    let directory = tempfile::tempdir().unwrap();
    let first_path = directory.path().join("first.sqlite");
    let second_path = directory.path().join("second.sqlite");
    fs::copy(&fixture.source, &first_path).unwrap();
    fs::copy(&fixture.source, &second_path).unwrap();
    let bytes = fs::read(&fixture.source).unwrap();
    let (first, first_registration) = source_connection(&first_path, OpenMode::ReadOnly);
    let (second, second_registration) = source_connection(&second_path, OpenMode::ReadOnly);
    let read = |database: &Database| -> rusqlite::Result<i64> {
        database
            .connection
            .query_row("SELECT count(*) FROM main.nodes", [], |r| r.get(0))
    };
    cold_source(&first);
    first_registration.control.start_reads();
    let count = read(&first).unwrap();
    let healthy = first_registration.control.stop();
    let target = &healthy[0];
    cold_source(&first);
    cold_source(&second);
    first_registration.control.start_reads();
    first_registration.control.fail_read(target, 0);
    second_registration.control.start_reads();
    // The same byte range on a different registered database is unaffected.
    assert_eq!(read(&second).unwrap(), count);
    let error = read(&first).unwrap_err();
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.extended_code == ffi::SQLITE_IOERR_READ)
    );
    first
        .connection
        .execute_batch("PRAGMA shrink_memory")
        .unwrap();
    // No stop, restart, or rearming between the error and this healthy read.
    assert_eq!(read(&first).unwrap(), count);
    let events = first_registration.control.stop();
    let failed: Vec<_> = events
        .iter()
        .filter(|e| e.result != ffi::SQLITE_OK)
        .collect();
    let mut expected = target.clone();
    expected.result = ffi::SQLITE_IOERR_READ;
    assert_eq!(failed, [&expected]);
    assert!(
        events.contains(target),
        "the failed byte range must subsequently read successfully"
    );
    let other = second_registration.control.stop();
    assert!(!other.is_empty());
    assert!(other.iter().all(|e| e.result == ffi::SQLITE_OK));
    assert_source(&first_path, &bytes, &fixture.replacement);
    assert_source(&second_path, &bytes, &fixture.replacement);
}
