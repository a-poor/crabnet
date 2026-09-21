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

#[test]
fn computed_stored_values_read_the_row_and_earlier_writes() {
    let mut database = Database::in_memory().unwrap();

    // CREATE properties may compute from earlier bindings in the statement.
    let created = database
        .execute(
            "CREATE (a:A {x: 1, name: 'a'}) \
             CREATE (b:B {y: a.x + 1, upper: toUpper(a.name), tags: [a.x, 2]}) \
             CREATE (a)-[:R {weight: a.x * 2.5}]->(b) \
             RETURN b.y AS y, b.upper AS upper, b.tags AS tags",
        )
        .unwrap();
    assert_eq!(created.rows, [vec![json!(2), json!("A"), json!([1, 2])]]);
    let weight = database
        .execute("MATCH ()-[r:R]->() RETURN r.weight AS weight")
        .unwrap();
    assert_eq!(weight.rows, [vec![json!(2.5)]]);

    // CREATE followed by SET in one statement, with the list concatenation
    // the TCK uses.
    let appended = database
        .execute(
            "CREATE (n:L {numbers: [1, 2, 3]}) \
             SET n.numbers = n.numbers + [4, 5] \
             RETURN n.numbers AS numbers",
        )
        .unwrap();
    assert_eq!(appended.rows, [vec![json!([1, 2, 3, 4, 5])]]);

    // Whole-map forms accept computed maps and copy another entity's
    // properties; per-row evaluation sees each row's own values.
    database
        .execute("CREATE (:P {k: 1, src: 'one'}), (:P {k: 2, src: 'two'})")
        .unwrap();
    let merged = database
        .execute(
            "MATCH (p:P) \
             SET p += {doubled: p.k * 2, label: 'p' + toString(p.k)} \
             RETURN p.doubled AS doubled, p.label AS label ORDER BY doubled",
        )
        .unwrap();
    assert_eq!(
        merged.rows,
        [vec![json!(2), json!("p1")], vec![json!(4), json!("p2")]]
    );
    let copied = database
        .execute("MATCH (a:A), (b:B) SET b = a RETURN b.x AS x, b.name AS name, b.y AS y")
        .unwrap();
    assert_eq!(copied.rows, [vec![json!(1), json!("a"), json!(null)]]);

    // A property set from another row's state uses the row's own value each
    // time, so a chained expression composes with earlier SET items.
    let chained = database
        .execute(
            "MATCH (p:P) SET p.k = p.k + 10, p.twice = p.k * 2 \
             RETURN p.k AS k, p.twice AS twice ORDER BY k",
        )
        .unwrap();
    assert_eq!(
        chained.rows,
        [vec![json!(11), json!(22)], vec![json!(12), json!(24)]]
    );

    // Null results remove the property, like a literal null.
    let removed = database
        .execute("MATCH (p:P) SET p.twice = p.missing RETURN p.twice AS twice")
        .unwrap();
    assert_eq!(removed.rows, [vec![json!(null)], vec![json!(null)]]);
}

#[test]
fn computed_stored_values_reject_entities_aggregates_and_non_maps() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Q {k: 1}), (:Q {k: 2})").unwrap();

    for query in [
        "MATCH (q:Q) SET q.other = q",
        "MATCH (q:Q) SET q.list = [q]",
        "MATCH (q:Q) SET q.total = count(*)",
        "MATCH (q:Q) SET q = q.k",
        "MATCH (q:Q) SET q += 'text'",
        "MATCH (q:Q) CREATE (:R {props: q})",
        "MATCH p = (q:Q) SET q.path = p",
    ] {
        let error = database.execute(query).unwrap_err();
        assert!(matches!(error, Error::Semantic(_)), "{query}: {error:?}");
    }

    // Nothing was written by the rejected statements.
    let stored = database
        .execute("MATCH (q:Q) RETURN keys(q) AS k ORDER BY q.k")
        .unwrap();
    assert_eq!(stored.rows, [vec![json!(["k"])], vec![json!(["k"])]]);
    let count = database
        .execute("MATCH (r:R) RETURN count(r) AS c")
        .unwrap();
    assert_eq!(count.rows, [vec![json!(0)]]);
}

#[test]
fn post_mutation_return_supports_computed_projections_and_modifiers() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:S {v: 3}), (:S {v: 1}), (:S {v: 2}), (:S {v: 2})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (s:S) SET s.v = s.v + 1 \
             RETURN DISTINCT s.v * 10 AS tens, CASE WHEN s.v > 2 THEN 'big' ELSE 'small' END AS size \
             ORDER BY tens DESC SKIP 1 LIMIT 2",
        )
        .unwrap();
    assert_eq!(result.columns, ["tens", "size"]);
    assert_eq!(
        result.rows,
        [
            vec![json!(30), json!("big")],
            vec![json!(20), json!("small")]
        ]
    );
    assert_eq!(result.summary.properties_set, 4);

    // A created entity returned whole reflects the statement's later SET.
    let node = database
        .execute("CREATE (n:T {a: 1}) SET n.b = n.a + 1 RETURN n")
        .unwrap();
    let node = node.rows[0][0].as_node().unwrap();
    assert_eq!(
        node.properties.get("b"),
        Some(&crabnet::CypherValue::Integer(2))
    );
}
