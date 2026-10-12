use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};

use rusqlite::Connection;

use super::traversal::TrailBudget;
use crate::{CypherValue, Error, QueryOptions, Result};

/// Shared with the SQLite progress callback, but never with another query.
#[derive(Clone)]
pub(super) struct QueryControl {
    options: QueryOptions,
    state: Arc<QueryState>,
}

#[derive(Default)]
struct QueryState {
    stopped: AtomicU8,
    sqlite_steps: AtomicU64,
    materialized_rows: AtomicUsize,
    materialized_bytes: AtomicUsize,
}

impl QueryControl {
    pub(super) fn new(options: &QueryOptions) -> Self {
        Self {
            options: options.clone(),
            state: Arc::new(QueryState::default()),
        }
    }

    pub(super) fn check(&self) -> Result<()> {
        if self.options.deadline.is_none()
            && self.options.cancellation.is_none()
            && self.options.max_sqlite_steps.is_none()
        {
            return Ok(());
        }
        let reason = if self
            .options
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            1
        } else if self
            .options
            .deadline
            .is_some_and(|deadline| now() >= deadline)
        {
            2
        } else {
            0
        };
        if reason != 0 {
            self.stop(reason);
        }
        match self.state.stopped.load(Ordering::Relaxed) {
            0 => Ok(()),
            1 => Err(Error::Cancelled),
            2 => Err(Error::DeadlineExceeded),
            _ => Err(Error::ResourceLimit(
                "SQLite instruction budget exhausted".into(),
            )),
        }
    }

    /// Charge cells while they still belong to SQLite, before copying them.
    pub(super) fn record_sql_row(&self, row: &rusqlite::Row<'_>, columns: usize) -> Result<()> {
        self.check()?;
        let options = &self.options;
        if options.max_materialized_rows.is_none() && options.max_materialized_bytes.is_none() {
            return Ok(());
        }
        let bytes = (0..columns).try_fold(0_usize, |bytes, index| -> Result<_> {
            let size = match row.get_ref(index)? {
                rusqlite::types::ValueRef::Text(value) | rusqlite::types::ValueRef::Blob(value) => {
                    value.len()
                }
                rusqlite::types::ValueRef::Null => 0,
                _ => 8,
            };
            Ok(bytes.saturating_add(size))
        })?;
        let rows = self
            .state
            .materialized_rows
            .load(Ordering::Relaxed)
            .saturating_add(1);
        let bytes = self
            .state
            .materialized_bytes
            .load(Ordering::Relaxed)
            .saturating_add(bytes);
        check_limit("materialized rows", rows, options.max_materialized_rows)?;
        check_limit("materialized bytes", bytes, options.max_materialized_bytes)?;
        self.state.materialized_rows.store(rows, Ordering::Relaxed);
        self.state
            .materialized_bytes
            .store(bytes, Ordering::Relaxed);
        Ok(())
    }

    fn stop(&self, reason: u8) {
        let _ =
            self.state
                .stopped
                .compare_exchange(0, reason, Ordering::Relaxed, Ordering::Relaxed);
    }
}

pub(super) struct ExecutionContext {
    pub(super) trails: Rc<RefCell<TrailBudget>>,
    pub(super) control: QueryControl,
}

impl ExecutionContext {
    pub(super) fn new(options: &QueryOptions) -> Self {
        let control = QueryControl::new(options);
        Self {
            trails: Rc::new(RefCell::new(TrailBudget::configured(
                &options.traversal,
                control.clone(),
            ))),
            control,
        }
    }

    #[cfg(test)]
    pub(super) fn with_trails(budget: TrailBudget) -> Self {
        Self {
            trails: Rc::new(RefCell::new(budget)),
            ..Self::new(&QueryOptions::default())
        }
    }

    pub(super) fn check(&self) -> Result<()> {
        self.control.check()
    }

    /// The guard is removed before the caller commits or rolls back. In
    /// particular, an expired deadline must never interrupt ROLLBACK.
    pub(super) fn run<T>(
        &self,
        connection: &Connection,
        body: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.check()?;
        let options = &self.control.options;
        let enabled = options.deadline.is_some()
            || options.cancellation.is_some()
            || options.max_sqlite_steps.is_some();
        let guard = if enabled {
            let control = self.control.clone();
            let limit = options.max_sqlite_steps;
            connection.progress_handler(
                if limit.is_some() { 1 } else { 1000 },
                Some(move || {
                    if let Some(limit) = limit {
                        // One context can span multiple cursor advances. Removing
                        // the callback between advances must not reset its budget.
                        let steps = control
                            .state
                            .sqlite_steps
                            .try_update(Ordering::Relaxed, Ordering::Relaxed, |steps| {
                                Some(steps.saturating_add(1))
                            })
                            .expect("the step counter always accepts an update");
                        if steps >= limit {
                            control.stop(3);
                        }
                    }
                    control.check().is_err()
                }),
            )?;
            Some(ProgressGuard(connection))
        } else {
            None
        };
        let result = body();
        drop(guard);
        match result {
            Err(Error::Sqlite(ref error))
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) =>
            {
                self.check()?;
                result
            }
            Ok(value) => {
                self.check()?;
                Ok(value)
            }
            error => error,
        }
    }

    pub(super) fn record_sql_row(&self, row: &rusqlite::Row<'_>, columns: usize) -> Result<()> {
        self.control.record_sql_row(row, columns)
    }

    pub(super) fn result_rows(&self, rows: usize) -> Result<()> {
        self.check()?;
        check_limit("result rows", rows, self.control.options.max_result_rows)
    }

    pub(super) fn counts_result_bytes(&self) -> bool {
        self.control.options.max_result_bytes.is_some()
    }

    pub(super) fn admit_result_bytes(&self, amount: usize, bytes: &mut usize) -> Result<()> {
        self.check()?;
        *bytes = bytes.saturating_add(amount);
        check_limit(
            "result bytes",
            *bytes,
            self.control.options.max_result_bytes,
        )
    }

    pub(super) fn result_row(&self, row: &[CypherValue], bytes: &mut usize) -> Result<()> {
        self.check()?;
        if self.control.options.max_result_bytes.is_some() {
            *bytes = bytes.saturating_add(8);
            for value in row {
                *bytes = bytes.saturating_add(self.value_bytes(value)?);
            }
            check_limit(
                "result bytes",
                *bytes,
                self.control.options.max_result_bytes,
            )?;
        }
        Ok(())
    }

    pub(super) fn value_bytes(&self, value: &CypherValue) -> Result<usize> {
        self.check()?;
        let payload = match value {
            CypherValue::Null
            | CypherValue::Boolean(_)
            | CypherValue::Integer(_)
            | CypherValue::Float(_) => 0,
            CypherValue::String(value) => value.len(),
            CypherValue::List(values) => self.values_bytes(values.iter())?,
            CypherValue::Map(values) => self.properties_bytes(values)?,
            CypherValue::Node(node) => self.node_bytes(node)?,
            CypherValue::Relationship(edge) => self.relationship_bytes(edge)?,
            CypherValue::Path(path) => {
                let mut bytes = 16_usize; // two containers
                for node in &path.nodes {
                    bytes = bytes
                        .saturating_add(8)
                        .saturating_add(self.node_bytes(node)?);
                }
                for edge in &path.relationships {
                    bytes = bytes
                        .saturating_add(8)
                        .saturating_add(self.relationship_bytes(edge)?);
                }
                bytes
            }
        };
        Ok(8_usize.saturating_add(payload))
    }

    fn values_bytes<'a>(&self, mut values: impl Iterator<Item = &'a CypherValue>) -> Result<usize> {
        values.try_fold(0_usize, |bytes, value| {
            Ok(bytes.saturating_add(self.value_bytes(value)?))
        })
    }

    fn properties_bytes(
        &self,
        properties: &std::collections::BTreeMap<String, CypherValue>,
    ) -> Result<usize> {
        properties.iter().try_fold(0_usize, |bytes, (key, value)| {
            Ok(bytes
                .saturating_add(key.len())
                .saturating_add(self.value_bytes(value)?))
        })
    }

    pub(super) fn node_bytes(&self, node: &crate::NodeValue) -> Result<usize> {
        self.check()?;
        let mut bytes = 16_usize.saturating_add(node.element_id.len()); // labels and properties containers
        for label in &node.labels {
            bytes = bytes.saturating_add(8).saturating_add(label.len());
        }
        Ok(bytes.saturating_add(self.properties_bytes(&node.properties)?))
    }

    pub(super) fn relationship_bytes(&self, edge: &crate::RelationshipValue) -> Result<usize> {
        self.check()?;
        Ok(8_usize
            .saturating_add(edge.element_id.len())
            .saturating_add(edge.relationship_type.len())
            .saturating_add(edge.start_node_element_id.len())
            .saturating_add(edge.end_node_element_id.len())
            .saturating_add(self.properties_bytes(&edge.properties)?))
    }
}

fn check_limit(name: &str, used: usize, limit: Option<usize>) -> Result<()> {
    if let Some(limit) = limit
        && used > limit
    {
        return Err(Error::ResourceLimit(format!(
            "{name} exceeded the {limit} budget"
        )));
    }
    Ok(())
}

struct ProgressGuard<'a>(&'a Connection);

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        // This connection is owned by Database and was accepted on installation.
        let _ = self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

fn now() -> std::time::Instant {
    #[cfg(test)]
    if let Some(now) = TEST_NOW.with(std::cell::Cell::get) {
        return now;
    }
    std::time::Instant::now()
}

// The override is local to one test thread, so parallel tests and SQLite
// progress callbacks on other connections keep using their own clocks.
#[cfg(test)]
thread_local! {
    static TEST_NOW: std::cell::Cell<Option<std::time::Instant>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(super) struct TestClock {
    previous: Option<std::time::Instant>,
    // A thread-local override must be restored on the thread that set it.
    _thread: std::marker::PhantomData<Rc<()>>,
}

#[cfg(test)]
impl TestClock {
    pub(super) fn at(now: std::time::Instant) -> Self {
        Self {
            previous: TEST_NOW.with(|clock| clock.replace(Some(now))),
            _thread: std::marker::PhantomData,
        }
    }

    pub(super) fn advance(&self, duration: std::time::Duration) {
        TEST_NOW.with(|clock| clock.set(Some(clock.get().unwrap() + duration)));
    }
}

#[cfg(test)]
impl Drop for TestClock {
    fn drop(&mut self) {
        TEST_NOW.with(|clock| clock.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancellationToken;
    use std::sync::mpsc;

    #[test]
    fn sqlite_cancellation_from_another_thread_removes_the_progress_handler() {
        let connection = Connection::open_in_memory().unwrap();
        let token = CancellationToken::new();
        let other = token.clone();
        let (started, start) = mpsc::channel();
        let (done, cancelled) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            start.recv().unwrap();
            other.cancel();
            done.send(()).unwrap();
        });
        connection
            .create_scalar_function(
                "signal",
                0,
                rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                move |_| {
                    started.send(()).unwrap();
                    cancelled.recv().unwrap();
                    Ok(1)
                },
            )
            .unwrap();
        let context = ExecutionContext::new(&QueryOptions {
            cancellation: Some(token),
            ..QueryOptions::default()
        });
        let result = context.run(&connection, || {
            Ok(connection.query_row("WITH RECURSIVE n(x) AS (SELECT signal() UNION ALL SELECT x+1 FROM n WHERE x < 1000000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0))?)
        });
        worker.join().unwrap();
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        // Enough work to invoke a leaked callback, if it was not removed.
        let count: i64 = connection.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 1000) SELECT count(*) FROM n", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1000);
    }

    #[test]
    fn panic_also_removes_the_progress_handler() {
        let connection = Connection::open_in_memory().unwrap();
        let context = ExecutionContext::new(&QueryOptions {
            max_sqlite_steps: Some(0),
            ..QueryOptions::default()
        });
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = context.run(&connection, || panic!("simulated operator panic"));
        }));
        assert!(panic.is_err());
        assert_eq!(
            connection
                .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
