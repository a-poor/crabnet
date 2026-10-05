use std::{fs, io::ErrorKind, time::Duration};

use crabnet::{CypherValue, Database, Error, OpenMode, OpenOptions, UniqueConstraint};
use rusqlite::Connection;
use serde_json::json;

#[test]
fn backup_never_overwrites_an_existing_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backup.sqlite");
    let mut saved = Database::open(&path).unwrap();
    saved.execute("CREATE (:Saved {id: 1})").unwrap();
    drop(saved);
    let bytes = std::fs::read(&path).unwrap();
    let mut source = Database::in_memory().unwrap();
    source.execute("CREATE (:Replacement)").unwrap();
    assert!(
        matches!(source.backup_to(&path), Err(Error::Io(error)) if error.kind() == ErrorKind::AlreadyExists)
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let mut saved = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        saved.execute("MATCH (n:Saved) RETURN n.id").unwrap().rows,
        [vec![json!(1)]]
    );
}

fn seed(database: &mut Database) {
    database.create_unique_constraint("Person", "key").unwrap();
    database
        .execute(
            "CREATE (a:Person:Member {key: 'a', nested: [1, null, {x: true}]}),
        (b:Person {key: 'b'}), (a)-[:LINK {weight: 2}]->(b), (a)-[:LINK {weight: 2}]->(b)",
        )
        .unwrap();
}

fn state(
    database: &mut Database,
) -> (
    Vec<Vec<CypherValue>>,
    Vec<Vec<CypherValue>>,
    Vec<UniqueConstraint>,
) {
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

#[test]
fn backup_round_trips_memory_and_read_only_files_with_identity_and_constraints() {
    let directory = tempfile::tempdir().unwrap();
    let backup = directory.path().join("snapshot.sqlite");
    let second = directory.path().join("snapshot-copy.sqlite");
    let mut source = Database::in_memory().unwrap();
    seed(&mut source);
    let expected = state(&mut source);
    assert_eq!(expected.1.len(), 2); // Preserve the parallel relationship bag.
    source.backup_to(&backup).unwrap();
    source.execute("MATCH (n) DETACH DELETE n").unwrap();
    let mut reader = Database::open_with_options(
        &backup,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert_eq!(state(&mut reader), expected);
    reader.backup_to(&second).unwrap();
    let mut restored = Database::in_memory().unwrap();
    restored.restore_from(&second).unwrap();
    assert_eq!(state(&mut restored), expected);
    assert!(matches!(
        restored.execute("CREATE (:Person {key: 'a'})"),
        Err(Error::Constraint(_))
    ));
    restored.execute("CREATE (:Person {key: 'c'})").unwrap();
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn restore_replaces_graph_and_constraints_and_refreshes_existing_connections() {
    let directory = tempfile::tempdir().unwrap();
    let backup = directory.path().join("backup.sqlite");
    let live = directory.path().join("live.sqlite");
    let mut source = Database::in_memory().unwrap();
    seed(&mut source);
    let expected = state(&mut source);
    source.backup_to(&backup).unwrap();
    let bytes = fs::read(&backup).unwrap();
    let mut destination = Database::open(&live).unwrap();
    destination.create_unique_constraint("Old", "id").unwrap();
    destination.execute("CREATE (:Old {id: 1})").unwrap();
    // Populate prepared-statement caches and the mutation TEMP relation first.
    destination
        .execute("MATCH (n:Old) SET n.id = 2 RETURN n")
        .unwrap();
    let mut observer = Database::open(&live).unwrap();
    assert_eq!(state(&mut observer).0.len(), 1);
    destination.restore_from(&backup).unwrap();
    assert_eq!(state(&mut destination), expected);
    assert_eq!(state(&mut observer), expected);
    destination
        .execute("MATCH (n:Person {key: 'a'}) SET n.restored = true RETURN n")
        .unwrap();
    assert!(matches!(
        observer.execute("CREATE (:Person {key: 'a'})"),
        Err(Error::Constraint(_))
    ));
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    drop(observer);
    drop(destination);
    let mut reopened = Database::open(&live).unwrap();
    assert_eq!(
        reopened
            .execute("MATCH (n:Person {key: 'a'}) RETURN n.restored")
            .unwrap()
            .rows,
        [vec![json!(true)]]
    );
}

#[test]
fn backup_excludes_another_connections_uncommitted_changes() {
    let directory = tempfile::tempdir().unwrap();
    let live = directory.path().join("live.sqlite");
    let backup = directory.path().join("backup.sqlite");
    let mut writer = Database::open(&live).unwrap();
    seed(&mut writer);
    let expected = state(&mut writer);
    let mut reader = Database::open(&live).unwrap();
    let mut transaction = writer.transaction().unwrap();
    transaction.execute("CREATE (:Pending)").unwrap();
    transaction.drop_unique_constraint("Person", "key").unwrap();
    reader.backup_to(&backup).unwrap();
    transaction.commit().unwrap();
    let mut copy = Database::open(&backup).unwrap();
    assert_eq!(state(&mut copy), expected);
}

#[test]
fn invalid_restore_sources_preserve_the_destination_and_never_create_missing_sources() {
    let directory = tempfile::tempdir().unwrap();
    let mut destination = Database::in_memory().unwrap();
    seed(&mut destination);
    let expected = state(&mut destination);
    let path = directory.path().join("missing.sqlite");
    assert!(destination.restore_from(&path).is_err());
    assert!(!path.exists());
    assert_eq!(state(&mut destination), expected);
    for (name, setup) in [
        ("foreign", "CREATE TABLE unrelated (id INTEGER)"),
        (
            "future",
            "PRAGMA application_id = 1129464130; PRAGMA user_version = 999",
        ),
        (
            "old",
            "PRAGMA application_id = 1129464130; PRAGMA user_version = 1",
        ),
        ("empty", ""),
    ] {
        let path = directory.path().join(name);
        Connection::open(&path)
            .unwrap()
            .execute_batch(setup)
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(destination.restore_from(&path).is_err(), "{name}");
        assert_eq!(state(&mut destination), expected);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    let corrupt = directory.path().join("corrupt.sqlite");
    fs::write(&corrupt, b"this is not a SQLite database").unwrap();
    assert!(destination.restore_from(&corrupt).is_err());
    assert_eq!(state(&mut destination), expected);
}

#[test]
fn inconsistent_backup_is_rejected_before_restore_writes() {
    let directory = tempfile::tempdir().unwrap();
    let backup = directory.path().join("invalid.sqlite");
    let mut destination = Database::in_memory().unwrap();
    seed(&mut destination);
    let expected = state(&mut destination);
    destination.backup_to(&backup).unwrap();
    // Simulate unsupported external tampering. Layout/version checks alone
    // accept this file; the foreign-key check must reject it before replacement.
    let raw = Connection::open(&backup).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF; UPDATE edges SET from_node = 'missing'")
        .unwrap();
    drop(raw);
    assert!(
        matches!(destination.restore_from(&backup), Err(Error::Data(message)) if message.contains("foreign-key"))
    );
    assert_eq!(state(&mut destination), expected);
    let bad_check = directory.path().join("invalid-check.sqlite");
    destination.backup_to(&bad_check).unwrap();
    Connection::open(&bad_check)
        .unwrap()
        .execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE edges SET edge_type = ''")
        .unwrap();
    let error = destination.restore_from(&bad_check).unwrap_err();
    assert!(
        matches!(&error, Error::Data(message) if message.contains("integrity check failed")),
        "{error:?}"
    );
    assert_eq!(state(&mut destination), expected);
}

#[test]
fn lock_failures_preserve_destination_and_release_connections_and_staging_files() {
    let directory = tempfile::tempdir().unwrap();
    let live = directory.path().join("live.sqlite");
    let backup = directory.path().join("backup.sqlite");
    let mut destination = Database::open_with_options(
        &live,
        &OpenOptions {
            busy_timeout: Duration::ZERO,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    seed(&mut destination);
    let expected = state(&mut destination);
    destination.backup_to(&backup).unwrap();
    let blocker = Connection::open(&live).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let failed = directory.path().join("failed.sqlite");
    assert!(matches!(
        destination.backup_to(&failed),
        Err(Error::Busy(_))
    ));
    assert!(!failed.exists());
    blocker
        .execute_batch("ROLLBACK; BEGIN; SELECT * FROM nodes;")
        .unwrap();
    assert!(matches!(
        destination.restore_from(&backup),
        Err(Error::Busy(_))
    ));
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(state(&mut destination), expected);
    let source_blocker = Connection::open(&backup).unwrap();
    source_blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert!(matches!(
        destination.restore_from(&backup),
        Err(Error::Busy(_))
    ));
    source_blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(state(&mut destination), expected);
    destination.restore_from(&backup).unwrap();
    assert!(matches!(
        destination.restore_from(&live),
        Err(Error::Busy(_))
    ));
    destination.execute("CREATE (:Usable)").unwrap();
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn read_only_restore_and_reserved_paths_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("live.sqlite");
    Database::open(&path).unwrap();
    let mut database = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert!(matches!(
        database.restore_from(directory.path().join("absent")),
        Err(Error::ReadOnly)
    ));
    for path in ["", ":memory:", "file:backup?mode=memory"] {
        assert!(matches!(database.backup_to(path), Err(Error::Argument(_))));
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn existing_empty_files_and_source_aliases_cannot_be_overwritten_by_backup() {
    let directory = tempfile::tempdir().unwrap();
    let live = directory.path().join("live.sqlite");
    let empty = directory.path().join("empty.sqlite");
    let alias = directory.path().join("alias.sqlite");
    let mut database = Database::open(&live).unwrap();
    seed(&mut database);
    let expected = state(&mut database);
    fs::write(&empty, []).unwrap();
    fs::hard_link(&live, &alias).unwrap();
    for path in [&live, &empty, &alias] {
        assert!(
            matches!(database.backup_to(path), Err(Error::Io(error)) if error.kind() == ErrorKind::AlreadyExists)
        );
    }
    assert!(fs::read(&empty).unwrap().is_empty());
    assert_eq!(state(&mut database), expected);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 3);
    #[cfg(unix)]
    for target in [&live, &directory.path().join("absent.sqlite")] {
        let link = directory.path().join("link.sqlite");
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(
            matches!(database.backup_to(&link), Err(Error::Io(error)) if error.kind() == ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read_link(&link).unwrap(), *target);
        fs::remove_file(&link).unwrap();
    }
}
