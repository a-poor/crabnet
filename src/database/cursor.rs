use std::{collections::VecDeque, iter::FusedIterator};

mod distinct;
use distinct::SeenRows;

use rusqlite::{
    CachedStatement, Connection, Rows, Statement, Transaction, TransactionBehavior,
    params_from_iter, types::Value as SqlValue,
};

use super::{
    Database, ExecutionContext, PathHydrator, PendingRow, decode_pending_row,
    execute_prepared_read_plan, finish_pending_row, transaction::TransactionState,
    traversal::TrailStream,
};
use crate::{
    CypherValue, Error, Parameters, QueryOptions, Result,
    planner::{
        self, OutputShape, Plan, PreparedFragmentArgument, PreparedPhysicalOperator,
        PreparedReadPlan, PreparedSqlFragment, PreparedValueOperator, ResolvedTrailExpansion,
        UnionKind,
    },
};

/// How a prepared read supplies rows to Rust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadMode {
    /// SQLite fragments and eligible single-trail projections, including
    /// rowwise WITH stages, compose through UNION, DISTINCT, pagination, and
    /// batch hydration without buffering the
    /// complete result. Rust trails retain graph inputs and a search frontier.
    /// DISTINCT retains seen rows; hydration retains one batch of entities.
    /// SQLite may still buffer internally for sorting, grouping, or traversal.
    Incremental,
    /// The plan requires the existing eager executor. Cursor creation
    /// computes the complete result. Standalone reads release their snapshot
    /// before returning; an application transaction remains open.
    Buffered,
}

/// A read-only query with resolved parameters, borrowing a database or transaction.
///
/// Inspect [`Self::mode`] before starting a cursor. Each [`Self::cursor`] call
/// starts fresh resource counters with the same bound parameters and options.
/// Standalone reads start a fresh snapshot; transactional reads reuse the parent
/// transaction and see its earlier writes. Drop this handle to use its database
/// or transaction again. Parameters are owned; the input query text, parameter
/// map, and options need not remain alive.
pub struct PreparedRead<'db> {
    connection: &'db Connection,
    plan: PreparedReadPlan,
    statements: Vec<Option<Statement<'db>>>,
    options: QueryOptions,
    parent: Option<&'db TransactionState>,
}

/// An iterator yielding owned, typed rows from one read snapshot.
///
/// Rows already yielded remain valid after closing the cursor. Incremental
/// execution can yield rows before a later error: an error is yielded once,
/// then the iterator is exhausted. Exhaustion or drop closes the SQLite cursor
/// and any standalone snapshot. An application transaction stays open on success;
/// a query error rolls it back and makes it unusable. Merely pausing iteration
/// keeps the snapshot open and can block another connection's commit in
/// rollback-journal mode.
/// No query runs while the cursor is idle; deadlines are checked on resumption.
/// Path hydration reads ahead in batches after the first row. An error while
/// filling or loading a batch discards that batch and terminates the cursor.
///
/// This cursor borrows its [`PreparedRead`], which exclusively borrows its
/// database or application [`crate::Transaction`]. Transactional rows remain
/// provisional until the enclosing transaction commits.
#[must_use = "consume or drop the cursor to close its read statements"]
pub struct ReadCursor<'read> {
    // Field order resets the statement before rolling back the snapshot,
    // including during unwinding and early drop.
    state: Option<CursorState<'read>>,
    transaction: Option<Transaction<'read>>,
    parent: Option<&'read TransactionState>,
    connection: &'read Connection,
    columns: &'read [String],
    context: ExecutionContext,
    result_rows: usize,
    result_bytes: usize,
}

enum CursorState<'read> {
    Incremental {
        stream: Stream<'read>,
        hydration: Option<Hydration>,
    },
    Buffered(std::vec::IntoIter<Vec<CypherValue>>),
}

// Keep first-row latency low, then amortize entity reads during continued
// consumption. This bounds row count, not bytes: one path can be arbitrarily
// large. Caches are replaced, never accumulated across batches.
const HYDRATION_BATCH_ROWS: usize = 64;

struct Hydration {
    pending: std::vec::IntoIter<PendingRow>,
    cache: PathHydrator,
    batch_rows: usize,
}

impl Hydration {
    fn new() -> Self {
        Self {
            pending: Vec::new().into_iter(),
            cache: PathHydrator::default(),
            batch_rows: 1,
        }
    }

    fn next(
        &mut self,
        input: &mut Stream<'_>,
        connection: &Connection,
        context: &ExecutionContext,
    ) -> Result<Option<PendingRow>> {
        if self.pending.len() == 0 {
            self.cache = PathHydrator::default();
            let mut rows = Vec::with_capacity(self.batch_rows);
            for _ in 0..self.batch_rows {
                let Some(row) = input.next(context)? else {
                    break;
                };
                rows.push(row);
            }
            self.cache = PathHydrator::load(connection, &rows, context)?;
            self.pending = rows.into_iter();
            self.batch_rows = HYDRATION_BATCH_ROWS;
        }
        Ok(self.pending.next())
    }
}

struct FragmentRows<'read> {
    rows: Rows<'read>,
    shapes: &'read [crate::planner::OutputShape],
}

struct TrailRows<'read> {
    connection: &'read Connection,
    statement: CachedStatement<'read>,
    spec: &'read ResolvedTrailExpansion,
    bindings: Vec<SqlValue>,
    runtime_index: usize,
    skip: usize,
    shapes: &'read [OutputShape],
    producer: Option<TrailStream<'read>>,
    finished: bool,
}

impl TrailRows<'_> {
    fn next(&mut self, context: &ExecutionContext) -> Result<Option<PendingRow>> {
        if self.finished {
            return Ok(None);
        }
        if self.producer.is_none() {
            self.producer = Some(TrailStream::new(
                self.connection,
                self.spec,
                context.trails.clone(),
            )?);
        }
        loop {
            let Some(encoded) = self.producer.as_mut().expect("started producer").next()? else {
                self.producer = None;
                self.finished = true;
                return Ok(None);
            };
            self.bindings[self.runtime_index] = SqlValue::Text(encoded);
            *self.bindings.last_mut().expect("cursor skip binding") =
                SqlValue::Integer(i64::from(self.skip > 0));
            let mut rows = self.statement.query(params_from_iter(&self.bindings))?;
            let Some(row) = rows.next()? else {
                continue;
            };
            context.record_sql_row(row, self.shapes.len())?;
            let row = (self.skip == 0)
                .then(|| decode_pending_row(row, self.shapes, context))
                .transpose()?;
            // Endpoint identity joins and rowwise WITH stages produce at most
            // one row per trail. Fail closed if a future planner breaks that rule.
            if rows.next()?.is_some() {
                return Err(Error::Data(
                    "streaming trail projection produced multiple rows".into(),
                ));
            }
            if row.is_some() {
                return Ok(row);
            }
            self.skip -= 1;
        }
    }
}

enum Stream<'read> {
    Sqlite(FragmentRows<'read>),
    Trail(Box<TrailRows<'read>>),
    Union(VecDeque<Self>),
    Distinct {
        input: Box<Self>,
        seen: SeenRows,
    },
    Paginate {
        input: Box<Self>,
        skip: usize,
        remaining: Option<usize>,
    },
}

impl<'read> Stream<'read> {
    fn build(plan: &PreparedPhysicalOperator, fragments: &mut impl Iterator<Item = Self>) -> Self {
        match plan {
            PreparedPhysicalOperator::Sqlite(_) => fragments
                .next()
                .expect("each incremental fragment has a source"),
            PreparedPhysicalOperator::SemanticDistinct { input } => Self::Distinct {
                input: Box::new(Self::build(input, fragments)),
                seen: SeenRows::default(),
            },
            PreparedPhysicalOperator::Paginate {
                input,
                offset,
                limit,
            } => Self::Paginate {
                input: Box::new(Self::build(input, fragments)),
                skip: *offset,
                remaining: *limit,
            },
            PreparedPhysicalOperator::Union { kind, .. } => {
                // Consecutive unions share one duplicate policy. Flatten them
                // so a long UNION chain needs only one seen set, while keeping
                // each branch's DISTINCT and pagination in their own scopes.
                let mut pending = vec![plan];
                let mut branches = VecDeque::new();
                while let Some(operator) = pending.pop() {
                    match operator {
                        PreparedPhysicalOperator::Union {
                            left,
                            right,
                            kind: inner,
                        } if inner == kind => {
                            pending.push(right);
                            pending.push(left);
                        }
                        branch => branches.push_back(Self::build(branch, fragments)),
                    }
                }
                let union = Self::Union(branches);
                if *kind == UnionKind::Distinct {
                    Self::Distinct {
                        input: Box::new(union),
                        seen: SeenRows::default(),
                    }
                } else {
                    union
                }
            }
            PreparedPhysicalOperator::BatchHydrate { .. } => {
                unreachable!("hydration is handled at the root")
            }
        }
    }

    fn next(&mut self, context: &ExecutionContext) -> Result<Option<PendingRow>> {
        context.check()?;
        match self {
            Self::Sqlite(FragmentRows { rows, shapes }) => {
                let Some(row) = rows.next()? else {
                    return Ok(None);
                };
                context.record_sql_row(row, shapes.len())?;
                decode_pending_row(row, shapes, context).map(Some)
            }
            Self::Trail(rows) => rows.next(context),
            Self::Union(branches) => {
                while let Some(branch) = branches.front_mut() {
                    if let Some(row) = branch.next(context)? {
                        return Ok(Some(row));
                    }
                    // Release exhausted statements and branch-local seen sets.
                    branches.pop_front();
                }
                Ok(None)
            }
            Self::Distinct { input, seen } => {
                while let Some(row) = input.next(context)? {
                    if seen.insert(&row, context)? {
                        return Ok(Some(row));
                    }
                }
                Ok(None)
            }
            Self::Paginate {
                input,
                skip,
                remaining,
            } => {
                if *remaining == Some(0) {
                    return Ok(None);
                }
                while *skip > 0 {
                    if input.next(context)?.is_none() {
                        return Ok(None);
                    }
                    *skip -= 1;
                }
                let row = input.next(context)?;
                if row.is_some()
                    && let Some(remaining) = remaining
                {
                    *remaining -= 1;
                }
                Ok(row)
            }
        }
    }
}

/// Preserve branch order and accept a union only when every leaf can stream.
/// Only the planner's single-trail projection can stream a runtime producer.
fn incremental_fragments(root: &PreparedPhysicalOperator) -> Option<Vec<&PreparedSqlFragment>> {
    let mut pending = vec![root];
    let mut fragments = Vec::new();
    while let Some(operator) = pending.pop() {
        match operator {
            PreparedPhysicalOperator::Sqlite(fragment)
                if fragment.inputs.is_empty() || fragment.trail_cursor.is_some() =>
            {
                fragments.push(fragment);
            }
            PreparedPhysicalOperator::Union { left, right, .. } => {
                pending.push(right);
                pending.push(left);
            }
            PreparedPhysicalOperator::SemanticDistinct { input }
            | PreparedPhysicalOperator::Paginate { input, .. }
            | PreparedPhysicalOperator::BatchHydrate { input } => pending.push(input),
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
        PreparedRead::prepare(&self.connection, cypher, parameters, options, None)
    }
}

impl<'db> PreparedRead<'db> {
    pub(super) fn prepare(
        connection: &'db Connection,
        cypher: &str,
        parameters: &Parameters,
        options: &QueryOptions,
        parent: Option<&'db TransactionState>,
    ) -> Result<Self> {
        let context = ExecutionContext::new(options);
        context.check()?;
        let Plan::Read(plan) = planner::compile(cypher)? else {
            return Err(Error::Unsupported(
                "read cursors cannot execute mutations".into(),
            ));
        };
        let plan = plan.prepare(parameters)?;
        let statements = context.run(connection, || {
            incremental_fragments(&plan.root)
                .unwrap_or_default()
                .into_iter()
                .map(|fragment| {
                    let sql = fragment
                        .trail_cursor
                        .as_ref()
                        .map_or(fragment.sql.as_str(), |cursor| cursor.sql.as_str());
                    let statement = connection.prepare(sql)?;
                    if !statement.readonly() {
                        return Err(Error::Data("read fragment is not read-only".into()));
                    }
                    // Trail projections restart a cached statement per row.
                    // Validate now; acquire its owned cursor handle on start.
                    Ok(fragment.trail_cursor.is_none().then_some(statement))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        Ok(PreparedRead {
            connection,
            plan,
            statements,
            options: options.clone(),
            parent,
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

    /// Starts a cursor with fresh counters and returns its row iterator.
    ///
    /// For standalone reads, the snapshot is pinned before this returns, even
    /// before the first row is requested. For [`ReadMode::Buffered`], this
    /// executes and validates all rows immediately, then closes the snapshot. For incremental reads, the
    /// snapshot lasts until exhaustion, error, or drop. Dropping after the last
    /// yielded row also closes it; requesting `None` is not required.
    ///
    /// Transactional reads use the parent's existing snapshot and earlier
    /// writes. Success and early drop leave the parent open; any error aborts
    /// it immediately. A prepared read on an aborted transaction cannot be reused.
    pub fn cursor(&mut self) -> Result<ReadCursor<'_>> {
        match self.parent {
            Some(parent) => parent.run(self.connection, || self.open_cursor()),
            None => self.open_cursor(),
        }
    }

    fn open_cursor(&mut self) -> Result<ReadCursor<'_>> {
        let context = ExecutionContext::new(&self.options);
        context.check()?;
        // Statements and the transaction borrow the same connection. The
        // public mutable database/read borrows prohibit overlapping cursors.
        let transaction = if self.parent.is_none() {
            Some(Transaction::new_unchecked(
                self.connection,
                TransactionBehavior::Deferred,
            )?)
        } else {
            None
        };
        let state = context.run(self.connection, || {
            // BEGIN DEFERRED alone does not establish a snapshot. This read is
            // control work, not a materialized Cypher row, but counts VM steps.
            if self.parent.is_none() {
                self.connection
                    .query_row("SELECT count(*) FROM main.sqlite_schema", [], |_| Ok(()))?;
            }
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
                        if let Some(cursor) = &fragment.trail_cursor {
                            let [PreparedValueOperator::TrailExpand { output, spec }] =
                                fragment.inputs.as_slice()
                            else {
                                return Err(Error::Data("invalid streaming trail input".into()));
                            };
                            let mut runtime_index = None;
                            let mut bindings = fragment
                                .arguments
                                .iter()
                                .take(cursor.argument_count)
                                .enumerate()
                                .map(|(index, argument)| match argument {
                                    PreparedFragmentArgument::Value(value) => Ok(value.clone()),
                                    PreparedFragmentArgument::Runtime(slot) if slot == output => {
                                        runtime_index = Some(index);
                                        Ok(SqlValue::Null)
                                    }
                                    _ => Err(Error::Data("invalid streaming runtime slot".into())),
                                })
                                .collect::<Result<Vec<_>>>()?;
                            bindings.push(SqlValue::Integer(0));
                            let stream = Stream::Trail(Box::new(TrailRows {
                                connection: self.connection,
                                statement: self.connection.prepare_cached(&cursor.sql)?,
                                spec,
                                bindings,
                                runtime_index: runtime_index.ok_or_else(|| {
                                    Error::Data("missing streaming runtime slot".into())
                                })?,
                                skip: cursor.offset,
                                shapes: &fragment.output_shapes,
                                producer: None,
                                finished: false,
                            }));
                            return Ok(Stream::Paginate {
                                input: Box::new(stream),
                                skip: 0,
                                remaining: cursor.limit,
                            });
                        }
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
                        Ok(Stream::Sqlite(FragmentRows {
                            rows: statement
                                .as_mut()
                                .expect("SQLite fragment statement")
                                .query(params_from_iter(bindings))?,
                            shapes: &fragment.output_shapes,
                        }))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let (input, hydration) = match &self.plan.root {
                    PreparedPhysicalOperator::BatchHydrate { input } => {
                        (input.as_ref(), Some(Hydration::new()))
                    }
                    input => (input, None),
                };
                Ok(CursorState::Incremental {
                    stream: Stream::build(input, &mut branches.into_iter()),
                    hydration,
                })
            } else {
                let result =
                    execute_prepared_read_plan(self.connection, self.plan.clone(), true, &context)?;
                Ok(CursorState::Buffered(result.rows.into_iter()))
            }
        })?;
        // Cleanup must happen outside the progress handler: cancellation or a
        // depleted step budget must never interrupt ROLLBACK.
        let transaction = if matches!(state, CursorState::Incremental { .. }) {
            transaction
        } else {
            if let Some(transaction) = transaction {
                transaction.rollback()?;
            }
            None
        };
        Ok(ReadCursor {
            state: Some(state),
            transaction,
            parent: self.parent,
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
            CursorState::Incremental { stream, hydration } => {
                self.context.run(self.connection, || {
                    let pending = match hydration {
                        Some(hydration) => {
                            hydration.next(stream, self.connection, &self.context)?
                        }
                        None => stream.next(&self.context)?,
                    };
                    let Some(row) = pending else {
                        return Ok(None);
                    };
                    // Transfers count every input row, including duplicates and
                    // skipped rows. Result budgets count only surviving output.
                    self.result_rows = self.result_rows.saturating_add(1);
                    self.context.result_rows(self.result_rows)?;
                    let row = match hydration {
                        Some(hydration) => hydration.cache.finish_row(
                            row,
                            Some(&mut self.result_bytes),
                            &self.context,
                        )?,
                        None => {
                            let row = finish_pending_row(row)?;
                            self.context.result_row(&row, &mut self.result_bytes)?;
                            row
                        }
                    };
                    Ok(Some(row))
                })
            }
            CursorState::Buffered(rows) => self.context.check().map(|()| rows.next()),
        };
        match result {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => self.close().err().map(Err),
            Err(error) => {
                let _ = self.close();
                // Reset every row source before aborting the parent, and never
                // run rollback under a depleted/cancelled progress callback.
                if let Some(parent) = self.parent
                    && let Err(rollback) = parent.abort(self.connection)
                {
                    return Some(Err(rollback));
                }
                Some(Err(error))
            }
        }
    }
}

impl FusedIterator for ReadCursor<'_> {}
