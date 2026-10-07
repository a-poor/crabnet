use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn ints(values: &[i64]) -> CypherValue {
    CypherValue::List(values.iter().copied().map(CypherValue::Integer).collect())
}

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Step {rank: 1}), (b:Step {rank: 2}), (c:Step {rank: 3}), \
                (a)-[:NEXT {weight: 10}]->(b), (b)-[:NEXT {weight: 20}]->(c)",
    )
    .unwrap();
    db
}

fn rows(db: &mut Database, query: &str) -> Vec<Vec<CypherValue>> {
    let eager = db
        .execute(query)
        .unwrap_or_else(|error| panic!("{query}: {error:?}"));
    let mut read = db.prepare_read(query).unwrap();
    let cursor = read
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(cursor, eager.rows, "cursor parity: {query}");
    eager.rows
}

#[test]
fn collected_entities_filter_and_project_without_losing_types_or_multiplicity() {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (:Person {name: 'Ada', rank: 1, active: true}), \
                       (:Person {name: 'Bob', rank: 2, active: false}), \
                       (:Person {name: 'Cy', rank: 3})",
    )
    .unwrap();
    let result = rows(
        &mut db,
        "MATCH (n:Person) UNWIND [1, 2] AS copy WITH n ORDER BY n.rank \
         WITH collect(n) AS people \
         RETURN people, [n IN people | n], [n IN people WHERE n.active], \
                [n IN people WHERE n.rank > 1 | n.name], \
                [n IN people | {rank: n.rank, labels: labels(n)}], \
                [n IN people WHERE n = people[0] | elementId(n)], \
                [n IN people | n.missing]",
    );
    let row = &result[0];
    let CypherValue::List(people) = &row[0] else {
        panic!("entities")
    };
    assert_eq!(people.len(), 6);
    assert!(people.iter().all(|n| n.as_node().is_some()));
    assert_eq!(row[1], row[0]);
    assert_eq!(row[2], CypherValue::List(people[..2].to_vec()));
    assert_eq!(
        row[3],
        CypherValue::List(
            ["Bob", "Bob", "Cy", "Cy"]
                .map(|s| CypherValue::String(s.into()))
                .to_vec()
        )
    );
    let CypherValue::List(maps) = &row[4] else {
        panic!("maps")
    };
    assert_eq!(maps[0].as_map().unwrap()["rank"], CypherValue::Integer(1));
    assert_eq!(
        row[5],
        CypherValue::List(vec![
            CypherValue::String(
                people[0].as_node().unwrap().element_id.clone()
            );
            2
        ])
    );
    assert_eq!(row[6], CypherValue::List(vec![CypherValue::Null; 6]));
}

#[test]
fn path_lists_and_aliases_support_all_traversal_transports() {
    let mut db = graph();
    for pattern in [
        "p = (:Step {rank: 1})-[:NEXT]->()-[:NEXT]->()",
        "p = (:Step {rank: 1})-[rs:NEXT*2]->()",
        "p = (:Step {rank: 1})-[rs:NEXT*1..40]->(:Step {rank: 3})",
        "p = (:Step {rank: 1})-[rs:NEXT*]->(:Step {rank: 3})",
        "p = shortestPath((:Step {rank: 1})-[rs:NEXT*]->(:Step {rank: 3}))",
        "p = allShortestPaths((:Step {rank: 1})-[rs:NEXT*]->(:Step {rank: 3}))",
        "p = (:Step {rank: 1})-[:NEXT*0]->()-[:NEXT]->()-[:NEXT*0]->()-[:NEXT]->()",
    ] {
        let query = format!(
            "MATCH {pattern} \
            WITH p, nodes(p) AS ns, relationships(p) AS rs \
            RETURN [n IN nodes(p) | n.rank], [n IN ns WHERE n.rank <> 2], \
                   [r IN relationships(p) | r.weight], [r IN rs WHERE (r:NEXT) | r], \
                   [r IN rs | type(r)], [n IN ns WHERE (n:Step) | n['rank']]"
        );
        let result = rows(&mut db, &query);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0][0], ints(&[1, 2, 3]));
        assert_eq!(result[0][2], ints(&[10, 20]));
        assert_eq!(result[0][5], ints(&[1, 2, 3]));
        let CypherValue::List(ns) = &result[0][1] else {
            panic!("nodes")
        };
        assert_eq!(ns.len(), 2);
        assert_eq!(
            ns[0].as_node().unwrap().properties["rank"],
            CypherValue::Integer(1)
        );
        let CypherValue::List(rs) = &result[0][3] else {
            panic!("relationships")
        };
        assert_eq!(rs.len(), 2);
        assert_eq!(
            rs[0].as_relationship().unwrap().start_node_element_id,
            ns[0].as_node().unwrap().element_id
        );
        assert_eq!(
            rs[1].as_relationship().unwrap().end_node_element_id,
            ns[1].as_node().unwrap().element_id
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (:Step {rank:1})-[rs:NEXT*2]->() RETURN [r IN rs | r.weight]"
        ),
        [vec![ints(&[10, 20])]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p = (:Step {rank:3})<-[:NEXT*2]-() RETURN [n IN nodes(p) | n.rank], [r IN relationships(p) | r.weight]"
        ),
        [vec![ints(&[3, 2, 1]), ints(&[20, 10])]]
    );
}

#[test]
fn nullable_members_survive_aliases_unwind_distinct_grouping_and_union() {
    let mut db = graph();
    let prefix = "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns ";
    let query = format!(
        "{prefix} WITH [i IN [0,99,0] | ns[i]] AS selected \
        RETURN selected, [n IN selected | n], [n IN selected | n.rank], \
               [n IN selected WHERE n IS NOT NULL], selected[1], size(selected), \
               [n IN selected WHERE n IS NULL]"
    );
    let result = rows(&mut db, &query);
    let CypherValue::List(selected) = &result[0][0] else {
        panic!("nodes")
    };
    assert_eq!(selected.len(), 3);
    assert!(selected[0].as_node().is_some());
    assert_eq!(selected[1], CypherValue::Null);
    assert_eq!(selected[0], selected[2]);
    assert_eq!(result[0][0], result[0][1]);
    assert_eq!(
        result[0][2],
        CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::Null,
            CypherValue::Integer(1)
        ])
    );
    assert_eq!(
        result[0][3],
        CypherValue::List(vec![selected[0].clone(); 2])
    );
    assert_eq!(result[0][4], CypherValue::Null);
    assert_eq!(result[0][5], CypherValue::Integer(3));
    assert_eq!(result[0][6], CypherValue::List(vec![CypherValue::Null]));
    assert_eq!(
        rows(
            &mut db,
            &format!(
                "{prefix} WITH [i IN [0,99,0] | ns[i]] AS selected UNWIND selected AS n RETURN n.rank"
            )
        ),
        [
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Null],
            vec![CypherValue::Integer(1)]
        ]
    );
    for projection in [
        "WITH DISTINCT selected RETURN selected",
        "RETURN selected, count(*)",
    ] {
        let result = rows(
            &mut db,
            &format!(
                "{prefix} UNWIND [1,2] AS copy WITH [i IN [0,99] | ns[i]] AS selected {projection}"
            ),
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0][0], CypherValue::List(selected[..2].to_vec()));
        if projection.contains("count") {
            assert_eq!(result[0][1], CypherValue::Integer(2));
        }
    }
    let branch = format!("{prefix} RETURN [i IN [0,99] | ns[i]] AS selected");
    assert_eq!(rows(&mut db, &format!("{branch} UNION {branch}")).len(), 1);
    assert_eq!(
        rows(
            &mut db,
            "OPTIONAL MATCH (n:Missing) RETURN [i IN [1,2] | n]"
        ),
        [vec![CypherValue::List(vec![CypherValue::Null; 2])]]
    );
}

#[test]
fn comprehensions_compose_and_shadow_outer_variables_without_leaking_scope() {
    let mut db = graph();
    let result = rows(
        &mut db,
        "MATCH (n:Step {rank:3}) MATCH p = (:Step {rank:1})-[:NEXT*2]->() \
        WITH n, nodes(p) AS ns RETURN n.rank AS before, [n IN ns | n.rank], n.rank AS after, \
        [n IN ns WHERE n.rank < 3 | [n IN ns WHERE n.rank > 1 | n.rank]], \
        [n IN [m IN ns WHERE m.rank > 1] | n.rank], \
        [n IN ns WHERE (n)-[:NEXT]->() | n.rank], \
        [n IN ns WHERE exists((n)<-[:NEXT]-()) | n.rank]",
    );
    assert_eq!(
        result,
        [vec![
            CypherValue::Integer(3),
            ints(&[1, 2, 3]),
            CypherValue::Integer(3),
            CypherValue::List(vec![ints(&[2, 3]); 2]),
            ints(&[2, 3]),
            ints(&[1, 2]),
            ints(&[2, 3])
        ]]
    );
    assert!(matches!(
        db.execute(
            "MATCH (n:Step) WITH collect(n) AS ns RETURN [member IN ns | member.rank], member"
        ),
        Err(Error::Semantic(_))
    ));
    // A scalar local must not accidentally resolve to the outer graph entity.
    assert!(matches!(
        db.execute("MATCH (n:Step) RETURN [n IN [1] WHERE (n)-->() | n]"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn empty_null_inputs_parameters_and_direct_aggregate_inputs_keep_list_semantics() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH n ORDER BY n.rank RETURN [n IN collect(n) WHERE n.rank > 1 | n.rank]"
        ),
        [vec![ints(&[2, 3])]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Missing) WITH collect(n) AS ns RETURN [n IN ns | n], [n IN ns WHERE false | n.rank]"
        ),
        [vec![ints(&[]), ints(&[])]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p = (:Step {rank:1})-[:NEXT*2]->() RETURN [n IN nodes(p)[null..] | n], [r IN relationships(p)[null..] | r.weight]"
        ),
        [vec![CypherValue::Null; 2]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p = (:Step {rank:1})-[:NEXT*0]->() RETURN [r IN relationships(p) | r], [n IN nodes(p) | n.rank]"
        ),
        [vec![ints(&[]), ints(&[1])]]
    );
    let query = "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns RETURN [n IN ns WHERE n.rank > $min | n[$key]]";
    let params: Parameters = [("min".into(), json!(1)), ("key".into(), json!("rank"))].into();
    let result = db.execute_with(query, &params).unwrap();
    assert_eq!(result.rows, [vec![ints(&[2, 3])]]);
    {
        let mut read = db.prepare_read_with(query, &params).unwrap();
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            result.rows
        );
    }
    assert!(matches!(db.execute(query), Err(Error::MissingParameter(_))));
}

#[test]
fn graph_lists_refresh_through_writes_and_keep_nulls_and_deleted_snapshots() {
    let mut db = graph();
    let scalar_result = db
        .execute(
            "MATCH (n:Step) WITH collect(n) AS ns \
        CREATE (report:Sizes {values: [n IN ns | size(ns)], missing: ns IS NULL}) \
        RETURN report.values, report.missing",
        )
        .unwrap();
    assert_eq!(
        scalar_result.rows,
        [vec![ints(&[3, 3, 3]), CypherValue::Boolean(false)]]
    );
    let result = db
        .execute(
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        WITH [i IN [0,99,0] | ns[i]] AS selected WITH selected, selected[0] AS n \
        SET n.rank = 10, n:Changed \
        CREATE (report:Report {ranks: [n IN selected | n.rank]}) \
        DETACH DELETE n RETURN selected, [n IN selected | n.rank], report.ranks",
        )
        .unwrap();
    let CypherValue::List(selected) = &result.rows[0][0] else {
        panic!("entities")
    };
    assert_eq!(selected[0], selected[2]);
    assert_eq!(selected[1], CypherValue::Null);
    let node = selected[0].as_node().unwrap();
    assert_eq!(node.properties["rank"], CypherValue::Integer(10));
    assert!(node.labels.contains(&"Changed".to_owned()));
    assert_eq!(
        result.rows[0][1],
        CypherValue::List(vec![
            CypherValue::Integer(10),
            CypherValue::Null,
            CypherValue::Integer(10)
        ])
    );
    assert_eq!(result.rows[0][1], result.rows[0][2]);
    let error = db.execute("CREATE (n:Rollback {bad:'bad'}) WITH [i IN [0,1] | n] AS ns RETURN [n IN ns | [1][n.bad]]").unwrap_err();
    assert!(
        matches!(error, Error::Type(_) | Error::Semantic(_)),
        "{error:?}"
    );
    assert_eq!(
        rows(&mut db, "MATCH (n:Rollback) RETURN count(n)"),
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn unsupported_entity_scalar_uses_remain_structured_errors() {
    let mut db = graph();
    for projection in [
        "[n IN ns | n + 1]",
        "[n IN ns WHERE n | n]",
        "[n IN ns | count(n)]",
        "[n IN ns WHERE count(n) > 0]",
        "[n IN ns | type(n)]",
        "[n IN ns[0] | n]",
        "[n IN ns | n].id",
    ] {
        let query = format!("MATCH (n:Step) WITH collect(n) AS ns RETURN {projection}");
        assert!(
            matches!(
                db.execute(&query),
                Err(Error::Unsupported(_) | Error::Semantic(_))
            ),
            "{query}"
        );
    }
    assert!(matches!(
        db.execute("MATCH (n:Step) WITH collect(n) AS ns CREATE (:Bad {nodes:[n IN ns | n]})"),
        Err(Error::Semantic(_) | Error::Unsupported(_))
    ));
}

#[test]
fn patterns_correlate_new_node_and_relationship_members_across_read_and_write_scopes() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH p = (:Step {rank:1})-[:NEXT*2]->() \
        WITH nodes(p) AS ns, relationships(p)[0] AS edge, nodes(p)[1] AS other \
        RETURN [member IN ns WHERE (member)-[edge]->(other) | member.rank]"
        ),
        [vec![ints(&[1])]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p = (:Step {rank:1})-[:NEXT*2]->() \
        RETURN [member IN nodes(p) WHERE (member)-[:NEXT]->() | member.rank], \
               [edge IN relationships(p) WHERE (:Step {rank:1})-[edge]->() | edge.weight], \
               [member IN nodes(p) | [other IN nodes(p) WHERE (member)-->(other) | other.rank]]"
        ),
        [vec![
            ints(&[1, 2]),
            ints(&[10]),
            CypherValue::List(vec![ints(&[2]), ints(&[3]), ints(&[])])
        ]]
    );
    let result = db
        .execute(
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        CREATE (report:Report {connected: [member IN ns | exists((member)-->() )]}) \
        RETURN report.connected, [member IN ns WHERE (member)-->() | member.rank]",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::List(vec![
                CypherValue::Boolean(true),
                CypherValue::Boolean(true),
                CypherValue::Boolean(false)
            ]),
            ints(&[1, 2])
        ]]
    );
    let plan = crabnet::explain(
        "MATCH (n) WITH collect(n) AS ns RETURN [member IN ns WHERE (member)-->() | member]",
    )
    .unwrap();
    assert!(plan.contains("exists(bind "), "{plan}");
}

#[test]
fn cursors_report_runtime_predicate_errors_after_prior_rows_and_remain_reusable() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input {rank:0, active:true}), (:Input {rank:1, active:42})")
        .unwrap();
    let query = "MATCH (n:Input) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        UNWIND [0,1] AS i RETURN [n IN ns[i..i+1] WHERE n.active | n.rank]";
    assert!(matches!(db.execute(query), Err(Error::Type(_))));
    {
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), vec![ints(&[0])]);
        assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
        assert!(cursor.next().is_none());
    }
    assert_eq!(
        rows(&mut db, "MATCH (n:Input) RETURN count(n)"),
        [vec![CypherValue::Integer(2)]]
    );
}
