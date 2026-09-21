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

#[test]
fn consecutive_create_clauses_share_bindings_and_follow_multiple_matches() {
    let mut database = Database::in_memory().unwrap();

    // Later CREATE clauses reuse variables bound by earlier ones.
    let seeded = database
        .execute(
            "CREATE (a:A {name: 'a'}), (b:B {name: 'b'}) \
             CREATE (a)-[:T]->(b), (b)-[:T]->(:C {name: 'c'}) \
             CREATE (a)-[:U]->(b)",
        )
        .unwrap();
    assert_eq!(seeded.summary.nodes_created, 3);
    assert_eq!(seeded.summary.relationships_created, 3);
    let edges = database
        .execute("MATCH (x)-[r]->(y) RETURN x.name AS x, type(r) AS t, y.name AS y ORDER BY t, x")
        .unwrap();
    assert_eq!(
        edges.rows,
        [
            vec![json!("a"), json!("T"), json!("b")],
            vec![json!("b"), json!("T"), json!("c")],
            vec![json!("a"), json!("U"), json!("b")],
        ]
    );

    // Independent CREATE clauses simply accumulate.
    let repeated = database
        .execute("CREATE (:N {num: 1}) CREATE (:N {num: 2}) CREATE (:N {num: 3}) RETURN 1 AS ok")
        .unwrap();
    assert_eq!(repeated.summary.nodes_created, 3);
    assert_eq!(repeated.rows, [vec![json!(1)]]);

    // Several MATCH clauses may precede the CREATE clauses and bind inputs.
    let linked = database
        .execute(
            "MATCH (a:A) MATCH (c:C) \
             CREATE (a)-[:V]->(m:M) \
             CREATE (m)-[:V]->(c) \
             RETURN m.name AS name",
        )
        .unwrap();
    assert_eq!(linked.summary.nodes_created, 1);
    assert_eq!(linked.summary.relationships_created, 2);
    assert_eq!(linked.rows, [vec![json!(null)]]);
    let path = database
        .execute("MATCH (:A)-[:V]->(:M)-[:V]->(c:C) RETURN c.name AS name")
        .unwrap();
    assert_eq!(path.rows, [vec![json!("c")]]);

    // MATCH after CREATE stays rejected.
    let error = database
        .execute("CREATE (:X) MATCH (n) RETURN n")
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
}
