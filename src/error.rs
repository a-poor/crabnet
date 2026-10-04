use std::fmt;

/// An error produced while parsing, planning, or executing a Cypher query.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The input is empty (an optional semicolon does not count as a query).
    EmptyQuery,
    /// The input is not syntactically valid openCypher.
    Parse(String),
    /// The input is valid Cypher but outside Crabnet's current subset.
    Unsupported(String),
    /// The syntax is supported, but names or types do not make sense.
    Semantic(String),
    /// A referenced named parameter was not supplied.
    MissingParameter(String),
    /// A query failed in an explicit transaction. Its handle can no longer
    /// execute queries or commit.
    TransactionAborted,
    /// Execution stopped because a query resource budget was exhausted.
    ResourceLimit(String),
    /// The caller cancelled this query through its cancellation token.
    Cancelled,
    /// The query's cooperative deadline expired.
    DeadlineExceeded,
    /// SQLite rejected a schema or query operation.
    Sqlite(rusqlite::Error),
    /// Stored data or an internally generated result had an invalid shape.
    Data(String),
    /// A function received an argument value it cannot accept at runtime.
    Argument(String),
    /// A write would leave the graph inconsistent, such as deleting a node
    /// that still has relationships without `DETACH`, or violating a node
    /// property uniqueness constraint (including when installing one).
    Constraint(String),
    /// An operator or function met a value of a kind it cannot accept, such
    /// as `NOT 1` or indexing a string, where the kind was only known at
    /// runtime. Kinds known while planning fail with `Semantic` instead.
    Type(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuery => formatter.write_str("the Cypher query is empty"),
            Self::Parse(message) => write!(formatter, "invalid Cypher: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported Cypher: {message}"),
            Self::Semantic(message) => write!(formatter, "invalid Cypher semantics: {message}"),
            Self::MissingParameter(name) => write!(formatter, "missing Cypher parameter `${name}`"),
            Self::TransactionAborted => formatter.write_str("the transaction has been aborted"),
            Self::ResourceLimit(message) => write!(formatter, "query resource limit: {message}"),
            Self::Cancelled => formatter.write_str("query cancelled"),
            Self::DeadlineExceeded => formatter.write_str("query deadline exceeded"),
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::Data(message) => write!(formatter, "invalid graph data: {message}"),
            Self::Argument(message) => write!(formatter, "invalid argument: {message}"),
            Self::Type(message) => write!(formatter, "type error: {message}"),
            Self::Constraint(message) => write!(formatter, "constraint violated: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}

/// Prefix the `crabnet_raise` SQL function puts on argument error messages.
pub(crate) const ARGUMENT_ERROR_TAG: &str = "crabnet-argument-error: ";

/// Prefix the `crabnet_type_error` SQL function puts on type error messages.
pub(crate) const TYPE_ERROR_TAG: &str = "crabnet-type-error: ";

/// Prefix used by the persistent uniqueness triggers.
const CONSTRAINT_ERROR_TAG: &str = "crabnet-constraint-error: ";

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        // Generated SQL raises runtime argument errors through a registered
        // function. SQLite only carries the message text back out of the
        // statement, so the function tags it and the tag is recovered here.
        if let rusqlite::Error::SqliteFailure(_, Some(message)) = &error {
            if let Some(message) = message.strip_prefix(ARGUMENT_ERROR_TAG) {
                return Self::Argument(message.to_owned());
            }
            if let Some(message) = message.strip_prefix(TYPE_ERROR_TAG) {
                return Self::Type(message.to_owned());
            }
            if let Some(message) = message.strip_prefix(CONSTRAINT_ERROR_TAG) {
                return Self::Constraint(message.to_owned());
            }
        }
        Self::Sqlite(error)
    }
}
