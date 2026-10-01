use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

#[test]
fn distinct_uses_typed_recursive_equality_before_expanding_the_next_bag() {
    let mut database = Database::in_memory().unwrap();
    let parameters: Parameters = [(
        "values".into(),
        json!([null, null, true, true, 1, 1.0, "1", "1", [1, null], [1.0, null],
               {"a": [1], "b": false}, {"b": false, "a": [1.0]},
               9007199254740992_i64, 9007199254740993_i64, 9007199254740992.0]),
    )]
    .into();
    let result = database
        .execute_with(
            "UNWIND $values AS value WITH DISTINCT value \
             UNWIND [1, 1] AS copy RETURN value, count(*) AS copies",
            &parameters,
        )
        .unwrap();
    assert_eq!(result.rows.len(), 8);
    assert!(
        result
            .rows
            .iter()
            .all(|row| row[1] == CypherValue::Integer(2))
    );
    assert!(result.rows.iter().any(|row| row[0] == CypherValue::Null));
    assert!(
        result
            .rows
            .iter()
            .any(|row| row[0] == CypherValue::Boolean(true))
    );
    assert!(
        result
            .rows
            .iter()
            .any(|row| row[0] == CypherValue::String("1".into()))
    );
}

#[test]
fn distinct_shapes_complete_rows_before_pagination_and_later_matches() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Tag {name: 'a'}), (:Tag {name: 'b'})")
        .unwrap();
    let result = database
        .execute_with(
            "UNWIND [3, 1, 1, 2, 2, 3] AS x \
         WITH DISTINCT x AS value ORDER BY value SKIP $skip LIMIT $limit \
         MATCH (tag:Tag) RETURN value, tag.name AS tag ORDER BY tag",
            &[("skip".into(), json!(1)), ("limit".into(), json!(1))].into(),
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![CypherValue::Integer(2), CypherValue::String("a".into())],
            vec![CypherValue::Integer(2), CypherValue::String("b".into())],
        ]
    );
    let rows = database
        .execute(
            "UNWIND [1, 1, 2] AS x UNWIND [null, null, true] AS y \
         WITH DISTINCT * RETURN x, y ORDER BY x, y",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![
            vec![CypherValue::Integer(1), CypherValue::Boolean(true)],
            vec![CypherValue::Integer(1), CypherValue::Null],
            vec![CypherValue::Integer(2), CypherValue::Boolean(true)],
            vec![CypherValue::Integer(2), CypherValue::Null],
        ]
    );
    let rows = database
        .execute(
            "UNWIND [3, 1, 3, 2, 1] AS x WITH x ORDER BY x DESC \
         WITH DISTINCT x OFFSET 1 RETURN x",
        )
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![vec![CypherValue::Integer(2)], vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn distinct_handles_empty_scopes_empty_inputs_and_aggregate_outputs() {
    let mut database = Database::in_memory().unwrap();
    for (query, count) in [
        ("WITH DISTINCT * RETURN count(*) AS count", 1),
        ("WITH DISTINCT 1 AS x RETURN count(*) AS count", 1),
        ("UNWIND [] AS x WITH DISTINCT x RETURN count(*) AS count", 0),
        ("WITH DISTINCT 1 AS x LIMIT 0 RETURN count(*) AS count", 0),
        (
            "UNWIND [] AS x WITH DISTINCT count(*) AS count RETURN count",
            0,
        ),
        (
            "UNWIND [1, 1, 2, 2] AS x WITH DISTINCT x, count(*) AS c RETURN count(*) AS count",
            2,
        ),
        // DISTINCT applies to the projection, not the aggregate's input bag.
        (
            "UNWIND [1, 1, 2, 2] AS x WITH DISTINCT count(*) AS count RETURN count",
            4,
        ),
    ] {
        assert_eq!(
            database.execute(query).unwrap().rows,
            vec![vec![CypherValue::Integer(count)]],
            "{query}"
        );
    }
    assert_eq!(
        database
            .execute(
                "UNWIND [1, 1, 2, 2] AS x WITH DISTINCT x % 2 AS key, count(*) AS c \
         WHERE c = 2 WITH DISTINCT c RETURN c"
            )
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(2)]]
    );
    assert_eq!(
        database
            .execute(
                "UNWIND [1, 1, 2] AS x WITH DISTINCT x, count(*) AS c \
         ORDER BY count(*) + 1 DESC WHERE count(*) > 0 RETURN c"
            )
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(2)], vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn distinct_filters_incoming_rows_and_orders_only_by_projected_values() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Item {value: 1, keep: false}), (:Item {value: 1, keep: true}), (:Item {value: 2, keep: true})").unwrap();
    assert_eq!(
        database
            .execute(
                "MATCH (n:Item) WITH DISTINCT n.value AS v WHERE n.keep \
         RETURN v ORDER BY v"
            )
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(1)], vec![CypherValue::Integer(2)]]
    );
    for query in [
        "MATCH (n:Item) WITH DISTINCT n.value AS v ORDER BY n.value DESC LIMIT 1 RETURN v",
        "MATCH (n:Item) WITH DISTINCT n.value AS v ORDER BY v + 1 DESC LIMIT 1 RETURN v",
        "MATCH (n:Item) WITH DISTINCT n ORDER BY n.value DESC LIMIT 1 RETURN n.value AS v",
    ] {
        assert_eq!(
            database.execute(query).unwrap().rows,
            vec![vec![CypherValue::Integer(2)]],
            "{query}"
        );
    }
    for query in [
        "MATCH (n:Item) WITH DISTINCT n.value AS v ORDER BY n.keep RETURN v",
        "MATCH (n:Item) WITH DISTINCT n.value AS v RETURN n",
        "UNWIND [1, 2] AS x WITH DISTINCT x AS v, v + 1 AS next RETURN next",
        "UNWIND [1, 2] AS x WITH DISTINCT x ORDER BY count(*) RETURN x",
        "UNWIND [1, 2] AS x WITH DISTINCT sum(x) AS s ORDER BY count(*) RETURN s",
        "UNWIND [1, 2] AS x WITH DISTINCT sum(x) AS s WHERE count(*) > 0 RETURN s",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    assert_eq!(
        database
            .execute("WITH 1 AS x, 2 AS y WITH DISTINCT y AS x, x AS y RETURN x, y")
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(2), CypherValue::Integer(1)]]
    );
}

#[test]
fn distinct_entities_use_identity_and_keep_nullable_bindings() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (a:Node {v: 1}), (b:Node {v: 1}), (a)-[:LINK]->(b), (a)-[:LINK]->(b)")
        .unwrap();
    for (projection, count) in [("a", 1), ("r", 2), ("a, r", 2)] {
        let query = format!(
            "MATCH (a)-[r:LINK]->() UNWIND [1, 1] AS copy WITH DISTINCT {projection} RETURN count(*) AS count"
        );
        assert_eq!(
            database.execute(&query).unwrap().rows,
            vec![vec![CypherValue::Integer(count)]]
        );
    }
    let nodes = database
        .execute("MATCH (n:Node) UNWIND [1, 1] AS copy WITH DISTINCT n RETURN n")
        .unwrap()
        .rows;
    assert_eq!(nodes.len(), 2);
    assert_ne!(nodes[0], nodes[1]);
    assert_eq!(
        database
            .execute("UNWIND [1, 2] AS copy OPTIONAL MATCH (n:Missing) WITH DISTINCT n RETURN n")
            .unwrap()
            .rows,
        vec![vec![CypherValue::Null]]
    );
    assert_eq!(database.execute(
        "MATCH (a)-[:LINK]->() WITH DISTINCT a MATCH (a)-[r:LINK]->() RETURN count(*) AS count"
    ).unwrap().rows, vec![vec![CypherValue::Integer(2)]]);
}

#[test]
fn distinct_paths_and_graph_lists_compare_their_own_ordered_identities() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (a:Start), (b:End), (a)-[:LINK]->(b), (a)-[:LINK]->(b)")
        .unwrap();
    for pattern in [
        "(a:Start)-[:LINK*1..1]->(b:End)",
        "(a:Start)-[:LINK*1..]->(b:End)",
        "allShortestPaths((a:Start)-[:LINK*]->(b:End))",
    ] {
        for (projection, count) in [
            ("p", 2),
            ("nodes(p) AS ns", 1),
            ("relationships(p) AS rs", 2),
            ("nodes(p) AS ns, p", 2),
        ] {
            let query = format!(
                "MATCH p = {pattern} UNWIND [1, 1] AS copy WITH DISTINCT {projection} RETURN count(*) AS count"
            );
            assert_eq!(
                database.execute(&query).unwrap().rows,
                vec![vec![CypherValue::Integer(count)]],
                "{query}"
            );
        }
    }
    assert_eq!(
        database
            .execute(
                "UNWIND [1, 1] AS copy MATCH (a:Start) WITH DISTINCT a \
             MATCH p = (a)-[:LINK*1..]->(:End) RETURN count(*) AS count"
            )
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(2)]]
    );
    // Segment boundaries are not part of a public path's identity.
    let paths = database
        .execute(
            "MATCH p = (:Start)-[:LINK*0..1]->()-[:LINK*0..1]->(:End) \
         WITH DISTINCT p RETURN p",
        )
        .unwrap()
        .rows;
    assert_eq!(paths.len(), 2);
    assert!(
        paths
            .iter()
            .all(|row| matches!(row[0], CypherValue::Path(_)))
    );
    assert_eq!(
        database
            .execute(
                "MATCH p = (a)-[:LINK*0..0]->(b) WITH DISTINCT relationships(p) AS rs RETURN rs"
            )
            .unwrap()
            .rows,
        vec![vec![CypherValue::List(vec![])]]
    );
    assert_eq!(database.execute(
        "MATCH p = ()-[:LINK*1..1]-() WITH DISTINCT relationships(p) AS rs RETURN count(*) AS count"
    ).unwrap().rows, vec![vec![CypherValue::Integer(2)]]);
    assert_eq!(
        database
            .execute(
                "MATCH (:Start)-[rs:LINK*1..]->(:End) UNWIND [1, 1] AS copy \
         WITH DISTINCT rs MATCH (:Start)-[rs:LINK*1..1]->(:End) RETURN count(*) AS count"
            )
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(2)]]
    );
}

#[test]
fn distinct_controls_later_writes_without_erasing_earlier_side_effects() {
    let mut database = Database::in_memory().unwrap();
    let result = database
        .execute(
            "UNWIND [1, 1, 2] AS x CREATE (:Before {v: x}) \
         WITH DISTINCT x ORDER BY x LIMIT 1 CREATE (n:After {v: x}) \
         WITH DISTINCT n SET n.done = true RETURN n.v AS value",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![CypherValue::Integer(1)]]);
    assert_eq!(result.summary.nodes_created, 4);
    assert_eq!(
        database
            .execute("MATCH (n:Before) RETURN count(*) AS count")
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(3)]]
    );
    assert_eq!(
        database
            .execute("MATCH (n:After) RETURN n.v, n.done")
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(1), CypherValue::Boolean(true)]]
    );
    let result = database.execute(
        "UNWIND [1, 1, 2, 2] AS x WITH DISTINCT x MERGE (n:Merged {v: x}) RETURN n.v AS value ORDER BY value"
    ).unwrap();
    assert_eq!(result.summary.nodes_created, 2);
    assert_eq!(
        result.rows,
        vec![vec![CypherValue::Integer(1)], vec![CypherValue::Integer(2)]]
    );
}

#[test]
fn distinct_failures_roll_back_writes_and_validate_pagination_parameters() {
    let mut database = Database::in_memory().unwrap();
    for (query, error_kind) in [
        (
            "CREATE (:Before) WITH DISTINCT 1 AS x LIMIT $missing CREATE (:After)",
            "parameter",
        ),
        (
            "CREATE (:Before) WITH DISTINCT 1 AS x LIMIT -1 CREATE (:After)",
            "semantic",
        ),
        (
            "UNWIND [1, 1, 2] AS x CREATE (:Before) WITH DISTINCT x CREATE (:After) RETURN range(1, 2, 0)",
            "argument",
        ),
        (
            "CREATE (:Before) WITH DISTINCT range(1, 2, 0) AS x RETURN x",
            "argument",
        ),
    ] {
        let error = database.execute(query).unwrap_err();
        assert!(
            matches!(
                (&error, error_kind),
                (Error::MissingParameter(_), "parameter")
                    | (Error::Semantic(_), "semantic")
                    | (Error::Argument(_), "argument")
            ),
            "{query}: {error}"
        );
        assert_eq!(
            database
                .execute("MATCH (n) RETURN count(*) AS count")
                .unwrap()
                .rows,
            vec![vec![CypherValue::Integer(0)]]
        );
    }
}

#[test]
fn distinct_materializes_volatile_values_and_composes_with_union() {
    let mut database = Database::in_memory().unwrap();
    let rows = database
        .execute(
            "UNWIND range(1, 20) AS i WITH DISTINCT rand() AS draw \
         WITH draw AS a, draw AS b RETURN a = b AS same",
        )
        .unwrap()
        .rows;
    assert_eq!(rows, vec![vec![CypherValue::Boolean(true)]; 20]);
    assert_eq!(
        database
            .execute(
                "UNWIND [1, 1] AS x WITH DISTINCT x RETURN x UNION ALL \
         UNWIND [1, 1] AS x WITH DISTINCT x RETURN x"
            )
            .unwrap()
            .rows,
        vec![vec![CypherValue::Integer(1)]; 2]
    );
    assert!(
        crabnet::explain("UNWIND [1, 1] AS x WITH DISTINCT x RETURN x")
            .unwrap()
            .contains("distinct: true")
    );
}

#[test]
fn distinct_modifiers_reuse_projected_draws_without_re_evaluating_them() {
    let mut database = Database::in_memory().unwrap();
    let rows = database
        .execute(
            "UNWIND range(1, 100) AS i WITH DISTINCT rand() AS a, rand() AS b \
         ORDER BY b + 1 WHERE a < 0.5 RETURN a, b",
        )
        .unwrap()
        .rows;
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| matches!(row[0], CypherValue::Float(a) if a < 0.5))
    );
    assert!(
        rows.windows(2)
            .all(|pair| match (&pair[0][1], &pair[1][1]) {
                (CypherValue::Float(a), CypherValue::Float(b)) => a <= b,
                _ => false,
            })
    );
}
