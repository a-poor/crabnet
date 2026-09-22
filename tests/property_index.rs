//! The `node_properties` index: triggers keep it equal to the top-level
//! properties stored in `nodes`, and property lookups that use it return what
//! a label scan would.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crabnet::{CypherValue, Database, Parameters, QueryResult};
use rusqlite::Connection;
use serde_json::json;

static NEXT_DATABASE_PATH: AtomicUsize = AtomicUsize::new(0);

struct TemporaryDatabasePath(PathBuf);

impl TemporaryDatabasePath {
    fn new() -> Self {
        let sequence = NEXT_DATABASE_PATH.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "crabnet-property-index-{}-{timestamp}-{sequence}.sqlite",
            std::process::id(),
        )))
    }
}

impl Drop for TemporaryDatabasePath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Asserts that the index holds exactly one row per top-level non-null node
/// property, with the value `json_extract` would return.
fn assert_index_matches_nodes(path: &TemporaryDatabasePath) {
    let connection = Connection::open(&path.0).unwrap();
    let expected = "SELECT nodes.id, property.key, property.value \
                    FROM nodes, json_each(nodes.props) AS property \
                    WHERE property.type <> 'null'";
    let indexed = "SELECT node_id, key, value FROM node_properties";
    for (left, right) in [(expected, indexed), (indexed, expected)] {
        let stray: i64 = connection
            .query_row(
                &format!("SELECT count(*) FROM ({left} EXCEPT {right})"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stray, 0,
            "index and node properties differ:\n{left}\nEXCEPT\n{right}"
        );
    }
    let extracted_mismatches: i64 = connection
        .query_row(
            "SELECT count(*) FROM node_properties JOIN nodes ON nodes.id = node_id \
             WHERE json_extract(nodes.props, '$.' || json_quote(key)) IS NOT value",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(extracted_mismatches, 0);
}

fn integers(result: &QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            CypherValue::Integer(value) => *value,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn every_kind_of_write_keeps_the_index_in_step() {
    let path = TemporaryDatabasePath::new();
    let mut database = Database::open(&path.0).unwrap();
    let statements = [
        "CREATE (:P {id: 1, name: 'a', tags: ['x', 1], nested: {k: [1.5]}, gone: null}), \
                (:P {id: 2}), (:Q {id: 3})",
        "UNWIND range(4, 6) AS id CREATE (:P {id: id, weird: 'w'})",
        "MATCH (p:P {id: 2}) SET p.name = 'b', p.id = 20",
        "MATCH (p:P {id: 1}) SET p.name = null",
        "MATCH (p:P {id: 4}) SET p = {id: 40, fresh: true}",
        "MATCH (p:P {id: 5}) SET p += {id: 50, weird: null, more: 2.5}",
        "MATCH (p:P {id: 6}) REMOVE p.weird",
        "MATCH (a:P {id: 1}), (b:Q {id: 3}) CREATE (a)-[:R {id: 1}]->(b)",
        "MATCH (q:Q {id: 3}) DETACH DELETE q",
        "MATCH (p:P {id: 50}) DELETE p",
        "MERGE (m:P {id: 7}) ON CREATE SET m.made = true",
        "MERGE (m:P {id: 7}) ON MATCH SET m.made = false, m.id = 70",
        "UNWIND [1, 2, 1, 3] AS id MERGE (:Shadow {id: id})",
    ];
    for statement in statements {
        database.execute(statement).unwrap();
        assert_index_matches_nodes(&path);
    }

    let ids = database
        .execute("MATCH (p:P) WHERE p.id IN [1, 20, 40, 6, 70] RETURN p.id ORDER BY p.id")
        .unwrap();
    assert_eq!(integers(&ids), [1, 6, 20, 40, 70]);
    let shadows = database
        .execute("MATCH (s:Shadow) RETURN count(s)")
        .unwrap();
    assert_eq!(integers(&shadows), [3]);
}

#[test]
fn a_rolled_back_statement_leaves_the_index_unchanged() {
    let path = TemporaryDatabasePath::new();
    let mut database = Database::open(&path.0).unwrap();
    database
        .execute("CREATE (a:P {id: 1})-[:R]->(:P {id: 2})")
        .unwrap();
    // Setting a property succeeds, then deleting a connected node fails the
    // foreign-key check and rolls the whole statement back.
    let failed = database.execute("MATCH (p:P {id: 1}) SET p.id = 100 DELETE p");
    assert!(failed.is_err());
    assert_index_matches_nodes(&path);
    let found = database.execute("MATCH (p:P {id: 1}) RETURN p.id").unwrap();
    assert_eq!(integers(&found), [1]);
    let missing = database
        .execute("MATCH (p:P {id: 100}) RETURN p.id")
        .unwrap();
    assert!(missing.rows.is_empty());
}

#[test]
fn indexed_lookups_keep_cypher_equality() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:V {k: 1, tag: 'int'}), (:V {k: 1.0, tag: 'float'}), \
                    (:V {k: true, tag: 'true'}), (:V {k: '1', tag: 'text'}), \
                    (:V {k: [1, 2], tag: 'list'}), (:V {k: {a: 1}, tag: 'map'}), \
                    (:V {tag: 'missing'}), (:W {k: 1, tag: 'other-label'})",
        )
        .unwrap();

    let tags = |database: &mut Database, query: &str, value: serde_json::Value| {
        let parameters = Parameters::from([("value".to_owned(), value)]);
        let mut tags = database
            .execute_with(query, &parameters)
            .unwrap()
            .rows
            .into_iter()
            .map(|row| match &row[0] {
                CypherValue::String(tag) => tag.clone(),
                other => panic!("expected a tag, got {other:?}"),
            })
            .collect::<Vec<_>>();
        tags.sort();
        tags
    };

    let inline = "MATCH (v:V {k: $value}) RETURN v.tag";
    let filtered = "MATCH (v:V) WHERE v.k = $value RETURN v.tag";
    let reversed = "MATCH (v:V) WHERE $value = v.k AND v.tag IS NOT NULL RETURN v.tag";
    let cases = [
        (json!(1), vec!["float", "int"]),
        (json!(1.0), vec!["float", "int"]),
        (json!(true), vec!["true"]),
        (json!("1"), vec!["text"]),
        (json!([1, 2]), vec!["list"]),
        (json!({"a": 1}), vec!["map"]),
        (json!(null), vec![]),
        (json!(2), vec![]),
    ];
    for (value, expected) in cases {
        for query in [inline, filtered, reversed] {
            assert_eq!(
                tags(&mut database, query, value.clone()),
                expected,
                "{query} with {value}"
            );
        }
    }
}

#[test]
fn correlated_and_merge_lookups_use_the_row_value() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "UNWIND range(1, 20) AS id \
             CREATE (:Person {id: id, city: 'c' + toString(id % 3)})",
        )
        .unwrap();
    database
        .execute("UNWIND ['c0', 'c1', 'c2', 'c9'] AS name CREATE (:City {name: name})")
        .unwrap();

    let joined = database
        .execute(
            "MATCH (p:Person), (c:City) WHERE c.name = p.city \
             RETURN c.name AS city, count(p) AS people ORDER BY city",
        )
        .unwrap();
    assert_eq!(joined.rows.len(), 3);
    assert_eq!(
        joined.rows[0],
        [CypherValue::String("c0".into()), CypherValue::Integer(6)]
    );

    let merged = database
        .execute(
            "MATCH (p:Person) MERGE (c:City {name: p.city}) \
             MERGE (p)-[:LIVES_IN]->(c)",
        )
        .unwrap();
    assert_eq!(merged.summary.nodes_created, 0);
    assert_eq!(merged.summary.relationships_created, 20);
    let reverse = database
        .execute("MATCH (p:Person) WHERE p.id = 7 MATCH (p)-[:LIVES_IN]->(c) RETURN c.name")
        .unwrap();
    assert_eq!(reverse.rows, [[CypherValue::String("c1".into())]]);
}

#[test]
fn property_lookups_are_planned_through_the_index() {
    for query in [
        "MATCH (p:Person {id: $id}) RETURN p",
        "MATCH (p:Person) WHERE p.id = $id RETURN p",
        "MATCH (p:Person {id: $id})-[:KNOWS*1..3]->(f) RETURN count(f)",
        "MATCH (p:Person {id: $id})-[:KNOWS*]->(f) RETURN count(f)",
    ] {
        let physical = crabnet::explain_physical(query).unwrap();
        assert!(
            physical.contains("FROM node_properties AS _crabnet_property"),
            "{query} should probe the property index:\n{physical}"
        );
    }
    // Relationship properties and non-equality predicates have no index.
    for query in [
        "MATCH ()-[r:KNOWS {since: 1}]->() RETURN r",
        "MATCH (p:Person) WHERE p.id > $id RETURN p",
        "MATCH (p:Person) WHERE p.id = $id OR p.name = 'x' RETURN p",
    ] {
        let physical = crabnet::explain_physical(query).unwrap();
        assert!(
            !physical.contains("node_properties"),
            "{query}:\n{physical}"
        );
    }
}
