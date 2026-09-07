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
    /// Execution stopped because a built-in query resource budget was exhausted.
    ResourceLimit(String),
    /// SQLite rejected a schema or query operation.
    Sqlite(rusqlite::Error),
    /// Stored data or an internally generated result had an invalid shape.
    Data(String),
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
            Self::ResourceLimit(message) => write!(formatter, "query resource limit: {message}"),
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::Data(message) => write!(formatter, "invalid graph data: {message}"),
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

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
