use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crabnet::{Database, Error, Parameters, UniqueConstraint};
use serde_json::json;

#[test]
fn installing_a_constraint_checks_existing_data_and_duplicate_writes_roll_back() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {email: 'ada'}), (:Person {email: 'ada'})")
        .unwrap();
    assert!(matches!(
        database.create_unique_constraint("Person", "email"),
        Err(Error::Constraint(_))
    ));
    assert!(database.unique_constraints().unwrap().is_empty());
    database.execute("MATCH (p:Person) DELETE p").unwrap();
    assert!(
        database
            .create_unique_constraint("Person", "email")
            .unwrap()
    );
    assert!(
        !database
            .create_unique_constraint("Person", "email")
            .unwrap()
    );
    let constraints = database.unique_constraints().unwrap();
    assert_eq!(constraints.len(), 1);
    assert_eq!(constraints[0].label, "Person");
    assert_eq!(constraints[0].property, "email");
    database.execute("CREATE (:Person {email: 'ada'})").unwrap();
    let error = database
        .execute("CREATE (:Temporary), (:Person {email: 'ada'})")
        .unwrap_err();
    assert!(
        matches!(&error, Error::Constraint(message) if message.contains("Person") && message.contains("email")),
        "{error}"
    );
    assert_eq!(
        database.execute("MATCH (n) RETURN count(n)").unwrap().rows,
        [vec![json!(1)]]
    );
    assert!(database.drop_unique_constraint("Person", "email").unwrap());
    assert!(!database.drop_unique_constraint("Person", "email").unwrap());
    database.execute("CREATE (:Person {email: 'ada'})").unwrap();
}

fn constrained_database() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .create_unique_constraint("Person", "email")
        .unwrap();
    database
}

fn assert_conflict<T: std::fmt::Debug>(result: crabnet::Result<T>) {
    assert!(matches!(result, Err(Error::Constraint(_))), "{result:?}");
}

#[test]
fn typed_values_use_recursive_distinct_equivalence_without_float_rounding() {
    let cases = [
        (json!(1), json!(1.0), true),
        (json!(0), json!(-0.0), true),
        (json!(true), json!(1), false),
        (json!(false), json!(0), false),
        (json!("1"), json!(1), false),
        (json!("Ada"), json!("ada"), false),
        (json!("é"), json!("e\u{301}"), false),
        (json!(9007199254740993i64), json!(9007199254740992.0), false),
        (json!(9007199254740992i64), json!(9007199254740992.0), true),
        (json!(i64::MAX), json!(9223372036854775808.0), false),
        (json!(i64::MIN), json!(-9223372036854775808.0), true),
        (json!(1.0000000000000002), json!(1.0000000000000004), false),
        (json!(1.2345678901234567), json!(1.2345678901234567), true),
        (
            json!([1, null, {"a": [2]}]),
            json!([1.0, null, {"a": [2.0]}]),
            true,
        ),
        (json!([1, 2]), json!([2, 1]), false),
        (json!([null, 1]), json!([null, 2]), false),
        (
            json!({"a": 1, "b": null}),
            json!({"b": null, "a": 1.0}),
            true,
        ),
        (json!({"a": null}), json!({}), false),
        (json!([]), json!({}), false),
        (json!(null), json!(null), false),
    ];
    for (left, right, conflicts) in cases {
        // Enforcing a new write and installing over existing values must
        // agree, including near the numeric precision boundaries.
        for install_first in [true, false] {
            let mut database = Database::in_memory().unwrap();
            if install_first {
                database
                    .create_unique_constraint("Person", "email")
                    .unwrap();
            }
            let insert = |database: &mut Database, value| {
                database.execute_with(
                    "CREATE (:Person {email: $value})",
                    &Parameters::from([("value".into(), value)]),
                )
            };
            insert(&mut database, left.clone()).unwrap();
            let result = if install_first {
                insert(&mut database, right.clone()).map(|_| true)
            } else {
                insert(&mut database, right.clone()).unwrap();
                database.create_unique_constraint("Person", "email")
            };
            if conflicts {
                assert_conflict(result);
            } else {
                assert!(result.unwrap(), "{left} / {right}");
            }
        }
    }
}

#[test]
fn constraints_are_scoped_to_exact_node_labels_and_property_keys() {
    let mut database = constrained_database();
    database
        .execute(
            "CREATE (:Person), (:Person {email: null}), (:Person {email: null}), \
         (:Person:Member {email: 'a'}), (:Other {email: 'a'}), (:person {email: 'a'}), \
         (:Person {Email: 'a'}), (a)-[:Person {email: 'a'}]->(b), (a)-[:Person {email: 'a'}]->(b)",
        )
        .unwrap();
    assert_conflict(database.execute("CREATE (:Member:Person {email: 'a'})"));
    // A second property and a second label are independent declarations.
    database.create_unique_constraint("Other", "email").unwrap();
    database
        .create_unique_constraint("Person", "handle")
        .unwrap();
    database
        .execute("CREATE (:Person {email: 'b', handle: 'h'})")
        .unwrap();
    assert_conflict(database.execute("CREATE (:Person {email: 'c', handle: 'h'})"));
    database.drop_unique_constraint("Person", "email").unwrap();
    database.execute("CREATE (:Person {email: 'a'})").unwrap();
    assert_conflict(database.execute("CREATE (:Other {email: 'a'})"));
    assert_eq!(
        database.unique_constraints().unwrap(),
        [
            UniqueConstraint {
                label: "Other".into(),
                property: "email".into()
            },
            UniqueConstraint {
                label: "Person".into(),
                property: "handle".into()
            },
        ]
    );
}

#[test]
fn every_conflicting_mutation_rolls_back_the_graph_and_indexes() {
    let mutations = [
        "CREATE (:Person {email: 'a'})",
        "SET b.email = a.email",
        "SET b = {email: 'a'}",
        "SET b = a",
        "SET b += {email: 'a'}",
        "SET q:Person",
        "SET q:Added:Person",
        "MERGE (:Person {email: 'a', extra: 1})",
        "MERGE (m:Person {email: 'b'}) ON MATCH SET m.email = 'a'",
        "MERGE (m:Person {email: 'c'}) ON CREATE SET m.email = 'a'",
        "MERGE (m:Other {email: 'a'}) ON MATCH SET m:Person",
        "MERGE (m:Other {email: 'b'}) ON CREATE SET m:Person",
        "WITH b SET b.email = 'c' WITH b SET b.email = 'a'",
    ];
    for mutation in mutations {
        let mut database = constrained_database();
        database
            .execute("CREATE (:Person {email: 'a'}), (:Person {email: 'b'}), (:Other {email: 'a'})")
            .unwrap();
        let snapshot = "MATCH (n) RETURN n ORDER BY elementId(n)";
        let before = database.execute(snapshot).unwrap().rows;
        let query = format!(
            "MATCH (a:Person {{email: 'a'}}), (b:Person {{email: 'b'}}), (q:Other) \
             CREATE (:Temporary)-[:TEMP]->(a) {mutation}"
        );
        let result = database.execute(&query);
        assert!(
            matches!(result, Err(Error::Constraint(_))),
            "{query}: {result:?}"
        );
        assert_eq!(
            database.execute(snapshot).unwrap().rows,
            before,
            "{mutation}"
        );
        assert_eq!(
            database
                .execute("MATCH ()-[r]->() RETURN count(r)")
                .unwrap()
                .rows,
            [vec![json!(0)]]
        );
        // Both the original reservation and rolled-back fresh keys are usable
        // in exactly the same way as before the failed statement.
        assert_conflict(database.execute("CREATE (:Person {email: 'b'})"));
        database.execute("CREATE (:Person {email: 'c'})").unwrap();
        assert_eq!(
            database
                .execute("MATCH (n:Person {email: 'c'}) RETURN count(n)")
                .unwrap()
                .rows,
            [vec![json!(1)]]
        );
    }
}

#[test]
fn changing_or_removing_a_key_releases_it_for_reuse() {
    for mutation in [
        "SET p.email = 'changed'",
        "SET p.email = null",
        "SET p = {email: 'changed'}",
        "SET p = {other: 1}",
        "SET p += {email: null}",
        "REMOVE p.email",
        "REMOVE p:Person",
        "DELETE p",
        "DETACH DELETE p",
    ] {
        let mut database = constrained_database();
        database.execute("CREATE (:Person {email: 'a'})").unwrap();
        if mutation == "DETACH DELETE p" {
            database
                .execute("MATCH (p:Person) CREATE (p)-[:LINK]->(:Neighbor)")
                .unwrap();
        }
        database
            .execute(&format!("MATCH (p:Person) {mutation}"))
            .unwrap();
        database.execute("CREATE (:Person {email: 'a'})").unwrap();
        assert_conflict(database.execute("CREATE (:Person {email: 'a'})"));
        if mutation.contains("changed") {
            assert_conflict(database.execute("CREATE (:Person {email: 'changed'})"));
        }
    }
}

#[test]
fn bag_writes_merge_and_same_node_updates_keep_their_existing_semantics() {
    let mut database = constrained_database();
    let result = database
        .execute(
            "UNWIND ['a', 'b', 'a'] AS email MERGE (p:Person {email: email}) \
         ON CREATE SET p.visits = 0 ON MATCH SET p.visits = p.visits + 1 \
         RETURN p.email ORDER BY p.email",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![json!("a")], vec![json!("a")], vec![json!("b")]]
    );
    assert_eq!(result.summary.nodes_created, 2);
    let result = database
        .execute(
            "MATCH (p:Person {email: 'a'}) UNWIND [1, 1] AS i \
         SET p.email = 'a', p:Person:Member RETURN p.email",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!("a")], vec![json!("a")]]);
    assert_eq!(result.summary.properties_set, 0);
    assert_eq!(result.summary.labels_added, 1);
    assert_conflict(database.execute("UNWIND ['c', 'c'] AS email CREATE (:Person {email: email})"));
    database.execute("CREATE (:Person {email: 'c'})").unwrap();
    // Constraint checks are immediate, even when a later mutation would
    // remove the conflicting label. Applications must release the key first.
    assert_conflict(
        database.execute("MATCH (p:Person {email: 'b'}) SET p.email = 'a' REMOVE p:Person"),
    );
    database
        .execute("MATCH (p:Person {email: 'b'}) REMOVE p:Person SET p.email = 'a'")
        .unwrap();
}

#[test]
fn constraint_changes_and_data_share_transaction_commit_and_rollback() {
    let mut database = Database::in_memory().unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction
        .execute("CREATE (:Person {email: 'a'})")
        .unwrap();
    assert!(
        transaction
            .create_unique_constraint("Person", "email")
            .unwrap()
    );
    assert!(
        !transaction
            .create_unique_constraint("Person", "email")
            .unwrap()
    );
    assert_eq!(transaction.unique_constraints().unwrap().len(), 1);
    transaction.commit().unwrap();

    for explicit in [true, false] {
        let mut transaction = database.transaction().unwrap();
        transaction
            .drop_unique_constraint("Person", "email")
            .unwrap();
        transaction
            .execute("CREATE (:Person {email: 'a'})")
            .unwrap();
        transaction
            .create_unique_constraint("Temporary", "key")
            .unwrap();
        if explicit {
            transaction.rollback().unwrap();
        } else {
            drop(transaction);
        }
        assert_eq!(database.unique_constraints().unwrap().len(), 1);
        assert_conflict(database.execute("CREATE (:Person {email: 'a'})"));
    }
    let mut transaction = database.transaction().unwrap();
    transaction
        .drop_unique_constraint("Person", "email")
        .unwrap();
    transaction
        .execute("CREATE (:Person {email: 'a'})")
        .unwrap();
    transaction.commit().unwrap();
    assert!(database.unique_constraints().unwrap().is_empty());
    assert_conflict(database.create_unique_constraint("Person", "email"));
}

#[test]
fn constraint_errors_abort_prior_schema_changes_and_all_prior_queries() {
    for fail_install in [true, false] {
        let mut database = constrained_database();
        database.execute("CREATE (:Person {email: 'a'})").unwrap();
        let mut transaction = database.transaction().unwrap();
        transaction
            .execute("MATCH (p:Person) SET p.email = 'b'")
            .unwrap();
        transaction
            .create_unique_constraint("Temporary", "key")
            .unwrap();
        if fail_install {
            transaction
                .drop_unique_constraint("Person", "email")
                .unwrap();
            transaction
                .execute("CREATE (:Person {email: 'b'})")
                .unwrap();
            assert_conflict(transaction.create_unique_constraint("Person", "email"));
        } else {
            transaction
                .execute("CREATE (:Person {email: 'a'})")
                .unwrap();
            assert_conflict(transaction.execute("CREATE (:Person {email: 'b'})"));
        }
        assert!(matches!(
            transaction.execute("RETURN 1"),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(
            transaction.unique_constraints(),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(
            transaction.create_unique_constraint("New", "key"),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(
            transaction.drop_unique_constraint("Person", "email"),
            Err(Error::TransactionAborted)
        ));
        assert!(matches!(
            transaction.commit(),
            Err(Error::TransactionAborted)
        ));
        assert_eq!(database.unique_constraints().unwrap().len(), 1);
        assert_eq!(
            database
                .execute("MATCH (p:Person) RETURN p.email")
                .unwrap()
                .rows,
            [vec![json!("a")]]
        );
        assert_conflict(database.execute("CREATE (:Person {email: 'a'})"));
        database.execute("CREATE (:Person {email: 'b'})").unwrap();
    }
}

#[test]
fn parameter_and_result_failures_restore_unique_keys() {
    let mut database = constrained_database();
    database.execute("CREATE (:Person {email: 'a'})").unwrap();
    assert!(matches!(
        database.execute("MATCH (p:Person) SET p.email = 'b' RETURN $missing"),
        Err(Error::MissingParameter(_))
    ));
    let parameters = Parameters::from([("index".into(), json!(true))]);
    for mutation in [
        "SET p.email = 'b'",
        "REMOVE p.email",
        "REMOVE p:Person",
        "DELETE p",
    ] {
        assert!(matches!(
            database.execute_with(
                &format!("MATCH (p:Person) {mutation} RETURN [1][$index]"),
                &parameters
            ),
            Err(Error::Type(_))
        ));
        assert_conflict(database.execute("CREATE (:Person {email: 'a'})"));
    }
    assert_conflict(database.execute("CREATE (:Person {email: 'a'})"));
    database.execute("CREATE (:Person {email: 'b'})").unwrap();
}

#[test]
fn overlapping_labels_keep_independent_reservations() {
    let mut database = constrained_database();
    database
        .create_unique_constraint("Member", "email")
        .unwrap();
    database.execute("CREATE (:Other {email: 'a'})").unwrap();
    database
        .execute("MATCH (p:Other) SET p:Person:Member")
        .unwrap();
    assert_conflict(database.execute("CREATE (:Person {email: 'a'})"));
    assert_conflict(database.execute("CREATE (:Member {email: 'a'})"));
    database.execute("MATCH (p:Other) REMOVE p:Person").unwrap();
    database.execute("CREATE (:Person {email: 'a'})").unwrap();
    assert_conflict(database.execute("CREATE (:Member {email: 'a'})"));
    assert_conflict(database.execute("MATCH (p:Other) SET p:Person"));
    database.drop_unique_constraint("Person", "email").unwrap();
    database.execute("MATCH (p:Other) SET p:Person").unwrap();
    assert_conflict(database.execute("CREATE (:Member {email: 'a'})"));
}

#[test]
fn names_are_bound_data_including_quotes_unicode_and_empty_strings() {
    for (label, property) in [
        ("Person'); DROP TABLE nodes; --", "a\".$['b']`"),
        ("人", "名"),
        ("", ""),
    ] {
        let mut database = Database::in_memory().unwrap();
        database.create_unique_constraint(label, property).unwrap();
        let escaped_label = label.replace('`', "``");
        let query = format!("CREATE (:`{escaped_label}` $props)");
        let parameters = Parameters::from([("props".into(), json!({property: "same"}))]);
        database.execute_with(&query, &parameters).unwrap();
        assert_conflict(database.execute_with(&query, &parameters));
        assert_eq!(
            database.unique_constraints().unwrap(),
            [UniqueConstraint {
                label: label.into(),
                property: property.into()
            }]
        );
        database.drop_unique_constraint(label, property).unwrap();
        database.execute_with(&query, &parameters).unwrap();
    }
}

struct DatabaseFile(PathBuf);

impl DatabaseFile {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "crabnet-uniqueness-{}-{timestamp}-{sequence}.sqlite",
            std::process::id()
        )))
    }
}

impl Drop for DatabaseFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn constraints_persist_and_other_connections_observe_schema_changes() {
    let file = DatabaseFile::new();
    let mut first = Database::open(&file.0).unwrap();
    let mut second = Database::open(&file.0).unwrap();
    second.execute("CREATE (:Person {email: 'a'})").unwrap();
    first.create_unique_constraint("Person", "email").unwrap();
    assert_eq!(second.unique_constraints().unwrap().len(), 1);
    assert_conflict(second.execute("CREATE (:Person {email: 'a'})"));
    second.execute("CREATE (:Person {email: 'b'})").unwrap();
    assert_conflict(first.execute("CREATE (:Person {email: 'b'})"));
    drop(first);
    drop(second);

    let mut reopened = Database::open(&file.0).unwrap();
    assert_eq!(reopened.unique_constraints().unwrap().len(), 1);
    assert_conflict(reopened.execute("CREATE (:Person {email: 'a'})"));
    reopened
        .execute("MATCH (p:Person {email: 'a'}) SET p.email = 'c'")
        .unwrap();
    reopened.execute("CREATE (:Person {email: 'a'})").unwrap();
    let mut other = Database::open(&file.0).unwrap();
    reopened.drop_unique_constraint("Person", "email").unwrap();
    other.execute("CREATE (:Person {email: 'a'})").unwrap();
    drop(reopened);
    drop(other);
    let mut reopened = Database::open(&file.0).unwrap();
    assert!(reopened.unique_constraints().unwrap().is_empty());
    assert_conflict(reopened.create_unique_constraint("Person", "email"));
}
