//! An experimental, embedded property-graph database backed by SQLite.
//!
//! Crabnet currently implements a deliberately small openCypher subset. The
//! query is parsed with `open-cypher` and lowered to a row-oriented logical
//! plan. Reads and matched write inputs become parameterized SQL; mutations are
//! then applied transactionally to each input row.

mod database;
mod error;
mod planner;
mod value;

pub use database::{Database, constraints::UniqueConstraint, transaction::Transaction};
pub use error::{Error, Result};
pub use serde_json::Value;
pub use value::{CypherValue, NodeValue, PathValue, RelationshipValue};

use std::collections::BTreeMap;

/// Parses, validates, and renders Crabnet's normalized logical plan.
///
/// The explanation describes graph operators rather than generated SQLite SQL,
/// so it is useful while experimenting with lowering and planner behavior.
pub fn explain(cypher: &str) -> Result<String> {
    planner::explain_logical(cypher)
}

/// Parses, validates, and renders Crabnet's current SQLite execution plan.
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
    pub columns: Vec<String>,
    pub rows: Vec<Vec<CypherValue>>,
    pub summary: QuerySummary,
}
