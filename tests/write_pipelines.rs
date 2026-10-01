//! Updating clauses inside a clause pipeline: reads before, between, and
//! after writes in one statement.

use crabnet::{CypherValue, Database, Error};

fn integers(rows: &[Vec<CypherValue>]) -> Vec<i64> {
    rows.iter()
        .map(|row| match &row[0] {
            CypherValue::Integer(value) => *value,
            other => panic!("expected an integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn unwind_and_with_values_feed_written_properties() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute("UNWIND [1, 2, 3] AS i CREATE (n:Num {v: i}) RETURN n.v ORDER BY n.v")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![1, 2, 3]);
    assert_eq!(result.summary.nodes_created, 3);

    let result = database
        .execute(
            "MATCH (n:Num) WITH n, n.v * 10 AS scaled \
             SET n.scaled = scaled \
             RETURN n.scaled ORDER BY n.scaled",
        )
        .unwrap();
    assert_eq!(integers(&result.rows), vec![10, 20, 30]);
    assert_eq!(result.summary.properties_set, 3);
}

#[test]
fn with_aliases_keep_the_bound_entity() {
    let mut database = Database::in_memory().unwrap();

    // `b` is the node `a` under another name, so only one node exists before
    // the second CREATE adds its target.
    let result = database
        .execute("CREATE (a:Src) WITH a AS b CREATE (b)-[:R]->(:Dst) RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![1]);
    assert_eq!(result.summary.nodes_created, 2);
    assert_eq!(result.summary.relationships_created, 1);

    let result = database
        .execute("MATCH (a:Src) WITH a AS b, a AS c CREATE (b)-[:SELF]->(c) RETURN count(*) AS n")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![1]);
    assert_eq!(result.summary.nodes_created, 0);
    assert_eq!(result.summary.relationships_created, 1);
}

#[test]
fn match_after_a_write_sees_the_written_graph() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute("CREATE (a:Seen) WITH a MATCH (b:Seen) RETURN count(b) AS c")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![1]);

    // MATCH may follow an updating clause directly.
    let result = database
        .execute("CREATE (a:Seen) MATCH (b:Seen) CREATE (a)-[:SAW]->(b) RETURN count(*) AS c")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![2]);
    assert_eq!(result.summary.nodes_created, 1);
    assert_eq!(result.summary.relationships_created, 2);

    // OPTIONAL MATCH after a write null-extends and later updates skip nulls.
    let result = database
        .execute(
            "CREATE (a:Lonely) WITH a OPTIONAL MATCH (a)-[r:NONE]->() \
             SET r.touched = true RETURN r",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![CypherValue::Null]]);
    assert_eq!(result.summary.properties_set, 0);
}

#[test]
fn modifiers_after_a_write_shape_the_result_but_not_the_side_effects() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "UNWIND range(1, 5) AS i CREATE (n:Page {i: i}) \
             WITH n ORDER BY n.i DESC SKIP 1 LIMIT 2 RETURN n.i",
        )
        .unwrap();
    assert_eq!(integers(&result.rows), vec![4, 3]);
    assert_eq!(result.summary.nodes_created, 5);

    let result = database
        .execute(
            "MATCH (p:Page) CREATE (p)-[:NEXT]->(:Page) WITH p WHERE p.i > 3 RETURN count(p) AS c",
        )
        .unwrap();
    assert_eq!(integers(&result.rows), vec![2]);
    assert_eq!(result.summary.nodes_created, 5);
    assert_eq!(result.summary.relationships_created, 5);

    let result = database
        .execute(
            "MATCH (p:Page) WHERE p.i IS NOT NULL DELETE p WITH count(*) AS deleted RETURN deleted",
        )
        .unwrap_err();
    assert!(matches!(result, Error::Constraint(_)), "{result:?}");

    let result = database
        .execute(
            "MATCH (p:Page) WHERE p.i IS NOT NULL DETACH DELETE p \
             WITH count(*) AS deleted MATCH (rest:Page) RETURN deleted, count(rest) AS rest",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![CypherValue::Integer(5), CypherValue::Integer(5)]]
    );
    assert_eq!(result.summary.nodes_deleted, 5);
    assert_eq!(result.summary.relationships_deleted, 5);
}

#[test]
fn reads_and_writes_alternate_within_one_statement() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "CREATE (a:Chain {v: 1}) \
             WITH a, a.v + 1 AS next \
             CREATE (b:Chain {v: next}) \
             WITH b, count(*) AS created \
             UNWIND [10, 20] AS delta \
             CREATE (c:Delta {v: b.v + delta, created: created}) \
             RETURN c.v ORDER BY c.v",
        )
        .unwrap();
    assert_eq!(integers(&result.rows), vec![12, 22]);
    assert_eq!(result.summary.nodes_created, 4);

    let explanation = crabnet::explain(
        "CREATE (a) WITH a MATCH (b) CREATE (a)-[:R]->(b) WITH count(*) AS c RETURN c",
    )
    .unwrap();
    assert!(explanation.contains("segments:"), "{explanation}");
    assert!(explanation.contains("segment 0: rows %0"), "{explanation}");
    // The anonymous relationship is not a variable, so it is not carried.
    assert!(
        explanation.contains("segment 1: rows %0, %1\n"),
        "{explanation}"
    );
}

#[test]
fn merge_after_unwind_or_with_matches_across_rows() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "UNWIND ['a', 'b', 'a'] AS name MERGE (p:Person {name: name}) RETURN count(p) AS c",
        )
        .unwrap();
    assert_eq!(integers(&result.rows), vec![3]);
    assert_eq!(result.summary.nodes_created, 2);

    let result = database
        .execute(
            "MATCH (p:Person) WITH p AS q \
             MERGE (q)-[:KNOWS]->(f:Friend {of: q.name}) \
             ON CREATE SET f.created = true \
             RETURN count(f) AS c",
        )
        .unwrap();
    assert_eq!(integers(&result.rows), vec![2]);
    assert_eq!(result.summary.nodes_created, 2);
    assert_eq!(result.summary.relationships_created, 2);
    assert_eq!(result.summary.properties_set, 2);
}

#[test]
fn wildcard_projections_carry_every_variable() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:W {name: 'x'}), (:W {name: 'y'})")
        .unwrap();

    let result = database
        .execute("MATCH (n:W) WITH * RETURN * ORDER BY n.name")
        .unwrap();
    assert_eq!(result.columns, vec!["n"]);
    assert_eq!(result.rows.len(), 2);

    // Columns come out in name order; ORDER BY may use them.
    let result = database
        .execute("MATCH (n:W) WITH n, n.name AS name RETURN * ORDER BY name DESC")
        .unwrap();
    assert_eq!(result.columns, vec!["n", "name"]);
    assert_eq!(result.rows[0][1], CypherValue::String("y".into()));

    // `WITH *` over an empty scope keeps the rows.
    let result = database
        .execute("MATCH (:W) CREATE () WITH * CREATE ()")
        .unwrap();
    assert_eq!(result.summary.nodes_created, 4);

    assert!(matches!(
        database.execute("RETURN *"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn chained_comparisons_compare_pairwise() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute("UNWIND [1, 2, 3] AS i WITH i WHERE 1 < i < 3 RETURN i")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![2]);

    let result = database
        .execute("UNWIND [1, 2, 3] AS i WITH i WHERE 1 <= i <= 2 RETURN i")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![1, 2]);

    let result = database
        .execute("UNWIND [1, 2, 3] AS i WITH i WHERE 0 < i = 2 <> 3 RETURN i")
        .unwrap();
    assert_eq!(integers(&result.rows), vec![2]);

    // Parentheses end the chain: a boolean compared with an integer is null.
    let result = database
        .execute("UNWIND [1, 2, 3] AS i WITH i WHERE (1 < i) < 3 RETURN i")
        .unwrap();
    assert!(result.rows.is_empty());
}

#[test]
fn a_statement_must_end_with_return_or_an_updating_clause() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute("CREATE (a:Dangling) WITH a"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (a:Dangling)"),
        Err(Error::Semantic(_))
    ));
    let remaining = database.execute("MATCH (a:Dangling) RETURN a").unwrap();
    assert!(remaining.rows.is_empty());
}
