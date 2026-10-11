//! Recovery faults use a real hot DELETE journal left by a killed writer.

use super::{
    Snapshot, assert_integrity,
    backup_io::verify_published,
    io_vfs, open_existing, pragma, seed, snapshot,
    source_read_io::{assert_files, assert_reopened, source_cache_guard, verify_replacement},
};
use crate::database::backup::{BackupIo, NativeBackupIo};
use crate::{Database, Error, OpenMode, OpenOptions, Parameters, Result};
use io_vfs::{Event, FileKind, Operation, Registration};
use rusqlite::{Connection, ErrorCode, OpenFlags, ffi};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

const CHILD_PATH: &str = "CRABNET_RECOVERY_IO_CHILD_PATH";
const TEST_NAME: &str = "database::storage_failures::recovery_io::hot_journal_recovery_io_failures_preserve_recovery_and_allow_public_retry";
const MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child_writes(path: &Path) -> ! {
    let mut database = open_existing(path);
    database
        .connection
        .pragma_update(None, "cache_size", 8)
        .unwrap();
    database
        .connection
        .pragma_update(None, "cache_spill", true)
        .unwrap();
    database
        .connection
        .pragma_update(None, "synchronous", "FULL")
        .unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction.drop_unique_constraint("Saved", "key").unwrap();
    transaction
        .create_unique_constraint("Pending", "id")
        .unwrap();
    transaction
        .execute(
            "MATCH (a:Saved {key:'a'})-[r:LINK]->(b:Saved {key:'b'})
         SET a.key='changed', a.number=11, a:Changed, r.weight=9
         REMOVE a:Original",
        )
        .unwrap();
    transaction
        .execute("MATCH (n:Saved {key:'gone'}) DELETE n")
        .unwrap();
    transaction
        .execute_with(
            "UNWIND range(1, 8) AS i CREATE (:Pending {id:i, payload:$payload})",
            &Parameters::from([("payload".into(), json!("x".repeat(64 * 1024)))]),
        )
        .unwrap();
    println!("CRABNET_RECOVERY_IO_READY");
    std::io::stdout().flush().unwrap();
    // Keep the uncommitted transaction live. The parent kills and reaps this
    // process; no SQLite rollback or Rust destructor runs in the child.
    std::io::stdin().read_exact(&mut [0]).unwrap();
    panic!("parent must terminate the writer");
}

fn ready_child(path: &Path) -> ChildGuard {
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_PATH, path)
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
                Ok(line) if line.trim() == "CRABNET_RECOVERY_IO_READY" => {
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
        "writer readiness"
    );
    child
}

fn journal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push("-journal");
    name.into()
}

struct Fixture {
    _directory: tempfile::TempDir,
    original: Vec<u8>,
    dirty: Vec<u8>,
    hot: Vec<u8>,
    expected: Snapshot,
    page_size: usize,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.sqlite");
        let mut database = seed(&path);
        let expected = snapshot(&mut database);
        let page_size = usize::try_from(pragma(&database.connection, "page_size")).unwrap();
        drop(database);
        let original = fs::read(&path).unwrap();
        let mut child = ready_child(&path);
        let dirty = fs::read(&path).unwrap();
        let hot = fs::read(journal(&path)).unwrap();
        assert_ne!(dirty, original, "uncommitted pages must have spilled");
        assert!(
            dirty.len() > original.len(),
            "recovery must truncate spilled growth"
        );
        assert_ne!(
            &dirty[..original.len()],
            original,
            "existing pages must also have spilled changes"
        );
        assert!(hot.len() > 512);
        assert_eq!(hot[..8], MAGIC);
        assert!(u32::from_be_bytes(hot[8..12].try_into().unwrap()) > 0);
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        assert_eq!(fs::read(&path).unwrap(), dirty);
        assert_eq!(fs::read(journal(&path)).unwrap(), hot);
        Self {
            _directory: directory,
            original,
            dirty,
            hot,
            expected,
            page_size,
        }
    }

    fn copy_to(&self, path: &Path) {
        fs::write(path, &self.dirty).unwrap();
        fs::write(journal(path), &self.hot).unwrap();
    }
}

struct ReadOnlySource<'a> {
    registration: &'a Registration,
    opened: bool,
    copied: bool,
}

impl BackupIo for ReadOnlySource<'_> {
    fn open_source(&mut self, path: &Path, options: &OpenOptions) -> Result<Database> {
        assert_eq!(options.mode, OpenMode::ReadOnly);
        assert_eq!(options.busy_timeout, Duration::ZERO);
        self.opened = true;
        Database::open_with_connector(path, options, |_, flags| self.registration.connect(flags))
    }

    fn copy_snapshot(&mut self, source: &Connection, destination: &mut Connection) -> Result<()> {
        self.copied = true;
        NativeBackupIo.copy_snapshot(source, destination)
    }
}

fn options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        busy_timeout: Duration::ZERO,
    }
}

fn open(path: &Path, mode: OpenMode, registration: &Registration) -> Result<Database> {
    Database::open_with_connector(path, &options(mode), |_, flags| registration.connect(flags))
}

#[derive(Debug)]
struct Fault {
    index: usize,
    event: Event,
    occurrence: usize,
}

fn faults(events: &[Event]) -> Vec<Fault> {
    events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.file == FileKind::Journal || event.operation != Operation::Read)
        .map(|(index, event)| Fault {
            index,
            event: event.clone(),
            occurrence: events[..index].iter().filter(|e| *e == event).count(),
        })
        .collect()
}

fn assert_sqlite_error(result: Result<()>, expected: i32) {
    let error = result.expect_err("the fault must reject opening");
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite error, got {error:?}");
    };
    assert_eq!(code.extended_code, expected);
    assert_eq!(
        code.code,
        if expected == ffi::SQLITE_READONLY_ROLLBACK {
            ErrorCode::ReadOnly
        } else {
            ErrorCode::SystemIoFailure
        }
    );
}

fn assert_fault(events: &[Event], healthy: &[Event], fault: &Fault) {
    assert!(events.len() > fault.index);
    assert_eq!(
        &events[..fault.index],
        &healthy[..fault.index],
        "exact healthy I/O prefix"
    );
    let mut expected = fault.event.clone();
    expected.result = expected.operation.error_code();
    assert_eq!(events[fault.index], expected);
    assert!(
        events[fault.index + 1..]
            .iter()
            .all(|e| e.result == ffi::SQLITE_OK),
        "healthy cleanup after one-shot error"
    );
}

fn assert_unlocked_without_recovery(path: &Path) {
    let before = fs::read(path).unwrap();
    let hot = fs::read(journal(path)).ok();
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .unwrap();
    // SAFETY: The connection owns a live main-file pointer until it is dropped.
    // No statement or pager transaction runs here. Exercise the native file's
    // locks directly, then unlock before returning it to SQLite. BEGIN would
    // replay the hot journal and conceal a broken failed-recovery state.
    unsafe {
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        assert_eq!(
            ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                std::ptr::addr_of_mut!(file).cast()
            ),
            ffi::SQLITE_OK
        );
        assert!(!file.is_null());
        let methods = &*(*file).pMethods;
        assert_eq!(
            methods.xLock.unwrap()(file, ffi::SQLITE_LOCK_SHARED),
            ffi::SQLITE_OK
        );
        assert_eq!(
            methods.xLock.unwrap()(file, ffi::SQLITE_LOCK_EXCLUSIVE),
            ffi::SQLITE_OK
        );
        assert_eq!(
            methods.xUnlock.unwrap()(file, ffi::SQLITE_LOCK_NONE),
            ffi::SQLITE_OK
        );
    }
    drop(connection);
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(
        fs::read(journal(path)).ok(),
        hot,
        "lock probe must not perform recovery"
    );
}

fn assert_partial_replay(fixture: &Fixture, path: &Path, events: &[Event]) {
    assert_eq!(
        fs::read(journal(path)).unwrap(),
        fixture.hot,
        "retain the complete hot journal for retry"
    );
    let actual = fs::read(path).unwrap();
    assert!(
        actual.len() == fixture.original.len() || actual.len() == fixture.dirty.len(),
        "native recovery may truncate spilled growth before the failed call"
    );
    let mut expected = fixture.dirty[..actual.len()].to_vec();
    for event in events
        .iter()
        .filter(|e| e.operation == Operation::Write && e.result == ffi::SQLITE_OK)
    {
        assert_eq!(event.file, FileKind::Database);
        assert_eq!(usize::try_from(event.amount).unwrap(), fixture.page_size);
        let start = usize::try_from(event.offset).unwrap();
        assert_eq!(start % fixture.page_size, 0);
        let end = start + fixture.page_size;
        expected[start..end].copy_from_slice(&fixture.original[start..end]);
    }
    assert_eq!(
        actual, expected,
        "only successful replay writes may change original database pages"
    );
}

fn run_case(fixture: &Fixture, mode: OpenMode, fault: Option<(&[Event], &Fault)>) -> Vec<Event> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite");
    fixture.copy_to(&path);
    let registration = io_vfs::register(&path);
    registration.control.start_reads();
    if let Some((_, fault)) = fault {
        registration.control.fail_io(&fault.event, fault.occurrence);
    }
    let result = open(&path, mode, &registration);
    let events = registration.control.stop();
    if let Some((healthy, fault)) = fault {
        assert_sqlite_error(result.map(drop), fault.event.operation.error_code());
        assert_fault(&events, healthy, fault);
        assert_partial_replay(fixture, &path, &events);
        let partial = fs::read(&path).unwrap();
        assert_sqlite_error(
            Database::open_with_options(&path, &options(OpenMode::ReadOnly)).map(drop),
            ffi::SQLITE_READONLY_ROLLBACK,
        );
        assert_eq!(fs::read(&path).unwrap(), partial);
        assert_eq!(fs::read(journal(&path)).unwrap(), fixture.hot);
        assert_files(
            directory.path(),
            &["source.sqlite", "source.sqlite-journal"],
        );
    } else {
        let mut database = result.unwrap();
        assert_eq!(registration.control.open_file_count(), 1);
        assert_eq!(pragma(&database.connection, "synchronous"), 2);
        assert!(events.iter().all(|e| e.result == ffi::SQLITE_OK));
        assert_eq!(fs::read(&path).unwrap(), fixture.original);
        assert!(!journal(&path).exists());
        assert_eq!(snapshot(&mut database), fixture.expected);
    }
    assert_eq!(registration.control.open_file_count(), 0);
    assert_unlocked_without_recovery(&path);

    // Public retry, while the old test registration remains live. SQLite must
    // replay the retained journal, then Crabnet validates the original graph.
    let mut recovered = Database::open_with_options(&path, &options(mode)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), fixture.original);
    assert!(!journal(&path).exists());
    assert_eq!(snapshot(&mut recovered), fixture.expected);
    assert_integrity(&recovered);
    assert!(recovered.connection.is_autocommit());
    let backup = directory.path().join("backup.sqlite");
    recovered.backup_to(&backup).unwrap();
    verify_published(
        &backup,
        &fixture.expected,
        &directory.path().join("restored.sqlite"),
    );
    let committed = verify_replacement(&mut recovered, &fixture.expected);
    drop(recovered);
    assert_reopened(&path, &committed);
    assert_files(
        directory.path(),
        &["source.sqlite", "backup.sqlite", "restored.sqlite"],
    );
    events
}

fn read_only_case(
    fixture: &Fixture,
    restore: bool,
    fault: Option<(&[Event], &Fault)>,
) -> Vec<Event> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite");
    fixture.copy_to(&path);
    let registration = io_vfs::register(&path);
    let destination = directory.path().join("destination.sqlite");
    let mut database = Database::open(&destination).unwrap();
    database
        .create_unique_constraint("Incumbent", "key")
        .unwrap();
    database
        .execute("CREATE (:Incumbent {key:'kept'})")
        .unwrap();
    drop(database);
    let (mut database, destination_registration) = io_vfs::open(&destination);
    database.connection.busy_timeout(Duration::ZERO).unwrap();
    database
        .execute("MATCH (n:Incumbent) SET n.key='kept' RETURN n")
        .unwrap();
    let original = snapshot(&mut database);
    let destination_bytes = fs::read(&destination).unwrap();
    let mut observer = open_existing(&destination);
    assert_eq!(snapshot(&mut observer), original);
    destination_registration.control.start(None);
    registration.control.start_reads();
    if let Some((_, fault)) = fault {
        registration.control.fail_io(&fault.event, fault.occurrence);
    }
    let mut io = ReadOnlySource {
        registration: &registration,
        opened: false,
        copied: false,
    };
    let result = if restore {
        database.restore_from_with_io(&path, &mut io)
    } else {
        open(&path, OpenMode::ReadOnly, &registration).map(drop)
    };
    let events = registration.control.stop();
    assert_eq!(io.opened, restore);
    assert!(!io.copied);
    assert_eq!(registration.control.open_file_count(), 0);
    assert!(destination_registration.control.stop().is_empty());
    if let Some((healthy, fault)) = fault {
        assert_sqlite_error(result, fault.event.operation.error_code());
        assert_fault(&events, healthy, fault);
    } else {
        assert_sqlite_error(result, ffi::SQLITE_READONLY_ROLLBACK);
        assert!(events.iter().all(|e| e.result == ffi::SQLITE_OK));
        assert!(
            events
                .iter()
                .any(|e| e.file == FileKind::Journal && e.operation == Operation::Read)
        );
    }
    assert!(events.iter().all(|e| e.operation == Operation::Read));
    assert_eq!(fs::read(&path).unwrap(), fixture.dirty);
    assert_eq!(fs::read(journal(&path)).unwrap(), fixture.hot);
    assert_eq!(fs::read(&destination).unwrap(), destination_bytes);
    assert!(database.connection.is_autocommit());
    assert_unlocked_without_recovery(&path);
    assert_unlocked_without_recovery(&destination);
    assert_eq!(snapshot(&mut database), original);
    assert_eq!(snapshot(&mut observer), original);
    assert_integrity(&database);
    assert!(matches!(
        database.execute("CREATE (:Incumbent {key:'kept'})"),
        Err(Error::Constraint(_))
    ));
    assert_files(
        directory.path(),
        &[
            "source.sqlite",
            "source.sqlite-journal",
            "destination.sqlite",
        ],
    );

    // Recovery is a separate explicit writable open. Restore never upgrades
    // its source to writable just because a hot journal exists.
    let mut recovered = Database::open_with_options(&path, &options(OpenMode::ReadWrite)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), fixture.original);
    assert_eq!(snapshot(&mut recovered), fixture.expected);
    drop(recovered);
    assert!(!journal(&path).exists());
    let mut reader = Database::open_with_options(&path, &options(OpenMode::ReadOnly)).unwrap();
    assert_eq!(snapshot(&mut reader), fixture.expected);
    assert!(matches!(
        reader.execute("CREATE (:Forbidden)"),
        Err(Error::ReadOnly)
    ));
    drop(reader);
    if restore {
        database.restore_from(&path).unwrap();
        assert_eq!(snapshot(&mut observer), fixture.expected);
        let committed = verify_replacement(&mut database, &fixture.expected);
        drop(observer);
        drop(database);
        assert_reopened(&destination, &committed);
    } else {
        assert_eq!(snapshot(&mut database), original);
        drop(observer);
        drop(database);
        assert_reopened(&destination, &original);
    }
    assert_eq!(destination_registration.control.open_file_count(), 0);
    assert_eq!(fs::read(&path).unwrap(), fixture.original);
    assert_files(directory.path(), &["source.sqlite", "destination.sqlite"]);
    events
}

#[test]
fn hot_journal_recovery_io_failures_preserve_recovery_and_allow_public_retry() {
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        child_writes(Path::new(&path));
    }
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new();
    for mode in [OpenMode::ReadWrite, OpenMode::OpenOrCreate] {
        let healthy = run_case(&fixture, mode, None);
        let selected = faults(&healthy);
        for (file, operation) in [
            (FileKind::Journal, Operation::Read),
            (FileKind::Journal, Operation::Sync),
            (FileKind::Database, Operation::Write),
            (FileKind::Database, Operation::Sync),
        ] {
            assert!(
                selected
                    .iter()
                    .any(|f| (f.event.file, f.event.operation) == (file, operation))
            );
        }
        assert!(
            selected.iter().any(|f| f.event.file == FileKind::Journal
                && f.event.operation == Operation::Read
                && healthy[..f.index]
                    .iter()
                    .any(|e| e.operation == Operation::Write)),
            "journal read failures must follow earlier replay writes"
        );
        assert!(
            !healthy
                .iter()
                .any(|e| e.file == FileKind::Journal && e.operation == Operation::Write)
        );
        eprintln!("recovery {mode:?}: {} I/O fault points", selected.len());
        for fault in selected {
            eprintln!("recovery {mode:?}: {fault:?}");
            run_case(&fixture, mode, Some((&healthy, &fault)));
        }
    }
}

#[test]
fn read_only_and_restore_sources_refuse_hot_journals_without_modifying_files() {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new();
    for restore in [false, true] {
        let healthy = read_only_case(&fixture, restore, None);
        let selected = faults(&healthy);
        assert!(!selected.is_empty());
        for fault in selected {
            assert_eq!(fault.event.file, FileKind::Journal);
            assert_eq!(fault.event.operation, Operation::Read);
            eprintln!("read-only hot journal restore={restore}: {fault:?}");
            read_only_case(&fixture, restore, Some((&healthy, &fault)));
        }
    }
}

#[test]
fn recovery_journal_faults_are_path_scoped_and_consumed_once() {
    let _cache_guard = source_cache_guard();
    let fixture = Fixture::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.sqlite");
    let other_path = directory.path().join("other.sqlite");
    fixture.copy_to(&path);
    fixture.copy_to(&other_path);
    let registration = io_vfs::register(&path);
    let other = io_vfs::register(&other_path);
    let target = Event {
        file: FileKind::Journal,
        operation: Operation::Read,
        offset: 0,
        amount: 1,
        result: ffi::SQLITE_OK,
    };
    registration.control.start_reads();
    registration.control.fail_read(&target, 0);
    let mut unrelated = open(&other_path, OpenMode::ReadWrite, &other).unwrap();
    assert_eq!(snapshot(&mut unrelated), fixture.expected);
    drop(unrelated);
    assert_eq!(fs::read(&other_path).unwrap(), fixture.original);
    assert!(!journal(&other_path).exists());
    assert_eq!(registration.control.event_count(), 0);
    assert_sqlite_error(
        open(&path, OpenMode::ReadWrite, &registration).map(drop),
        ffi::SQLITE_IOERR_READ,
    );
    assert_eq!(registration.control.open_file_count(), 0);
    assert_eq!(fs::read(&path).unwrap(), fixture.dirty);
    assert_eq!(fs::read(journal(&path)).unwrap(), fixture.hot);
    assert_unlocked_without_recovery(&path);
    // Keep recording and do not reset or rearm: retry through the same VFS
    // succeeds only if the journal fault has been consumed.
    let mut retry = open(&path, OpenMode::ReadWrite, &registration).unwrap();
    assert_eq!(snapshot(&mut retry), fixture.expected);
    assert_integrity(&retry);
    drop(retry);
    assert_eq!(registration.control.open_file_count(), 0);
    assert_eq!(other.control.open_file_count(), 0);
    let events = registration.control.stop();
    let failed: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.result != ffi::SQLITE_OK)
        .collect();
    assert_eq!(failed.len(), 1);
    let (index, actual) = failed[0];
    let mut expected = target;
    expected.result = ffi::SQLITE_IOERR_READ;
    assert_eq!(*actual, expected);
    assert!(
        events[..index]
            .iter()
            .any(|e| e.file == FileKind::Database && e.operation == Operation::Read)
    );
    assert!(
        events[index + 1..]
            .iter()
            .any(|e| e.operation == Operation::Write)
    );
    assert_eq!(fs::read(&path).unwrap(), fixture.original);
    assert_files(directory.path(), &["source.sqlite", "other.sqlite"]);
}
