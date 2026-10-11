//! Staging lifecycle boundaries, with native filesystem operations between
//! injected failures. Cleanup failures skip removal but still close handles.
//! This does not simulate OS descriptor-close errors or a full filesystem.

use super::{
    assert_competing_writer_can_commit, assert_integrity, assert_saved_constraint,
    backup_io::{Fixture, SourceKind, read_only, verify_published},
    io_vfs, open_existing, snapshot,
};
use crate::database::backup::{BackupIo, NativeBackupIo};
use crate::{Database, Error, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, ffi};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

const SOURCES: [SourceKind; 3] = [
    SourceKind::Writable,
    SourceKind::ReadOnly,
    SourceKind::Memory,
];
// Synthetic pre-call filesystem errors, deliberately distinct from cleanup.
const PRIMARY_IO_ERROR: i32 = 5;
const CLEANUP_IO_ERROR: i32 = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Directory,
    File,
    Open,
    Copy,
    Close,
    Sync,
    Publish,
    CleanupFile,
    CleanupDirectory,
}

#[derive(Clone, Copy, Debug)]
enum Cleanup {
    Healthy,
    File,
    Directory,
    Both,
}

impl Cleanup {
    fn file_fails(self) -> bool {
        matches!(self, Self::File | Self::Both)
    }
    fn directory_fails(self) -> bool {
        matches!(self, Self::Directory | Self::Both)
    }
}

fn sqlite_error(code: i32, message: &str) -> Error {
    rusqlite::Error::SqliteFailure(ffi::Error::new(code), Some(message.into())).into()
}

struct LifecycleIo<'a> {
    fixture: &'a Fixture,
    destination: PathBuf,
    existing: bool,
    failure: Option<Step>,
    cleanup: Cleanup,
    panic_at: Option<Step>,
    steps: Vec<Step>,
    directory: Option<PathBuf>,
    file: Option<PathBuf>,
    registration: Option<io_vfs::Registration>,
    copied: bool,
    published: bool,
}

impl LifecycleIo<'_> {
    fn step(&mut self, step: Step) {
        self.steps.push(step);
        if self.panic_at == Some(step) {
            panic!("staging panic at {step:?}");
        }
    }

    fn assert_unpublished(&self) {
        assert!(!self.published);
        if self.existing {
            assert_eq!(
                fs::read(&self.destination).unwrap(),
                self.fixture.winner_bytes
            );
        } else {
            assert!(!self.destination.exists());
        }
    }

    fn assert_complete_stage(&self) {
        assert!(self.copied);
        let mut reader = read_only(self.file.as_ref().unwrap());
        assert_eq!(snapshot(&mut reader), self.fixture.expected);
        assert_integrity(&reader);
    }
}

impl BackupIo for LifecycleIo<'_> {
    fn create_directory(&mut self, parent: &Path) -> io::Result<tempfile::TempDir> {
        self.step(Step::Directory);
        self.assert_unpublished();
        assert_eq!(parent, self.destination.parent().unwrap());
        if self.failure == Some(Step::Directory) {
            return Err(io::Error::from_raw_os_error(PRIMARY_IO_ERROR));
        }
        let directory = NativeBackupIo.create_directory(parent)?;
        self.directory = Some(directory.path().into());
        Ok(directory)
    }

    fn create_file(&mut self, directory: &Path) -> io::Result<tempfile::NamedTempFile> {
        self.step(Step::File);
        self.assert_unpublished();
        assert_eq!(directory, self.directory.as_ref().unwrap());
        if self.failure == Some(Step::File) {
            return Err(io::Error::from_raw_os_error(PRIMARY_IO_ERROR));
        }
        let file = NativeBackupIo.create_file(directory)?;
        self.file = Some(file.path().into());
        Ok(file)
    }

    fn open_destination(&mut self, path: &Path) -> Result<Connection> {
        self.step(Step::Open);
        self.assert_unpublished();
        assert_eq!(path, self.file.as_ref().unwrap());
        if self.failure == Some(Step::Open) {
            return Err(sqlite_error(ffi::SQLITE_CANTOPEN, "staging open fault"));
        }
        let (connection, registration) = io_vfs::connect(path);
        connection
            .pragma_update(None, "synchronous", "FULL")
            .unwrap();
        connection
            .pragma_update(None, "cache_spill", false)
            .unwrap();
        self.registration = Some(registration);
        Ok(connection)
    }

    fn copy_snapshot(&mut self, source: &Connection, destination: &mut Connection) -> Result<()> {
        self.step(Step::Copy);
        self.assert_unpublished();
        // A real SQLite copy failure through the forwarding VFS, so error
        // precedence also covers cleanup after a partially attempted copy.
        self.registration
            .as_ref()
            .unwrap()
            .control
            .start((self.failure == Some(Step::Copy)).then_some(0));
        NativeBackupIo.copy_snapshot(source, destination)?;
        self.copied = true;
        Ok(())
    }

    fn close_destination(&mut self, connection: Connection) -> Result<()> {
        self.step(Step::Close);
        self.assert_unpublished();
        self.assert_complete_stage();
        if self.failure == Some(Step::Close) {
            // Inject an API-boundary error before explicit close. The owned
            // connection's Drop still performs a healthy native close.
            return Err(sqlite_error(ffi::SQLITE_IOERR_CLOSE, "staging close fault"));
        }
        NativeBackupIo.close_destination(connection)
    }

    fn sync(&mut self, file: &fs::File) -> io::Result<()> {
        self.step(Step::Sync);
        self.assert_unpublished();
        self.assert_complete_stage();
        if self.failure == Some(Step::Sync) {
            return Err(io::Error::from_raw_os_error(PRIMARY_IO_ERROR));
        }
        NativeBackupIo.sync(file)
    }

    fn publish(&mut self, source: &Path, destination: &Path) -> io::Result<()> {
        self.step(Step::Publish);
        self.assert_unpublished();
        self.assert_complete_stage();
        assert_eq!(source, self.file.as_ref().unwrap());
        assert_eq!(destination, self.destination);
        if self.failure == Some(Step::Publish) {
            return Err(io::Error::from_raw_os_error(PRIMARY_IO_ERROR));
        }
        NativeBackupIo.publish(source, destination)?;
        self.published = true;
        Ok(())
    }

    fn cleanup_file(&mut self, mut file: tempfile::NamedTempFile) -> io::Result<()> {
        self.step(Step::CleanupFile);
        assert_eq!(file.path(), self.file.as_ref().unwrap());
        if self.copied {
            self.assert_complete_stage();
            // No data changes: the staging snapshot's locks must be released
            // before filesystem cleanup, including on the close-error path.
            assert_unlocked(file.path());
        }
        // An inert sidecar marker proves recursive cleanup covers more than
        // the main file. It is not a simulated hot SQLite journal.
        fs::write(
            self.directory.as_ref().unwrap().join("sidecar-marker"),
            b"staging sidecar",
        )?;
        if self.cleanup.file_fails() {
            file.disable_cleanup(true);
            return Err(io::Error::from_raw_os_error(CLEANUP_IO_ERROR));
        }
        NativeBackupIo.cleanup_file(file)
    }

    fn cleanup_directory(&mut self, mut directory: tempfile::TempDir) -> io::Result<()> {
        self.step(Step::CleanupDirectory);
        assert_eq!(directory.path(), self.directory.as_ref().unwrap());
        if self.cleanup.directory_fails() {
            directory.disable_cleanup(true);
            return Err(io::Error::from_raw_os_error(CLEANUP_IO_ERROR));
        }
        NativeBackupIo.cleanup_directory(directory)
    }
}

fn assert_unlocked(path: &Path) {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection.execute_batch("BEGIN EXCLUSIVE; COMMIT").unwrap();
}

fn expected_steps(failure: Option<Step>) -> Vec<Step> {
    let mut steps = Vec::new();
    for step in [
        Step::Directory,
        Step::File,
        Step::Open,
        Step::Copy,
        Step::Close,
        Step::Sync,
        Step::Publish,
    ] {
        steps.push(step);
        if failure == Some(step) {
            break;
        }
    }
    if !matches!(failure, Some(Step::Directory | Step::File)) {
        steps.push(Step::CleanupFile);
    }
    if failure != Some(Step::Directory) {
        steps.push(Step::CleanupDirectory);
    }
    steps
}

fn assert_result(result: Result<()>, failure: Option<Step>, existing: bool) {
    match failure {
        Some(Step::Open | Step::Copy | Step::Close) => {
            let error = result.unwrap_err();
            let Error::Sqlite(rusqlite::Error::SqliteFailure(code, message)) = error else {
                panic!("expected primary SQLite error, got {error:?}");
            };
            let (extended, primary, diagnostic) = match failure.unwrap() {
                Step::Open => (
                    ffi::SQLITE_CANTOPEN,
                    ErrorCode::CannotOpen,
                    Some("staging open fault"),
                ),
                Step::Copy => (ffi::SQLITE_IOERR_WRITE, ErrorCode::SystemIoFailure, None),
                Step::Close => (
                    ffi::SQLITE_IOERR_CLOSE,
                    ErrorCode::SystemIoFailure,
                    Some("staging close fault"),
                ),
                _ => unreachable!(),
            };
            assert_eq!(code.code, primary);
            assert_eq!(code.extended_code, extended);
            if let Some(diagnostic) = diagnostic {
                assert_eq!(message.as_deref(), Some(diagnostic));
            }
        }
        Some(Step::Directory | Step::File | Step::Sync | Step::Publish) => {
            let error = result.unwrap_err();
            let Error::Io(error) = error else {
                panic!("expected primary filesystem error, got {error:?}");
            };
            assert_eq!(error.raw_os_error(), Some(PRIMARY_IO_ERROR));
        }
        None if existing => {
            assert!(
                matches!(result, Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists)
            );
        }
        None => result.unwrap(),
        _ => unreachable!(),
    }
}

fn remaining_files(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    if !directory.exists() {
        return Vec::new();
    }
    let mut files: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let contents = fs::read(&path).unwrap();
            (path, contents)
        })
        .collect();
    files.sort();
    files
}

fn run_case(
    fixture: &Fixture,
    kind: SourceKind,
    existing: bool,
    failure: Option<Step>,
    cleanup: Cleanup,
) {
    eprintln!("staging {kind:?}, existing={existing}, {failure:?}, {cleanup:?}");
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join("source.sqlite");
    fs::copy(&fixture.source, &source_path).unwrap();
    let mut source = match kind {
        SourceKind::Writable => open_existing(&source_path),
        SourceKind::ReadOnly => read_only(&source_path),
        SourceKind::Memory => {
            let mut source = Database::in_memory().unwrap();
            source.restore_from(&source_path).unwrap();
            source
        }
    };
    source.connection.busy_timeout(Duration::ZERO).unwrap();
    assert_eq!(snapshot(&mut source), fixture.expected);
    let source_bytes = fs::read(&source_path).unwrap();
    let output = directory.path().join("output");
    fs::create_dir(&output).unwrap();
    let destination = output.join("backup.sqlite");
    if existing {
        fs::copy(&fixture.winner, &destination).unwrap();
    }
    let mut io = LifecycleIo {
        fixture,
        destination: destination.clone(),
        existing,
        failure,
        cleanup,
        panic_at: None,
        steps: Vec::new(),
        directory: None,
        file: None,
        registration: None,
        copied: false,
        published: false,
    };
    let result = source.backup_to_with_io(&destination, &mut io);
    assert_result(result, failure, existing);
    assert_eq!(io.steps, expected_steps(failure));
    if let Some(registration) = &io.registration {
        let events = registration.control.stop();
        let failed: Vec<_> = events
            .iter()
            .filter(|e| e.result != ffi::SQLITE_OK)
            .collect();
        if failure == Some(Step::Copy) {
            assert_eq!(failed.len(), 1);
            assert_eq!(failed[0].operation, io_vfs::Operation::Write);
            assert_eq!(failed[0].result, ffi::SQLITE_IOERR_WRITE);
        } else {
            assert!(failed.is_empty());
        }
    }
    let published = failure.is_none() && !existing;
    assert_eq!(io.published, published);
    if existing {
        assert_eq!(fs::read(&destination).unwrap(), fixture.winner_bytes);
        assert_eq!(snapshot(&mut read_only(&destination)), fixture.winner_graph);
    } else if !published {
        assert!(!destination.exists());
    }
    let leftover = io.directory.as_ref().filter(|_| cleanup.directory_fails());
    if let Some(stage) = &io.directory {
        assert_eq!(stage.exists(), leftover.is_some());
    }
    let remnants = io
        .directory
        .as_ref()
        .map(|p| remaining_files(p))
        .unwrap_or_default();
    if let Some(file) = &io.file {
        assert_eq!(file.exists(), leftover.is_some() && cleanup.file_fails());
        let marker = file.parent().unwrap().join("sidecar-marker");
        assert_eq!(marker.exists(), leftover.is_some());
        assert_eq!(
            remnants.len(),
            if leftover.is_some() {
                1 + usize::from(cleanup.file_fails())
            } else {
                0
            }
        );
    } else {
        assert!(remnants.is_empty());
    }

    // Before original-handle reuse: unchanged bytes, no source snapshot lock,
    // and a fully usable typed graph on both the original and native readers.
    assert!(source.connection.is_autocommit());
    assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
    if !matches!(kind, SourceKind::Memory) {
        assert_unlocked(&source_path);
    }
    assert_eq!(snapshot(&mut source), fixture.expected);
    assert_integrity(&source);
    if matches!(kind, SourceKind::ReadOnly) {
        assert!(matches!(
            source.execute("CREATE (:Forbidden)"),
            Err(Error::ReadOnly)
        ));
    } else {
        assert_competing_writer_can_commit(&mut source, &fixture.expected);
        assert_saved_constraint(&mut source);
    }
    if published {
        verify_published(
            &destination,
            &fixture.expected,
            &directory.path().join("first-restore.sqlite"),
        );
        let bytes = fs::read(&destination).unwrap();
        assert_result(source.backup_to(&destination), None, true);
        assert_eq!(fs::read(&destination).unwrap(), bytes);
    }
    let retry = if published || existing {
        output.join("retry.sqlite")
    } else {
        destination.clone()
    };
    let before_retry = fs::read(&source_path).unwrap();
    source.backup_to(&retry).unwrap();
    assert_eq!(fs::read(&source_path).unwrap(), before_retry);
    assert!(source.connection.is_autocommit());
    if !matches!(kind, SourceKind::Memory) {
        assert_unlocked(&source_path);
    }
    verify_published(
        &retry,
        &fixture.expected,
        &directory.path().join("retry-restore.sqlite"),
    );
    if existing {
        assert_eq!(fs::read(&destination).unwrap(), fixture.winner_bytes);
    }
    if let Some(stage) = &io.directory {
        assert_eq!(stage.exists(), leftover.is_some());
        assert_eq!(
            remaining_files(stage),
            remnants,
            "public retry must leave earlier remnants untouched"
        );
    }
    let mut expected = vec![retry];
    if published || existing {
        expected.push(destination.clone());
    }
    if let Some(leftover) = leftover {
        expected.push(leftover.clone());
    }
    expected.sort();
    let mut actual: Vec<_> = fs::read_dir(&output)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    actual.sort();
    assert_eq!(actual, expected);
    if published && let Some(leftover) = leftover {
        // Once the simulated removal problem is resolved, deleting only the
        // staging directory must leave the published hard link intact.
        let bytes = fs::read(&destination).unwrap();
        if cleanup.file_fails() {
            assert_eq!(fs::read(io.file.as_ref().unwrap()).unwrap(), bytes);
        }
        fs::remove_dir_all(leftover).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert_eq!(snapshot(&mut read_only(&destination)), fixture.expected);
    }
}

#[test]
fn staging_creation_open_and_close_failures_preserve_files_and_allow_retry() {
    let fixture = Fixture::new();
    for kind in SOURCES {
        for existing in [false, true] {
            for failure in [Step::Directory, Step::File, Step::Open, Step::Close] {
                run_case(&fixture, kind, existing, Some(failure), Cleanup::Healthy);
            }
        }
    }
}

#[test]
fn cleanup_failures_preserve_primary_backup_errors_and_allow_retry() {
    let fixture = Fixture::new();
    for kind in SOURCES {
        for cleanup in [Cleanup::File, Cleanup::Directory, Cleanup::Both] {
            for existing in [false, true] {
                for failure in [
                    Step::File,
                    Step::Open,
                    Step::Copy,
                    Step::Close,
                    Step::Sync,
                    Step::Publish,
                ] {
                    run_case(&fixture, kind, existing, Some(failure), cleanup);
                }
            }
            // The real native AlreadyExists error also survives failed cleanup.
            run_case(&fixture, kind, true, None, cleanup);
        }
    }
}

#[test]
fn cleanup_failures_after_publication_keep_success_and_a_restorable_backup() {
    let fixture = Fixture::new();
    for kind in SOURCES {
        for cleanup in [Cleanup::File, Cleanup::Directory, Cleanup::Both] {
            run_case(&fixture, kind, false, None, cleanup);
        }
    }
}

#[test]
fn healthy_staging_cleanup_preserves_success_and_no_overwrite_errors() {
    let fixture = Fixture::new();
    for kind in SOURCES {
        for existing in [false, true] {
            run_case(&fixture, kind, existing, None, Cleanup::Healthy);
        }
    }
}

#[test]
fn staging_unwind_keeps_native_cleanup_and_releases_source_snapshot() {
    let fixture = Fixture::new();
    for step in [Step::File, Step::Open, Step::Copy, Step::Close] {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.sqlite");
        fs::copy(&fixture.source, &source_path).unwrap();
        let bytes = fs::read(&source_path).unwrap();
        let mut source = open_existing(&source_path);
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        let destination = output.join("backup.sqlite");
        let mut io = LifecycleIo {
            fixture: &fixture,
            destination: destination.clone(),
            existing: false,
            failure: None,
            cleanup: Cleanup::Healthy,
            panic_at: Some(step),
            steps: Vec::new(),
            directory: None,
            file: None,
            registration: None,
            copied: false,
            published: false,
        };
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            source.backup_to_with_io(&destination, &mut io)
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<String>().unwrap(),
            &format!("staging panic at {step:?}")
        );
        assert!(!io.steps.contains(&Step::CleanupFile));
        assert!(!io.steps.contains(&Step::CleanupDirectory));
        assert!(fs::read_dir(&output).unwrap().next().is_none());
        assert_eq!(fs::read(&source_path).unwrap(), bytes);
        assert!(source.connection.is_autocommit());
        assert_unlocked(&source_path);
        source.backup_to(&destination).unwrap();
        verify_published(
            &destination,
            &fixture.expected,
            &directory.path().join("restored.sqlite"),
        );
    }
}
