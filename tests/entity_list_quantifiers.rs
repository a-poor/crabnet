use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

const TRUE: CypherValue = CypherValue::Boolean(true);
const FALSE: CypherValue = CypherValue::Boolean(false);
const NULL: CypherValue = CypherValue::Null;

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Step {rank: 1, active: true}), (b:Step {rank: 2, active: false}), \
                (c:Step {rank: 3}), (a)-[:NEXT {weight: 10}]->(b), \
                (b)-[:NEXT {weight: 20}]->(c)",
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
fn collected_nodes_and_relationships_keep_member_types_and_multiplicity() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) UNWIND [1,2] AS copy WITH collect(n) AS ns \
             RETURN all(n IN ns WHERE n:Step), any(n IN ns WHERE n.rank = 1), \
                    none(n IN ns WHERE n.rank > 3), single(n IN ns WHERE n.rank = 1), \
                    all(n IN ns WHERE n = ns[0]), \
                    all(n IN ns WHERE elementId(n) IS NOT NULL AND 'Step' IN labels(n))"
        ),
        [vec![TRUE, TRUE, TRUE, FALSE, FALSE, TRUE]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH ()-[r:NEXT]->() WITH collect(r) AS rs \
             RETURN all(r IN rs WHERE r:NEXT), single(r IN rs WHERE r.weight = 10), \
                    none(r IN rs WHERE type(r) <> 'NEXT'), \
                    all(r IN rs WHERE properties(r).weight > 0 AND r['weight'] > 0)"
        ),
        [vec![TRUE; 4]]
    );
}

#[test]
fn null_members_missing_properties_and_empty_lists_follow_three_valued_truth_tables() {
    let mut db = graph();
    // Index 0 has a true predicate, 1 false, 2 a missing property, and 99
    // selects a null entity. Repeated indexes must remain repeated members.
    let cases = [
        ("[]", [TRUE, FALSE, TRUE, FALSE]),
        ("[0]", [TRUE, TRUE, FALSE, TRUE]),
        ("[1]", [FALSE, FALSE, TRUE, FALSE]),
        ("[2]", [NULL, NULL, NULL, NULL]),
        ("[99]", [NULL, NULL, NULL, NULL]),
        ("[0,0]", [TRUE, TRUE, FALSE, FALSE]),
        ("[0,1]", [FALSE, TRUE, FALSE, TRUE]),
        ("[0,99]", [NULL, TRUE, FALSE, NULL]),
        ("[1,99]", [FALSE, NULL, NULL, NULL]),
        ("[0,0,99]", [NULL, TRUE, FALSE, FALSE]),
    ];
    for (indices, expected) in cases {
        let query = format!(
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
             WITH [i IN {indices} | ns[i]] AS selected \
             RETURN all(n IN selected WHERE n.active), any(n IN selected WHERE n.active), \
                    none(n IN selected WHERE n.active), single(n IN selected WHERE n.active)"
        );
        assert_eq!(rows(&mut db, &query), [expected.to_vec()], "{indices}");
    }
    for list in ["ns[null..]", "ns[..null]"] {
        let query = format!(
            "MATCH (n:Step) WITH collect(n) AS ns \
             RETURN all(n IN {list} WHERE true), any(n IN {list} WHERE true), \
                    none(n IN {list} WHERE true), single(n IN {list} WHERE true)"
        );
        assert_eq!(rows(&mut db, &query), [vec![NULL; 4]]);
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Missing) RETURN all(n IN collect(n) WHERE n.active), \
             any(n IN collect(n) WHERE n.active), none(n IN collect(n) WHERE n.active), \
             single(n IN collect(n) WHERE n.active)"
        ),
        [vec![TRUE, FALSE, TRUE, FALSE]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH ()-[r:NEXT]->() WITH collect(r) AS rs \
             WITH [i IN [0,99] | rs[i]] AS rs \
             RETURN all(r IN rs WHERE r IS NOT NULL), single(r IN rs WHERE r IS NULL), \
                    any(r IN rs WHERE r.missing), none(r IN rs WHERE r:NEXT)"
        ),
        [vec![FALSE, TRUE, NULL, FALSE]]
    );
}

#[test]
fn path_lists_aliases_slices_and_comprehensions_support_every_traversal_transport() {
    let mut db = graph();
    for pattern in [
        "p = (:Step {rank: 1})-[:NEXT]->()-[:NEXT]->()",
        "p = (:Step {rank: 1})-[:NEXT*2]->()",
        "p = (:Step {rank: 1})-[:NEXT*1..40]->(:Step {rank: 3})",
        "p = (:Step {rank: 1})-[:NEXT*]->(:Step {rank: 3})",
        "p = shortestPath((:Step {rank: 1})-[:NEXT*]->(:Step {rank: 3}))",
        "p = allShortestPaths((:Step {rank: 1})-[:NEXT*]->(:Step {rank: 3}))",
        "p = (:Step {rank: 1})-[:NEXT*0]->()-[:NEXT]->()-[:NEXT*0]->()-[:NEXT]->()",
        "p = (:Step {rank: 3})<-[:NEXT*2]-()",
    ] {
        let query = format!(
            "MATCH {pattern} WITH p, nodes(p) AS ns, relationships(p) AS rs \
             RETURN all(n IN nodes(p) WHERE n:Step), \
                    any(r IN relationships(p) WHERE r.weight = 20), \
                    none(n IN ns[1..] WHERE n.rank = ns[0].rank), \
                    single(r IN [edge IN rs WHERE edge.weight < 20] WHERE r:NEXT), \
                    all(r IN rs WHERE (:Step)-[r]->(:Step))"
        );
        assert_eq!(rows(&mut db, &query), [vec![TRUE; 5]], "{pattern}");
    }
    for bound in ["2", "1..40", ""] {
        assert_eq!(
            rows(
                &mut db,
                &format!(
                    "MATCH (:Step {{rank:1}})-[rs:NEXT*{bound}]->(:Step {{rank:3}}) \
                          RETURN single(r IN rs WHERE r.weight = 10)"
                )
            ),
            [vec![TRUE]]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p = (:Step {rank:1})-[rs:NEXT*0]->() \
             RETURN all(r IN relationships(p) WHERE true), any(r IN rs WHERE true), \
                    none(r IN rs WHERE true), single(r IN rs WHERE true), \
                    single(n IN nodes(p) WHERE n.rank = 1), \
                    any(n IN nodes(p)[null..] WHERE true)"
        ),
        [vec![TRUE, FALSE, TRUE, FALSE, TRUE, NULL]]
    );
}

#[test]
fn nested_quantifiers_and_patterns_use_local_bindings_without_leaking_scope() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:3}) MATCH (m:Step) WITH n, collect(m) AS ns \
             RETURN n.rank AS before, any(n IN ns WHERE n.rank = 1), n.rank AS after, \
                    all(ns IN ns WHERE ns:Step), \
                    all(n IN ns WHERE any(n IN ns WHERE n.rank = 2)), \
                    any(member IN ns WHERE (member)-[:NEXT]->()), \
                    single(n IN ns WHERE NOT exists((n)-->())), \
                    all(n IN ns WHERE any(m IN ns WHERE n = m)), \
                    any(n IN ns WHERE single(m IN ns WHERE (n)-->(m)))"
        ),
        [vec![
            CypherValue::Integer(3),
            TRUE,
            CypherValue::Integer(3),
            TRUE,
            TRUE,
            TRUE,
            TRUE,
            TRUE,
            TRUE
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH p = (:Step {rank:1})-[:NEXT*2]->() \
             WITH nodes(p) AS ns, relationships(p)[0] AS edge, nodes(p)[1] AS other \
             RETURN single(member IN ns WHERE (member)-[edge]->(other)), \
                    [n IN ns | any(m IN ns WHERE (n)-->(m))]"
        ),
        [vec![TRUE, CypherValue::List(vec![TRUE, TRUE, FALSE])]]
    );
    for query in [
        "MATCH (n:Step) WITH collect(n) AS ns RETURN any(member IN ns WHERE true), member",
        "MATCH (n:Step) RETURN any(n IN [1] WHERE (n)-->() )",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN any(member IN ns WHERE (unknown)-->(member))",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    let plan = crabnet::explain(
        "MATCH (n) WITH collect(n) AS ns RETURN any(member IN ns WHERE (member)-->() )",
    )
    .unwrap();
    assert!(
        plan.contains("any(#l0") && plan.contains("exists(bind "),
        "{plan}"
    );
}

#[test]
fn quantifiers_compose_with_parameters_aggregation_filters_and_scalar_results() {
    let mut db = graph();
    let query = "MATCH (n:Step) RETURN any(member IN collect(n) WHERE member[$key] > $min)";
    let params: Parameters = [("key".into(), json!("rank")), ("min".into(), json!(2))].into();
    let result = db.execute_with(query, &params).unwrap();
    assert_eq!(result.rows, [vec![TRUE]]);
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
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND [1,2,4] AS wanted \
             WITH ns, wanted WHERE any(n IN ns WHERE n.rank = wanted) \
             RETURN wanted, {hit: single(n IN ns WHERE n.rank = wanted)} AS result ORDER BY wanted"
        ),
        [1, 2].map(|rank| vec![
            CypherValue::Integer(rank),
            CypherValue::Map([("hit".into(), TRUE)].into())
        ])
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND [1,2,4] AS wanted \
             RETURN any(n IN ns WHERE n.rank = wanted) AS hit, count(*) ORDER BY hit"
        ),
        [
            vec![FALSE, CypherValue::Integer(1)],
            vec![TRUE, CypherValue::Integer(2)]
        ]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND [1,2,4] AS wanted \
             WITH ns, wanted ORDER BY wanted \
             RETURN collect(any(n IN ns WHERE n.rank = wanted))"
        ),
        [vec![CypherValue::List(vec![TRUE, TRUE, FALSE])]]
    );
}

#[test]
fn quantifier_results_can_be_stored_and_observe_mutations_and_deleted_snapshots() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
         WITH ns, ns[0] AS first SET first.rank = 10, first:Changed \
         CREATE (report:Report {changed: any(n IN ns WHERE n:Changed), \
                                connected: any(n IN ns WHERE (n)-->() )}) \
         DETACH DELETE first RETURN report.changed, report.connected, \
         single(n IN ns WHERE n.rank = 10), any(n IN ns WHERE n:Changed)",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![TRUE; 4]]);
    let result = db
        .execute(
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND [1,2] AS copy \
         WITH ns WHERE any(n IN ns WHERE n.rank = 2) \
         CREATE (:Hit {value: all(n IN ns WHERE n.rank > 1)}) RETURN count(*)",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![CypherValue::Integer(2)]]);
    assert_eq!(
        rows(&mut db, "MATCH (n:Hit) RETURN n.value"),
        [vec![TRUE], vec![TRUE]]
    );
}

#[test]
fn invalid_predicates_fail_with_structured_errors_and_roll_back_writes() {
    let mut db = graph();
    for predicate in ["n", "n + 1", "count(n) > 0", "type(n) = 'NEXT'"] {
        let query =
            format!("MATCH (n:Step) WITH collect(n) AS ns RETURN any(n IN ns WHERE {predicate})");
        let error = db.execute(&query).unwrap_err();
        assert!(
            matches!(error, Error::Semantic(_) | Error::Unsupported(_)),
            "{query}: {error:?}"
        );
    }
    assert!(matches!(
        db.execute("MATCH (n:Step) WITH collect(n) AS ns RETURN all(n IN ns[0] WHERE true)"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        db.execute("MATCH p = ()-->() RETURN any(n IN p WHERE true)"),
        Err(Error::Semantic(_))
    ));
    // Aggregate grouping rules still apply; materialize the collection in
    // WITH before using a pattern predicate over its members.
    for query in [
        "MATCH (n:Step) RETURN any(member IN collect(n) WHERE (member)-->() )",
        "MATCH (n:Step), (m:Step) RETURN any(member IN collect(n) WHERE member.rank > m.rank)",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    for kind in ["all", "any", "none", "single"] {
        let query = format!(
            "CREATE (n:Rollback {{active:42}}) WITH collect(n) AS ns \
             RETURN {kind}(member IN ns WHERE member.active)"
        );
        assert!(matches!(db.execute(&query), Err(Error::Type(_))), "{query}");
    }
    assert_eq!(
        rows(&mut db, "MATCH (n:Rollback) RETURN count(n)"),
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn cursors_report_predicate_errors_after_prior_rows_and_remain_reusable() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input {rank:0, active:true}), (:Input {rank:1, active:42})")
        .unwrap();
    for kind in ["all", "any", "none", "single"] {
        let query = format!(
            "MATCH (n:Input) WITH n ORDER BY n.rank WITH collect(n) AS ns \
             UNWIND [0,1] AS i RETURN {kind}(n IN ns[i..i+1] WHERE n.active)"
        );
        assert!(matches!(db.execute(&query), Err(Error::Type(_))));
        let mut read = db.prepare_read(&query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap(),
            vec![CypherValue::Boolean(kind != "none")]
        );
        assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
        assert!(cursor.next().is_none());
    }
    assert_eq!(
        rows(&mut db, "MATCH (n:Input) RETURN count(n)"),
        [vec![CypherValue::Integer(2)]]
    );
}
