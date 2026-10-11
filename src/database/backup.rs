use std::{fs, io, path::Path, time::Duration};

use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};

use crate::{Database, Error, OpenMode, OpenOptions, Result};

// Private I/O boundary shared by the public path and fault tests. Each call
// receives its own adapter; no global hooks or consumer-visible controls are needed.
pub(super) trait BackupIo {
    fn create_directory(&mut self, parent: &Path) -> io::Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix(".crabnet-backup-")
            .tempdir_in(parent)
    }

    fn create_file(&mut self, directory: &Path) -> io::Result<tempfile::NamedTempFile> {
        tempfile::NamedTempFile::new_in(directory)
    }

    fn open_source(&mut self, path: &Path, options: &OpenOptions) -> Result<Database> {
        Database::open_with_options(path, options)
    }

    fn open_destination(&mut self, path: &Path) -> Result<Connection> {
        Ok(Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        )?)
    }

    fn sync(&mut self, file: &fs::File) -> io::Result<()> {
        file.sync_all()
    }

    fn close_destination(&mut self, connection: Connection) -> Result<()> {
        connection.close().map_err(|(_, error)| error.into())
    }

    fn cleanup_file(&mut self, file: tempfile::NamedTempFile) -> io::Result<()> {
        file.close()
    }

    fn cleanup_directory(&mut self, directory: tempfile::TempDir) -> io::Result<()> {
        directory.close()
    }

    fn copy_snapshot(&mut self, source: &Connection, destination: &mut Connection) -> Result<()> {
        copy_snapshot(source, destination)
    }

    fn publish(&mut self, source: &Path, destination: &Path) -> io::Result<()> {
        fs::hard_link(source, destination)
    }
}

pub(super) struct NativeBackupIo;
impl BackupIo for NativeBackupIo {}

impl Database {
    /// Saves one committed snapshot to a new file without overwriting any path.
    ///
    /// Works with in-memory, writable, and read-only databases. The snapshot
    /// includes identities, properties, indexes, and uniqueness constraints.
    /// It holds a source read snapshot until copying finishes. This synchronous
    /// operation uses the connection's lock timeout, not query resource limits.
    ///
    /// The copy is staged beside the destination and published with an atomic
    /// hard link after SQLite completes and the file is synced. An existing
    /// destination (including an empty file or symlink) returns [`Error::Io`]
    /// with `AlreadyExists`. Returned errors leave that destination untouched.
    /// Staging cleanup is best effort: its errors neither replace an earlier
    /// failure nor turn a published backup into a reported failure. Cleanup
    /// failure or process death can leave a `.crabnet-backup-*` directory.
    /// Once no backup is using it, remove that directory rather than modifying
    /// its files: a retained staging file may hard-link to the published backup.
    /// The filesystem must support hard links; power-loss durability of the
    /// published directory entry is not guaranteed.
    ///
    /// ```no_run
    /// use crabnet::Database;
    /// let mut graph = Database::open("graph.sqlite")?;
    /// graph.backup_to("graph-backup.sqlite")?;
    /// # Ok::<(), crabnet::Error>(())
    /// ```
    pub fn backup_to(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.backup_to_with_io(path.as_ref(), &mut NativeBackupIo)
    }

    pub(super) fn backup_to_with_io(&mut self, path: &Path, io: &mut impl BackupIo) -> Result<()> {
        super::open::validate_path(path)?;
        let timeout = busy_timeout(&self.connection)?;
        let snapshot = self.connection.transaction()?;
        validate_snapshot(&snapshot)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // Isolate SQLite sidecars as well as the main staging file. Explicit
        // cleanup makes error precedence clear; RAII still handles unwinding.
        let directory = io.create_directory(parent)?;
        let result = (|| {
            let file = io.create_file(directory.path())?;
            let result: Result<()> = (|| {
                let mut destination = io.open_destination(file.path())?;
                destination.busy_timeout(timeout)?;
                io.copy_snapshot(&snapshot, &mut destination)?;
                io.close_destination(destination)?;
                io.sync(file.as_file())?;
                // Unlike rename(), hard_link() cannot replace an existing
                // entry, even if a competing publisher created it meanwhile.
                io.publish(file.path(), path)?;
                Ok(())
            })();
            // Best effort, like tempfile's Drop: retain the operation's error
            // or its published success. Never turn a completed backup into a
            // reported failure. Directory cleanup also retries file removal.
            let _ = io.cleanup_file(file);
            result
        })();
        let _ = io.cleanup_directory(directory);
        result
    }

    /// Atomically replaces this graph with a validated, current-version backup.
    ///
    /// The source is opened read-only and checked under the same snapshot that
    /// is copied. Missing, foreign, old-version, and invalid backup files fail
    /// before replacing the destination. SQLite integrity and foreign-key
    /// checks detect storage inconsistencies, not every possible semantic edit
    /// made by an external SQLite writer.
    ///
    /// This discards the destination's current graph and constraints on success.
    /// Other connections observe the replacement on subsequent reads. A blocked
    /// or incomplete copy rolls back; the source is never modified or migrated.
    /// Read-only destinations return [`Error::ReadOnly`]. Source and destination
    /// lock waits use this connection's timeout. There is no automatic retry,
    /// progress callback, or query deadline/cancellation during copying.
    ///
    /// ```no_run
    /// use crabnet::Database;
    /// let mut restored = Database::open("restored.sqlite")?;
    /// restored.restore_from("graph-backup.sqlite")?;
    /// # Ok::<(), crabnet::Error>(())
    /// ```
    pub fn restore_from(&mut self, path: impl AsRef<Path>) -> Result<()> {
        self.restore_from_with_io(path.as_ref(), &mut NativeBackupIo)
    }

    pub(super) fn restore_from_with_io(
        &mut self,
        path: &Path,
        io: &mut impl BackupIo,
    ) -> Result<()> {
        self.ensure_writable()?;
        let mut source = io.open_source(
            path,
            &OpenOptions {
                mode: OpenMode::ReadOnly,
                busy_timeout: busy_timeout(&self.connection)?,
            },
        )?;
        let snapshot = source.connection.transaction()?;
        validate_snapshot(&snapshot)?;
        self.connection.flush_prepared_statement_cache();
        // Do not run any fallible operation after successful replacement: an
        // error from later cleanup would misreport an already committed restore.
        io.copy_snapshot(&snapshot, &mut self.connection)
    }
}

fn busy_timeout(connection: &Connection) -> Result<Duration> {
    let milliseconds: u32 =
        connection.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
    Ok(Duration::from_millis(u64::from(milliseconds)))
}

fn validate_snapshot(connection: &Connection) -> Result<()> {
    super::inspect_schema(connection, OpenMode::ReadOnly)?;
    super::validate_schema(connection)?;
    let mut statement = connection.prepare("PRAGMA main.integrity_check")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let message: String = row.get(0)?;
        if message != "ok" {
            return Err(Error::Data(format!(
                "backup integrity check failed: {message}"
            )));
        }
    }
    if connection
        .prepare("PRAGMA main.foreign_key_check")?
        .query([])?
        .next()?
        .is_some()
    {
        return Err(Error::Data(
            "backup contains a foreign-key violation".into(),
        ));
    }
    // SQLite omits CHECK expressions when loading a read-only schema, so its
    // integrity check alone does not enforce them on restore sources. Keep
    // these predicates aligned with INITIAL_SCHEMA's persisted CHECK clauses.
    let invalid: bool = connection.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM main.nodes WHERE
                CASE WHEN json_valid(labels) THEN json_type(labels) <> 'array' ELSE 1 END
                OR CASE WHEN json_valid(props) THEN json_type(props) <> 'object' ELSE 1 END
            UNION ALL
            SELECT 1 FROM main.edges WHERE edge_type = ''
                OR CASE WHEN json_valid(props) THEN json_type(props) <> 'object' ELSE 1 END
        )",
        [],
        |row| row.get(0),
    )?;
    if invalid {
        return Err(Error::Data(
            "backup integrity check failed: invalid labels, properties, or relationship type"
                .into(),
        ));
    }
    Ok(())
}

fn copy_snapshot(source: &Connection, destination: &mut Connection) -> Result<()> {
    let backup = Backup::new(source, destination)?;
    // One pinned source snapshot and one destination transaction. In contrast
    // to run_to_completion(), contention never starts an unbounded retry loop.
    match backup.step(-1)? {
        StepResult::Done => Ok(()),
        StepResult::Busy => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )
        .into()),
        StepResult::Locked => Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        )
        .into()),
        _ => Err(Error::Data(
            "SQLite backup stopped before completion".into(),
        )),
    }
}
