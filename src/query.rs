use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub(crate) const DEFAULT_TRAIL_STATE_LIMIT: usize = 100_000;
pub(crate) const DEFAULT_TRAIL_PAYLOAD_LIMIT: usize = 128 * 1024 * 1024;
pub(crate) const DEFAULT_TRAIL_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

/// A clonable, thread-safe signal for cancelling query execution.
///
/// Cancellation is permanent for this token and every clone. Use a new token
/// for a later independent query. Cancellation is cooperative: SQLite checks
/// periodically and Rust operators check between units of work.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Creates a token that has not been cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation of every query using this token.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Limits for Rust-backed deep/open-ended traversal and shortest-path search.
///
/// These limits do not apply to bounded traversals executed entirely in SQLite.
/// Zero is a real limit, not a request to disable accounting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraversalLimits {
    /// Total explored states across every traversal producer in one query.
    pub max_states: usize,
    /// Total compact JSON bytes passed from traversal producers to SQLite.
    pub max_payload_bytes: usize,
    /// Peak retained traversal memory, using deterministic per-ID costs.
    /// This is an accounting budget, not a bound on process memory.
    pub max_memory_bytes: usize,
}

impl Default for TraversalLimits {
    fn default() -> Self {
        Self {
            max_states: DEFAULT_TRAIL_STATE_LIMIT,
            max_payload_bytes: DEFAULT_TRAIL_PAYLOAD_LIMIT,
            max_memory_bytes: DEFAULT_TRAIL_MEMORY_LIMIT,
        }
    }
}

/// Resource controls for one query, including all its branches and write stages.
///
/// Existing execution methods use these defaults. Counters reset on each call,
/// including within an application transaction, and for each prepared-read
/// cursor. Limits fail with a structured error rather than silently truncating.
/// A write failure rolls back the statement, or the entire explicit transaction.
/// Read cursors can report errors after yielding earlier rows; an error from a
/// transactional cursor aborts all writes in its enclosing transaction.
///
/// Deadlines and cancellation are checked around planning, periodically inside
/// SQLite, and between Rust work units. They do not preempt parsing, a single
/// scalar/aggregate callback, an allocation, or SQLite lock/I/O waits. Use
/// [`crate::Database::prepare_read_with_options`] or
/// [`crate::Transaction::prepare_read_with_options`] for cursor consumption; the
/// `execute` methods remain eager.
///
/// ```
/// use crabnet::{Database, Parameters, QueryOptions};
/// use std::time::{Duration, Instant};
///
/// let mut database = Database::in_memory()?;
/// let options = QueryOptions {
///     deadline: Some(Instant::now() + Duration::from_secs(5)),
///     max_result_rows: Some(100),
///     max_materialized_bytes: Some(16 * 1024 * 1024),
///     ..QueryOptions::default()
/// };
/// let rows = database.execute_with_options("RETURN 1", &Parameters::new(), &options)?;
/// assert_eq!(rows.rows.len(), 1);
/// # Ok::<(), crabnet::Error>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct QueryOptions {
    /// Budgets for Rust-backed traversal. Defaults preserve the existing limits.
    pub traversal: TraversalLimits,
    /// Absolute deadline, checked cooperatively. `None` means no deadline.
    pub deadline: Option<Instant>,
    /// Optional shared cancellation signal.
    pub cancellation: Option<CancellationToken>,
    /// SQLite progress callback budget across all statements in this query.
    /// When set, the callback runs at SQLite's one-VM-instruction interval.
    /// This adds overhead and is not stable across SQLite versions or plans.
    pub max_sqlite_steps: Option<u64>,
    /// Maximum final rows, after DISTINCT, pagination, and UNION processing.
    pub max_result_rows: Option<usize>,
    /// Maximum final value bytes under the deterministic accounting described
    /// in `docs/semantics.md`: 8 per row/value/container, UTF-8 string/key bytes,
    /// and recursively counted entity and path contents. Not allocator bytes.
    pub max_result_bytes: Option<usize>,
    /// Cumulative rows fetched by query fragments, mutation row sources,
    /// traversal inputs, and hydration. Repeated reads count again.
    pub max_materialized_rows: Option<usize>,
    /// Cumulative UTF-8 bytes in encoded cells at those same row boundaries.
    /// This excludes SQLite's internal storage, Rust allocator overhead, and
    /// parameter/planner memory; it is not a whole-query heap limit.
    pub max_materialized_bytes: Option<usize>,
}
