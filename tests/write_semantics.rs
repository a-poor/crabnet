use crabnet::{Database, Error};
use serde_json::json;

#[test]
fn create_duplicate_property_keys_fail_before_any_mutation() {
    let mut database = Database::in_memory().unwrap();

    let error = database
        .execute("CREATE (:Thing {name: 'first'}), (:Thing {value: 1, value: 2})")
        .unwrap_err();
    assert!(matches!(error, Error::Semantic(_)));

    let remaining = database
        .execute("MATCH (thing:Thing) RETURN thing.name AS name")
        .unwrap();
    assert!(remaining.rows.is_empty());
}

#[test]
fn mutations_without_return_have_empty_rows_and_accurate_summaries() {
    let mut database = Database::in_memory().unwrap();

    let seeded = database
        .execute("CREATE (:Person {name: 'Ada'}), (:Person {name: 'Grace'})")
        .unwrap();
    assert!(seeded.columns.is_empty());
    assert!(seeded.rows.is_empty());
    assert_eq!(seeded.summary.nodes_created, 2);
    assert_eq!(seeded.summary.relationships_created, 0);

    let connected = database
        .execute(
            "MATCH (person:Person) \
             CREATE (person)-[:USES]->(:Tool {name: 'crabnet'})",
        )
        .unwrap();
    assert!(connected.columns.is_empty());
    assert!(connected.rows.is_empty());
    assert_eq!(connected.summary.nodes_created, 2);
    assert_eq!(connected.summary.relationships_created, 2);

    let tools = database
        .execute(
            "MATCH (person:Person)-[:USES]->(tool:Tool) \
             RETURN person.name AS person, tool.name AS tool ORDER BY person",
        )
        .unwrap();
    assert_eq!(
        tools.rows,
        [
            vec![json!("Ada"), json!("crabnet")],
            vec![json!("Grace"), json!("crabnet")],
        ]
    );
}

#[test]
fn matched_property_and_delete_mutations_without_return_report_only_summaries() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada'})-[:USES]->(:Tool {name: 'sqlite'}), \
                    (:Person {name: 'Grace'})-[:USES]->(:Tool {name: 'rust'})",
        )
        .unwrap();

    let updated = database
        .execute("MATCH (person:Person) SET person.active = true")
        .unwrap();
    assert!(updated.columns.is_empty());
    assert!(updated.rows.is_empty());
    assert_eq!(updated.summary.properties_set, 2);
    assert_eq!(updated.summary.nodes_deleted, 0);
    assert_eq!(updated.summary.relationships_deleted, 0);

    let deleted = database
        .execute("MATCH (person:Person) DETACH DELETE person")
        .unwrap();
    assert!(deleted.columns.is_empty());
    assert!(deleted.rows.is_empty());
    assert_eq!(deleted.summary.properties_set, 0);
    assert_eq!(deleted.summary.nodes_deleted, 2);
    assert_eq!(deleted.summary.relationships_deleted, 2);

    let remaining = database
        .execute("MATCH (tool:Tool) RETURN tool.name AS name ORDER BY name")
        .unwrap();
    assert_eq!(remaining.rows, [vec![json!("rust")], vec![json!("sqlite")]]);
}
