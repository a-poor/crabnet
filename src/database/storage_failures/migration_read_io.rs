//! Reads during historical opening and migration, before native I/O, with
//! healthy cleanup. Raw storage is checked before any public migration retry.

use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, ErrorCode, OpenFlags, ffi};
use serde_json::Value;

use super::{
    assert_connection_integrity, io_vfs,
    migration_io::{
        FIXTURES, Stored, assert_graph, assert_indexes_work, assert_writer_can_commit, raw_open,
        stored,
    },
    open_existing, pragma, snapshot,
    source_read_io::{assert_files, source_cache_guard},
};
use crate::{Database, Error, OpenMode, OpenOptions, Result};
use io_vfs::{Event, FileKind, Operation};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Retained,
    Public(OpenMode),
}

#[derive(Debug)]
struct ReadFault {
    index: usize,
    event: Event,
    occurrence: usize,
    completes: bool,
}

struct Trace {
    events: Vec<Event>,
    connect_end: Option<usize>,
    commit_at: Option<usize>,
    stored: Stored,
    check_reads: Vec<(i64, i32)>,
}

impl Trace {
    fn faults(&self) -> Vec<ReadFault> {
        self.events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.operation == Operation::Read)
            .map(|(index, event)| {
                let occurrence = self.events[..index].iter().filter(|e| *e == event).count();
                ReadFault {
                    index,
                    event: event.clone(),
                    occurrence,
                    // Schema 1's ALTER ADD COLUMN runs SQLite's internal
                    // quick_check on nodes. Its SQL filters constraint/type
                    // diagnostics, absorbing these one-shot read diagnostics.
                    // Pin only the first read of the retained nodes/index roots;
                    // later reads during actual backfills must still fail.
                    completes: occurrence == 0
                        && event.file == FileKind::Database
                        && self.check_reads.contains(&(event.offset, event.amount)),
                }
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

fn install_commit_marker(
    connection: &Connection,
    control: &io_vfs::Control,
    commits: &Arc<Mutex<Vec<usize>>>,
) {
    let control = control.clone();
    let commits = commits.clone();
    connection
        .commit_hook(Some(move || {
            commits.lock().unwrap().push(control.event_count());
            false
        }))
        .unwrap();
}

fn assert_read_error(result: Result<()>) {
    let error = result.expect_err("an injected read fault must reject the historical upgrade");
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite read error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::SystemIoFailure);
    assert_eq!(code.extended_code, ffi::SQLITE_IOERR_READ);
}

fn healthy(event: &Event) -> bool {
    event.result == ffi::SQLITE_OK
        // SQLite probes past the journal's end before synchronizing it. This
        // native short read is normal, unlike our injected SQLITE_IOERR_READ.
        || (event.file == FileKind::Journal && event.operation == Operation::Read
            && event.result == ffi::SQLITE_IOERR_SHORT_READ)
}

fn run_case(version: i64, mode: Mode, fault: Option<(&Trace, &ReadFault)>) -> Trace {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("upgrade.sqlite");
    fs::copy(
        Path::new(FIXTURES).join(format!("v{version}.sqlite")),
        &path,
    )
    .unwrap();
    let oracle: Value = serde_json::from_slice(
        &fs::read(Path::new(FIXTURES).join(format!("v{version}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(oracle["schema_version"], version);
    let raw = raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY);
    let before = stored(&raw);
    let page_size = pragma(&raw, "page_size");
    let check_reads = if version == 1 {
        raw.prepare("SELECT rootpage FROM sqlite_schema WHERE tbl_name='nodes' AND rootpage>0")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .map(|root| {
                (
                    (root.unwrap() - 1) * page_size,
                    i32::try_from(page_size).unwrap(),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    drop(raw);
    let original = fs::read(&path).unwrap();
    let registration = io_vfs::register(&path);
    let control = &registration.control;
    let commits = Arc::new(Mutex::new(Vec::new()));
    let mut retained = None;
    if matches!(mode, Mode::Retained) {
        let connection = registration
            .connect(
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
            )
            .unwrap();
        connection.busy_timeout(Duration::ZERO).unwrap();
        // Evict clean pages between statements so schema 1's initial check
        // cannot warm away the later migration/backfill reads.
        connection
            .execute_batch(
                "PRAGMA synchronous=FULL;
                 PRAGMA cache_spill=OFF;
                 PRAGMA cache_size=1;",
            )
            .unwrap();
        assert_eq!(pragma(&connection, "synchronous"), 2);
        assert_eq!(pragma(&connection, "cache_spill"), 0);
        assert_eq!(pragma(&connection, "cache_size"), 1);
        connection.execute_batch("PRAGMA shrink_memory;").unwrap();
        install_commit_marker(&connection, control, &commits);
        retained = Some(connection);
    }
    control.start_reads();
    if let Some((_, fault)) = fault {
        control.fail_read(&fault.event, fault.occurrence);
    }
    let mut connect_end = None;
    let mut opened = None;
    let result = match mode {
        Mode::Retained => {
            connect_end = Some(0);
            super::super::initialize_with_mode(retained.as_mut().unwrap(), OpenMode::ReadWrite)
        }
        Mode::Public(mode) => {
            Database::open_with_connector(&path, &options(mode), |actual, flags| {
                assert_eq!(actual, path);
                let connection = registration.connect(flags)?;
                connect_end = Some(control.event_count());
                install_commit_marker(&connection, control, &commits);
                Ok(connection)
            })
            .map(|database| opened = Some(database))
        }
    };
    // Stop before any assertion/reader can accidentally consume an unhit fault.
    let events = control.stop();
    if let Some(database) = opened.take() {
        assert_eq!(pragma(&database.connection, "synchronous"), 2);
        let journal: String = database
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "delete");
        drop(database);
    }
    let commits = commits.lock().unwrap();
    assert!(
        commits.len() <= 1,
        "unexpected write transactions: {commits:?}"
    );
    let commit_at = commits.first().copied();
    if let Some(connection) = &retained {
        connection.commit_hook(None::<fn() -> bool>).unwrap();
        assert!(connection.is_autocommit());
        assert_eq!(control.open_file_count(), 1);
    } else {
        assert_eq!(control.open_file_count(), 0);
    }
    if let Some((baseline, fault)) = fault {
        let failures: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !healthy(e))
            .collect();
        assert_eq!(
            failures.len(),
            1,
            "one-shot fault and healthy cleanup: {events:?}"
        );
        assert_eq!(failures[0].0, fault.index);
        let mut failed = fault.event.clone();
        failed.result = ffi::SQLITE_IOERR_READ;
        assert_eq!(*failures[0].1, failed);
        assert_eq!(&events[..fault.index], &baseline.events[..fault.index]);
        if fault.completes {
            result.unwrap();
            assert!(commit_at.is_some());
            // Check completion before another public open could conceal a
            // falsely successful migration left at an intermediate version.
            assert_eq!(
                stored(&raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)),
                baseline.stored
            );
        } else {
            assert_read_error(result);
            assert_eq!(
                commit_at,
                (fault.index >= baseline.commit_at.unwrap()).then_some(baseline.commit_at.unwrap())
            );
            // Do not let an automatic upgrade conceal partial DDL/backfills/version.
            assert_eq!(fs::read(&path).unwrap(), original);
            assert_files(directory.path(), &["upgrade.sqlite"]);
            let reader = raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY);
            assert_eq!(stored(&reader), before);
            assert_connection_integrity(&reader);
            assert_writer_can_commit(&path, &before);
            if let Some(connection) = &retained {
                assert_eq!(stored(connection), before);
                assert_connection_integrity(connection);
            }
            drop(retained.take());
            assert_eq!(control.open_file_count(), 0);
            assert_eq!(
                stored(&raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)),
                before
            );
            assert!(
                matches!(Database::open_with_options(&path, &options(OpenMode::ReadOnly)),
            Err(Error::Data(message)) if message.contains("requires migration"))
            );
            assert_eq!(stored(&reader), before);
        }
    } else {
        result.unwrap();
        assert!(events.iter().all(healthy), "{events:?}");
        assert_eq!(
            pragma(
                &raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY),
                "user_version"
            ),
            super::super::SCHEMA_VERSION
        );
    }
    drop(retained);
    assert_eq!(control.open_file_count(), 0);
    drop(registration);
    let persisted = stored(&raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY));
    let mut upgraded = open_existing(&path);
    assert_graph(&mut upgraded, &oracle);
    drop(upgraded);
    let mut reopened = Database::open_with_options(&path, &options(OpenMode::ReadOnly)).unwrap();
    assert_graph(&mut reopened, &oracle);
    drop(reopened);
    let mut upgraded = open_existing(&path);
    assert_indexes_work(&mut upgraded);
    let committed = snapshot(&mut upgraded);
    drop(upgraded);
    assert_eq!(snapshot(&mut open_existing(&path)), committed);
    assert_files(directory.path(), &["upgrade.sqlite"]);
    Trace {
        events,
        connect_end,
        commit_at,
        stored: persisted,
        check_reads,
    }
}

fn sweep(mode: Mode) {
    let _cache_guard = source_cache_guard();
    for version in 1..super::super::SCHEMA_VERSION {
        let baseline = run_case(version, mode, None);
        let connect_end = baseline.connect_end.unwrap();
        let commit_at = baseline.commit_at.unwrap();
        let faults = baseline.faults();
        assert!(
            faults
                .iter()
                .any(|f| f.index >= connect_end && f.index < commit_at)
        );
        // Schema 2 backfills read nodes in either cache configuration. The
        // tiny cache additionally forces schema 1's later node/index reads
        // and schema 4's schema rereads after journal writes have begun.
        if version == 2 || (matches!(mode, Mode::Retained) && matches!(version, 1 | 4)) {
            assert!(faults.iter().any(|f| {
                !f.completes
                    && f.index < commit_at
                    && baseline.events[..f.index]
                        .iter()
                        .any(|e| e.operation == Operation::Write && e.file == FileKind::Journal)
            }));
        }
        assert!(
            faults
                .iter()
                .any(|f| f.event.file == FileKind::Journal && f.index >= commit_at)
        );
        assert!(
            baseline.events[..commit_at]
                .iter()
                .all(|e| e.file != FileKind::Database || e.operation != Operation::Write)
        );
        if matches!(mode, Mode::Public(_)) {
            assert!(faults.iter().any(|f| f.index < connect_end));
        }
        if version == 1 {
            assert_eq!(faults.iter().filter(|f| f.completes).count(), 3);
            if matches!(mode, Mode::Retained) {
                assert!(faults.iter().any(|f| {
                    f.occurrence > 0
                        && !f.completes
                        && baseline
                            .check_reads
                            .contains(&(f.event.offset, f.event.amount))
                }));
            }
        }
        eprintln!(
            "schema {version}, {mode:?}: {} read targets, {} absorbed by SQLite's internal check",
            faults.len(),
            faults.iter().filter(|f| f.completes).count()
        );
        for fault in faults {
            eprintln!(
                "schema {version}, {mode:?}, migration read {fault:?} (connect at {connect_end}, commit at {commit_at})"
            );
            run_case(version, mode, Some((&baseline, &fault)));
        }
    }
}

#[test]
fn migration_read_failures_restore_historical_files_before_retained_connection_close() {
    // MEMORY_MANAGEMENT pools page-cache budgets across connections. Merely
    // guarding shrink_memory cannot force eviction with other tests' larger
    // caches alive. Run this exact test alone in a fresh process, preserving
    // the cold-cache coverage assertions under the parallel full suite.
    const CHILD: &str = "CRABNET_MIGRATION_READ_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "database::storage_failures::migration_read_io::migration_read_failures_restore_historical_files_before_retained_connection_close",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        eprintln!("{}", String::from_utf8_lossy(&output.stdout));
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "isolated migration read sweep failed: {}",
            output.status
        );
        return;
    }
    sweep(Mode::Retained);
}

#[test]
fn migration_open_read_failures_release_handles_and_allow_public_retry() {
    for mode in [OpenMode::ReadWrite, OpenMode::OpenOrCreate] {
        sweep(Mode::Public(mode));
    }
}
