//! An experimental, embedded property-graph database backed by SQLite.
//!
//! Crabnet currently implements a deliberately small openCypher subset. The
//! query is parsed with `open-cypher` and lowered to a row-oriented logical
//! plan. Reads and matched write inputs become parameterized SQL; mutations are
//! then applied transactionally to each input row.

mod database;
mod error;
mod planner;

pub use database::Database;
pub use error::{Error, Result};
pub use serde_json::Value;

use std::collections::BTreeMap;

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
    pub rows: Vec<Vec<Value>>,
    pub summary: QuerySummary,
}
