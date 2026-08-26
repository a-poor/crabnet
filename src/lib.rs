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

pub use database::Database;
pub use error::{Error, Result};
pub use serde_json::Value;
pub use value::{CypherValue, NodeValue, RelationshipValue};

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
    pub nodes_created: usize,
    pub relationships_created: usize,
}

/// Ordered columns, rows, and mutation counters returned by a query.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<CypherValue>>,
    pub summary: QuerySummary,
}
