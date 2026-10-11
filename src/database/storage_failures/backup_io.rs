//! Exercise the public backup implementation through its private I/O adapter.
//! SQLite faults affect only the staging file. Final sync/publication failures
//! occur before native I/O; the competing-publisher case uses real hard links.

use super::{
    Snapshot, assert_competing_writer_can_commit, assert_integrity, assert_saved_constraint,
    io_vfs, open_existing, pragma, seed, snapshot,
};
use crate::database::backup::{BackupIo, NativeBackupIo};
use crate::{Database, Error, OpenMode, OpenOptions, Parameters};
use io_vfs::{Event, FileKind, Operation, Registration};
use rusqlite::{Connection, ErrorCode, ffi};
use serde_json::json;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
pub(super) enum SourceKind {
    Writable,
    ReadOnly,
    Memory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Copy(usize),
    Sync,
    Publish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Absent,
    Existing,
    CompetingPublisher,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    Sync,
    Publish,
}

pub(super) struct Fixture {
    _directory: tempfile::TempDir,
    pub(super) source: PathBuf,
    pub(super) expected: Snapshot,
    pub(super) winner: PathBuf,
    pub(super) winner_bytes: Vec<u8>,
    pub(super) winner_graph: Snapshot,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite");
        let mut database = seed(&source);
        database
            .execute_with(
                "CREATE (:Payload {body:$body})",
                &Parameters::from([(
                    "body".into(),
                    json!("x".repeat(
                        usize::try_from(pragma(&database.connection, "page_size") * 4).unwrap()
                    )),
                )]),
            )
            .unwrap();
        let expected = snapshot(&mut database);
        drop(database);
        let winner = directory.path().join("winner.sqlite");
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:Incumbent {key:'winner'})")
            .unwrap();
        database.backup_to(&winner).unwrap();
        Self {
            expected,
            source,
            winner_bytes: fs::read(&winner).unwrap(),
            winner_graph: snapshot(&mut database),
            winner,
            _directory: directory,
        }
    }
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

struct FaultIo<'a> {
    fixture: &'a Fixture,
    failure: Option<Failure>,
    target: Target,
    destination: PathBuf,
    stage: Option<PathBuf>,
    registration: Option<Registration>,
    boundaries: Vec<Boundary>,
}

// A synthetic raw OS code, returned before the selected filesystem operation.
// This checks propagation without simulating an actual device failure/ENOSPC.
const IO_ERROR: i32 = 5;

impl FaultIo<'_> {
    fn assert_target_untouched(&self) {
        if self.target == Target::Existing {
            assert_eq!(
                fs::read(&self.destination).unwrap(),
                self.fixture.winner_bytes
            );
        } else {
            assert!(!self.destination.exists());
        }
    }

    fn assert_complete_staging(&self) {
        let mut staged = read_only(self.stage.as_ref().unwrap());
        assert_eq!(snapshot(&mut staged), self.fixture.expected);
        assert_integrity(&staged);
    }
}

impl BackupIo for FaultIo<'_> {
    fn open_destination(&mut self, path: &Path) -> crate::Result<Connection> {
        assert!(self.stage.is_none(), "one staging connection per backup");
        self.assert_target_untouched();
        self.stage = Some(path.into());
        let (connection, registration) = io_vfs::connect(path);
        connection
            .pragma_update(None, "synchronous", "FULL")
            .unwrap();
        connection
            .pragma_update(None, "cache_spill", false)
            .unwrap();
        assert_eq!(pragma(&connection, "synchronous"), 2);
        assert_eq!(pragma(&connection, "cache_spill"), 0);
        assert_eq!(pragma(&connection, "page_count"), 0);
        let journal: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "delete");
        registration.control.start(match self.failure {
            Some(Failure::Copy(index)) => Some(index),
            _ => None,
        });
        self.registration = Some(registration);
        Ok(connection)
    }

    fn sync(&mut self, file: &fs::File) -> io::Result<()> {
        assert!(self.boundaries.is_empty());
        self.assert_target_untouched();
        self.assert_complete_staging();
        self.boundaries.push(Boundary::Sync);
        if self.failure == Some(Failure::Sync) {
            return Err(io::Error::from_raw_os_error(IO_ERROR));
        }
        NativeBackupIo.sync(file)
    }

    fn publish(&mut self, source: &Path, destination: &Path) -> io::Result<()> {
        assert_eq!(source, self.stage.as_ref().unwrap());
        assert_eq!(destination, self.destination);
        assert_eq!(self.boundaries, [Boundary::Sync]);
        self.assert_target_untouched();
        self.assert_complete_staging();
        self.boundaries.push(Boundary::Publish);
        if self.failure == Some(Failure::Publish) {
            return Err(io::Error::from_raw_os_error(IO_ERROR));
        }
        if self.target == Target::CompetingPublisher {
            // Deterministic interleaving: another publisher claims the name
            // after our copy/sync completed, immediately before our hard link.
            fs::hard_link(&self.fixture.winner, destination).unwrap();
        }
        NativeBackupIo.publish(source, destination)
    }
}

fn assert_copy_failure(error: Error, events: &[Event], index: usize, healthy: &[Event]) {
    let expected = &healthy[index];
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite I/O error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::SystemIoFailure);
    assert_eq!(code.extended_code, expected.operation.error_code());
    assert_eq!(events[..index], healthy[..index]);
    let failed: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.result != ffi::SQLITE_OK)
        .collect();
    let mut target = expected.clone();
    target.result = expected.operation.error_code();
    assert_eq!(
        failed,
        [(index, &target)],
        "one-shot fault with healthy cleanup"
    );
}

fn assert_output_directory(directory: &Path, expected: &[&str]) {
    let mut names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    let mut expected: Vec<_> = expected.iter().map(std::ffi::OsString::from).collect();
    expected.sort();
    assert_eq!(names, expected, "no leaked staging directories or sidecars");
}

pub(super) fn verify_published(path: &Path, expected: &Snapshot, restore_path: &Path) {
    let bytes = fs::read(path).unwrap();
    let mut backup = read_only(path);
    assert_eq!(snapshot(&mut backup), *expected);
    assert_integrity(&backup);
    drop(backup);
    let mut restored = Database::open(restore_path).unwrap();
    restored
        .create_unique_constraint("Discarded", "key")
        .unwrap();
    restored.execute("CREATE (:Discarded {key:1})").unwrap();
    restored.restore_from(path).unwrap();
    assert_eq!(snapshot(&mut restored), *expected);
    assert_integrity(&restored);
    assert_saved_constraint(&mut restored);
    restored
        .execute("MATCH (n:Saved {key:'a'}) SET n.restored=true RETURN n")
        .unwrap();
    let committed = snapshot(&mut restored);
    drop(restored);
    let mut reopened = open_existing(restore_path);
    assert_eq!(snapshot(&mut reopened), committed);
    assert_integrity(&reopened);
    assert_eq!(fs::read(path).unwrap(), bytes);
}

fn run_case(
    fixture: &Fixture,
    kind: SourceKind,
    failure: Option<Failure>,
    target: Target,
    healthy: &[Event],
) -> Vec<Event> {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.sqlite");
    fs::copy(&fixture.source, &source_path).unwrap();
    let mut source = match kind {
        SourceKind::Writable => open_existing(&source_path),
        SourceKind::ReadOnly => read_only(&source_path),
        SourceKind::Memory => {
            let mut memory = Database::in_memory().unwrap();
            memory.restore_from(&source_path).unwrap();
            memory
        }
    };
    source.connection.busy_timeout(Duration::ZERO).unwrap();
    if !matches!(kind, SourceKind::ReadOnly) {
        source
            .execute("MATCH (n:Saved {key:'a'}) SET n.number=1 RETURN n")
            .unwrap();
    }
    assert_eq!(snapshot(&mut source), fixture.expected);
    let source_bytes = fs::read(&source_path).unwrap();
    let mut observer = if matches!(kind, SourceKind::Memory) {
        None
    } else {
        let mut observer = open_existing(&source_path);
        assert_eq!(snapshot(&mut observer), fixture.expected);
        Some(observer)
    };
    let output = directory.path().join("output");
    fs::create_dir(&output).unwrap();
    let destination = output.join("backup.sqlite");
    if target == Target::Existing {
        fs::copy(&fixture.winner, &destination).unwrap();
    }
    let mut io = FaultIo {
        fixture,
        failure,
        target,
        destination: destination.clone(),
        stage: None,
        registration: None,
        boundaries: Vec::new(),
    };
    let result = source.backup_to_with_io(&destination, &mut io);
    let events = io.registration.as_ref().unwrap().control.stop();
    assert!(!events.is_empty(), "must intercept the staged copy's I/O");
    let failed = failure.is_some() || target != Target::Absent;
    if failed {
        let error = result.expect_err("fault or competing destination must reject backup");
        match failure {
            Some(Failure::Copy(index)) => {
                assert_copy_failure(error, &events, index, healthy);
                assert!(
                    io.boundaries.is_empty(),
                    "failed copies cannot reach sync/publication"
                );
            }
            Some(Failure::Sync | Failure::Publish) => {
                let Error::Io(error) = error else {
                    panic!("expected filesystem I/O error, got {error:?}");
                };
                assert_eq!(error.raw_os_error(), Some(IO_ERROR));
                assert_eq!(
                    io.boundaries,
                    if failure == Some(Failure::Sync) {
                        vec![Boundary::Sync]
                    } else {
                        vec![Boundary::Sync, Boundary::Publish]
                    }
                );
            }
            None => {
                assert!(
                    matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::AlreadyExists)
                );
                assert_eq!(io.boundaries, [Boundary::Sync, Boundary::Publish]);
            }
        }
    } else {
        result.unwrap();
        assert_eq!(io.boundaries, [Boundary::Sync, Boundary::Publish]);
    }
    if !matches!(failure, Some(Failure::Copy(_))) {
        assert!(events.iter().all(|event| event.result == ffi::SQLITE_OK));
    }
    let stage = io.stage.as_ref().unwrap();
    assert!(!stage.exists());
    assert!(!stage.parent().unwrap().exists());
    let protected = target != Target::Absent;
    assert_output_directory(
        &output,
        if protected || !failed {
            &["backup.sqlite"]
        } else {
            &[]
        },
    );
    if protected {
        assert_eq!(fs::read(&destination).unwrap(), fixture.winner_bytes);
        assert_eq!(snapshot(&mut read_only(&destination)), fixture.winner_graph);
    } else if failed {
        assert!(!destination.exists());
    }

    // Verify source bytes and another writer BEFORE reusing the backup handle:
    // its failed snapshot must not require a subsequent call to release locks.
    assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
    assert!(source.connection.is_autocommit());
    if let Some(observer) = observer.as_mut() {
        assert_competing_writer_can_commit(observer, &fixture.expected);
    }
    assert_eq!(snapshot(&mut source), fixture.expected);
    assert_integrity(&source);
    assert_eq!(snapshot(&mut read_only(&source_path)), fixture.expected);
    if matches!(kind, SourceKind::ReadOnly) {
        assert!(matches!(
            source.execute("CREATE (:Forbidden)"),
            Err(Error::ReadOnly)
        ));
        assert_saved_constraint(observer.as_mut().unwrap());
    } else {
        assert_competing_writer_can_commit(&mut source, &fixture.expected);
        assert_saved_constraint(&mut source);
    }

    let published = if failed {
        // Existing/published destinations retain their names; retry at a fresh
        // name in those cases. All retries use the SAME source and public API,
        // while the failed staging registration is still alive.
        let retry = if protected {
            output.join("retry.sqlite")
        } else {
            destination.clone()
        };
        let before_retry = fs::read(&source_path).unwrap();
        source.backup_to(&retry).unwrap();
        assert_eq!(fs::read(&source_path).unwrap(), before_retry);
        assert!(source.connection.is_autocommit());
        if let Some(observer) = observer.as_mut() {
            assert_competing_writer_can_commit(observer, &fixture.expected);
        }
        assert_eq!(snapshot(&mut source), fixture.expected);
        retry
    } else {
        destination.clone()
    };
    verify_published(
        &published,
        &fixture.expected,
        &directory.path().join("restored.sqlite"),
    );
    assert_output_directory(
        &output,
        if protected {
            &["backup.sqlite", "retry.sqlite"]
        } else {
            &["backup.sqlite"]
        },
    );
    if protected {
        assert_eq!(fs::read(&destination).unwrap(), fixture.winner_bytes);
    }
    events
}

fn sweep(kind: SourceKind) {
    let fixture = Fixture::new();
    let healthy = run_case(&fixture, kind, None, Target::Absent, &[]);
    for file in [FileKind::Database, FileKind::Journal] {
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
            > 1
    );
    for (index, expected) in healthy.iter().enumerate() {
        for target in [Target::Absent, Target::Existing] {
            eprintln!("backup {kind:?}, {target:?}, fault {index}: {expected:?}");
            run_case(&fixture, kind, Some(Failure::Copy(index)), target, &healthy);
        }
    }
    eprintln!(
        "backup {kind:?}: {} copy write/sync fault points, each with absent/existing destinations",
        healthy.len()
    );
}

#[test]
fn writable_backup_copy_io_failures_clean_staging_and_allow_public_retry() {
    sweep(SourceKind::Writable);
}

#[test]
fn read_only_backup_copy_io_failures_clean_staging_and_allow_public_retry() {
    sweep(SourceKind::ReadOnly);
}

#[test]
fn memory_backup_copy_io_failures_clean_staging_and_allow_public_retry() {
    sweep(SourceKind::Memory);
}

#[test]
fn final_sync_and_publication_failures_preserve_files_and_allow_public_retry() {
    let fixture = Fixture::new();
    for kind in [
        SourceKind::Writable,
        SourceKind::ReadOnly,
        SourceKind::Memory,
    ] {
        for target in [Target::Absent, Target::Existing] {
            for failure in [Failure::Sync, Failure::Publish] {
                eprintln!("backup {kind:?}, {target:?}, {failure:?}");
                run_case(&fixture, kind, Some(failure), target, &[]);
            }
        }
    }
}

#[test]
fn competing_backup_publisher_is_not_overwritten_and_retry_keeps_its_file() {
    let fixture = Fixture::new();
    for kind in [
        SourceKind::Writable,
        SourceKind::ReadOnly,
        SourceKind::Memory,
    ] {
        run_case(&fixture, kind, None, Target::CompetingPublisher, &[]);
    }
}
