use std::{path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags};

use crate::{Database, Error, Result};

/// Whether opening a file may initialize a graph or modify an existing one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpenMode {
    /// Open a graph or initialize an empty/new file. Existing graphs may migrate.
    #[default]
    OpenOrCreate,
    /// Open an existing Crabnet graph for reads and writes, migrating if needed.
    ReadWrite,
    /// Open an existing, current-version graph without permitting writes or migration.
    ReadOnly,
}

/// File-opening and connection-local lock-wait policy.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// Defaults to [`OpenMode::OpenOrCreate`].
    pub mode: OpenMode,
    /// SQLite's busy timeout, applied before schema inspection or migration.
    /// Defaults to five seconds; zero fails immediately on contention.
    /// Fractions of a millisecond are discarded. Values above `i32::MAX`
    /// milliseconds return [`Error::Argument`]. This is a per-lock wait policy,
    /// not a deadline for the whole operation; SQLite may return busy early.
    pub busy_timeout: Duration,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            mode: OpenMode::OpenOrCreate,
            busy_timeout: Duration::from_secs(5),
        }
    }
}

impl Database {
    /// Opens a filesystem path with an explicit access mode and lock timeout.
    ///
    /// Read/write modes never silently fall back to read-only access. Only
    /// `OpenOrCreate` initializes an empty file; `ReadOnly` requires the current
    /// schema. Empty paths, `:memory:`, and SQLite `file:` URIs are rejected;
    /// use [`Self::in_memory`] for a transient graph.
    ///
    /// ```no_run
    /// use crabnet::{Database, OpenMode, OpenOptions};
    /// use std::time::Duration;
    /// let mut graph = Database::open_with_options("graph.sqlite", &OpenOptions {
    ///     mode: OpenMode::ReadOnly,
    ///     busy_timeout: Duration::from_millis(250),
    /// })?;
    /// let nodes = graph.execute("MATCH (n) RETURN n")?;
    /// # Ok::<(), crabnet::Error>(())
    /// ```
    pub fn open_with_options(path: impl AsRef<Path>, options: &OpenOptions) -> Result<Self> {
        let path = path.as_ref();
        validate_path(path)?;
        if options.busy_timeout > Duration::from_millis(i32::MAX as u64) {
            return Err(Error::Argument(
                "busy timeout exceeds i32::MAX milliseconds".into(),
            ));
        }
        let access = match options.mode {
            OpenMode::OpenOrCreate => {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
            }
            OpenMode::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
            OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
        };
        let connection = Connection::open_with_flags(
            path,
            access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        )?;
        connection.busy_timeout(options.busy_timeout)?;
        if options.mode != OpenMode::ReadOnly && connection.is_readonly("main")? {
            return Err(Error::ReadOnly);
        }
        let mut connection = connection;
        super::initialize_with_mode(&mut connection, options.mode)?;
        super::register_functions(&connection)?;
        Ok(Self { connection })
    }

    pub(super) fn ensure_writable(&self) -> Result<()> {
        if self.connection.is_readonly("main")? {
            return Err(Error::ReadOnly);
        }
        Ok(())
    }
}

pub(super) fn validate_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path == Path::new(":memory:")
        || path
            .to_str()
            .is_some_and(|value| value.starts_with("file:"))
    {
        return Err(Error::Argument(
            "expected a filesystem database path".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_uses_the_documented_lock_timeout() {
        // Use an in-memory connection to inspect SQLite's configured value
        // without depending on scheduling or a held filesystem lock.
        let database = Database::in_memory().unwrap();
        let actual: i64 = database
            .connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actual, 5000);
    }
}
