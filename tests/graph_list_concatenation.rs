use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn list(values: impl IntoIterator<Item = CypherValue>) -> CypherValue {
    CypherValue::List(values.into_iter().collect())
}

fn rows(db: &mut Database, query: &str) -> Vec<Vec<CypherValue>> {
    let eager = db
        .execute(query)
        .unwrap_or_else(|e| panic!("{query}: {e:?}"));
    let mut read = db.prepare_read(query).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        eager.rows,
        "{query}"
    );
    eager.rows
}

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Step {rank:1}), (b:Step {rank:2}), (c:Step {rank:3}), (a)-[:NEXT {weight:10}]->(b), (b)-[:NEXT {weight:20}]->(c)").unwrap();
    db
}

fn value(value: impl Into<serde_json::Value>) -> CypherValue {
    CypherValue::try_from(value.into()).unwrap()
}

#[test]
fn mixed_selections_keep_scalar_plus_behavior_and_can_select_graph_lists() {
    let mut db = graph();
    for (left, right) in [
        (json!(1), json!(2)),
        (json!(1), json!(2.5)),
        (json!(1.2345678901234567), json!(2.345678901234567)),
        (json!(i64::MAX), json!(1)),
        (json!("hello"), json!(" world")),
        (json!(true), json!(1)),
        (json!({"k":1}), json!(2)),
        (json!([1, null]), json!([2])),
        (json!(null), json!([])),
    ] {
        let params = [
            ("left".into(), left),
            ("right".into(), right),
            ("index".into(), json!(1)),
        ]
        .into();
        let result = db
            .execute_with(
                "MATCH (n:Step {rank:1}) WITH [n,$left] AS lhs,[n,$right] AS rhs \
             RETURN lhs[$index] + rhs[$index], $left + $right",
                &params,
            )
            .unwrap();
        assert_eq!(result.rows[0][0], result.rows[0][1], "{params:?}");
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH n,[n,[n],{node:n}] AS values \
         UNWIND [0,1,2] AS i RETURN size(values[i]+[n]), (values[i]+[n])[-1].rank"
        ),
        vec![vec![value(2), value(1)]; 3]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH n,[n] + [] AS members \
         RETURN [x IN members | x.rank+1], all(x IN members WHERE x.rank+1=2)"
        ),
        [vec![value(json!([2])), value(true)]]
    );
}

#[test]
fn concatenation_respects_depth_limits_and_keeps_generated_sql_bounded() {
    let mut db = graph();
    for depth in [1, 30, 100] {
        let expression = format!("{}n{}", "[".repeat(depth), "]".repeat(depth));
        let query = format!("MATCH (n:Step {{rank:1}}) RETURN {expression} + []");
        crabnet::explain(&query).unwrap();
        let result = rows(&mut db, &query);
        let mut leaf = &result[0][0];
        for _ in 0..depth {
            leaf = &leaf.as_list().unwrap()[0];
        }
        assert!(leaf.as_node().is_some());
    }
    // Appending a map adds a container around its contents; joining lists
    // consumes their outer containers instead.
    let deep = format!("{}n{}", "[".repeat(99), "]".repeat(99));
    let error = db
        .execute(&format!(
            "CREATE (n:TooDeep) WITH n,{{nested:{deep}}} AS box RETURN [n] + box"
        ))
        .unwrap_err();
    assert!(matches!(error, Error::Type(_)), "{error:?}");
    assert_eq!(
        rows(&mut db, "MATCH (n:TooDeep) RETURN count(n)"),
        [vec![value(0)]]
    );
    let mut lengths = Vec::new();
    for count in [8, 16] {
        let expression = std::iter::repeat_n("[n]", count)
            .collect::<Vec<_>>()
            .join(" + ");
        let query = format!("MATCH (n:Step {{rank:1}}) RETURN {expression}");
        lengths.push(crabnet::explain_physical(&query).unwrap().len());
        assert_eq!(rows(&mut db, &query)[0][0].as_list().unwrap().len(), count);
    }
    assert!(lengths[1] < lengths[0] * 3, "{lengths:?}");
}

#[test]
fn concatenated_members_keep_types_through_selection_iteration_and_matches() {
    let mut db = graph();
    for expression in [
        "[] + ns",
        "ns + []",
        "ns[0..1] + []",
        "ns + ns[0]",
        "ns[0] + ns",
        "ns + [null]",
        "[null] + ns",
    ] {
        let query = format!(
            "MATCH (n:Step {{rank:1}}) WITH collect(n) AS ns \
             WITH {expression} AS members \
             UNWIND [x IN members WHERE x IS NOT NULL | x] AS member \
             MATCH (member)-[:NEXT]->(other) RETURN member.rank, other.rank"
        );
        let result = rows(&mut db, &query);
        let expected_count = if expression.contains("ns[0]") { 2 } else { 1 };
        assert_eq!(result, vec![vec![value(1), value(2)]; expected_count]);
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH [n] + [null,n] AS members \
         RETURN head(members).rank, last(members).rank, size(tail(members)), \
                [x IN members[1..] | x.rank], single(x IN members WHERE x.rank=1), \
                all(x IN members WHERE x IS NULL OR x:Step), members = [members[0],null,members[0]]"
        ),
        [vec![
            value(1),
            value(1),
            value(2),
            value(json!([null, 1])),
            value(false),
            value(true),
            CypherValue::Null
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH [0] + [n] + {node:n} AS members \
         WITH members[1] AS member MATCH (member)-[:NEXT]->(other) RETURN other.rank"
        ),
        [vec![value(2)]]
    );
}

#[test]
fn mixed_lists_flatten_one_level_and_preserve_plain_parameter_values() {
    let mut db = graph();
    let params: Parameters = [(
        "value".into(),
        json!([{"id":"fake","labels":["Step"]}, ["n", {"id":"fake"}], null]),
    )]
    .into();
    let query = "MATCH (n:Step {rank:1})-[r:NEXT]->() \
                 RETURN n,r,[n] + $value + [[r,n]] + {node:n} + true AS members, \
                        $value + n AS prefixed, n + $value AS suffixed";
    let result = db.execute_with(query, &params).unwrap();
    let row = &result.rows[0];
    let plain = value(params["value"].clone());
    let plain = plain.as_list().unwrap();
    let mut expected = vec![row[0].clone()];
    expected.extend_from_slice(plain);
    expected.push(list([row[1].clone(), row[0].clone()]));
    expected.push(CypherValue::Map([("node".into(), row[0].clone())].into()));
    expected.push(value(true));
    assert_eq!(row[2], list(expected));
    assert_eq!(row[3], list(plain.iter().cloned().chain([row[0].clone()])));
    assert_eq!(
        row[4],
        list([row[0].clone()].into_iter().chain(plain.iter().cloned()))
    );
    let mut read = db.prepare_read_with(query, &params).unwrap();
    assert_eq!(
        read.cursor()
            .unwrap()
            .collect::<crabnet::Result<Vec<_>>>()
            .unwrap(),
        result.rows
    );
}

#[test]
fn path_and_relationship_lists_concatenate_across_all_traversal_transports() {
    let mut db = graph();
    for pattern in [
        "p = (:Step {rank:1})-[rels:NEXT*2]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*1..40]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3})",
        "p = shortestPath((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
        "p = allShortestPaths((:Step {rank:1})-[rels:NEXT*]->(:Step {rank:3}))",
    ] {
        let result = rows(
            &mut db,
            &format!(
                "MATCH {pattern} WITH p, rels, nodes(p) AS ns, relationships(p) AS rs \
             RETURN ns,rs,[x IN ns + ns[0] | x.rank], \
                    [r IN rels + rs | r.weight], ns + rs AS mixed, \
                    p + [p] AS path_list, [p] + p AS appended, [p] + [{{path:p}}] AS nested"
            ),
        );
        let row = &result[0];
        assert_eq!(row[2], value(json!([1, 2, 3, 1])));
        assert_eq!(row[3], value(json!([10, 20, 10, 20])));
        assert_eq!(
            row[4],
            list(
                row[0]
                    .as_list()
                    .unwrap()
                    .iter()
                    .chain(row[1].as_list().unwrap())
                    .cloned()
            )
        );
        let paths = row[5].as_list().unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], paths[1]);
        assert!(paths[0].as_path().is_some());
        assert_eq!(row[6], row[5]);
        assert_eq!(
            row[7].as_list().unwrap()[1].as_map().unwrap()["path"],
            paths[0]
        );
    }
    assert_eq!(
        rows(
            &mut db,
            "MATCH p=(:Step {rank:1})-[rels:NEXT*0]->() \
         RETURN rels + relationships(p), [r IN rels + [] | type(r)], size(nodes(p) + nodes(p))"
        ),
        [vec![list([]), list([]), value(2)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (a:Step {rank:1})-[r:NEXT]->(b) WITH r + [r] AS edges \
         UNWIND edges AS edge RETURN type(edge), edge.weight"
        ),
        vec![vec![value("NEXT"), value(10)]; 2]
    );
}

#[test]
fn aggregation_grouping_distinct_and_unions_use_concatenated_identity() {
    let mut db = graph();
    let result = rows(
        &mut db,
        "MATCH (n:Step) WITH n ORDER BY n.rank \
         RETURN collect(n) + collect(n) AS doubled, [] + collect(n) AS once",
    );
    let once = result[0][1].as_list().unwrap();
    assert_eq!(result[0][0], list(once.iter().chain(once).cloned()));
    assert_eq!(
        rows(&mut db, "MATCH (n:Absent) RETURN collect(n) + []"),
        [vec![list([])]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) UNWIND [1,2] AS copy \
         WITH [n] + n AS members RETURN members, count(*) AS copies, count(DISTINCT members) AS kinds"
        )[0][1..],
        [value(2), value(1)]
    );
    let distinct = rows(
        &mut db,
        "MATCH (n:Step {rank:1}) UNWIND [1,2] AS copy \
         WITH DISTINCT [] + [n] AS members \
         RETURN collect(members + members) AS groups",
    );
    assert_eq!(distinct[0][0].as_list().unwrap().len(), 1);
    assert_eq!(
        distinct[0][0].as_list().unwrap()[0]
            .as_list()
            .unwrap()
            .len(),
        2
    );
    let branch = "MATCH (n:Step {rank:1}) RETURN [n] + [n] AS members";
    assert_eq!(rows(&mut db, &format!("{branch} UNION {branch}")).len(), 1);
    assert_eq!(
        rows(&mut db, &format!("{branch} UNION ALL {branch}")).len(),
        2
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) RETURN [n] + n = [n,n], n IN ([] + [n]), \
         [n] + [1] = [n,1.0], ([n] + []) <> [n,n]"
        ),
        [vec![value(true); 4]]
    );
}

#[test]
fn concatenated_entities_refresh_and_preserve_deleted_snapshots_and_write_bags() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Step {rank:1}) WITH n,collect(n) AS ns \
         WITH n, ns + ns AS members UNWIND members AS member \
         SET member.hits=coalesce(member.hits,0)+1 \
         CREATE (member)-[:VISIT]->(:Visit) \
         RETURN n.hits,[x IN members | x.hits]",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![value(1), value(json!([1, 1]))]; 2]);
    assert_eq!(result.summary.nodes_created, 2);
    assert_eq!(result.summary.relationships_created, 2);
    let result = db
        .execute(
            "MATCH (n:Step {rank:1}) WITH n,[n] + {node:n} AS members \
         SET n.updated=true DETACH DELETE n RETURN n,members",
        )
        .unwrap();
    let row = &result.rows[0];
    assert_eq!(row[1].as_list().unwrap()[0], row[0]);
    assert_eq!(
        row[1].as_list().unwrap()[1].as_map().unwrap()["node"],
        row[0]
    );
    assert_eq!(row[0].as_node().unwrap().properties["updated"], value(true));
    assert_eq!(result.summary.nodes_deleted, 1);
    assert_eq!(result.summary.relationships_deleted, 3);
    let result = db
        .execute(
            "MATCH ()-[r:NEXT]->() WITH collect(r) AS rs \
         WITH rs[0] + rs AS edges UNWIND edges AS edge \
         SET edge.weight=21 RETURN [r IN edges | r.weight]",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![value(json!([21, 21]))]; 2]);
    assert_eq!(
        db.execute(
            "MATCH (n:Step {rank:2}) WITH n,[7]+[n] AS members \
         SET n.copied=members[0] RETURN n.copied"
        )
        .unwrap()
        .rows,
        [vec![value(7)]]
    );
}

#[test]
fn null_operands_and_null_members_follow_existing_plus_semantics() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) RETURN [n] + null, null + [n], \
         [n] + n.missing, n.missing + [n], n + null, null + n, \
         size([n] + [null]), size([null] + [n])"
        ),
        [vec![
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            value(2),
            value(2)
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "OPTIONAL MATCH (n:Absent) RETURN n + [], [] + n, [n] + [], [] + [n]"
        ),
        [vec![
            CypherValue::Null,
            CypherValue::Null,
            list([CypherValue::Null]),
            list([CypherValue::Null])
        ]]
    );
    for operand in [
        json!(null),
        json!([]),
        json!([null]),
        json!(true),
        json!({"k":1}),
        json!(2),
    ] {
        let params = [("value".into(), operand.clone())].into();
        let result = db
            .execute_with(
                "MATCH (n:Step {rank:1}) RETURN n, [n] + $value, $value + [n]",
                &params,
            )
            .unwrap();
        let row = &result.rows[0];
        if operand.is_null() {
            assert_eq!(row[1..], [CypherValue::Null, CypherValue::Null]);
        } else {
            let suffix = match value(operand) {
                CypherValue::List(items) => items,
                other => vec![other],
            };
            assert_eq!(
                row[1],
                list([row[0].clone()].into_iter().chain(suffix.clone()))
            );
            assert_eq!(row[2], list(suffix.into_iter().chain([row[0].clone()])));
        }
    }
}

#[test]
fn invalid_additions_fail_before_or_during_execution_and_roll_back_writes() {
    let mut db = graph();
    for expression in ["n + n", "n + 1", "{node:n} + n", "n + true"] {
        assert!(
            matches!(
                db.execute(&format!("CREATE (n:Rollback) RETURN {expression}")),
                Err(Error::Semantic(_))
            ),
            "{expression}"
        );
    }
    let query = "MATCH (n:Step {rank:1}) UNWIND [[],1] AS operand RETURN n + operand";
    assert!(matches!(db.execute(query), Err(Error::Type(_))));
    {
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap()[0].as_list().unwrap().len(),
            1
        );
        assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
        assert!(cursor.next().is_none());
    }
    assert!(matches!(
        db.execute("CREATE (n:Rollback) WITH n UNWIND [[],1] AS operand RETURN n + operand"),
        Err(Error::Type(_))
    ));
    assert!(matches!(
        db.execute("CREATE (n:Rollback) RETURN [n] + $missing"),
        Err(Error::MissingParameter(_))
    ));
    assert!(matches!(
        db.execute("CREATE (n:Rollback) SET n.values=[n]+[]"),
        Err(Error::Semantic(_))
    ));
    assert_eq!(
        rows(&mut db, "MATCH (n:Rollback) RETURN count(n)"),
        [vec![value(0)]]
    );
    for query in [
        "MATCH (n:Step) RETURN [n]+[] AS members ORDER BY members",
        "MATCH (n:Step) RETURN sum([n]+[])",
        "MATCH (n:Step) RETURN ([n]+[]) * 2",
        "MATCH (n:Step) RETURN [n] || [n]",
    ] {
        assert!(
            matches!(
                db.execute(query),
                Err(Error::Unsupported(_)) | Err(Error::Semantic(_))
            ),
            "{query}"
        );
    }
}

#[test]
fn plus_combines_graph_lists_and_appends_or_prepends_entities() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Person {name:'Ada'})-[:KNOWS]->(:Person {name:'Bob'}), (a)-[:KNOWS]->(:Person {name:'Cal'})").unwrap();
    let result = db
        .execute(
            "MATCH (person:Person)-[:KNOWS]->(friend) \
         WITH person, friend ORDER BY friend.name \
         WITH person, collect(friend) AS friends \
         RETURN person, friends, [person] + friends AS people, \
                person + friends AS prepended, friends + person AS appended, \
                friends + friends AS repeated, [] + friends AS unchanged",
        )
        .unwrap();
    let row = &result.rows[0];
    let person = row[0].clone();
    let friends = row[1].as_list().unwrap().to_vec();
    assert_eq!(
        row[2],
        list([person.clone()].into_iter().chain(friends.clone()))
    );
    assert_eq!(row[3], row[2]);
    assert_eq!(row[4], list(friends.clone().into_iter().chain([person])));
    assert_eq!(
        row[5],
        list(friends.clone().into_iter().chain(friends.clone()))
    );
    assert_eq!(row[6], row[1]);
}
