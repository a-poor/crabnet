//! Journal finalization can report failure on either side of unlink. These
//! tests distinguish recovered rollback from persisted writes after an error.

use super::{
    Snapshot, assert_competing_writer_can_commit, assert_integrity, assert_saved_constraint,
    commit_io::{Mode, apply_armed, assert_committed, configure},
    io_vfs::{self, DeleteFault, Event, FileKind, Operation},
    open_existing, seed, snapshot,
};
use crate::{CypherValue, Error};
use rusqlite::types::Value as SqlValue;
use rusqlite::{ErrorCode, ffi};
use serde_json::json;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
enum Recovery {
    Observer,
    Original,
    Reopen,
}

fn assert_delete_error(error: Error, fault: DeleteFault) {
    let Error::Sqlite(rusqlite::Error::SqliteFailure(code, _)) = error else {
        panic!("expected original SQLite I/O error, got {error:?}");
    };
    assert_eq!(code.code, ErrorCode::SystemIoFailure);
    assert_eq!(code.extended_code, fault.error_code());
}

// Fresh graphs generate different element IDs. Compare this fixture's bags
// using labels/properties as node roles (its equivalent Transient nodes share
// the same neighbor). Preserve duplicates, typed values and raw unique keys.
// Exact IDs are still compared across every reader/reopen of the SAME file.
fn fixture_roles(mut state: Snapshot) -> Snapshot {
    let mut roles = HashMap::new();
    for row in &mut state.nodes {
        let CypherValue::Node(node) = &mut row[0] else {
            panic!("expected node")
        };
        let role = format!("{:?}/{:?}", node.labels, node.properties);
        roles.insert(node.element_id.clone(), role.clone());
        node.element_id = role;
    }
    for row in &mut state.relationships {
        let CypherValue::Relationship(edge) = &mut row[0] else {
            panic!("expected relationship")
        };
        edge.start_node_element_id = roles[&edge.start_node_element_id].clone();
        edge.end_node_element_id = roles[&edge.end_node_element_id].clone();
        edge.element_id.clear();
    }
    state.nodes.sort_by_cached_key(|row| format!("{row:?}"));
    state
        .relationships
        .sort_by_cached_key(|row| format!("{row:?}"));
    for rows in &mut state.indexes {
        for row in rows.iter_mut() {
            for value in row {
                if let SqlValue::Text(text) = value
                    && let Some(role) = roles.get(text)
                {
                    *text = role.clone();
                }
            }
        }
        rows.sort_by_cached_key(|row| format!("{row:?}"));
    }
    state
}

fn healthy(mode: Mode, extra: bool) -> (Snapshot, Vec<Event>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.sqlite");
    drop(seed(&path));
    let (mut database, registration) = io_vfs::open(&path);
    configure(&database);
    if extra {
        database
            .connection
            .pragma_update(None, "synchronous", "EXTRA")
            .unwrap();
    }
    let control = registration.control.clone();
    let result = apply_armed(&mut database, mode, move || control.start_deletes(None)).unwrap();
    let events = registration.control.stop();
    assert!(events.iter().all(|event| event.result == ffi::SQLITE_OK));
    assert_committed(&mut database, mode, result);
    (fixture_roles(snapshot(&mut database)), events)
}

fn outcomes(mode: Mode) {
    for extra in [false, true] {
        let (committed, healthy_events) = healthy(mode, extra);
        let delete_index = healthy_events
            .iter()
            .position(|event| event.operation == Operation::Delete)
            .unwrap();
        assert_eq!(delete_index, healthy_events.len() - 1);
        assert!(healthy_events[..delete_index].iter().any(|event| event.file == FileKind::Database && event.operation == Operation::Sync));
        assert_eq!(healthy_events[delete_index].amount, i32::from(extra));
        for fault in [DeleteFault::Before, DeleteFault::After] {
            // Native directory synchronization is requested only with EXTRA.
            if !extra && matches!(fault, DeleteFault::After) {
                continue;
            }
            for recovery in [Recovery::Observer, Recovery::Original, Recovery::Reopen] {
                eprintln!("{mode:?}, extra={extra}, {fault:?}, {recovery:?}");
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("graph.sqlite");
                let journal = path.with_extension("sqlite-journal");
                drop(seed(&path));
                let (mut database, registration) = io_vfs::open(&path);
                configure(&database);
                if extra {
                    database
                        .connection
                        .pragma_update(None, "synchronous", "EXTRA")
                        .unwrap();
                }
                let before = snapshot(&mut database);
                let mut observer = open_existing(&path);
                let control = registration.control.clone();
                let error = apply_armed(&mut database, mode, move || {
                    control.start_deletes(Some(fault))
                })
                .unwrap_err();
                let events = registration.control.stop();
                assert_delete_error(error, fault);
                assert_eq!(&events[..delete_index], &healthy_events[..delete_index]);
                assert_eq!(events.len(), healthy_events.len());
                let mut expected_event = healthy_events[delete_index].clone();
                expected_event.result = fault.error_code();
                assert_eq!(events[delete_index], expected_event);
                assert!(database.connection.is_autocommit());
                assert_eq!(journal.exists(), matches!(fault, DeleteFault::Before));
                let recovered = match recovery {
                    Recovery::Observer => snapshot(&mut observer),
                    Recovery::Original => snapshot(&mut database),
                    Recovery::Reopen => {
                        drop(database);
                        assert_eq!(registration.control.open_file_count(), 0);
                        database = open_existing(&path);
                        snapshot(&mut database)
                    }
                };
                match fault {
                    DeleteFault::Before => assert_eq!(recovered, before),
                    DeleteFault::After => {
                        let same_file = match recovery {
                            Recovery::Observer => snapshot(&mut observer),
                            _ => snapshot(&mut database),
                        };
                        assert_eq!(fixture_roles(same_file), committed);
                    }
                }
                let expected = &recovered;
                // With observer-first recovery, prove writer lock release before touching
                // or closing the original handle, including its cached state.
                assert_competing_writer_can_commit(&mut observer, expected);
                assert_eq!(snapshot(&mut database), *expected);
                assert_eq!(snapshot(&mut observer), *expected);
                assert!(!journal.exists());
                assert_integrity(&database);
                assert_competing_writer_can_commit(&mut observer, expected);
                let mut reopened = open_existing(&path);
                assert_eq!(snapshot(&mut reopened), *expected);
                assert_integrity(&reopened);
                drop(reopened);
                if matches!(fault, DeleteFault::Before) {
                    assert_saved_constraint(&mut database);
                    let control = registration.control.clone();
                    let result =
                        apply_armed(&mut database, mode, move || control.start_deletes(None))
                            .unwrap();
                    assert_committed(&mut database, mode, result);
                    assert!(
                        registration
                            .control
                            .stop()
                            .iter()
                            .all(|event| event.result == ffi::SQLITE_OK)
                    );
                }
                let final_state = snapshot(&mut database);
                drop(database);
                drop(registration);
                let mut reopened = open_existing(&path);
                assert_eq!(snapshot(&mut reopened), final_state);
                assert_integrity(&reopened);
            }
        }
    }
}

#[test]
fn standalone_journal_delete_failures_distinguish_rollback_from_committed_writes() {
    outcomes(Mode::Standalone);
}

#[test]
fn transaction_journal_delete_failures_distinguish_rollback_from_committed_writes() {
    outcomes(Mode::Transaction);
}

#[test]
fn journal_delete_fault_is_scoped_and_consumed_but_blind_retry_can_duplicate_writes() {
    for mode in [Mode::Standalone, Mode::Transaction] {
        for fault in [DeleteFault::Before, DeleteFault::After] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("first.sqlite");
            let other_path = directory.path().join("second.sqlite");
            drop(seed(&path));
            drop(seed(&other_path));
            let (mut database, registration) = io_vfs::open(&path);
            let (mut other, other_registration) = io_vfs::open(&other_path);
            configure(&database);
            database
                .connection
                .pragma_update(None, "synchronous", "EXTRA")
                .unwrap();
            let query = "CREATE (:RetryProbe {request:'one'})";
            registration.control.start_deletes(Some(fault));
            other_registration.control.start_deletes(None);
            other.execute(query).unwrap();
            let other_events = other_registration.control.stop();
            assert!(
                other_events
                    .iter()
                    .any(|event| event.operation == Operation::Delete)
            );
            assert!(
                other_events
                    .iter()
                    .all(|event| event.result == ffi::SQLITE_OK)
            );
            let error = match mode {
                Mode::Standalone => database.execute(query).unwrap_err(),
                Mode::Transaction => {
                    let mut transaction = database.transaction().unwrap();
                    transaction.execute(query).unwrap();
                    transaction.commit().unwrap_err()
                }
            };
            assert_delete_error(error, fault);
            // Do not reset the controller: recovery and retry must consume no
            // second fault. Unconditional CREATE is deliberately non-idempotent.
            match mode {
                Mode::Standalone => {
                    database.execute(query).unwrap();
                }
                Mode::Transaction => {
                    let mut transaction = database.transaction().unwrap();
                    transaction.execute(query).unwrap();
                    transaction.commit().unwrap();
                }
            }
            let events = registration.control.stop();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.result != ffi::SQLITE_OK)
                    .count(),
                1
            );
            let count = match fault {
                DeleteFault::Before => 1,
                DeleteFault::After => 2,
            };
            for database in [&mut database, &mut open_existing(&path)] {
                assert_eq!(
                    database
                        .execute("MATCH (n:RetryProbe) RETURN n.request")
                        .unwrap()
                        .rows,
                    vec![vec![json!("one")]; count]
                );
                assert_integrity(database);
            }
            assert_eq!(
                other
                    .execute("MATCH (n:RetryProbe) RETURN n.request")
                    .unwrap()
                    .rows,
                [vec![json!("one")]]
            );
        }
    }
}
