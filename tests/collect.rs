use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

#[test]
fn collected_nodes_keep_identity_duplicates_and_can_be_matched_after_unwind() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Person {name: 'Ada'}), (b:Person {name: 'Ada'}), (a)-[:KNOWS]->(b)")
        .unwrap();
    let result = db.execute("MATCH (n:Person) UNWIND [1, 2] AS duplicate RETURN collect(n) AS bag, collect(DISTINCT n) AS unique").unwrap();
    let CypherValue::List(bag) = &result.rows[0][0] else {
        panic!("expected list")
    };
    let CypherValue::List(unique) = &result.rows[0][1] else {
        panic!("expected list")
    };
    assert_eq!(bag.len(), 4);
    assert_eq!(unique.len(), 2);
    assert_ne!(
        unique[0].as_node().unwrap().element_id,
        unique[1].as_node().unwrap().element_id
    );
    for node in unique {
        assert_eq!(bag.iter().filter(|value| *value == node).count(), 2);
    }
    let expanded = db.execute("MATCH (n:Person) UNWIND [1, 2] AS duplicate WITH collect(n) AS people UNWIND people AS n RETURN count(n)").unwrap();
    assert_eq!(expanded.rows, [vec![CypherValue::Integer(4)]]);
    let result = db
        .execute(
            "MATCH (n:Person) UNWIND [1, 2] AS duplicate WITH collect(DISTINCT n) AS people WITH \
             people AS renamed UNWIND renamed AS person MATCH (person)-[:KNOWS]->(friend) RETURN \
             person.name, friend.name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("Ada".into()),
            CypherValue::String("Ada".into())
        ]]
    );
}

#[test]
fn distinct_collection_uses_recursive_typed_equality_and_preserves_first_occurrences() {
    let mut db = Database::in_memory().unwrap();
    let params: Parameters = [("values".into(), json!([null, true, true, 1, 1.0, "1", [1], [1.0], {"a": 1}, {"a": 1.0}, 9007199254740992_i64, 9007199254740993_i64, 9007199254740992.0]))].into();
    let result = db
        .execute_with(
            "UNWIND range(0, size($values)-1) AS i WITH i, $values[i] AS value ORDER BY i RETURN \
             collect(DISTINCT value) AS values",
            &params,
        )
        .unwrap();
    let expected = json!([true, 1, "1", [1], {"a": 1}, 9007199254740992_i64, 9007199254740993_i64]);
    assert_eq!(
        result.rows,
        [vec![CypherValue::try_from(expected).unwrap()]]
    );
}

#[test]
fn empty_and_nullable_entities_collect_to_empty_lists() {
    let mut db = Database::in_memory().unwrap();
    for query in [
        "MATCH (n:Missing) RETURN collect(n), collect(DISTINCT n)",
        "OPTIONAL MATCH (n:Missing) RETURN collect(n), collect(DISTINCT n)",
        "OPTIONAL MATCH ()-[n:MISSING]->() RETURN collect(n), collect(DISTINCT n)",
    ] {
        assert_eq!(
            db.execute(query).unwrap().rows,
            [vec![CypherValue::List(vec![]), CypherValue::List(vec![])]]
        );
    }
    assert!(
        db.execute("MATCH (n:Missing) WITH collect(n) AS nodes UNWIND nodes AS n RETURN n")
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(matches!(
        db.execute("MATCH (n) UNWIND collect(n) AS m RETURN m"),
        Err(Error::Semantic(_))
    ));
    for query in [
        "UNWIND [] AS value RETURN collect(DISTINCT value)",
        "UNWIND [null, null] AS value RETURN collect(DISTINCT value)",
    ] {
        assert_eq!(
            db.execute(query).unwrap().rows,
            [vec![CypherValue::List(vec![])]]
        );
    }
}

#[test]
fn relationships_keep_endpoints_and_can_be_rebound_and_updated() {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Start), (b:End), (a)-[:LINK {value: 1}]->(b), (a)-[:LINK {value: 1}]->(b)",
    )
    .unwrap();
    let result = db.execute(
        "MATCH (a)-[r:LINK]->(b) UNWIND [1,2] AS duplicate WITH collect(DISTINCT r) AS edges \
             WITH edges, size(edges) AS amount UNWIND edges AS edge MATCH (a)-[edge]->(b) SET \
             edge.value = $value RETURN edge, a, b, amount",
    );
    assert!(matches!(result, Err(Error::MissingParameter(_))));
    let result = db.execute_with("MATCH ()-[r:LINK]->() UNWIND [1,2] AS duplicate WITH collect(DISTINCT r) AS edges WITH \
             edges, size(edges) AS amount UNWIND edges AS edge MATCH (a)-[edge]->(b) SET edge.value \
             = $value RETURN edge, a, b, amount", &[("value".into(), json!(7))].into()).unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.summary.properties_set, 2);
    for row in result.rows {
        let edge = row[0].as_relationship().unwrap();
        assert_eq!(
            edge.start_node_element_id,
            row[1].as_node().unwrap().element_id
        );
        assert_eq!(
            edge.end_node_element_id,
            row[2].as_node().unwrap().element_id
        );
        assert_eq!(edge.properties["value"], CypherValue::Integer(7));
        assert_eq!(row[3], CypherValue::Integer(2));
    }
    let result = db.execute("MATCH ()-[r:LINK]->() UNWIND [1,2] AS duplicate RETURN collect(r), collect(DISTINCT r)").unwrap();
    for (column, length) in [(0, 4), (1, 2)] {
        let CypherValue::List(edges) = &result.rows[0][column] else {
            panic!("expected list")
        };
        assert_eq!(edges.len(), length);
        assert!(
            edges.iter().all(
                |v| v.as_relationship().unwrap().properties["value"] == CypherValue::Integer(7)
            )
        );
    }
}

#[test]
fn grouping_ordering_aliases_and_distinct_boundaries_keep_typed_lists() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Person {team:'a', rank:1}), (:Person {team:'a', rank:2}), (:Person {team:'b', rank:3})").unwrap();
    let result = db.execute("MATCH (n:Person) WITH n ORDER BY n.rank DESC WITH n.team AS team, collect(n) AS people \
             WHERE size(people) > 1 WITH DISTINCT people AS renamed UNWIND renamed AS n RETURN \
             n.rank ORDER BY n.rank DESC").unwrap();
    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(2)], vec![CypherValue::Integer(1)]]
    );
    let result = db
        .execute(
            "MATCH (n:Person) WITH n ORDER BY n.rank WITH collect(n) AS people UNWIND [1,2] AS \
             duplicate WITH people, count(*) AS amount RETURN people, amount",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][1], CypherValue::Integer(2));
    let CypherValue::List(people) = &result.rows[0][0] else {
        panic!("expected list")
    };
    assert_eq!(
        people
            .iter()
            .map(|n| n.as_node().unwrap().properties["rank"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let result = db
        .execute(
            "MATCH (n:Person) WITH collect(n) AS people UNWIND [1,2] AS duplicate WITH DISTINCT \
             people LIMIT 1 RETURN *",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert!(
        matches!(&result.rows[0][0], CypherValue::List(values) if values.len() == 3 && values.iter().all(|v| v.as_node().is_some()))
    );
}

#[test]
fn collection_survives_write_boundaries_and_tracks_mutations_and_delete_snapshots() {
    let mut db = Database::in_memory().unwrap();
    let result = db
        .execute(
            "UNWIND [1,1,2] AS value CREATE (n:Person {value:value}) WITH collect(n) AS people \
             UNWIND people AS n SET n.updated = true SET n:Updated WITH DISTINCT people UNWIND \
             people AS n RETURN n, people",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_created, 3);
    assert_eq!(result.summary.properties_set, 3);
    assert_eq!(result.rows.len(), 3);
    for row in &result.rows {
        let node = row[0].as_node().unwrap();
        assert_eq!(node.properties["updated"], CypherValue::Boolean(true));
        assert!(node.labels.contains(&"Updated".into()));
        let CypherValue::List(people) = &row[1] else {
            panic!("expected list")
        };
        assert!(
            people
                .iter()
                .all(|v| v.as_node().unwrap().properties["updated"] == CypherValue::Boolean(true))
        );
    }
    let result = db
        .execute(
            "MATCH (n:Person) WITH collect(n) AS people UNWIND people AS n SET n.value = 9 REMOVE \
             n:Updated DELETE n RETURN people",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_deleted, 3);
    assert_eq!(result.rows.len(), 3);
    for row in result.rows {
        let CypherValue::List(people) = &row[0] else {
            panic!("expected list")
        };
        assert!(people.iter().all(|v| {
            let n = v.as_node().unwrap();
            n.properties["value"] == CypherValue::Integer(9)
                && !n.labels.contains(&"Updated".into())
        }));
    }
}

#[test]
fn distinct_collection_controls_later_write_multiplicity_and_errors_roll_back() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Person {value:1}), (:Person {value:2})")
        .unwrap();
    let result = db
        .execute(
            "MATCH (n:Person) UNWIND [1,2] AS duplicate WITH collect(DISTINCT n) AS people UNWIND \
             people AS n CREATE (:Copy {value:n.value}) RETURN n.value ORDER BY n.value",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_created, 2);
    for query in [
        "MATCH (n:Person) WITH collect(n) AS people UNWIND people AS n SET n.changed = true \
             WITH people UNWIND range(1,3,0) AS bad RETURN bad",
        "CREATE (n:Failed) WITH collect(n) AS people UNWIND people AS n SET n.value = $missing RETURN n",
    ] {
        assert!(db.execute(query).is_err(), "{query}");
    }
    assert_eq!(
        db.execute("MATCH (n:Person) RETURN count(n.changed)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(0)]]
    );
    assert_eq!(
        db.execute("MATCH (n:Failed) RETURN count(n)").unwrap().rows,
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn collection_composes_with_optional_matches_hybrid_traversals_and_union() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Start), (b:End), (a)-[:LINK]->(b), (a)-[:LINK]->(b)")
        .unwrap();
    for pattern in [
        "(a:Start)-[:LINK*1..]->(b)",
        "p=allShortestPaths((a:Start)-[:LINK*]->(b:End))",
    ] {
        let query = format!(
            "MATCH {pattern} WITH collect(DISTINCT b) AS targets UNWIND targets AS b OPTIONAL MATCH \
             (b)-[:MISSING]->(c) RETURN b, collect(c) AS missing"
        );
        let result = db.execute(&query).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(result.rows[0][0].as_node().is_some());
        assert_eq!(result.rows[0][1], CypherValue::List(vec![]));
    }
    let result = db.execute("MATCH (a:Start) WITH collect(a) AS sources UNWIND sources AS a MATCH (a)-[:LINK*1..]->(b) RETURN b").unwrap();
    assert_eq!(result.rows.len(), 2);
    let result = db.execute("MATCH (n:Start) RETURN collect(n) AS nodes UNION MATCH (n:Start) RETURN collect(DISTINCT n) AS nodes").unwrap();
    assert_eq!(result.rows.len(), 1);
    assert!(
        crabnet::explain("MATCH (n) WITH collect(n) AS ns UNWIND ns AS n RETURN n")
            .unwrap()
            .contains("entity-list")
    );
}

#[test]
fn entity_lists_cannot_silently_become_json_maps_or_stored_properties() {
    let mut db = Database::in_memory().unwrap();
    for query in [
        "MATCH (n) WITH collect(n) AS ns RETURN ns[0]",
        "MATCH (n) WITH collect(n) AS ns RETURN [ns]",
        "MATCH (n) WITH collect(n) AS ns RETURN collect(ns)",
        "MATCH (n) WITH collect(n) AS ns CREATE (:Bad {nodes:ns})",
        "MATCH (n) WITH collect(n) AS ns RETURN [x IN ns | x]",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::Unsupported(_))),
            "{query}"
        );
    }
    assert!(matches!(
        db.execute("UNWIND [{id:'fake', props:{}}] AS n MATCH (n) RETURN n"),
        Err(Error::Semantic(_))
    ));
}
