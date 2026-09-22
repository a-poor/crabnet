//! Multiple node labels: creation, matching, label predicates, and the
//! `SET`/`REMOVE` label mutations, plus `XOR`.

use crabnet::{CypherValue, Database, Error};
use serde_json::json;

fn integers(rows: &[Vec<CypherValue>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match &row[0] {
            CypherValue::Integer(value) => *value,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}

fn count(database: &mut Database, query: &str) -> i64 {
    integers(&database.execute(query).unwrap().rows)[0]
}

#[test]
fn nodes_carry_any_number_of_labels_in_creation_order() {
    let mut database = Database::in_memory().unwrap();

    let created = database
        .execute(
            "CREATE (a:A:B {n: 1}), (b:B), (c), (d:B:A:B) \
             RETURN labels(a) AS a, labels(b) AS b, labels(c) AS c, labels(d) AS d",
        )
        .unwrap();
    assert_eq!(
        created.rows,
        vec![vec![
            json!(["A", "B"]),
            json!(["B"]),
            json!([]),
            json!(["B", "A"]),
        ]]
    );
    assert_eq!(created.summary.nodes_created, 4);
    assert_eq!(created.summary.labels_added, 5);

    assert_eq!(count(&mut database, "MATCH (n:B) RETURN count(n)"), 3);
    assert_eq!(count(&mut database, "MATCH (n:A:B) RETURN count(n)"), 2);
    assert_eq!(count(&mut database, "MATCH (n:B:A) RETURN count(n)"), 2);
    assert_eq!(count(&mut database, "MATCH (n:A&B) RETURN count(n)"), 2);
    assert_eq!(count(&mut database, "MATCH (n:A|B) RETURN count(n)"), 3);
    assert_eq!(
        count(&mut database, "MATCH (n:(A|Zzz)&B) RETURN count(n)"),
        2
    );
    assert_eq!(count(&mut database, "MATCH (n:Zzz) RETURN count(n)"), 0);

    // Whole-node values and later stages see the full label list.
    let carried = database
        .execute("MATCH (n:A) WITH n MATCH (n:B) RETURN n ORDER BY n.n")
        .unwrap();
    assert_eq!(carried.rows.len(), 2);
    let CypherValue::Node(node) = &carried.rows[0][0] else {
        panic!("expected a node");
    };
    assert_eq!(node.labels, ["A", "B"]);
}

#[test]
fn label_predicates_are_three_valued_booleans() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:A:B {n: 1}), (:B {n: 2}), ({n: 3}), ()-[:T]->(), ()-[:U]->()")
        .unwrap();

    let query = |database: &mut Database, predicate: &str| {
        integers(
            &database
                .execute(&format!(
                    "MATCH (n) WHERE n.n IS NOT NULL AND {predicate} RETURN n.n ORDER BY n.n"
                ))
                .unwrap()
                .rows,
        )
    };
    assert_eq!(query(&mut database, "n:A"), vec![1]);
    assert_eq!(query(&mut database, "n:B"), vec![1, 2]);
    assert_eq!(query(&mut database, "NOT n:A"), vec![2, 3]);
    assert_eq!(query(&mut database, "n:A OR n.n = 3"), vec![1, 3]);
    assert_eq!(query(&mut database, "n:A:B"), vec![1]);
    assert_eq!(query(&mut database, "n:A|B"), vec![1, 2]);

    let rows = database
        .execute("MATCH ()-[r]->() WHERE r:T RETURN count(r) AS c")
        .unwrap();
    assert_eq!(integers(&rows.rows), vec![1]);
    let rows = database
        .execute("MATCH ()-[r]->() WHERE r:T|U RETURN count(r) AS c")
        .unwrap();
    assert_eq!(integers(&rows.rows), vec![2]);

    // A null node has null labels and a null label test.
    let rows = database
        .execute("OPTIONAL MATCH (n:Nope) RETURN n:Nope AS test, labels(n) AS labels")
        .unwrap();
    assert_eq!(rows.rows, vec![vec![CypherValue::Null, CypherValue::Null]]);

    assert!(matches!(
        database.execute("WITH 1 AS x RETURN x:Label"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (n:%) RETURN n"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn set_and_remove_labels_count_changed_pairs_and_update_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:A {n: 1}), (:B {n: 2}), ({n: 3})")
        .unwrap();

    let set = database
        .execute("MATCH (n) SET n:C:A RETURN labels(n) AS labels ORDER BY n.n")
        .unwrap();
    assert_eq!(
        set.rows,
        vec![
            vec![json!(["A", "C"])],
            vec![json!(["B", "C", "A"])],
            vec![json!(["C", "A"])],
        ]
    );
    // Five node/label pairs changed: node 1 already had A.
    assert_eq!(set.summary.labels_added, 5);

    let again = database.execute("MATCH (n) SET n:C").unwrap();
    assert_eq!(again.summary.labels_added, 0);

    let removed = database
        .execute("MATCH (n:C) REMOVE n:A:B, n:Zzz RETURN labels(n) AS labels ORDER BY n.n")
        .unwrap();
    assert_eq!(
        removed.rows,
        vec![vec![json!(["C"])], vec![json!(["C"])], vec![json!(["C"])]]
    );
    // Node 1 loses A, node 2 loses B and A, node 3 loses A.
    assert_eq!(removed.summary.labels_removed, 4);
    assert_eq!(count(&mut database, "MATCH (n:A) RETURN count(n)"), 0);
    assert_eq!(count(&mut database, "MATCH (n:C) RETURN count(n)"), 3);

    // Null targets are skipped and relationships cannot carry labels.
    let skipped = database
        .execute("OPTIONAL MATCH (n:Nope) SET n:Touched REMOVE n:C RETURN n")
        .unwrap();
    assert_eq!(skipped.rows, vec![vec![CypherValue::Null]]);
    assert_eq!(skipped.summary.labels_added, 0);
    database.execute("CREATE ()-[:R]->()").unwrap();
    assert!(matches!(
        database.execute("MATCH ()-[r:R]->() SET r:Label"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (n) SET n:A|B"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn merge_matches_label_subsets_and_sets_labels_in_actions() {
    let mut database = Database::in_memory().unwrap();

    let created = database
        .execute("MERGE (m:M:N {k: 1}) ON CREATE SET m:Created RETURN labels(m) AS labels")
        .unwrap();
    assert_eq!(created.rows, vec![vec![json!(["M", "N", "Created"])]]);
    assert_eq!(created.summary.nodes_created, 1);

    let matched = database
        .execute("MERGE (m:N {k: 1}) ON MATCH SET m:Matched RETURN labels(m) AS labels")
        .unwrap();
    assert_eq!(
        matched.rows,
        vec![vec![json!(["M", "N", "Created", "Matched"])]]
    );
    assert_eq!(matched.summary.nodes_created, 0);
    assert_eq!(matched.summary.labels_added, 1);

    let explanation =
        crabnet::explain("MATCH (n:A|B) WHERE n:C SET n:D REMOVE n:A RETURN n").unwrap();
    assert!(
        explanation.contains("filter or(has-label(%0, \"A\"), has-label(%0, \"B\"))"),
        "{explanation}"
    );
    assert!(
        explanation.contains("filter has-label(%0, \"C\")"),
        "{explanation}"
    );
    assert!(
        explanation.contains("set-labels slot=%0 labels=[\"D\"]"),
        "{explanation}"
    );
    assert!(
        explanation.contains("remove-labels slot=%0 labels=[\"A\"]"),
        "{explanation}"
    );
    let explanation = crabnet::explain("MATCH (n:A:B) CREATE (m:C:D) RETURN m").unwrap();
    assert!(
        explanation.contains("has-label %0 \"A\"\n"),
        "{explanation}"
    );
    assert!(
        explanation.contains("has-label %0 \"B\"\n"),
        "{explanation}"
    );
    assert!(
        explanation.contains("create-node slot=%1 labels=[\"C\", \"D\"]"),
        "{explanation}"
    );
}

#[test]
fn xor_is_three_valued() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "UNWIND [[true, true], [true, false], [false, false], [false, null], [null, null]] AS p \
             RETURN p[0] XOR p[1] AS x",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![CypherValue::Boolean(false)],
            vec![CypherValue::Boolean(true)],
            vec![CypherValue::Boolean(false)],
            vec![CypherValue::Null],
            vec![CypherValue::Null],
        ]
    );
    let result = database
        .execute("RETURN true XOR true XOR true AS x, (1 = 1) XOR (2 > 3) AS y")
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![CypherValue::Boolean(true), CypherValue::Boolean(true)]]
    );
    database
        .execute("CREATE ({a: true, b: false}), ({a: true, b: true}), ({a: false, b: true})")
        .unwrap();
    assert_eq!(
        count(&mut database, "MATCH (n) WHERE n.a XOR n.b RETURN count(n)"),
        2
    );
}
