//! An experimental, embedded property-graph database backed by SQLite.
//!
//! `crabnet` lets you store connected data and query it with Cypher, directly
//! from your Rust application. Model nodes and relationships, follow paths,
//! and get typed values back without running a separate database server.
//!
//! It is intended for local tools and rebuildable graph indexes. The API is
//! still evolving, and only a subset of openCypher is supported.
//!
//! # Quickstart
//!
//! ```
//! use crabnet::{Database, Parameters, Value};
//!
//! let mut graph = Database::in_memory()?;
//! let parameters = Parameters::from([("name".into(), Value::from("Ada"))]);
//! graph.execute_with("CREATE (:Person {name: $name})", &parameters)?;
//! let result = graph.execute("MATCH (p:Person) RETURN p.name AS name")?;
//! assert_eq!(result.columns, ["name"]);
//! assert_eq!(result.rows[0][0].as_str(), Some("Ada"));
//! # Ok::<(), crabnet::Error>(())
//! ```
//!
//! # Choosing an API
//!
//! - [`Database::open`] opens or creates a file. [`OpenOptions`] selects
//!   existing-only or read-only access and a lock-wait timeout.
//! - [`Parameters`] uses JSON [`Value`] inputs. Results use [`CypherValue`]
//!   to retain integers, floats, graph identities, paths, and nested values.
//! - [`Database::transaction`] groups queries and constraint changes. Results
//!   remain provisional until commit. A commit I/O error can accompany persisted
//!   writes: reconcile the stored outcome before retrying a failed write.
//! - [`compile_read`] creates an owned reusable plan; [`PreparedRead`] and
//!   [`ReadCursor`] expose streaming where supported and explicit buffered fallback.
//! - [`QueryOptions`] supplies cooperative cancellation, deadlines and resource
//!   controls. These are not a whole-process memory cap.
//! - [`Database::backup_to`] creates a new snapshot; [`Database::restore_from`]
//!   replaces a quiescent destination graph. There is no automatic downgrade.
//!
//! # Further reading
//!
//! - [Supported queries](https://github.com/a-poor/crabnet/blob/main/docs/coverage.md)
//!   describes the supported openCypher subset and limitations.
//! - [Compatibility](https://github.com/a-poor/crabnet/blob/main/docs/compatibility.md)
//!   covers API changes, storage upgrades, and rollback.
//! - [Support](https://github.com/a-poor/crabnet/blob/main/docs/support.md)
//!   describes tested platforms, storage configuration, and recovery limits.
//!
//! These guides track the main branch; the crate archive includes the `docs/`
//! files for this version. SQLite is bundled through `rusqlite`.

#![warn(missing_docs)]

mod database;
mod error;
mod planner;
mod query;
mod value;

pub use database::{
    Database,
    compiled::{CompiledRead, compile_read, compile_read_with_options},
    constraints::UniqueConstraint,
    cursor::{PreparedRead, ReadCursor, ReadMode},
    open::{OpenMode, OpenOptions},
    transaction::Transaction,
};
pub use error::{Error, Result};
pub use query::{CancellationToken, QueryOptions, TraversalLimits};
pub use serde_json::Value;
pub use value::{CypherValue, NodeValue, PathValue, RelationshipValue};

use std::collections::BTreeMap;

/// Parses, validates, and renders `crabnet`’s normalized logical plan.
///
/// The explanation describes graph operators rather than generated SQLite SQL,
/// so it is useful while experimenting with lowering and planner behavior.
pub fn explain(cypher: &str) -> Result<String> {
    planner::explain_logical(cypher)
}

/// Parses, validates, and renders `crabnet`’s current SQLite execution plan.
///
/// Unlike [`explain`], this experimental view includes generated SQL and bind
/// positions. It currently supports read pipelines only.
pub fn explain_physical(cypher: &str) -> Result<String> {
    planner::explain_physical(cypher)
}

/// Named values available to `$parameter` expressions in a Cypher query.
pub type Parameters = BTreeMap<String, Value>;

/// The counters produced while executing a query.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuerySummary {
    /// Nodes inserted by the statement.
    pub nodes_created: usize,
    /// Relationships inserted by the statement.
    pub relationships_created: usize,
    /// Top-level entity properties whose stored value changed or was removed.
    pub properties_set: usize,
    /// Node labels added by `SET n:Label`, counted per node and label.
    pub labels_added: usize,
    /// Node labels removed by `REMOVE n:Label`, counted per node and label.
    pub labels_removed: usize,
    /// Distinct stored nodes removed by the statement.
    pub nodes_deleted: usize,
    /// Distinct stored relationships removed explicitly or by `DETACH DELETE`.
    pub relationships_deleted: usize,
}

/// Ordered columns, rows, and mutation counters returned by a query.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct QueryResult {
    /// Projection names in the same order as each row's values.
    pub columns: Vec<String>,
    /// Materialized rows; duplicates are preserved. Row order is only promised
    /// where the query contract specifies it, for example with `ORDER BY`.
    pub rows: Vec<Vec<CypherValue>>,
    /// Mutation counters for this query, not a cumulative transaction total.
    pub summary: QuerySummary,
}
