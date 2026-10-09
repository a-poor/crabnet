//! The `node_properties` index: triggers keep it equal to the top-level
//! properties stored in `nodes`, and property lookups that use it return what
//! a label scan would.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions, QueryResult};
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
             WHERE json_extract(nodes.props, '$.' || json_quote(key)) IS NOT value
                OR typeof(json_extract(nodes.props, '$.' || json_quote(key))) <> typeof(value)",
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
fn incremental_updates_preserve_storage_kinds_and_remove_missing_keys() {
    let path = TemporaryDatabasePath::new();
    let mut database = Database::open(&path.0).unwrap();
    database
        .execute("CREATE (:P {anchor: 'one', kept: [1,true]})")
        .unwrap();
    for (value, matches) in [
        (json!(1), 1),
        (json!(1.0), 1),
        (json!(true), 1),
        (json!(false), 1),
        (json!(0.0), 1),
        (json!(9_007_199_254_740_993_i64), 1),
        (json!(9_007_199_254_740_992.0), 1),
        (json!("1"), 1),
        (json!([1, null]), 0), // Equality is unknown when null comparison is required.
        (json!({"x":[1.5,true]}), 1),
        (json!(null), 0),
    ] {
        let parameters = Parameters::from([("value".into(), value.clone())]);
        database
            .execute_with(
                "MATCH (p:P {anchor:'one'}) SET p.value = $value",
                &parameters,
            )
            .unwrap();
        assert_index_matches_nodes(&path);
        assert_eq!(
            database
                .execute("MATCH (p:P {anchor:'one'}) RETURN p.value, p.kept")
                .unwrap()
                .rows,
            [vec![value.clone(), json!([1, true])]]
        );
        let found = database
            .execute_with("MATCH (p:P {value:$value}) RETURN count(p)", &parameters)
            .unwrap();
        assert_eq!(integers(&found), [matches], "value={value}");
        assert_eq!(
            database
                .execute_with(
                    "MATCH (p:P {anchor:'one'}) SET p.value = $value",
                    &parameters
                )
                .unwrap()
                .summary
                .properties_set,
            0
        );
    }
    for query in [
        "MATCH (p:P) SET p += {kept:null, added:'yes'}",
        "MATCH (p:P) SET p = {fresh:[1,2], absent:null}",
        "MATCH (p:P) REMOVE p.fresh",
    ] {
        database.execute(query).unwrap();
        assert_index_matches_nodes(&path);
    }
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
        // Composites equal values whose JSON text differs, so the probe
        // must not narrow them by text.
        (json!([1.0, 2]), vec!["list"]),
        (json!({"a": 1.0}), vec!["map"]),
        (json!([1, null]), vec![]),
        (json!(null), vec![]),
        (json!(2), vec![]),
    ];
    for (value, expected) in cases {
        for query in [
            inline,
            filtered,
            reversed,
            "WITH {key: $value} AS row MATCH (v:V) WHERE v.k = row.key RETURN v.tag",
            "MATCH (v:V) WHERE {nested: [$value]}.nested[0] = v.k RETURN v.tag",
            "MATCH (v:V) WHERE v.k = {nested: [$value]}['nested'][-1] RETURN v.tag",
            "MATCH (v:V) WHERE v.k = coalesce($value, null) RETURN v.tag",
            "MATCH (v:V) WHERE v.k = CASE WHEN $value IS NULL THEN null ELSE $value END RETURN v.tag",
        ] {
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

#[test]
fn computed_merge_keys_preserve_typed_bags_and_repeated_imports() {
    let values = json!([
        {"id": 1}, {"id": 1.0}, {"id": true}, {"id": "1"},
        {"id": [1.0, 2]}, {"id": {"a": 1.0}}, {"id": [1, 2]},
        {"id": 0}, {"id": -0.0}, {"id": 9007199254740992_i64},
        {"id": 9007199254740993_i64}, {"id": "new"}, {"id": "new"}
    ]);
    let parameters = Parameters::from([("rows".into(), values)]);
    let mut observations = Vec::new();
    for key in ["row.id", "row.nested.ids[-1]", "row[$key]", "key"] {
        let mut database = Database::in_memory().unwrap();
        database
            .execute("CREATE (:V {k: 1}), (:V {k: 1.0}), (:V {k: true}), (:Other {k: 'new'})")
            .unwrap();
        let mut parameters = parameters.clone();
        parameters.insert("key".into(), json!("id"));
        if key == "row.nested.ids[-1]" {
            for row in parameters.get_mut("rows").unwrap().as_array_mut().unwrap() {
                row["nested"] = json!({"ids": [null, row["id"]]});
            }
        }
        let alias = if key == "key" {
            "WITH row, row.id AS key"
        } else {
            ""
        };
        let query = format!(
            "UNWIND $rows AS row {alias} MERGE (n:V {{k: {key}}}) \
             ON CREATE SET n.hits = 1 ON MATCH SET n.hits = coalesce(n.hits, 0) + 1 \
             RETURN n.k, n.hits"
        );
        let mut first = database.execute_with(&query, &parameters).unwrap();
        assert_eq!(
            first.rows.len(),
            15,
            "{key}: duplicate nodes must expand rows"
        );
        assert_eq!(first.summary.nodes_created, 7, "{key}");
        let mut repeat = database.execute_with(&query, &parameters).unwrap();
        assert_eq!(repeat.rows.len(), 15, "{key}");
        assert_eq!(repeat.summary.nodes_created, 0, "{key}");
        assert_eq!(
            integers(&database.execute("MATCH (n:V) RETURN count(n)").unwrap()),
            [10]
        );
        // Multiple matches have no defined order. Compare typed bags without
        // collapsing duplicate input rows or equal numeric representations.
        first.rows.sort_by_cached_key(|row| format!("{row:?}"));
        repeat.rows.sort_by_cached_key(|row| format!("{row:?}"));
        observations.push((first, repeat));
    }
    for pair in observations.windows(2) {
        assert_eq!(pair[0], pair[1]);
    }
}

#[test]
fn computed_merge_keys_use_index_within_a_small_sqlite_work_budget() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("UNWIND range(1, 2000) AS id CREATE (:V {k: id})")
        .unwrap();
    let parameters = Parameters::from([(
        "rows".into(),
        json!([
            {"id": 500, "nested": {"ids": [500]}, "text": "500"},
            {"id": 1999, "nested": {"ids": [1999]}, "text": "1999"}
        ]),
    )]);
    // A coarse ceiling proves the MERGE matcher does indexed work. This is
    // deliberately far above two probes, but below scanning 2,000 nodes.
    let options = QueryOptions {
        max_sqlite_steps: Some(30_000),
        ..QueryOptions::default()
    };
    for key in [
        "row.id",
        "row['id']",
        "row.nested.ids[-1]",
        "toInteger(row.text)",
        "coalesce(row.missing, row.id)",
        "(row.id - 1) + 1",
    ] {
        let result = database
            .execute_with_options(
                &format!("UNWIND $rows AS row MERGE (n:V {{k: {key}}}) RETURN n.k"),
                &parameters,
                &options,
            )
            .unwrap_or_else(|error| panic!("{key}: {error}"));
        assert_eq!(integers(&result), [500, 1999]);
        assert_eq!(result.summary.nodes_created, 0);
    }
}

#[test]
fn computed_key_errors_rollback_writes_and_property_indexes() {
    let path = TemporaryDatabasePath::new();
    let mut database = Database::open(&path.0).unwrap();
    database.execute("CREATE (:V {k: 1})").unwrap();
    for bad in [json!(3), json!("not a map"), json!(true), json!([])] {
        let before = database.execute("MATCH (n) RETURN n ORDER BY n.k").unwrap();
        let parameters = Parameters::from([("rows".into(), json!([{"id": 2}, bad]))]);
        let mut transaction = database.transaction().unwrap();
        transaction
            .execute("MATCH (n:V) SET n.k = 10 CREATE (:Earlier {k: 99})")
            .unwrap();
        let error = transaction
            .execute_with(
                "UNWIND $rows AS row MERGE (n:V {k: row.id}) ON CREATE SET n.created = true",
                &parameters,
            )
            .unwrap_err();
        assert!(matches!(error, Error::Type(_)), "{error}");
        assert!(matches!(
            transaction.commit(),
            Err(Error::TransactionAborted)
        ));
        assert_eq!(
            database.execute("MATCH (n) RETURN n ORDER BY n.k").unwrap(),
            before
        );
        assert_index_matches_nodes(&path);
    }
    let missing =
        database.execute("UNWIND [{id: 2}] AS row MERGE (n:V {k: coalesce(row.id, $missing)})");
    assert!(matches!(missing, Err(Error::MissingParameter(_))));
    assert_index_matches_nodes(&path);
}

#[test]
fn computed_keys_preserve_nulls_guards_and_correlated_matches() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:V {k: 1}), (:V {k: 2}), (:Source {nested: {id: 1}})")
        .unwrap();
    let parameters = Parameters::from([
        (
            "rows".into(),
            json!([null, {}, {"id": null}, {"id": 1}, {"id": 1}]),
        ),
        ("bad".into(), json!(7)),
    ]);
    let query = "UNWIND $rows AS row OPTIONAL MATCH (n:V) WHERE n.k = row.id RETURN n.k";
    let mut found = database.execute_with(query, &parameters).unwrap().rows;
    found.sort_by_key(|row| row[0].as_i64());
    assert_eq!(
        found,
        [
            vec![json!(null)],
            vec![json!(null)],
            vec![json!(null)],
            vec![json!(1)],
            vec![json!(1)]
        ]
    );
    for expression in [
        "CASE WHEN $bad = 7 THEN 1 ELSE $bad.id END",
        "coalesce(1, $bad.id)",
    ] {
        let result = database
            .execute_with(
                &format!("MATCH (n:V) WHERE n.k = {expression} RETURN n.k"),
                &parameters,
            )
            .unwrap();
        assert_eq!(integers(&result), [1]);
    }
    let joined = database
        .execute("MATCH (s:Source), (n:V) WHERE n.k = s.nested.id RETURN n.k")
        .unwrap();
    assert_eq!(integers(&joined), [1]);
    // Computed nulls never match; the existing MERGE contract creates nodes
    // without the null-valued property on each invocation.
    for _ in 0..2 {
        let result = database
            .execute_with(
                "UNWIND $rows AS row MERGE (n:V {k: row.id}) RETURN n.k",
                &parameters,
            )
            .unwrap();
        assert_eq!(result.summary.nodes_created, 3);
        assert_eq!(
            result.rows,
            [
                vec![json!(null)],
                vec![json!(null)],
                vec![json!(null)],
                vec![json!(1)],
                vec![json!(1)]
            ]
        );
    }
    assert_eq!(
        integers(&database.execute("MATCH (n:V) RETURN count(n)").unwrap()),
        [8]
    );
    let empty = Parameters::from([("rows".into(), json!([]))]);
    let result = database
        .execute_with(
            "UNWIND $rows AS row MERGE (n:V {k: row.id}) RETURN n",
            &empty,
        )
        .unwrap();
    assert!(result.rows.is_empty());
    assert_eq!(result.summary.nodes_created, 0);
}

#[test]
fn computed_probes_follow_mutations_and_match_whole_patterns() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:V {k: 1})").unwrap();
    let result = database
        .execute(
            "UNWIND [{id: 1, next: 2}, {id: 2, next: 3}] AS row \
         MERGE (n:V {k: row.id}) ON MATCH SET n.k = row.next RETURN n.k",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_created, 0);
    assert_eq!(integers(&result), [2, 3]);
    assert_eq!(
        integers(&database.execute("MATCH (n:V) RETURN n.k").unwrap()),
        [3]
    );

    // A node probe only supplies candidates. Labels, the second property,
    // relationship type and properties, and every endpoint still constrain
    // the whole-pattern match.
    database
        .execute(
            "CREATE (:V {k: 4, tag: 'wrong'}), \
                (a:V {k: 4, tag: 'ok'})-[:R {kind: 'x'}]->(:V {k: 5}), \
                (a)-[:R {kind: 'x'}]->(:V {k: 5}), \
                (a)-[:R {kind: 'wrong'}]->(:V {k: 5}), \
                (a)-[:OTHER {kind: 'x'}]->(:V {k: 5}), \
                (:Other {k: 4, tag: 'ok'})-[:R {kind: 'x'}]->(:V {k: 5})",
        )
        .unwrap();
    let parameters = Parameters::from([(
        "rows".into(),
        json!([
            {"id": 4, "tag": "ok", "kind": "x", "target": 5},
            {"id": 4, "tag": "new", "kind": "x", "target": 6}
        ]),
    )]);
    let query = "UNWIND $rows AS row \
        MERGE (a:V {k: row.id, tag: row.tag})-[r:R {kind: row.kind}]->(b:V {k: row.target}) \
        RETURN a.tag, b.k";
    let first = database.execute_with(query, &parameters).unwrap();
    assert_eq!(first.summary.nodes_created, 2);
    assert_eq!(first.summary.relationships_created, 1);
    assert_eq!(
        first.rows,
        [
            vec![json!("ok"), json!(5)],
            vec![json!("ok"), json!(5)],
            vec![json!("new"), json!(6)]
        ]
    );
    let repeat = database.execute_with(query, &parameters).unwrap();
    assert_eq!(repeat.rows, first.rows);
    assert_eq!(repeat.summary.nodes_created, 0);
    assert_eq!(repeat.summary.relationships_created, 0);
}

#[test]
fn read_filters_do_not_evaluate_computed_keys_without_candidates() {
    for setup in ["RETURN 0", "CREATE (:Other {k: 1})", "CREATE (:V {k: 1})"] {
        let mut database = Database::in_memory().unwrap();
        database.execute(setup).unwrap();
        let parameters = Parameters::from([("rows".into(), json!([7]))]);
        let query = "UNWIND $rows AS row MATCH (n:V) WHERE n.k = row.id RETURN n.k";
        let physical = crabnet::explain_physical(query).unwrap();
        assert!(!physical.contains("node_properties"));
        let result = database.execute_with(query, &parameters);
        if setup == "CREATE (:V {k: 1})" {
            assert!(matches!(result, Err(Error::Type(_))));
        } else {
            assert!(result.unwrap().rows.is_empty());
        }
        let guarded = "UNWIND $rows AS row MATCH (n:V) WHERE false AND n.k = row.id RETURN n.k";
        assert!(
            database
                .execute_with(guarded, &parameters)
                .unwrap()
                .rows
                .is_empty()
        );
    }
}
