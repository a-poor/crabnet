use crabnet::{CypherValue, Database, Error};
use serde_json::json;

fn count(database: &mut Database, query: &str) -> i64 {
    let result = database.execute(query).unwrap();
    match result.rows[0][0] {
        CypherValue::Integer(value) => value,
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn merge_creates_once_and_matches_afterwards() {
    let mut database = Database::in_memory().unwrap();

    let first = database
        .execute("MERGE (a:Person {name: 'Ada'}) RETURN a.name AS name")
        .unwrap();
    assert_eq!(first.rows, [vec![json!("Ada")]]);
    assert_eq!(first.summary.nodes_created, 1);

    let second = database
        .execute("MERGE (a:Person {name: 'Ada'}) RETURN a.name AS name")
        .unwrap();
    assert_eq!(second.rows, [vec![json!("Ada")]]);
    assert_eq!(second.summary.nodes_created, 0);
    assert_eq!(count(&mut database, "MATCH (p:Person) RETURN count(p)"), 1);

    // Label and properties both take part in matching.
    database
        .execute("MERGE (:Person {name: 'Ada', role: 'x'})")
        .unwrap();
    database.execute("MERGE (:Other {name: 'Ada'})").unwrap();
    assert_eq!(count(&mut database, "MATCH (n) RETURN count(n)"), 3);

    // A whole-pattern merge with no match creates every fresh part.
    let path = database
        .execute(
            "MERGE (a:City {name: 'Paris'})-[r:IN]->(c:Country {name: 'France'}) \
             RETURN a.name AS a, type(r) AS r, c.name AS c",
        )
        .unwrap();
    assert_eq!(
        path.rows,
        [vec![json!("Paris"), json!("IN"), json!("France")]]
    );
    assert_eq!(path.summary.nodes_created, 2);
    assert_eq!(path.summary.relationships_created, 1);
    let again = database
        .execute("MERGE (:City {name: 'Paris'})-[:IN]->(:Country {name: 'France'})")
        .unwrap();
    assert_eq!(again.summary.nodes_created, 0);
    assert_eq!(again.summary.relationships_created, 0);
}

#[test]
fn merge_on_create_and_on_match_apply_to_the_right_rows() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:K {id: 1, hits: 5})").unwrap();
    database
        .execute("CREATE (:Src {id: 1}), (:Src {id: 2})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (s:Src) \
             MERGE (k:K {id: s.id}) \
               ON CREATE SET k.hits = 1, k.created = true \
               ON MATCH SET k.hits = k.hits + 1 \
             RETURN k.id AS id, k.hits AS hits, k.created AS created ORDER BY id",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![json!(1), json!(6), json!(null)],
            vec![json!(2), json!(1), json!(true)],
        ]
    );
    assert_eq!(result.summary.nodes_created, 1);
    assert_eq!(result.summary.properties_set, 3);
}

#[test]
fn merge_relationships_between_bound_nodes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:A {name: 'a'}), (:B {name: 'b1'}), (:B {name: 'b2'})")
        .unwrap();

    let linked = database
        .execute(
            "MATCH (a:A), (b:B) MERGE (a)-[r:KNOWS {since: 2020}]->(b) \
             RETURN b.name AS b, r.since AS since ORDER BY b",
        )
        .unwrap();
    assert_eq!(
        linked.rows,
        [
            vec![json!("b1"), json!(2020)],
            vec![json!("b2"), json!(2020)]
        ]
    );
    assert_eq!(linked.summary.relationships_created, 2);

    // Re-merging matches; a different property set creates alongside.
    let rematched = database
        .execute("MATCH (a:A), (b:B) MERGE (a)-[r:KNOWS {since: 2020}]->(b) RETURN count(r) AS c")
        .unwrap();
    assert_eq!(rematched.rows, [vec![json!(2)]]);
    assert_eq!(rematched.summary.relationships_created, 0);
    let other = database
        .execute("MATCH (a:A), (b:B) MERGE (a)-[r:KNOWS {since: 2021}]->(b) RETURN count(r) AS c")
        .unwrap();
    assert_eq!(other.summary.relationships_created, 2);

    // Direction matters for matching; an undirected merge matches either way
    // and creates left to right.
    let reversed = database
        .execute("MATCH (a:A), (b:B) MERGE (a)<-[r:KNOWS {since: 2020}]-(b) RETURN count(r) AS c")
        .unwrap();
    assert_eq!(reversed.summary.relationships_created, 2);
    let undirected = database
        .execute("MATCH (a:A), (b:B) MERGE (a)-[r:KNOWS {since: 2020}]-(b) RETURN count(r) AS c")
        .unwrap();
    assert_eq!(undirected.rows, [vec![json!(4)]]);
    assert_eq!(undirected.summary.relationships_created, 0);
    let fresh_undirected = database
        .execute("MATCH (a:A), (b:B {name: 'b1'}) MERGE (a)-[r:LIKES]-(b) RETURN r")
        .unwrap();
    assert_eq!(fresh_undirected.summary.relationships_created, 1);
    let direction = database
        .execute("MATCH (a:A)-[:LIKES]->(b:B) RETURN b.name AS b")
        .unwrap();
    assert_eq!(direction.rows, [vec![json!("b1")]]);
}

#[test]
fn merge_processes_rows_in_order_and_expands_multiple_matches() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:R), (:R), (:R)").unwrap();

    // Three input rows merge the same pattern: the first creates, the other
    // two match what it created.
    let once = database
        .execute("MATCH (:R) MERGE (x:X {k: 1}) RETURN count(x) AS c")
        .unwrap();
    assert_eq!(once.rows, [vec![json!(3)]]);
    assert_eq!(once.summary.nodes_created, 1);
    assert_eq!(count(&mut database, "MATCH (x:X) RETURN count(x)"), 1);

    // A merge that matches several entities expands the row per match.
    database.execute("CREATE (:Y {g: 1}), (:Y {g: 1})").unwrap();
    let expanded = database
        .execute("MATCH (r:R) MERGE (y:Y {g: 1}) RETURN count(*) AS c")
        .unwrap();
    assert_eq!(expanded.rows, [vec![json!(6)]]);
    assert_eq!(expanded.summary.nodes_created, 0);

    // A pattern with nothing new to create is an error, as in Cypher.
    let error = database
        .execute("MATCH (r:R) MERGE (r) RETURN count(r) AS c")
        .unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");

    // Merged entities are bound for later clauses in the statement.
    let chained = database
        .execute(
            "MERGE (p:P {n: 1}) MERGE (q:Q {n: 2}) MERGE (p)-[:TO]->(q) \
             SET p.linked = true \
             RETURN p.linked AS linked, q.n AS n",
        )
        .unwrap();
    assert_eq!(chained.rows, [vec![json!(true), json!(2)]]);
    assert_eq!(chained.summary.nodes_created, 2);
    assert_eq!(chained.summary.relationships_created, 1);
    let rerun = database
        .execute("MERGE (p:P {n: 1}) MERGE (q:Q {n: 2}) MERGE (p)-[:TO]->(q)")
        .unwrap();
    assert_eq!(rerun.summary.nodes_created, 0);
    assert_eq!(rerun.summary.relationships_created, 0);
}

#[test]
fn merge_rejections_and_null_pattern_values() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:A), (:B)").unwrap();

    for (query, semantic) in [
        ("MATCH (a:A), (b:B) MERGE (a)-[:R*1..2]->(b)", false),
        ("MATCH (a:A) MERGE (a:A)", true),
        ("MATCH (a:A) MERGE (a {x: 1})", true),
        ("MATCH (a:A), (b:B) MERGE (a)-[r]->(b)", true),
        ("MERGE p = (a:A) RETURN p", false),
    ] {
        let error = database.execute(query).unwrap_err();
        if semantic {
            assert!(matches!(error, Error::Semantic(_)), "{query}: {error:?}");
        } else {
            assert!(matches!(error, Error::Unsupported(_)), "{query}: {error:?}");
        }
    }

    // A computed pattern value that is null never matches, so each row
    // creates its own node with no such property stored.
    database.execute("CREATE (:S {v: 1}), (:S)").unwrap();
    let nulls = database
        .execute("MATCH (s:S) MERGE (t:T {v: s.v}) RETURN t.v AS v ORDER BY v")
        .unwrap();
    assert_eq!(nulls.rows, [vec![json!(1)], vec![json!(null)]]);
    assert_eq!(nulls.summary.nodes_created, 2);
}

#[test]
fn merge_settles_interleaved_duplicate_rows_in_order() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:K {id: 2, hits: 10})").unwrap();
    database
        .execute(
            "CREATE (:Src {pos: 0, id: 1}), (:Src {pos: 1, id: 2}), (:Src {pos: 2, id: 1}), \
                    (:Src {pos: 3, id: 3}), (:Src {pos: 4, id: 2}), (:Src {pos: 5, id: 3}), \
                    (:Src {pos: 6, id: 1})",
        )
        .unwrap();

    // Rows whose key already exists match at once; the first row of each new
    // key creates it, and every later row of that key matches the creation
    // even when rows of other keys sit between them. A computed action reads
    // the entity as the row matched it: the two rows matching the existing
    // key both saw ten hits, while the rows matching a created key were each
    // matched after the previous row's action.
    let result = database
        .execute(
            "MATCH (s:Src) WITH s ORDER BY s.pos \
             MERGE (k:K {id: s.id}) \
               ON CREATE SET k.hits = 1 \
               ON MATCH SET k.hits = k.hits + 1 \
             RETURN s.pos AS pos, k.id AS id ORDER BY pos",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![json!(0), json!(1)],
            vec![json!(1), json!(2)],
            vec![json!(2), json!(1)],
            vec![json!(3), json!(3)],
            vec![json!(4), json!(2)],
            vec![json!(5), json!(3)],
            vec![json!(6), json!(1)],
        ]
    );
    assert_eq!(result.summary.nodes_created, 2);
    assert_eq!(count(&mut database, "MATCH (k:K) RETURN count(k)"), 3);
    let hits = database
        .execute("MATCH (k:K) RETURN k.id AS id, k.hits AS hits ORDER BY id")
        .unwrap();
    assert_eq!(
        hits.rows,
        [
            vec![json!(1), json!(3)],
            vec![json!(2), json!(11)],
            vec![json!(3), json!(2)],
        ]
    );
}
