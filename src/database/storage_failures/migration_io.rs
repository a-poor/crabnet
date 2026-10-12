//! Historical upgrade faults, armed before the same initialization routine
//! used by public opens. Inspect raw storage before allowing any upgrade retry.

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, ErrorCode, OpenFlags, ffi, types::Value as SqlValue};
use serde_json::{Value, json};

use super::{assert_connection_integrity, io_vfs, open_existing, pragma, snapshot};
use crate::{
    CypherValue, Database, Error, NodeValue, OpenMode, OpenOptions, RelationshipValue,
    UniqueConstraint,
};
use io_vfs::{Event, FileKind, Operation};

pub(super) const FIXTURES: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/compatibility");

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

pub(super) fn assert_writer_can_commit(path: &Path, before: &Stored) {
    let mut writer = raw_open(path, OpenFlags::SQLITE_OPEN_READ_WRITE);
    // Use an existing edge: adding/removing a probe table would change the
    // schema cookie. Save and restore the exact property TEXT, avoiding JSON
    // normalization differences in historical storage.
    let (id, props): (String, String) = writer
        .query_row(
            "SELECT id, props FROM edges ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    assert_eq!(
        transaction
            .execute(
                r#"UPDATE edges SET props='{"__fault_probe":true}' WHERE id=?1"#,
                [&id]
            )
            .unwrap(),
        1
    );
    assert_eq!(
        transaction
            .execute("UPDATE edges SET props=?1 WHERE id=?2", [&props, &id])
            .unwrap(),
        1
    );
    transaction.commit().unwrap();
    assert_eq!(stored(&writer), *before);
}

fn properties(value: &Value) -> BTreeMap<String, CypherValue> {
    let CypherValue::Map(properties) = CypherValue::try_from(value).unwrap() else {
        panic!("fixture properties must be a map");
    };
    properties
}

pub(super) fn assert_graph(database: &mut Database, oracle: &Value) {
    let nodes: Vec<_> = oracle["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| {
            vec![CypherValue::Node(NodeValue {
                element_id: node["id"].as_str().unwrap().into(),
                labels: node["labels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|label| label.as_str().unwrap().into())
                    .collect(),
                properties: properties(&node["properties"]),
            })]
        })
        .collect();
    let relationships: Vec<_> = oracle["relationships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            vec![CypherValue::Relationship(RelationshipValue {
                element_id: edge["id"].as_str().unwrap().into(),
                relationship_type: edge["type"].as_str().unwrap().into(),
                start_node_element_id: edge["from"].as_str().unwrap().into(),
                end_node_element_id: edge["to"].as_str().unwrap().into(),
                properties: properties(&edge["properties"]),
            })]
        })
        .collect();
    let constraints: Vec<_> = oracle["constraints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|constraint| UniqueConstraint {
            label: constraint["label"].as_str().unwrap().into(),
            property: constraint["property"].as_str().unwrap().into(),
        })
        .collect();
    assert_eq!(
        database
            .execute("MATCH (n) RETURN n ORDER BY elementId(n)")
            .unwrap()
            .rows,
        nodes
    );
    assert_eq!(
        database
            .execute("MATCH ()-[r]->() RETURN r ORDER BY elementId(r)")
            .unwrap()
            .rows,
        relationships
    );
    assert_eq!(database.unique_constraints().unwrap(), constraints);
    assert_eq!(
        pragma(&database.connection, "user_version"),
        super::super::SCHEMA_VERSION
    );
    assert_connection_integrity(&database.connection);
}

pub(super) fn assert_indexes_work(database: &mut Database) {
    database.create_unique_constraint("Person", "key").unwrap();
    database
        .create_unique_constraint("Person", "number")
        .unwrap();
    let before = snapshot(database);
    assert!(matches!(
        database.execute("CREATE (:Person {number:2})"),
        Err(Error::Constraint(_))
    ));
    assert_eq!(snapshot(database), before);
    // A successful mutation exercises the migrated label/property/key triggers.
    database
        .execute("MATCH (n:Person {key:'a'}) SET n.key='after', n.number=11, n:Migrated")
        .unwrap();
    assert!(
        database
            .execute("MATCH (n:Person {key:'a'}) RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    assert_eq!(
        database
            .execute("MATCH (n:Migrated {number:11}) RETURN n.key")
            .unwrap()
            .rows,
        [vec![json!("after")]]
    );
    assert!(matches!(
        database.execute("CREATE (:Person {key:'after'})"),
        Err(Error::Constraint(_))
    ));
    assert_connection_integrity(&database.connection);
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    // Keep the failed connection alive to detect locks hidden by close/drop.
    Retained,
    // Exercise the owning opener's error path, which must drop its connection.
    Owned,
}

struct Trace {
    events: Vec<Event>,
    commit_at: Option<usize>,
}

fn run_case(version: i64, mode: Mode, fault: Option<(usize, &Event)>) -> Trace {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("upgrade.sqlite");
    fs::copy(
        Path::new(FIXTURES).join(format!("v{version}.sqlite")),
        &path,
    )
    .unwrap();
    let oracle: Value = serde_json::from_slice(
        &fs::read(Path::new(FIXTURES).join(format!("v{version}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(oracle["schema_version"], version);
    let before = stored(&raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY));
    assert_eq!(before.version, version);
    assert_eq!(before.application_id, super::super::APPLICATION_ID);
    let (connection, registration) = io_vfs::connect(&path);
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
        .pragma_update(None, "synchronous", "FULL")
        .unwrap();
    connection
        .pragma_update(None, "cache_spill", false)
        .unwrap();
    assert_eq!(pragma(&connection, "synchronous"), 2);
    assert_eq!(pragma(&connection, "cache_spill"), 0);
    let journal: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    // Record the write-commit boundary without querying SQLite from its hook.
    // This distinguishes migration statement I/O from the final commit's I/O.
    let commits = Arc::new(Mutex::new(Vec::new()));
    let callback_commits = commits.clone();
    let callback_control = registration.control.clone();
    connection
        .commit_hook(Some(move || {
            callback_commits
                .lock()
                .unwrap()
                .push(callback_control.event_count());
            false
        }))
        .unwrap();
    let mut connection = Some(connection);
    registration.control.start(fault.map(|(index, _)| index));
    let result = match mode {
        Mode::Retained => {
            super::super::initialize_with_mode(connection.as_mut().unwrap(), OpenMode::ReadWrite)
        }
        Mode::Owned => Database::from_connection(connection.take().unwrap()).map(drop),
    };
    let events = registration.control.stop();
    let commits = commits.lock().unwrap();
    assert!(
        commits.len() <= 1,
        "unexpected write transactions: {commits:?}"
    );
    let commit_at = commits.first().copied();
    if let Some(connection) = &connection {
        connection.commit_hook(None::<fn() -> bool>).unwrap();
    }
    if let Some((index, expected)) = fault {
        let error =
            result.expect_err("an injected migration fault must reject a valid historical upgrade");
        let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
            panic!("expected original SQLite I/O error, got {error:?}");
        };
        assert_eq!(code.code, ErrorCode::SystemIoFailure);
        assert_eq!(code.extended_code, expected.operation.error_code());
        let errors: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.result != ffi::SQLITE_OK)
            .collect();
        assert_eq!(
            errors.len(),
            1,
            "one-shot fault, healthy cleanup: {events:?}"
        );
        assert_eq!(errors[0].0, index);
        let mut failed = expected.clone();
        failed.result = expected.operation.error_code();
        assert_eq!(*errors[0].1, failed);
        if expected.file == FileKind::Database {
            assert!(
                events[index + 1..]
                    .iter()
                    .any(|event| event.file == FileKind::Database
                        && event.operation == Operation::Write
                        && event.result == ffi::SQLITE_OK),
                "missing rollback page writes: {events:?}"
            );
        }
        if let Some(connection) = &connection {
            assert!(connection.is_autocommit());
        }
        // No Crabnet open is allowed before these assertions: it would migrate
        // again and could conceal partially committed DDL/backfills/version.
        let reader = raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY);
        assert_eq!(stored(&reader), before);
        assert_connection_integrity(&reader);
        assert_writer_can_commit(&path, &before);
        if let Some(connection) = &connection {
            assert_eq!(stored(connection), before);
            assert_connection_integrity(connection);
        }
        drop(connection.take());
        assert_eq!(
            stored(&raw_open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)),
            before
        );
        // Public read-only opening must still refuse the *old* schema.
        assert!(matches!(
            Database::open_with_options(
                &path,
                &OpenOptions {
                    mode: OpenMode::ReadOnly,
                    busy_timeout: Duration::ZERO
                }
            ),
            Err(Error::Data(_))
        ));
        assert_eq!(stored(&reader), before);
    } else {
        result.unwrap();
        assert!(events.iter().all(|event| event.result == ffi::SQLITE_OK));
    }
    drop(connection);
    drop(registration);
    // Use the normal public opener and native VFS for the retry and read-only
    // reopen. Compare typed entities to independently retained JSON oracles.
    let mut upgraded = open_existing(&path);
    assert_graph(&mut upgraded, &oracle);
    drop(upgraded);
    let mut reopened = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            busy_timeout: Duration::ZERO,
        },
    )
    .unwrap();
    assert_graph(&mut reopened, &oracle);
    drop(reopened);
    let mut upgraded = open_existing(&path);
    assert_indexes_work(&mut upgraded);
    let committed = snapshot(&mut upgraded);
    drop(upgraded);
    assert_eq!(snapshot(&mut open_existing(&path)), committed);
    Trace { events, commit_at }
}

fn sweep(mode: Mode) {
    for version in 1..super::super::SCHEMA_VERSION {
        let baseline = run_case(version, mode, None);
        let commit_at = baseline.commit_at.unwrap();
        assert!(commit_at > 0 && commit_at < baseline.events.len());
        for file in [FileKind::Journal, FileKind::Database] {
            for operation in [Operation::Write, Operation::Sync] {
                assert!(
                    baseline
                        .events
                        .iter()
                        .any(|event| event.file == file && event.operation == operation)
                );
            }
        }
        assert!(
            baseline
                .events
                .iter()
                .filter(
                    |event| event.file == FileKind::Database && event.operation == Operation::Write
                )
                .count()
                > 1
        );
        for (index, event) in baseline.events.iter().enumerate() {
            eprintln!(
                "schema {version}, {mode:?}, migration fault {index} (commit at {commit_at}): {event:?}"
            );
            let failed = run_case(version, mode, Some((index, event)));
            assert_eq!(&failed.events[..index], &baseline.events[..index]);
            assert_eq!(failed.commit_at, (index >= commit_at).then_some(commit_at));
        }
    }
}

#[test]
fn migration_io_failures_restore_historical_schemas_and_release_locks_before_close() {
    sweep(Mode::Retained);
}

#[test]
fn failed_migration_open_preserves_historical_files_and_allows_public_retry() {
    sweep(Mode::Owned);
}
