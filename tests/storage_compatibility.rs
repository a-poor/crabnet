//! Retained historical files are copied before opening; the current schema
//! builder never creates the input to these compatibility tests.
use std::{collections::BTreeMap, fs, path::Path};

use crabnet::{CypherValue, Database, Error, OpenMode, OpenOptions, Parameters, UniqueConstraint};
use rusqlite::{Connection, OpenFlags, types::Value as SqlValue};
use serde_json::{Value, json};

const CURRENT_SCHEMA: i64 = 5;
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/compatibility");
type GraphState = (
    Vec<Vec<CypherValue>>,
    Vec<Vec<CypherValue>>,
    Vec<UniqueConstraint>,
);

fn options(mode: OpenMode) -> OpenOptions {
    OpenOptions {
        mode,
        ..OpenOptions::default()
    }
}

fn copy_fixture(version: i64, path: &Path) -> Value {
    let source = Path::new(FIXTURES).join(format!("v{version}.sqlite"));
    fs::copy(&source, path).unwrap_or_else(|error| panic!("{}: {error}", source.display()));
    let expected: Value = serde_json::from_slice(
        &fs::read(Path::new(FIXTURES).join(format!("v{version}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(expected["schema_version"], version);
    assert_eq!(expected["nodes"].as_array().unwrap().len(), 5);
    assert_eq!(expected["relationships"].as_array().unwrap().len(), 4);
    assert_eq!(schema_version(path), version);
    expected
}

fn raw_reader(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn schema_version(path: &Path) -> i64 {
    raw_reader(path)
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn state(database: &mut Database) -> GraphState {
    (
        database
            .execute("MATCH (n) RETURN n ORDER BY elementId(n)")
            .unwrap()
            .rows,
        database
            .execute("MATCH ()-[r]->() RETURN r ORDER BY elementId(r)")
            .unwrap()
            .rows,
        database.unique_constraints().unwrap(),
    )
}

fn properties(value: &Value) -> BTreeMap<String, CypherValue> {
    let CypherValue::Map(properties) = CypherValue::try_from(value).unwrap() else {
        panic!("fixture properties must be a map");
    };
    properties
}

fn assert_graph(database: &mut Database, expected: &Value) {
    let (nodes, relationships, constraints) = state(database);
    let expected_nodes = expected["nodes"].as_array().unwrap();
    let expected_relationships = expected["relationships"].as_array().unwrap();
    assert_eq!(nodes.len(), expected_nodes.len());
    assert_eq!(relationships.len(), expected_relationships.len());
    for (row, node) in nodes.iter().zip(expected_nodes) {
        let [CypherValue::Node(actual)] = row.as_slice() else {
            panic!("expected one typed node: {row:?}");
        };
        assert_eq!(actual.element_id, node["id"].as_str().unwrap());
        let expected_labels: Vec<_> = node["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|label| label.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(actual.labels, expected_labels);
        // Typed equality detects integer/float/boolean changes at every depth.
        assert_eq!(actual.properties, properties(&node["properties"]));
    }
    for (row, relationship) in relationships.iter().zip(expected_relationships) {
        let [CypherValue::Relationship(actual)] = row.as_slice() else {
            panic!("expected one typed relationship: {row:?}");
        };
        assert_eq!(actual.element_id, relationship["id"].as_str().unwrap());
        assert_eq!(
            actual.relationship_type,
            relationship["type"].as_str().unwrap()
        );
        assert_eq!(
            actual.start_node_element_id,
            relationship["from"].as_str().unwrap()
        );
        assert_eq!(
            actual.end_node_element_id,
            relationship["to"].as_str().unwrap()
        );
        assert_eq!(actual.properties, properties(&relationship["properties"]));
    }
    let expected_constraints: Vec<_> = expected["constraints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|constraint| UniqueConstraint {
            label: constraint["label"].as_str().unwrap().into(),
            property: constraint["property"].as_str().unwrap().into(),
        })
        .collect();
    assert_eq!(constraints, expected_constraints);
}

#[test]
fn historical_files_upgrade_without_losing_typed_graphs_or_identity() {
    for version in 1..=CURRENT_SCHEMA {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.sqlite");
        let expected = copy_fixture(version, &path);
        let original = fs::read(&path).unwrap();
        let read_only = Database::open_with_options(&path, &options(OpenMode::ReadOnly));
        if version < CURRENT_SCHEMA {
            assert!(matches!(read_only, Err(Error::Data(_))), "schema {version}");
        } else {
            assert_graph(&mut read_only.unwrap(), &expected);
        }
        assert_eq!(fs::read(&path).unwrap(), original);
        let mut database =
            Database::open_with_options(&path, &options(OpenMode::ReadWrite)).unwrap();
        assert_eq!(schema_version(&path), CURRENT_SCHEMA);
        assert_graph(&mut database, &expected);
        // Early writers stored explicit top-level nulls. Migration preserves
        // those entries, but property functions still treat them as absent.
        assert_eq!(
            database
                .execute(
                    "MATCH (n:Person {key:'a'}) RETURN n.explicit IS NULL,
                     'explicit' IN keys(n), 'explicit' IN keys(properties(n))"
                )
                .unwrap()
                .rows,
            [vec![json!(true), json!(false), json!(false)]]
        );
        drop(database);
        let mut reopened =
            Database::open_with_options(&path, &options(OpenMode::ReadOnly)).unwrap();
        assert_graph(&mut reopened, &expected);
    }
}

#[test]
fn upgraded_indexes_and_constraints_support_writes_and_atomic_failure() {
    for version in 1..=CURRENT_SCHEMA {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live.sqlite");
        let expected = copy_fixture(version, &path);
        let mut database = Database::open(&path).unwrap();
        assert_graph(&mut database, &expected);
        for property in ["key", "number"] {
            assert_eq!(
                database
                    .create_unique_constraint("Person", property)
                    .unwrap(),
                version < 4
            );
        }
        let before = state(&mut database);
        // For schemas 4 and 5 this key was encoded by the historical writer;
        // older inputs exercise keys backfilled from historical properties.
        assert!(matches!(
            database.execute("CREATE (:Person {number:2})"),
            Err(Error::Constraint(_))
        ));
        assert_eq!(state(&mut database), before);
        let person_id = expected["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["properties"]["key"] == "a")
            .unwrap()["id"]
            .as_str()
            .unwrap();
        let parameters = Parameters::from([("key".into(), json!("new'\n雪"))]);
        let result = database
            .execute_with(
                "MATCH (n:Person {key:'a'}) SET n.key=$key, n.number=10.0, n:Updated
                 REMOVE n.explicit RETURN elementId(n)",
                &parameters,
            )
            .unwrap();
        assert_eq!(result.rows, [vec![json!(person_id)]]);
        assert!(
            database
                .execute("MATCH (n:Person {key:'a'}) RETURN n")
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(
            database
                .execute_with(
                    "MATCH (n:Person:Updated {key:$key}) RETURN elementId(n)",
                    &parameters
                )
                .unwrap()
                .rows,
            [vec![json!(person_id)]]
        );
        let updated = state(&mut database);
        // Upgraded triggers must maintain keys after a property change too.
        assert!(matches!(
            database.execute("CREATE (:Person {number:10})"),
            Err(Error::Constraint(_))
        ));
        assert_eq!(state(&mut database), updated);
        let mut transaction = database.transaction().unwrap();
        assert_eq!(
            transaction
                .execute("MATCH (n:Person {key:'b'}) SET n.number=20, n:Pending RETURN n.number")
                .unwrap()
                .rows,
            [vec![json!(20)]]
        );
        assert!(matches!(
            transaction.execute("CREATE (:Person {key:'b'})"),
            Err(Error::Constraint(_))
        ));
        assert!(matches!(
            transaction.commit(),
            Err(Error::TransactionAborted)
        ));
        assert_eq!(state(&mut database), updated);
        for query in [
            "MATCH (n:Person {number:20}) RETURN n",
            "MATCH (n:Pending) RETURN n",
        ] {
            assert!(database.execute(query).unwrap().rows.is_empty());
        }
        assert_eq!(
            database
                .execute("MATCH (n:Person {number:2}) RETURN n.key")
                .unwrap()
                .rows,
            [vec![json!("b")]]
        );
        drop(database);
        assert_eq!(state(&mut Database::open(&path).unwrap()), updated);
    }
}

#[test]
fn retained_backups_restore_through_a_migrated_copy_and_keep_the_rollback_source() {
    for version in 1..=CURRENT_SCHEMA {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("pre-upgrade.sqlite");
        let expected = copy_fixture(version, &source);
        let original = fs::read(&source).unwrap();
        let destination_path = directory.path().join("destination.sqlite");
        let mut destination = Database::open(&destination_path).unwrap();
        destination
            .create_unique_constraint("Unrelated", "id")
            .unwrap();
        destination.execute("CREATE (:Unrelated {id:1})").unwrap();
        let before = state(&mut destination);
        if version < CURRENT_SCHEMA {
            assert!(matches!(
                destination.restore_from(&source),
                Err(Error::Data(_))
            ));
            assert_eq!(state(&mut destination), before);
        } else {
            // This is a real backup produced by the retained schema-5 writer.
            destination.restore_from(&source).unwrap();
            assert_graph(&mut destination, &expected);
        }
        assert_eq!(fs::read(&source).unwrap(), original);
        let working = directory.path().join("upgrade-copy.sqlite");
        fs::copy(&source, &working).unwrap();
        let backup = directory.path().join("upgraded-backup.sqlite");
        let mut upgraded =
            Database::open_with_options(&working, &options(OpenMode::ReadWrite)).unwrap();
        assert_graph(&mut upgraded, &expected);
        upgraded.backup_to(&backup).unwrap();
        drop(upgraded);
        let backup_bytes = fs::read(&backup).unwrap();
        destination.restore_from(&backup).unwrap();
        assert_graph(&mut destination, &expected);
        drop(destination);
        assert_graph(&mut Database::open(&destination_path).unwrap(), &expected);
        assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_eq!(schema_version(&source), version);
    }
}

// A logical snapshot of every persisted object/row, including unique-key BLOBs.
// Sorting preserves bags and ignores file-layout changes during rollback.
#[derive(Debug, PartialEq)]
struct PersistedState {
    version: i64,
    schema: Vec<String>,
    tables: Vec<(String, Vec<String>)>,
}

fn persisted_state(path: &Path) -> PersistedState {
    let connection = raw_reader(path);
    let schema = connection
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")
        .unwrap()
        .query_map([], |row| {
            Ok(format!(
                "{:?}",
                (
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?
                )
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let rows = tables
        .into_iter()
        .map(|name| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))
                .unwrap();
            let columns = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    let values = (0..columns)
                        .map(|column| row.get::<_, SqlValue>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(format!("{values:?}"))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows.sort();
            (name, rows)
        })
        .collect();
    PersistedState {
        version: schema_version(path),
        schema,
        tables: rows,
    }
}

#[test]
fn failed_historical_upgrades_restore_all_schema_objects_and_can_be_retried() {
    for version in 1..CURRENT_SCHEMA {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blocked.sqlite");
        let expected = copy_fixture(version, &path);
        let blocker = Connection::open(&path).unwrap();
        let repair = if version < 4 {
            blocker
                .execute_batch("CREATE TABLE node_unique_constraints (blocker INTEGER)")
                .unwrap();
            "DROP TABLE node_unique_constraints".to_owned()
        } else {
            let old_trigger: String = blocker
                .query_row(
                    "SELECT sql FROM sqlite_schema WHERE name='node_unique_update'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            blocker
                .execute_batch("DROP TRIGGER node_unique_update")
                .unwrap();
            old_trigger
        };
        drop(blocker);
        let before = persisted_state(&path);
        assert!(
            matches!(Database::open(&path), Err(Error::Sqlite(_))),
            "schema {version}"
        );
        assert_eq!(persisted_state(&path), before, "schema {version}");
        let repairer = Connection::open(&path).unwrap();
        repairer.execute_batch(&repair).unwrap();
        drop(repairer);
        assert_graph(&mut Database::open(&path).unwrap(), &expected);
        assert_eq!(schema_version(&path), CURRENT_SCHEMA);
    }
}

#[test]
fn future_versions_and_the_experimental_single_label_v2_are_refused_without_mutation() {
    for (version, marker) in [(1, 2), (CURRENT_SCHEMA, CURRENT_SCHEMA + 1)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsupported.sqlite");
        copy_fixture(version, &path);
        // Synthetic refusal probes, not representations of a released format.
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", marker)
            .unwrap();
        drop(connection);
        let original = fs::read(&path).unwrap();
        for mode in [
            OpenMode::ReadOnly,
            OpenMode::ReadWrite,
            OpenMode::OpenOrCreate,
        ] {
            assert!(matches!(
                Database::open_with_options(&path, &options(mode)),
                Err(Error::Data(_))
            ));
            assert_eq!(fs::read(&path).unwrap(), original);
        }
        let mut destination = Database::in_memory().unwrap();
        destination.execute("CREATE (:Saved {id:1})").unwrap();
        let before = state(&mut destination);
        assert!(matches!(
            destination.restore_from(&path),
            Err(Error::Data(_))
        ));
        assert_eq!(state(&mut destination), before);
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}
