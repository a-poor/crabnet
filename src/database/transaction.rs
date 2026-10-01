use rusqlite::{Connection, Transaction as SqliteTransaction, TransactionBehavior};

use crate::{Error, Parameters, QueryResult, Result, UniqueConstraint, planner::Plan};

use super::{
    constraints, execute_mutation_plan, execute_read_plan, planner, traversal::TrailBudget,
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
    // Taking the inner transaction makes an aborted handle permanently inert.
    // rusqlite also rolls back an active transaction when this field is dropped.
    inner: Option<SqliteTransaction<'database>>,
}

impl<'database> Transaction<'database> {
    pub(super) fn begin(connection: &'database mut Connection) -> Result<Self> {
        let inner = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(Self { inner: Some(inner) })
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
        self.run(|transaction| match planner::compile(cypher)? {
            Plan::Read(plan) => {
                execute_read_plan(transaction, plan, parameters, &mut TrailBudget::default())
            }
            Plan::Mutate(plan) => execute_mutation_plan(transaction, (*plan).resolve(parameters)?),
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
        let transaction = self.inner.as_ref().ok_or(Error::TransactionAborted)?;
        let result = operation(transaction);
        if result.is_err()
            && let Some(transaction) = self.inner.take()
        {
            // finish() rolls back unless SQLite already rolled back on its own
            // (for example after an I/O error). Do not mask the query error with
            // a redundant ROLLBACK in that case.
            transaction.finish()?;
        }
        result
    }

    /// Commits all queries and consumes this handle.
    ///
    /// An aborted transaction returns [`Error::TransactionAborted`]. If SQLite
    /// rejects the commit, its transaction guard attempts rollback on drop;
    /// the returned error must be handled before assuming writes persisted.
    pub fn commit(self) -> Result<()> {
        self.inner.ok_or(Error::TransactionAborted)?.commit()?;
        Ok(())
    }

    /// Rolls back all queries and consumes this handle.
    ///
    /// Calling this after a query has already aborted the transaction is a
    /// successful no-op. Dropping an active handle also attempts rollback;
    /// calling this method explicitly lets the caller observe rollback errors.
    pub fn rollback(self) -> Result<()> {
        if let Some(transaction) = self.inner {
            transaction.finish()?;
        }
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
