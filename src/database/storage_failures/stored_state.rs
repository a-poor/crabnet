//! Raw SQLite state snapshots for initialization failure tests.

use std::{path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags, types::Value as SqlValue};

use super::pragma;

// Exact SQLite storage classes, including real bits and canonical-key BLOBs.
// Sorting rows preserves bags while ignoring physical row/page order.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cell {
    Null,
    Integer(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<SqlValue> for Cell {
    fn from(value: SqlValue) -> Self {
        match value {
            SqlValue::Null => Self::Null,
            SqlValue::Integer(value) => Self::Integer(value),
            SqlValue::Real(value) => Self::Real(value.to_bits()),
            SqlValue::Text(value) => Self::Text(value),
            SqlValue::Blob(value) => Self::Blob(value),
        }
    }
}

fn rows(connection: &Connection, sql: &str) -> Vec<Vec<Cell>> {
    let mut statement = connection.prepare(sql).unwrap();
    let columns = statement.column_count();
    let mut rows = statement
        .query_map([], |row| {
            (0..columns)
                .map(|index| row.get::<_, SqlValue>(index).map(Cell::from))
                .collect()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows.sort();
    rows
}

#[derive(Debug, PartialEq)]
pub(super) struct Stored {
    application_id: i64,
    version: i64,
    schema_cookie: i64,
    schema: Vec<Vec<Cell>>,
    tables: Vec<(String, Vec<Vec<Cell>>)>,
}

pub(super) fn stored(connection: &Connection) -> Stored {
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .into_iter()
        .map(|name| {
            let contents = rows(
                connection,
                &format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")),
            );
            (name, contents)
        })
        .collect();
    Stored {
        application_id: pragma(connection, "application_id"),
        version: pragma(connection, "user_version"),
        schema_cookie: pragma(connection, "schema_version"),
        schema: rows(
            connection,
            "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema",
        ),
        tables,
    }
}

pub(super) fn raw_open(path: &Path, access: OpenFlags) -> Connection {
    let connection = Connection::open_with_flags(
        path,
        access | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    )
    .unwrap();
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
}
