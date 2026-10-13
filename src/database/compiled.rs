use crate::{
    Error, Parameters, QueryOptions, QueryResult, Result,
    planner::{self, Plan, ReadPlan},
};

use super::{Database, resources::ExecutionContext};

/// An owned, parameter-unbound read plan for repeated eager execution.
///
/// Create one with [`compile_read`], then use
/// [`Database::execute_compiled_with_options`] with fresh bindings and controls
/// on each call. The plan owns its query literals and generated SQL, but retains
/// no database, SQLite statement, snapshot, parameter values, or query counters.
/// Cloning copies the plan; an `Arc<CompiledRead>` can share it across callers.
///
/// A plan can execute on any database opened by the same Crabnet build, including
/// read-only connections. Planning does not specialize to a database's data or
/// uniqueness declarations, so supported writes, constraint changes, and restore
/// do not invalidate it. Opening handles supported storage migrations. Plans
/// cannot be serialized or reused across engine versions; externally modified
/// schemas remain outside the supported database contract.
///
/// This is separate from [`crate::PreparedRead`], which binds parameters and
/// options and borrows a connection for cursor consumption. Compiled execution
/// is eager and returns either the complete result or an error.
#[derive(Clone, Debug)]
pub struct CompiledRead {
    pub(super) plan: ReadPlan,
}

impl CompiledRead {
    /// The ordered output column names, available without binding or execution.
    pub fn columns(&self) -> &[String] {
        &self.plan.columns
    }
}

/// Parses, validates, and compiles a read once, without binding parameters.
///
/// Mutations return [`Error::Unsupported`]. Static query errors are reported
/// here; missing or invalid parameters are checked on every execution. The
/// input query text need not remain alive.
///
/// ```
/// use crabnet::{Database, Parameters, compile_read};
/// use serde_json::json;
///
/// let read = compile_read("MATCH (p:Person {name:$name}) RETURN p.name")?;
/// let mut db = Database::in_memory()?;
/// db.execute("CREATE (:Person {name:'Ada'}), (:Person {name:'Grace'})")?;
/// for name in ["Ada", "Grace"] {
///     let parameters = Parameters::from([("name".into(), json!(name))]);
///     let result = db.execute_compiled_with(&read, &parameters)?;
///     assert_eq!(result.rows[0][0].as_str(), Some(name));
/// }
/// # Ok::<(), crabnet::Error>(())
/// ```
pub fn compile_read(cypher: &str) -> Result<CompiledRead> {
    compile_read_with_options(cypher, &QueryOptions::default())
}

/// Compiles a read with cancellation and deadline checks before and after planning.
///
/// Checks do not preempt parsing or an individual planner operation. Other
/// query budgets account for execution work and do not limit compilation or
/// planner memory. No options are retained; pass execution controls separately
/// to [`Database::execute_compiled_with_options`]. This function borrows no
/// database and cannot abort an application transaction.
pub fn compile_read_with_options(cypher: &str, options: &QueryOptions) -> Result<CompiledRead> {
    let context = ExecutionContext::new(options);
    context.check()?;
    let plan = planner::compile(cypher)?;
    context.check()?;
    match plan {
        Plan::Read(plan) => Ok(CompiledRead { plan }),
        Plan::Mutate(_) => Err(Error::Unsupported(
            "compiled reads cannot execute mutations".into(),
        )),
    }
}

impl Database {
    /// Executes a compiled read without named parameters, with default controls.
    pub fn execute_compiled(&mut self, read: &CompiledRead) -> Result<QueryResult> {
        self.execute_compiled_with(read, &Parameters::new())
    }

    /// Executes a compiled read with fresh parameter bindings and default controls.
    pub fn execute_compiled_with(
        &mut self,
        read: &CompiledRead,
        parameters: &Parameters,
    ) -> Result<QueryResult> {
        self.execute_compiled_with_options(read, parameters, &QueryOptions::default())
    }

    /// Executes a compiled read eagerly with fresh bindings, counters, and options.
    ///
    /// All parameters are validated before query execution. Every branch,
    /// traversal, and hydration read shares one fresh read snapshot. No partial
    /// result is returned on failure. Success or failure releases the snapshot
    /// before returning; the database and compiled plan remain reusable. No
    /// Cypher parsing, lowering, or SQL generation runs during this call.
    pub fn execute_compiled_with_options(
        &mut self,
        read: &CompiledRead,
        parameters: &Parameters,
        options: &QueryOptions,
    ) -> Result<QueryResult> {
        let context = ExecutionContext::new(options);
        context.check()?;
        self.execute_read(&read.plan, parameters, &context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CancellationToken, OpenOptions};
    use rusqlite::functions::FunctionFlags;
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[test]
    fn snapshot_spans_union_traversal_and_hydration_until_eager_return() {
        for pattern in [
            "p=(:Start)-[:R*1..2]->(:End)",
            "p=(:Start)-[:R*1..40]->(:End)",
            "p=shortestPath((:Start)-[:R*]->(:End))",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("snapshot.sqlite");
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE (:Start {generation:1})-[:R {generation:1}]->(:End {generation:1})")
                .unwrap();
            let branch = format!("MATCH {pattern} RETURN p, 1.5 AS marker");
            let query = format!("{branch} UNION ALL {branch}");
            let expected = db.execute(&query).unwrap();
            let plan = compile_read(&query).unwrap();
            let writer = Arc::new(Mutex::new(
                Database::open_with_options(
                    &path,
                    &OpenOptions {
                        busy_timeout: Duration::ZERO,
                        ..OpenOptions::default()
                    },
                )
                .unwrap(),
            ));
            let attempts = Arc::new(AtomicUsize::new(0));
            let other = writer.clone();
            let observed = attempts.clone();
            // Invoked while each primary fragment is producing path identities,
            // before the shared hydration pass. A writer must fail to commit.
            db.connection
                .create_scalar_function(
                    "crabnet_json_real",
                    1,
                    FunctionFlags::SQLITE_UTF8,
                    move |context| {
                        let result = other
                            .lock()
                            .unwrap()
                            .execute("MATCH (n) SET n.generation = 2");
                        assert!(matches!(result, Err(Error::Busy(_))), "{result:?}");
                        observed.fetch_add(1, Ordering::Relaxed);
                        Ok(context.get::<f64>(0)?.to_string())
                    },
                )
                .unwrap();
            assert_eq!(db.execute_compiled(&plan).unwrap(), expected, "{pattern}");
            assert!(attempts.load(Ordering::Relaxed) >= 2);
            writer
                .lock()
                .unwrap()
                .execute("MATCH (n) SET n.generation = 2")
                .unwrap();
            assert!(db.connection.is_autocommit());
        }
    }

    #[test]
    fn cancellation_during_execution_discards_partial_results_and_resets_controls() {
        let mut db = Database::in_memory().unwrap();
        let plan = compile_read("RETURN 1 AS value UNION ALL RETURN 1.5 AS value").unwrap();
        let token = CancellationToken::new();
        let signal = token.clone();
        let armed = AtomicBool::new(true);
        db.connection
            .create_scalar_function(
                "crabnet_json_real",
                1,
                FunctionFlags::SQLITE_UTF8,
                move |context| {
                    if armed.swap(false, Ordering::Relaxed) {
                        signal.cancel();
                    }
                    Ok(context.get::<f64>(0)?.to_string())
                },
            )
            .unwrap();
        assert!(matches!(
            db.execute_compiled_with_options(
                &plan,
                &Parameters::new(),
                &QueryOptions {
                    cancellation: Some(token),
                    ..QueryOptions::default()
                }
            ),
            Err(Error::Cancelled)
        ));
        assert!(db.connection.is_autocommit());
        assert_eq!(db.execute_compiled(&plan).unwrap().rows.len(), 2);
        db.execute("CREATE (:AfterCancellation)").unwrap();
    }
}
