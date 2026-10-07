use std::{collections::VecDeque, iter::FusedIterator};

use rusqlite::{Connection, Rows, Statement, Transaction, TransactionBehavior, params_from_iter};

use super::{
    Database, ExecutionContext, decode_pending_row, execute_prepared_read_plan, finish_pending_row,
};
use crate::{
    CypherValue, Error, Parameters, QueryOptions, Result,
    planner::{
        self, Plan, PreparedFragmentArgument, PreparedPhysicalOperator, PreparedReadPlan,
        PreparedSqlFragment, UnionKind,
    },
};

/// How a prepared read supplies rows to Rust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadMode {
    /// SQLite fragments, optionally joined by UNION ALL, are decoded one row
    /// per cursor advance, without Rust runtime producers.
    /// SQLite may still buffer internally for sorting, grouping, or traversal.
    Incremental,
    /// Rust operators require the existing eager executor. Cursor creation
    /// computes the complete result and releases its snapshot before returning.
    Buffered,
}

/// A read-only query with resolved parameters, borrowing its database.
///
/// Inspect [`Self::mode`] before starting a cursor. Each [`Self::cursor`] call
/// starts a fresh snapshot and resource counters with the same bound parameters
/// and options. Drop this handle to use the database again. Parameters are owned;
/// the input query text, parameter map, and options need not remain alive.
pub struct PreparedRead<'db> {
    connection: &'db Connection,
    plan: PreparedReadPlan,
    statements: Vec<Statement<'db>>,
    options: QueryOptions,
}

/// An iterator yielding owned, typed rows from one read snapshot.
///
/// Rows already yielded remain valid after closing the cursor. Incremental
/// execution can yield rows before a later error: an error is yielded once,
/// then the iterator is exhausted. Exhaustion, error, or drop releases the
/// SQLite cursor and snapshot. Merely pausing iteration keeps the snapshot
/// open and can block another connection's commit in rollback-journal mode.
/// No query runs while the cursor is idle; deadlines are checked on resumption.
///
/// This cursor borrows its [`PreparedRead`], which exclusively borrows the
/// database. It is not available inside an application [`crate::Transaction`].
#[must_use = "consume or drop the cursor to release its read snapshot"]
pub struct ReadCursor<'read> {
    // Field order resets the statement before rolling back the snapshot,
    // including during unwinding and early drop.
    state: Option<CursorState<'read>>,
    transaction: Option<Transaction<'read>>,
    connection: &'read Connection,
    columns: &'read [String],
    context: ExecutionContext,
    result_rows: usize,
    result_bytes: usize,
}

enum CursorState<'read> {
    Incremental(VecDeque<FragmentRows<'read>>),
    Buffered(std::vec::IntoIter<Vec<CypherValue>>),
}

struct FragmentRows<'read> {
    rows: Rows<'read>,
    shapes: &'read [crate::planner::OutputShape],
}

/// Preserve branch order and accept a union only when every leaf can stream.
/// A distinct, hydration, pagination, or runtime-producer boundary keeps the
/// whole query on the eager fallback, with its existing accounting and order.
fn incremental_fragments(root: &PreparedPhysicalOperator) -> Option<Vec<&PreparedSqlFragment>> {
    let mut pending = vec![root];
    let mut fragments = Vec::new();
    while let Some(operator) = pending.pop() {
        match operator {
            PreparedPhysicalOperator::Sqlite(fragment) if fragment.inputs.is_empty() => {
                fragments.push(fragment);
            }
            PreparedPhysicalOperator::Union {
                left,
                right,
                kind: UnionKind::All,
            } => {
                pending.push(right);
                pending.push(left);
            }
            _ => return None,
        }
    }
    Some(fragments)
}

impl Database {
    /// Prepares a read-only query without parameters for cursor consumption.
    ///
    /// ```
    /// use crabnet::{Database, ReadMode};
    /// let mut database = Database::in_memory()?;
    /// database.execute("CREATE (:Person {name: 'Ada'})")?;
    /// let mut read = database.prepare_read("MATCH (p:Person) RETURN p.name")?;
    /// assert_eq!(read.mode(), ReadMode::Incremental);
    /// for row in read.cursor()? {
    ///     let row = row?;
    ///     assert_eq!(row.len(), 1);
    /// }
    /// drop(read);
    /// database.execute("CREATE (:Person {name: 'Grace'})")?;
    /// # Ok::<(), crabnet::Error>(())
    /// ```
    ///
    /// The prepared read exclusively borrows the database:
    ///
    /// ```compile_fail
    /// # use crabnet::Database;
    /// # let mut database = Database::in_memory().unwrap();
    /// let mut read = database.prepare_read("RETURN 1").unwrap();
    /// database.execute("CREATE ()").unwrap();
    /// let cursor = read.cursor().unwrap();
    /// ```
    pub fn prepare_read(&mut self, cypher: &str) -> Result<PreparedRead<'_>> {
        self.prepare_read_with(cypher, &Parameters::new())
    }

    /// Prepares a read-only query and resolves all named parameters up front.
    pub fn prepare_read_with(
        &mut self,
        cypher: &str,
        parameters: &Parameters,
    ) -> Result<PreparedRead<'_>> {
        self.prepare_read_with_options(cypher, parameters, &QueryOptions::default())
    }

    /// Prepares a read-only query with per-cursor resource controls.
    ///
    /// Mutations return [`Error::Unsupported`] before executing. Planning and
    /// parameter errors are returned here; execution errors can occur when
    /// creating or advancing a cursor. Result limits apply cumulatively and
    /// fail rather than silently truncate. Already yielded rows cannot be
    /// retracted. The absolute deadline and cancellation token also apply
    /// between advances and across repeated cursors from this handle.
    pub fn prepare_read_with_options(
        &mut self,
        cypher: &str,
        parameters: &Parameters,
        options: &QueryOptions,
    ) -> Result<PreparedRead<'_>> {
        let context = ExecutionContext::new(options);
        context.check()?;
        let Plan::Read(plan) = planner::compile(cypher)? else {
            return Err(Error::Unsupported(
                "read cursors cannot execute mutations".into(),
            ));
        };
        let plan = plan.prepare(parameters)?;
        let statements = context.run(&self.connection, || {
            incremental_fragments(&plan.root)
                .unwrap_or_default()
                .into_iter()
                .map(|fragment| {
                    let statement = self.connection.prepare(&fragment.sql)?;
                    if !statement.readonly() {
                        return Err(Error::Data("read fragment is not read-only".into()));
                    }
                    Ok(statement)
                })
                .collect::<Result<Vec<_>>>()
        })?;
        Ok(PreparedRead {
            connection: &self.connection,
            plan,
            statements,
            options: options.clone(),
        })
    }
}

impl PreparedRead<'_> {
    /// Ordered result column names, available before execution.
    pub fn columns(&self) -> &[String] {
        &self.plan.columns
    }

    /// Whether Rust decodes incrementally or buffers the complete result.
    pub fn mode(&self) -> ReadMode {
        if self.statements.is_empty() {
            ReadMode::Buffered
        } else {
            ReadMode::Incremental
        }
    }

    /// Starts a fresh read snapshot and returns its row iterator.
    ///
    /// The snapshot is pinned before this returns, even before the first row
    /// is requested. For [`ReadMode::Buffered`], this executes and validates all
    /// rows immediately, then closes the snapshot. For incremental reads, the
    /// snapshot lasts until exhaustion, error, or drop. Dropping after the last
    /// yielded row also closes it; requesting `None` is not required.
    pub fn cursor(&mut self) -> Result<ReadCursor<'_>> {
        let context = ExecutionContext::new(&self.options);
        context.check()?;
        // Statements and the transaction borrow the same connection. The
        // public mutable database/read borrows prohibit overlapping cursors.
        let transaction =
            Transaction::new_unchecked(self.connection, TransactionBehavior::Deferred)?;
        let state = context.run(self.connection, || {
            // BEGIN DEFERRED alone does not establish a snapshot. This read is
            // control work, not a materialized Cypher row, but counts VM steps.
            self.connection
                .query_row("SELECT count(*) FROM main.sqlite_schema", [], |_| Ok(()))?;
            if !self.statements.is_empty() {
                let fragments = incremental_fragments(&self.plan.root)
                    .expect("only incremental fragments have statements");
                // query() binds parameters but does not step SQLite. Later
                // branches therefore do no execution work until reached.
                let branches = self
                    .statements
                    .iter_mut()
                    .zip(fragments)
                    .map(|(statement, fragment)| {
                        let bindings = fragment
                            .arguments
                            .iter()
                            .map(|argument| match argument {
                                PreparedFragmentArgument::Value(value) => Ok(value),
                                PreparedFragmentArgument::Runtime(_) => Err(Error::Data(
                                    "incremental read has a runtime argument".into(),
                                )),
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok(FragmentRows {
                            rows: statement.query(params_from_iter(bindings))?,
                            shapes: &fragment.output_shapes,
                        })
                    })
                    .collect::<Result<VecDeque<_>>>()?;
                Ok(CursorState::Incremental(branches))
            } else {
                let result =
                    execute_prepared_read_plan(self.connection, self.plan.clone(), true, &context)?;
                Ok(CursorState::Buffered(result.rows.into_iter()))
            }
        })?;
        // Cleanup must happen outside the progress handler: cancellation or a
        // depleted step budget must never interrupt ROLLBACK.
        let transaction = if matches!(state, CursorState::Incremental(_)) {
            Some(transaction)
        } else {
            transaction.rollback()?;
            None
        };
        Ok(ReadCursor {
            state: Some(state),
            transaction,
            connection: self.connection,
            columns: &self.plan.columns,
            context,
            result_rows: 0,
            result_bytes: 0,
        })
    }
}

impl ReadCursor<'_> {
    /// Ordered result column names, shared with the prepared read.
    pub fn columns(&self) -> &[String] {
        self.columns
    }

    fn close(&mut self) -> Result<()> {
        self.state = None;
        if let Some(transaction) = self.transaction.take() {
            transaction.rollback()?;
        }
        Ok(())
    }
}

impl Iterator for ReadCursor<'_> {
    type Item = Result<Vec<CypherValue>>;

    fn next(&mut self) -> Option<Self::Item> {
        let state = self.state.as_mut()?;
        let result = match state {
            CursorState::Incremental(branches) => self.context.run(self.connection, || {
                while let Some(FragmentRows { rows, shapes }) = branches.front_mut() {
                    if let Some(row) = rows.next()? {
                        self.context.record_sql_row(row, shapes.len())?;
                        self.result_rows = self.result_rows.saturating_add(1);
                        self.context.result_rows(self.result_rows)?;
                        let row =
                            finish_pending_row(decode_pending_row(row, shapes, &self.context)?)?;
                        self.context.result_row(&row, &mut self.result_bytes)?;
                        return Ok(Some(row));
                    }
                    // Reset the exhausted statement without releasing the
                    // shared snapshot or resetting any query-wide counters.
                    branches.pop_front();
                }
                Ok(None)
            }),
            CursorState::Buffered(rows) => self.context.check().map(|()| rows.next()),
        };
        match result {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => self.close().err().map(Err),
            Err(error) => {
                let _ = self.close();
                Some(Err(error))
            }
        }
    }
}

impl FusedIterator for ReadCursor<'_> {}
