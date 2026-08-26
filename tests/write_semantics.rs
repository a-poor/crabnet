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
