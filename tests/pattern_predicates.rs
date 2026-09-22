//! Pattern predicates (`WHERE (n)-[:T]->()`) and identity comparison of
//! nodes and relationships.

use crabnet::{CypherValue, Database, Error};

fn integers(rows: &[Vec<CypherValue>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match &row[0] {
            CypherValue::Integer(value) => *value,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}

fn seed() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (a:A {n: 1})-[:REL1]->(b:B {n: 2})-[:REL2]->(c:C {n: 3}), (:D {n: 4})")
        .unwrap();
    database
}

fn numbers(database: &mut Database, predicate: &str) -> Vec<i64> {
    integers(
        &database
            .execute(&format!(
                "MATCH (n) WHERE {predicate} RETURN n.n ORDER BY n.n"
            ))
            .unwrap()
            .rows,
    )
}

#[test]
fn pattern_predicates_test_connections_from_bound_entities() {
    let mut database = seed();

    assert_eq!(numbers(&mut database, "(n)-[]->()"), vec![1, 2]);
    assert_eq!(numbers(&mut database, "(n)<-[]-()"), vec![2, 3]);
    assert_eq!(numbers(&mut database, "(n)-[]-()"), vec![1, 2, 3]);
    assert_eq!(numbers(&mut database, "(n)-[:REL1]->()"), vec![1]);
    assert_eq!(numbers(&mut database, "NOT (n)-[]-()"), vec![4]);
    assert_eq!(numbers(&mut database, "(n)-->()-->()"), vec![1]);
    assert_eq!(numbers(&mut database, "(n)-->(:B)"), vec![1]);
    assert_eq!(numbers(&mut database, "(n)-->({n: 3})"), vec![2]);
    assert_eq!(numbers(&mut database, "(n)-->() AND n.n > 1"), vec![2]);
    assert_eq!(
        numbers(&mut database, "(n)-->() OR (n)<--()"),
        vec![1, 2, 3]
    );
    assert_eq!(numbers(&mut database, "exists((n)-->())"), vec![1, 2]);
    assert_eq!(
        numbers(&mut database, "exists(n.n) AND NOT exists(n.missing)"),
        vec![1, 2, 3, 4]
    );

    let pairs = database
        .execute("MATCH (n), (m) WHERE (n)-->(m) RETURN n.n, m.n ORDER BY n.n")
        .unwrap();
    assert_eq!(
        pairs.rows,
        vec![
            vec![CypherValue::Integer(1), CypherValue::Integer(2)],
            vec![CypherValue::Integer(2), CypherValue::Integer(3)],
        ]
    );

    // Triadic selection: friends of friends that are not already friends.
    let triadic = database
        .execute("MATCH (a)-->(b)-->(c) WHERE NOT (a)-->(c) RETURN a.n, c.n")
        .unwrap();
    assert_eq!(
        triadic.rows,
        vec![vec![CypherValue::Integer(1), CypherValue::Integer(3)]]
    );
}

#[test]
fn pattern_predicates_work_as_values_and_in_later_stages() {
    let mut database = seed();

    let projected = database
        .execute("MATCH (n) RETURN n.n AS n, (n)-->() AS out ORDER BY n")
        .unwrap();
    assert_eq!(
        projected
            .rows
            .iter()
            .map(|row| row[1].clone())
            .collect::<Vec<_>>(),
        vec![
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            CypherValue::Boolean(false),
            CypherValue::Boolean(false),
        ]
    );

    let after_with = database
        .execute("MATCH (n) WITH n WHERE (n)-->() RETURN count(n) AS c")
        .unwrap();
    assert_eq!(integers(&after_with.rows), vec![2]);
    let across_stages = database
        .execute("MATCH (n) WITH n MATCH (m) WHERE (n)-->(m) RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&across_stages.rows), vec![2]);
    let inside_comprehension = database
        .execute("MATCH (n) RETURN size([x IN [1, 2] WHERE (n)-->()]) AS s ORDER BY n.n")
        .unwrap();
    assert_eq!(integers(&inside_comprehension.rows), vec![2, 2, 0, 0]);

    // Writes may filter on a pattern predicate; the predicate sees the graph
    // before the write.
    let written = database
        .execute("MATCH (n) WHERE (n)-->() SET n.out = true RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&written.rows), vec![2]);
    assert_eq!(written.summary.properties_set, 2);

    let explanation = crabnet::explain("MATCH (n) WHERE (n)-[:R]->(:B) RETURN n").unwrap();
    assert!(
        explanation.contains("filter exists(scan-relationship %1; has-relationship-type %1 \"R\"; scan-node %2; has-label %2 \"B\"; expand relationship=%1 left=%0 right=%2 direction=right)"),
        "{explanation}"
    );
}

#[test]
fn pattern_predicate_restrictions_are_reported() {
    let mut database = seed();

    assert!(matches!(
        database.execute("MATCH (n) WHERE (n)-->(x) RETURN n"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (n) WHERE p = (n)-->() RETURN n"),
        Err(Error::Parse(_) | Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (n) WHERE (n)-[*]->() RETURN n"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("MATCH (n) WHERE (n)-[:REL1|REL2]-() RETURN n"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute("MATCH (n) WHERE (n) RETURN n"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (n) WHERE exists() RETURN n"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn nodes_and_relationships_compare_by_identity() {
    let mut database = seed();

    let same = database
        .execute("MATCH (a), (b) WHERE a = b RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&same.rows), vec![4]);
    let different = database
        .execute("MATCH (a), (b) WHERE a <> b RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&different.rows), vec![12]);
    let parenthesized = database
        .execute("MATCH (a), (b) WHERE NOT ((a) = (b)) RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&parenthesized.rows), vec![12]);

    // Relationships bound by different clauses compare too; an unmatched
    // optional relationship makes the comparison null and drops the row.
    let subgraphs = database
        .execute(
            "MATCH (a)-[r]-(b) OPTIONAL MATCH (b)-[r2]-(c) WHERE r <> r2 \
             RETURN a.n, b.n, c.n ORDER BY a.n, b.n, c.n",
        )
        .unwrap();
    assert_eq!(
        subgraphs.rows,
        vec![
            vec![
                CypherValue::Integer(1),
                CypherValue::Integer(2),
                CypherValue::Integer(3)
            ],
            vec![
                CypherValue::Integer(2),
                CypherValue::Integer(1),
                CypherValue::Null
            ],
            vec![
                CypherValue::Integer(2),
                CypherValue::Integer(3),
                CypherValue::Null
            ],
            vec![
                CypherValue::Integer(3),
                CypherValue::Integer(2),
                CypherValue::Integer(1)
            ],
        ]
    );

    let null = database
        .execute("OPTIONAL MATCH (n:Nope) RETURN n = n AS same, n IS NULL AS missing")
        .unwrap();
    assert_eq!(
        null.rows,
        vec![vec![CypherValue::Null, CypherValue::Boolean(true)]]
    );
    let unlinked = database
        .execute(
            "MATCH (a:A)-->(b)-->(c) OPTIONAL MATCH (a)-[r]->(c) \
             WITH c WHERE r IS NULL RETURN c.n",
        )
        .unwrap();
    assert_eq!(integers(&unlinked.rows), vec![3]);
    let aliased = database
        .execute("MATCH (a:A) WITH a AS x MATCH (y:A) WHERE x = y RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&aliased.rows), vec![1]);
    let mismatch = database
        .execute("MATCH (n:A) WHERE n = 1 RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&mismatch.rows), vec![0]);
}
