use std::cell::Cell;

use rusqlite::{Connection, Transaction as SqliteTransaction, TransactionBehavior};

use crate::{
    CompiledRead, Error, Parameters, PreparedRead, QueryOptions, QueryResult, Result,
    UniqueConstraint, planner::Plan,
};

use super::{
    constraints, execute_mutation_plan, execute_read_plan, planner, resources::ExecutionContext,
};

/// An atomic group of Cypher queries, created by [`crate::Database::transaction`].
///
/// Queries share one SQLite transaction and see earlier writes. Only an
/// explicit [`commit`](Self::commit) persists those writes; dropping the handle
/// rolls them back, including when unwinding a panic. Results and summaries
/// describe each query's work and are provisional until commit succeeds.
///
/// Any query or constraint-management error immediately rolls back the whole
/// transaction, including planning or parameter errors. The failing call
/// returns its original error unless rollback itself fails. Subsequent
/// execution or commit returns [`Error::TransactionAborted`]. Consume or drop
/// the handle before reusing its database. Nested transactions and savepoints
/// are not exposed.
#[must_use = "transactions must be committed explicitly or their writes are rolled back"]
pub struct Transaction<'database> {
    // Keep the connection owner alive while prepared statements borrow it.
    // rusqlite rolls back an active transaction when this field is dropped.
    inner: SqliteTransaction<'database>,
    state: TransactionState,
}

/// A cursor can abort its parent without moving the transaction guard from
/// underneath borrowed statements. Mark it inert before attempting rollback.
#[derive(Default)]
pub(super) struct TransactionState {
    aborted: Cell<bool>,
}

impl TransactionState {
    fn check(&self) -> Result<()> {
        if self.aborted.get() {
            Err(Error::TransactionAborted)
        } else {
            Ok(())
        }
    }

    pub(super) fn abort(&self, connection: &Connection) -> Result<()> {
        self.aborted.set(true);
        // SQLite may have rolled back itself (for example after an I/O error).
        // Match Transaction::finish without consuming the borrowed guard.
        if !connection.is_autocommit() {
            connection.execute_batch("ROLLBACK")?;
        }
        Ok(())
    }

    pub(super) fn run<T>(
        &self,
        connection: &Connection,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.check()?;
        let result = operation();
        if result.is_err() {
            self.abort(connection)?;
        }
        result
    }
}

impl<'database> Transaction<'database> {
    pub(super) fn begin(connection: &'database mut Connection) -> Result<Self> {
        let inner = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(Self {
            inner,
            state: TransactionState::default(),
        })
    }

    /// Executes a query without parameters in this transaction.
    ///
    /// An error aborts all queries in the transaction, including earlier ones
    /// that succeeded. Hybrid traversal budgets apply separately to each query.
    pub fn execute(&mut self, cypher: &str) -> Result<QueryResult> {
        self.execute_with(cypher, &Parameters::new())
    }

    /// Executes a query with named parameters in this transaction.
    ///
    /// Planning and parameter validation finish before this query executes.
    /// Any error rolls back the entire transaction and prevents a later commit.
    pub fn execute_with(&mut self, cypher: &str, parameters: &Parameters) -> Result<QueryResult> {
        self.execute_with_options(cypher, parameters, &QueryOptions::default())
    }

    /// Executes a query with resource controls that reset for this call.
    /// Cancellation, deadlines, and budget failures abort and roll back the
    /// whole transaction, including all earlier successful statements.
    pub fn execute_with_options(
        &mut self,
        cypher: &str,
        parameters: &Parameters,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        self.run(|transaction| {
            let context = ExecutionContext::new(options);
            context.check()?;
            let plan = planner::compile(cypher)?;
            context.run(transaction, || match plan {
                Plan::Read(plan) => execute_read_plan(transaction, &plan, parameters, &context),
                Plan::Mutate(plan) => {
                    execute_mutation_plan(transaction, (*plan).resolve(parameters)?, &context)
                }
            })
        })
    }

    /// Executes a compiled read without named parameters in this transaction.
    pub fn execute_compiled(&mut self, read: &CompiledRead) -> Result<QueryResult> {
        self.execute_compiled_with(read, &Parameters::new())
    }

    /// Executes a compiled read with fresh bindings in this transaction.
    pub fn execute_compiled_with(
        &mut self,
        read: &CompiledRead,
        parameters: &Parameters,
    ) -> Result<QueryResult> {
        self.execute_compiled_with_options(read, parameters, &QueryOptions::default())
    }

    /// Executes a compiled read with fresh bindings, resource counters, and controls.
    ///
    /// Uses the parent's snapshot and sees earlier writes. Any binding,
    /// execution, resource, or control error immediately aborts and rolls back
    /// the whole transaction. Successful results remain provisional until commit.
    /// The compiled plan remains reusable after the parent is closed or aborted.
    pub fn execute_compiled_with_options(
        &mut self,
        read: &CompiledRead,
        parameters: &Parameters,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        self.run(|transaction| {
            let context = ExecutionContext::new(options);
            context.check()?;
            context.run(transaction, || {
                execute_read_plan(transaction, &read.plan, parameters, &context)
            })
        })
    }

    /// Prepares a read-only query that sees this transaction's earlier writes.
    ///
    /// Closing or dropping a cursor leaves the transaction open. Drop the
    /// prepared read before executing another query or committing. Any error
    /// from preparation, cursor creation, or iteration aborts the whole
    /// transaction, including earlier writes. Returned rows are provisional
    /// until the enclosing transaction commits.
    ///
    /// ```
    /// use crabnet::Database;
    /// let mut db = Database::in_memory()?;
    /// let mut tx = db.transaction()?;
    /// tx.execute("CREATE (:Person {name:'Ada'})")?;
    /// let mut read = tx.prepare_read("MATCH (p:Person) RETURN p.name")?;
    /// let row = read.cursor()?.next().unwrap()?;
    /// drop(read);
    /// tx.execute("MATCH (p:Person) SET p.checked = true")?;
    /// tx.commit()?;
    /// # Ok::<(), crabnet::Error>(())
    /// ```
    ///
    /// A prepared read prevents concurrent queries on its transaction:
    ///
    /// ```compile_fail
    /// # use crabnet::Database;
    /// # let mut db = Database::in_memory().unwrap();
    /// # let mut tx = db.transaction().unwrap();
    /// let mut read = tx.prepare_read("RETURN 1").unwrap();
    /// tx.execute("CREATE ()").unwrap();
    /// let cursor = read.cursor().unwrap();
    /// ```
    ///
    /// It also prevents committing while the borrowed read is still in use:
    ///
    /// ```compile_fail
    /// # use crabnet::Database;
    /// # let mut db = Database::in_memory().unwrap();
    /// # let mut tx = db.transaction().unwrap();
    /// let mut read = tx.prepare_read("RETURN 1").unwrap();
    /// tx.commit().unwrap();
    /// let cursor = read.cursor().unwrap();
    /// ```
    pub fn prepare_read(&mut self, cypher: &str) -> Result<PreparedRead<'_>> {
        self.prepare_read_with(cypher, &Parameters::new())
    }

    /// Prepares a transactional read with owned parameter bindings.
    /// All parameters are checked before execution; an error aborts the transaction.
    pub fn prepare_read_with(
        &mut self,
        cypher: &str,
        parameters: &Parameters,
    ) -> Result<PreparedRead<'_>> {
        self.prepare_read_with_options(cypher, parameters, &QueryOptions::default())
    }

    /// Prepares a transactional read with fresh resource counters per cursor.
    ///
    /// Cursors use the existing transaction and its snapshot, including earlier
    /// writes, rather than opening or closing a separate transaction. The same
    /// incremental/buffered modes apply as on [`crate::Database::prepare_read`].
    /// Cancellation, deadlines, budget failures, and rejected mutations abort
    /// all earlier writes. Reusing a prepared read after an error returns
    /// [`Error::TransactionAborted`]. Deadlines and cancellation tokens are shared
    /// across repeated cursors; counters reset, but the snapshot does not.
    pub fn prepare_read_with_options(
        &mut self,
        cypher: &str,
        parameters: &Parameters,
        options: &QueryOptions,
    ) -> Result<PreparedRead<'_>> {
        self.state.run(&self.inner, || {
            PreparedRead::prepare(&self.inner, cypher, parameters, options, Some(&self.state))
        })
    }

    /// Installs a node uniqueness constraint in this transaction.
    ///
    /// Uses [`crate::Database::create_unique_constraint`] semantics and sees
    /// earlier writes. A conflict aborts the whole transaction. A successful
    /// installation is provisional until commit, and applies to later writes.
    pub fn create_unique_constraint(&mut self, label: &str, property: &str) -> Result<bool> {
        self.run(|transaction| constraints::create(transaction, label, property))
    }

    /// Removes a constraint in this transaction, returning whether it existed.
    /// Rollback restores both the declaration and its enforced property keys.
    pub fn drop_unique_constraint(&mut self, label: &str, property: &str) -> Result<bool> {
        self.run(|transaction| constraints::drop(transaction, label, property))
    }

    /// Lists constraints in label/property order, including this transaction's changes.
    /// An error aborts the transaction, as for queries and constraint changes.
    pub fn unique_constraints(&mut self) -> Result<Vec<UniqueConstraint>> {
        self.run(|transaction| constraints::list(transaction))
    }

    fn run<T>(&mut self, operation: impl FnOnce(&SqliteTransaction<'_>) -> Result<T>) -> Result<T> {
        self.state.run(&self.inner, || operation(&self.inner))
    }

    /// Commits all queries and consumes this handle.
    ///
    /// An aborted transaction returns [`Error::TransactionAborted`]. If SQLite
    /// rejects the commit, its transaction guard attempts rollback on drop;
    /// an error does not guarantee rollback. For example, journal deletion can
    /// succeed before a later I/O error is reported, leaving writes committed.
    /// Resolve the storage problem and reconcile the operation's persisted
    /// outcome before retrying; blindly repeating a write can duplicate it.
    pub fn commit(self) -> Result<()> {
        self.state.check()?;
        self.inner.commit()?;
        Ok(())
    }

    /// Rolls back all queries and consumes this handle.
    ///
    /// Calling this after a query has already aborted the transaction is a
    /// successful no-op. Dropping an active handle also attempts rollback;
    /// calling this method explicitly lets the caller observe rollback errors.
    pub fn rollback(self) -> Result<()> {
        self.inner.finish()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Database, Error};

    #[test]
    fn a_failed_commit_rolls_back_all_queries_and_releases_the_connection() {
        let mut database = Database::in_memory().unwrap();
        database.execute("CREATE (:Saved)").unwrap();
        // Force an error at COMMIT rather than during execute: the extra
        // deferred constraint is a fault injector outside the graph schema.
        database
            .connection
            .execute_batch(
                "CREATE TABLE deferred_check (
                 node_id TEXT REFERENCES nodes(id) DEFERRABLE INITIALLY DEFERRED
             );
             CREATE TRIGGER fail_commit AFTER INSERT ON nodes
             WHEN json_extract(NEW.props, '$.fail') = 1
             BEGIN INSERT INTO deferred_check VALUES ('missing'); END;",
            )
            .unwrap();
        let mut transaction = database.transaction().unwrap();
        transaction.execute("CREATE (:Earlier)").unwrap();
        transaction.execute("CREATE (:Failure {fail: 1})").unwrap();
        assert!(matches!(transaction.commit(), Err(Error::Sqlite(_))));
        assert!(database.connection.is_autocommit());
        assert_eq!(
            database.execute("MATCH (n) RETURN labels(n)").unwrap().rows,
            [vec![serde_json::json!(["Saved"])]]
        );
        let mut transaction = database.transaction().unwrap();
        transaction.execute("CREATE (:Usable)").unwrap();
        transaction.commit().unwrap();
    }

    #[test]
    fn sqlite_automatic_rollback_preserves_the_original_error() {
        let mut database = Database::in_memory().unwrap();
        database
            .connection
            .execute_batch(
                "CREATE TRIGGER abort_insert BEFORE INSERT ON nodes
             WHEN json_extract(NEW.props, '$.fail') = 1
             BEGIN SELECT RAISE(ROLLBACK, 'forced rollback'); END;",
            )
            .unwrap();
        let mut transaction = database.transaction().unwrap();
        transaction.execute("CREATE (:Earlier)").unwrap();
        let error = transaction
            .execute("CREATE (:Failure {fail: 1})")
            .unwrap_err();
        assert!(matches!(error, Error::Sqlite(_)));
        assert!(error.to_string().contains("forced rollback"), "{error}");
        assert!(matches!(
            transaction.commit(),
            Err(Error::TransactionAborted)
        ));
        assert!(database.connection.is_autocommit());
        assert!(
            database
                .execute("MATCH (n) RETURN n")
                .unwrap()
                .rows
                .is_empty()
        );
        database.execute("CREATE (:Usable)").unwrap();
    }
}
