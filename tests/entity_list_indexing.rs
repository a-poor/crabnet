use crabnet::{CypherValue, Database};

#[test]
fn collected_entities_keep_types_order_and_duplicates_through_indexing_and_slicing() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Person {name: 'Ada', rank: 1}), (:Person {name: 'Bob', rank: 2})")
        .unwrap();
    let query = "MATCH (n:Person) UNWIND [1, 2] AS copy WITH n ORDER BY n.rank \
                 WITH collect(n) AS people \
                 RETURN people, people[0], people[-1], people[1..3], people[-3..-1], \
                        people[0].name, people[1..][0].rank, people[4], people[-5]";
    let result = db.execute(query).unwrap();
    let row = &result.rows[0];
    let CypherValue::List(people) = &row[0] else {
        panic!("expected entities");
    };
    assert_eq!(people.len(), 4);
    assert_eq!(people[0], people[1]);
    assert_eq!(people[2], people[3]);
    assert_ne!(people[0], people[2]);
    assert!(people.iter().all(|person| person.as_node().is_some()));
    assert_eq!(row[1], people[0]);
    assert_eq!(row[2], people[3]);
    assert_eq!(row[3], CypherValue::List(people[1..3].to_vec()));
    assert_eq!(row[4], row[3]);
    assert_eq!(row[5], CypherValue::String("Ada".into()));
    assert_eq!(row[6], CypherValue::Integer(1));
    assert_eq!(row[7..], [CypherValue::Null, CypherValue::Null]);
    let mut read = db.prepare_read(query).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        result.rows
    );
}

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Step {rank: 1, nullable: null}), (b:Step {rank: 2}), \
                       (c:Step {rank: 3}), (a)-[:NEXT {rank: 1}]->(b), \
                       (b)-[:NEXT {rank: 2}]->(c)",
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
fn path_lists_keep_order_and_entities_across_all_traversal_transports() {
    let mut db = graph();
    for pattern in [
        "p = (:Step {rank: 1})-[:NEXT]->()-[:NEXT]->(:Step {rank: 3})",
        "p = (:Step {rank: 1})-[rels:NEXT*2]->(:Step {rank: 3})",
        "p = (:Step {rank: 1})-[rels:NEXT*1..40]->(:Step {rank: 3})",
        "p = (:Step {rank: 1})-[rels:NEXT*]->(:Step {rank: 3})",
        "p = shortestPath((:Step {rank: 1})-[rels:NEXT*]->(:Step {rank: 3}))",
        "p = allShortestPaths((:Step {rank: 1})-[rels:NEXT*]->(:Step {rank: 3}))",
        "p = (:Step {rank: 1})-[:NEXT*0]->()-[:NEXT]->()-[:NEXT*0]->()-[:NEXT]->()-[:NEXT*0]->()",
    ] {
        let query = format!(
            "MATCH {pattern} WITH nodes(p) AS ns, relationships(p) AS rs \
            RETURN ns, ns[0], ns[-1], ns[1..], rs, rs[-1], rs[..1], ns[1..][0].rank"
        );
        let result = rows(&mut db, &query);
        assert_eq!(result.len(), 1);
        let row = &result[0];
        let CypherValue::List(ns) = &row[0] else {
            panic!("nodes")
        };
        let CypherValue::List(rs) = &row[4] else {
            panic!("relationships")
        };
        assert_eq!(ns.len(), 3);
        assert_eq!(rs.len(), 2);
        assert_eq!(row[1], ns[0]);
        assert_eq!(row[2], ns[2]);
        assert_eq!(row[3], CypherValue::List(ns[1..].to_vec()));
        assert_eq!(row[5], rs[1]);
        assert_eq!(row[6], CypherValue::List(rs[..1].to_vec()));
        assert_eq!(row[7], CypherValue::Integer(2));
        for (index, edge) in rs.iter().enumerate() {
            let edge = edge.as_relationship().unwrap();
            assert_eq!(
                edge.start_node_element_id,
                ns[index].as_node().unwrap().element_id
            );
            assert_eq!(
                edge.end_node_element_id,
                ns[index + 1].as_node().unwrap().element_id
            );
        }
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (:Step {rank:1})-[rels:NEXT*2]->() \
        WITH rels[0..1] AS first UNWIND first AS r RETURN r.rank"
        ),
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn slices_retain_repeated_nodes_and_parallel_relationship_identity() {
    let mut db = Database::in_memory().unwrap();
    db.execute(
        "CREATE (a:Loop {rank: 1}), (b:Loop {rank: 2}), \
        (a)-[:R {rank: 1}]->(b), (b)-[:R {rank: 2}]->(a), (a)-[:R {rank: 3}]->(b)",
    )
    .unwrap();
    let result = rows(
        &mut db,
        "MATCH p = (a:Loop {rank:1})-[:R {rank:1}]->()-[:R]->(a) \
        RETURN nodes(p), nodes(p)[..], nodes(p)[0] = nodes(p)[-1]",
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], result[0][1]);
    let CypherValue::List(ns) = &result[0][0] else {
        panic!("nodes")
    };
    assert_eq!(ns.len(), 3);
    assert_eq!(ns[0], ns[2]);
    assert_eq!(result[0][2], CypherValue::Boolean(true));
    let result = rows(
        &mut db,
        "MATCH (:Loop {rank:1})-[r:R]->() \
        WITH r ORDER BY r.rank WITH collect(r) AS rs RETURN rs[0], rs[-1], rs[..]",
    );
    assert_ne!(
        result[0][0].as_relationship().unwrap().element_id,
        result[0][1].as_relationship().unwrap().element_id
    );
    assert_eq!(result[0][2], CypherValue::List(result[0][..2].to_vec()));
}

#[test]
fn empty_null_and_extreme_bounds_match_scalar_lists() {
    let mut db = graph();
    let null = CypherValue::Null;
    let empty = CypherValue::List(vec![]);
    for prefix in [
        "MATCH (n:Missing) WITH collect(n) AS ns",
        "OPTIONAL MATCH (n:Missing) WITH collect(n) AS ns",
        "MATCH p = (:Step {rank:1})-[:NEXT*0]->() WITH relationships(p) AS ns",
    ] {
        assert_eq!(
            rows(
                &mut db,
                &format!("{prefix} RETURN ns[0], ns[-1], ns[..], ns[-2..4]")
            ),
            [vec![
                null.clone(),
                null.clone(),
                empty.clone(),
                empty.clone()
            ]]
        );
    }
    let result = rows(
        &mut db,
        "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        RETURN ns[null], ns[null..2], ns[1..null], ns[1.5..2], ns[3..1], ns[0..0], \
               ns[-9223372036854775808], ns[9223372036854775807], \
               ns[-9223372036854775808..9223372036854775807], ns, ns[null..] IS NULL",
    );
    assert_eq!(
        result[0][..8],
        [
            null.clone(),
            null.clone(),
            null.clone(),
            null.clone(),
            empty.clone(),
            empty,
            null.clone(),
            null
        ]
    );
    assert_eq!(result[0][8], result[0][9]);
    assert_eq!(result[0][10], CypherValue::Boolean(true));
    // A nullable typed slice retains its type through aliases and UNWIND.
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH collect(n)[null..] AS ns \
        WITH ns AS renamed RETURN renamed, renamed[0], renamed[..], renamed IS NULL"
        ),
        [vec![
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Boolean(true)
        ]]
    );
    assert!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH collect(n)[null..] AS ns UNWIND ns AS n RETURN n"
        )
        .is_empty()
    );
}

#[test]
fn null_slices_and_indexes_survive_distinct_grouping_and_write_boundaries() {
    let mut db = graph();
    for projection in ["ns[null..]", "ns[99]"] {
        let query = format!(
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND [1,2] AS copy \
            WITH DISTINCT {projection} AS missing RETURN missing"
        );
        assert_eq!(rows(&mut db, &query), [vec![CypherValue::Null]]);
        let query = format!(
            "MATCH (n:Step) WITH collect(n) AS ns UNWIND [1,2] AS copy \
            RETURN {projection} AS missing, count(*)"
        );
        assert_eq!(
            rows(&mut db, &query),
            [vec![CypherValue::Null, CypherValue::Integer(2)]]
        );
    }
    assert_eq!(
        db.execute(
            "CREATE (n:Empty) WITH collect(n)[null..] AS ns \
        CREATE (:Marker) RETURN ns, ns[0], ns[..]"
        )
        .unwrap()
        .rows,
        [vec![
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null
        ]]
    );
}

#[test]
fn indexed_entities_support_properties_functions_predicates_and_rebinding() {
    let mut db = graph();
    let result = rows(
        &mut db,
        "MATCH p = (:Step {rank:1})-[:NEXT*2]->() \
        RETURN nodes(p)[0].rank, nodes(p)[0]['rank'], nodes(p)[0][42], \
               labels(nodes(p)[0]), type(relationships(p)[0]), properties(nodes(p)[0]), \
               keys(nodes(p)[0]), elementId(nodes(p)[0]) = elementId(nodes(p)[0]), \
               nodes(p)[0]:Step, relationships(p)[0]:NEXT, nodes(p)[99] IS NULL, \
               properties(nodes(p)[99]), nodes(p)[0] = relationships(p)[0]",
    );
    assert_eq!(
        result[0][0..5],
        [
            CypherValue::Integer(1),
            CypherValue::Integer(1),
            CypherValue::Null,
            CypherValue::List(vec![CypherValue::String("Step".into())]),
            CypherValue::String("NEXT".into())
        ]
    );
    assert_eq!(
        result[0][5],
        CypherValue::Map([("rank".into(), CypherValue::Integer(1))].into())
    );
    assert_eq!(
        result[0][6],
        CypherValue::List(vec![CypherValue::String("rank".into())])
    );
    assert_eq!(
        result[0][7..],
        [
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            CypherValue::Null,
            CypherValue::Boolean(false)
        ]
    );
    for distinct in ["", "DISTINCT "] {
        let query = format!(
            "MATCH p = (:Step {{rank:1}})-[:NEXT*2]->() \
            WITH {distinct}nodes(p)[0] AS n, relationships(p)[0] AS r \
            WHERE n.rank = 1 MATCH (n)-[r]->(next) RETURN n.rank, next.rank"
        );
        assert_eq!(
            rows(&mut db, &query),
            [vec![CypherValue::Integer(1), CypherValue::Integer(2)]]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        WITH ns[0] AS first, ns[-1] AS last, ns[1..] AS rest \
        UNWIND rest AS n RETURN first.rank, last.rank, n.rank ORDER BY n.rank"
        ),
        [
            vec![
                CypherValue::Integer(1),
                CypherValue::Integer(3),
                CypherValue::Integer(2)
            ],
            vec![
                CypherValue::Integer(1),
                CypherValue::Integer(3),
                CypherValue::Integer(3)
            ]
        ]
    );
}

#[test]
fn aggregate_indexing_grouping_distinct_union_and_list_helpers_keep_graph_types() {
    let mut db = graph();
    let result = rows(
        &mut db,
        "MATCH (n:Step) WITH n ORDER BY n.rank \
        RETURN collect(n)[0], collect(n)[-1].rank, collect(n)[1..]",
    );
    assert_eq!(
        result[0][0].as_node().unwrap().properties["rank"],
        CypherValue::Integer(1)
    );
    assert_eq!(result[0][1], CypherValue::Integer(3));
    assert!(
        matches!(&result[0][2], CypherValue::List(ns) if ns.len() == 2 && ns[0].as_node().is_some())
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n)[0] AS n RETURN n.rank"
        ),
        [vec![CypherValue::Integer(1)]]
    );
    let result = rows(
        &mut db,
        "MATCH p = (:Step {rank:1})-[:NEXT*2]->() \
        UNWIND [1,2] AS copy RETURN nodes(p)[0], count(nodes(p)[0]), collect(DISTINCT nodes(p)[0])",
    );
    assert_eq!(result[0][1], CypherValue::Integer(2));
    assert_eq!(result[0][2], CypherValue::List(vec![result[0][0].clone()]));
    let result = rows(
        &mut db,
        "MATCH (n:Step {rank:1}) RETURN collect(n) AS ns UNION \
        MATCH p = (:Step {rank:1})-[:NEXT*2]->() RETURN nodes(p)[..1] AS ns",
    );
    assert_eq!(result.len(), 1);
    let result = rows(
        &mut db,
        "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        RETURN head(ns), last(ns), tail(ns), ns[0], ns[-1], ns[1..]",
    );
    assert_eq!(result[0][..3], result[0][3..]);
    let result = rows(
        &mut db,
        "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns UNWIND [1,2] AS copy \
        WITH DISTINCT ns[..1] AS first UNWIND first AS n RETURN n.rank",
    );
    assert_eq!(result, [vec![CypherValue::Integer(1)]]);
}

#[test]
fn parameters_and_invalid_indexes_preserve_error_and_rollback_contracts() {
    use crabnet::{Error, Parameters};
    use serde_json::json;
    let mut db = graph();
    let query = "MATCH p = (:Step {rank:1})-[:NEXT*2]->() RETURN nodes(p)[$i].rank, relationships(p)[$lo..$hi]";
    let params: Parameters = [
        ("i".into(), json!(-1)),
        ("lo".into(), json!(0)),
        ("hi".into(), json!(1)),
    ]
    .into();
    let eager = db.execute_with(query, &params).unwrap();
    assert_eq!(eager.rows[0][0], CypherValue::Integer(3));
    {
        let mut read = db.prepare_read_with(query, &params).unwrap();
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            eager.rows
        );
    }
    assert!(matches!(db.execute(query), Err(Error::MissingParameter(_))));
    for index in ["'x'", "1.0", "true", "[]", "{}"] {
        for base in ["nodes(p)", "relationships(p)"] {
            let query =
                format!("MATCH p = (:Step {{rank:1}})-[:NEXT*2]->() RETURN {base}[{index}]");
            assert!(matches!(db.execute(&query), Err(Error::Type(_))), "{query}");
        }
    }
    for query in [
        "MATCH (n:Step) WITH collect(n) AS ns RETURN ns[0] + 1",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN [ns[0]]",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN {n: ns[0]}",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN ns[0..1] + []",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN collect(ns[..1])",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN [x IN ns[..1] | {entity:x}]",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN ns.id",
        "MATCH (n:Step) WITH collect(n) AS ns RETURN size(ns[0])",
    ] {
        assert!(
            matches!(
                db.execute(query),
                Err(Error::Semantic(_) | Error::Unsupported(_))
            ),
            "{query}"
        );
    }
    let error = db
        .execute("CREATE (n:Rollback) WITH collect(n) AS ns RETURN ns['bad']")
        .unwrap_err();
    assert!(matches!(error, Error::Type(_)), "{error:?}");
    assert_eq!(
        rows(&mut db, "MATCH (n:Rollback) RETURN count(n)"),
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn selected_entities_and_slices_track_mutations_and_delete_snapshots() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        WITH ns, ns[0] AS n, ns[..2] AS selected \
        SET n.rank = 9 SET n:Changed WITH ns, n, selected \
        RETURN ns[0], selected[0], n, ns[1].rank",
        )
        .unwrap();
    assert_eq!(result.rows[0][0], result.rows[0][1]);
    assert_eq!(result.rows[0][0], result.rows[0][2]);
    assert_eq!(
        result.rows[0][0].as_node().unwrap().properties["rank"],
        CypherValue::Integer(9)
    );
    assert_eq!(result.rows[0][3], CypherValue::Integer(2));
    let result = db
        .execute(
            "MATCH ()-[r:NEXT]->() WITH r ORDER BY r.rank WITH collect(r) AS rs \
        WITH rs, rs[0] AS r DELETE r RETURN rs[0], r, rs[..1]",
        )
        .unwrap();
    assert_eq!(result.rows[0][0], result.rows[0][1]);
    assert_eq!(
        result.rows[0][2],
        CypherValue::List(vec![result.rows[0][0].clone()])
    );
    db.execute(
        "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        CREATE (:Copy {rank:ns[0].rank})",
    )
    .unwrap();
    assert_eq!(
        rows(&mut db, "MATCH (n:Copy) RETURN n.rank"),
        [vec![CypherValue::Integer(2)]]
    );
    for query in [
        "MATCH (n:Step) WITH collect(n) AS ns CREATE (:Bad {value: ns[0]})",
        "MATCH (n:Step) WITH collect(n) AS ns CREATE (:Bad {value: ns[..1]})",
    ] {
        assert!(
            matches!(
                db.execute(query),
                Err(crabnet::Error::Semantic(_) | crabnet::Error::Unsupported(_))
            ),
            "{query}"
        );
    }
}

#[test]
fn cursor_index_errors_are_partial_and_distinct_pagination_keeps_typed_bags() {
    let mut db = graph();
    let query = "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
        UNWIND [0,1] AS position \
        RETURN ns[CASE WHEN position = 0 THEN 0 ELSE 'bad' END]";
    assert!(matches!(db.execute(query), Err(crabnet::Error::Type(_))));
    {
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), crabnet::ReadMode::Incremental);
        let mut cursor = read.cursor().unwrap();
        let first = cursor.next().unwrap().unwrap();
        assert_eq!(
            first[0].as_node().unwrap().properties["rank"],
            CypherValue::Integer(1)
        );
        assert!(matches!(cursor.next(), Some(Err(crabnet::Error::Type(_)))));
        assert!(cursor.next().is_none());
    }
    let result = rows(
        &mut db,
        "MATCH (n:Step) UNWIND [1,2] AS copy \
        WITH n ORDER BY n.rank WITH collect(n) AS ns UNWIND [0,1,2,3,4,5] AS i \
        RETURN DISTINCT ns[i] AS n, ns[i].rank AS rank ORDER BY rank SKIP 1 LIMIT 1",
    );
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0][0].as_node().unwrap().properties["rank"],
        CypherValue::Integer(2)
    );
    assert!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH collect(n)[99] AS n MATCH (n) RETURN n"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH n ORDER BY n.rank \
        WITH DISTINCT collect(n)[0] AS first WHERE first.rank = 1 RETURN first.rank"
        ),
        [vec![CypherValue::Integer(1)]]
    );
}
