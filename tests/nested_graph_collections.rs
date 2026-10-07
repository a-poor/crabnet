use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn graph() -> Database {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Step {rank:1}), (b:Step {rank:2}), (c:Step {rank:3}), (a)-[:NEXT {weight:10}]->(b), (b)-[:NEXT {weight:20}]->(c)").unwrap();
    db
}

fn rows(db: &mut Database, query: &str) -> Vec<Vec<CypherValue>> {
    let eager = db
        .execute(query)
        .unwrap_or_else(|error| panic!("{query}: {error:?}"));
    let mut read = db.prepare_read(query).unwrap();
    let streamed = read
        .cursor()
        .unwrap()
        .collect::<crabnet::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(streamed, eager.rows, "{query}");
    eager.rows
}

#[test]
fn nested_lists_and_maps_preserve_graph_types_and_plain_maps() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (a:Person {name:'Ada'})-[r:KNOWS {weight:2}]->(b:Person {name:'Bob'})")
        .unwrap();
    let result = db.execute(
        "MATCH (a:Person {name:'Ada'})-[r:KNOWS]->(b) \
         RETURN a, r, [a, [r, null], {friend:b, ordinary:{id:'fake', labels:['Person']}}] AS nested"
    ).unwrap();
    let row = &result.rows[0];
    let nested = row[2].as_list().unwrap();
    assert_eq!(nested[0], row[0]);
    assert_eq!(
        nested[1],
        CypherValue::List(vec![row[1].clone(), CypherValue::Null])
    );
    let map = nested[2].as_map().unwrap();
    assert_eq!(
        map["friend"].as_node().unwrap().properties["name"].as_str(),
        Some("Bob")
    );
    assert!(map["ordinary"].as_map().is_some());
}

#[test]
fn aliases_indexes_slices_unwind_and_scoped_members_keep_nested_types() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
         WITH {groups:[ns, [ns[0],null,ns[0]]], first:ns[0], empty:[]} AS box \
         RETURN box.first.rank, box.groups[0][1].rank, box.groups[1][1], \
                size(box.groups), box.empty, \
                [group IN box.groups | [member IN group | member.rank]], \
                all(group IN box.groups WHERE any(member IN group WHERE member.rank = 1))"
        ),
        [vec![
            CypherValue::Integer(1),
            CypherValue::Integer(2),
            CypherValue::Null,
            CypherValue::Integer(2),
            CypherValue::List(vec![]),
            CypherValue::List(vec![
                CypherValue::List(vec![
                    CypherValue::Integer(1),
                    CypherValue::Integer(2),
                    CypherValue::Integer(3)
                ]),
                CypherValue::List(vec![
                    CypherValue::Integer(1),
                    CypherValue::Null,
                    CypherValue::Integer(1)
                ])
            ]),
            CypherValue::Boolean(true)
        ]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH [[n,null,n]] AS groups \
         UNWIND groups AS members UNWIND members[0..] AS member \
         RETURN member.rank"
        ),
        [
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Null],
            vec![CypherValue::Integer(1)]
        ]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH {n:n} AS box WITH box.n AS member \
         MATCH (member)-[:NEXT]->(other) RETURN other.rank"
        ),
        [vec![CypherValue::Integer(2)]]
    );
}

#[test]
fn nested_comprehensions_project_entities_collections_and_paths() {
    let mut db = graph();
    for pattern in [
        "p = (:Step {rank:1})-[:NEXT*2]->()",
        "p = (:Step {rank:1})-[:NEXT*1..40]->(:Step {rank:3})",
        "p = (:Step {rank:1})-[:NEXT*]->(:Step {rank:3})",
        "p = shortestPath((:Step {rank:1})-[:NEXT*]->(:Step {rank:3}))",
    ] {
        let query = format!(
            "MATCH {pattern} \
            WITH {{path:p, members:[n IN nodes(p) | {{node:n, edges:relationships(p)}}]}} AS box \
            RETURN box.path, length(box.path), [n IN nodes(box.path) | n.rank], \
                   [entry IN box.members | entry.node.rank], \
                   [entry IN box.members | [r IN entry.edges | r.weight]], \
                   [entry IN box.members | any(n IN [entry.node] WHERE (n)-->() )]"
        );
        let result = rows(&mut db, &query);
        assert_eq!(result[0][0].as_path().unwrap().nodes.len(), 3);
        assert_eq!(result[0][1], CypherValue::Integer(2));
        assert_eq!(
            result[0][2],
            CypherValue::List(vec![
                CypherValue::Integer(1),
                CypherValue::Integer(2),
                CypherValue::Integer(3)
            ])
        );
        assert_eq!(result[0][3], result[0][2]);
        assert_eq!(
            result[0][4],
            CypherValue::List(vec![
                CypherValue::List(vec![
                    CypherValue::Integer(10),
                    CypherValue::Integer(20)
                ]);
                3
            ])
        );
        assert_eq!(
            result[0][5],
            CypherValue::List(vec![
                CypherValue::Boolean(true),
                CypherValue::Boolean(true),
                CypherValue::Boolean(false)
            ])
        );
        let collected = rows(
            &mut db,
            &format!(
                "MATCH {pattern} WITH {{path:p}} AS box WITH collect(box) AS boxes \
             UNWIND boxes AS box RETURN box.path, length(box.path)"
            ),
        );
        assert_eq!(collected[0], result[0][..2]);
    }
}

#[test]
fn nested_values_follow_mutation_refresh_and_preserve_deleted_snapshots() {
    let mut db = graph();
    let result = db
        .execute(
            "MATCH (n:Step {rank:1})-[r:NEXT]->() WITH n,r,{copies:[[n,n]], edge:r} AS box \
         SET n.rank=9, n:Changed, r.weight=50 DETACH DELETE n \
         RETURN box, box.copies[0][0].rank, box.edge.weight",
        )
        .unwrap();
    assert_eq!(result.rows[0][1], CypherValue::Integer(9));
    assert_eq!(result.rows[0][2], CypherValue::Integer(50));
    let map = result.rows[0][0].as_map().unwrap();
    let copies = map["copies"].as_list().unwrap()[0].as_list().unwrap();
    assert_eq!(copies[0], copies[1]);
    assert!(
        copies[0]
            .as_node()
            .unwrap()
            .labels
            .iter()
            .any(|s| s == "Changed")
    );
    assert!(matches!(
        db.execute("CREATE (n:Bad) WITH {nodes:[n]} AS box CREATE (:Bad {value:box})"),
        Err(Error::Semantic(_))
    ));
    assert_eq!(
        rows(&mut db, "MATCH (n:Bad) RETURN count(n)"),
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn mixed_selection_and_parameter_maps_cannot_impersonate_graph_values() {
    let mut db = graph();
    let query = "MATCH (n:Step {rank:1}) RETURN [n, $fake], {node:n, user:$fake}";
    let params: Parameters = [(
        "fake".into(),
        json!(["n",{"id":"fake","labels":[],"props":{}}]),
    )]
    .into();
    let result = db.execute_with(query, &params).unwrap();
    let list = result.rows[0][0].as_list().unwrap();
    assert!(list[0].as_node().is_some());
    assert!(list[1].as_list().is_some());
    assert_eq!(result.rows[0][1].as_map().unwrap()["user"], list[1]);
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1})-[r:NEXT]->() WITH [n,r,{rank:3},null] AS values \
         RETURN [v IN values | v.rank], values[99], values[-1], \
                [v IN values[..2] | elementId(v)]"
        )[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::Null,
            CypherValue::Integer(3),
            CypherValue::Null
        ])
    );
}

#[test]
fn aggregation_distinct_grouping_equality_and_union_use_recursive_identity() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) UNWIND [1,2] AS copy WITH {node:n, value:copy/1.0} AS box \
         RETURN count(box), count(DISTINCT box), collect(box)"
        )[0][0..2],
        [CypherValue::Integer(2), CypherValue::Integer(2)]
    );
    let result = rows(
        &mut db,
        "MATCH (n:Step {rank:1}) UNWIND [1,1.0] AS number WITH {n:n,value:number} AS box \
         RETURN box, count(*) AS copies",
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][1], CypherValue::Integer(2));
    assert!(result[0][0].as_map().unwrap()["n"].as_node().is_some());
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) UNWIND [1,1.0] AS number WITH DISTINCT {n:n,value:number} AS box \
         RETURN count(*)"
        ),
        [vec![CypherValue::Integer(1)]]
    );
    let result = rows(
        &mut db,
        "MATCH (n:Step) WITH n ORDER BY n.rank WITH collect(n) AS ns \
         UNWIND [1,2] AS copy RETURN collect(DISTINCT ns), collect({nodes:ns}), count(DISTINCT {nodes:ns})",
    );
    assert_eq!(result[0][0].as_list().unwrap().len(), 1);
    assert_eq!(
        result[0][0].as_list().unwrap()[0].as_list().unwrap().len(),
        3
    );
    assert_eq!(result[0][1].as_list().unwrap().len(), 2);
    assert_eq!(result[0][2], CypherValue::Integer(1));
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1})-[r:NEXT]->() RETURN {n:n,x:[1]} = {x:[1.0],n:n}, \
         [n,null] = [n,null], [n,1] <> [n,2], n IN [n,r], n IN [r,null], \
         {n:n} IN [{n:n}], [n] = [{id:elementId(n)}]"
        ),
        [vec![
            CypherValue::Boolean(true),
            CypherValue::Null,
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            CypherValue::Null,
            CypherValue::Boolean(true),
            CypherValue::Boolean(false)
        ]]
    );
    let branch = "MATCH (n:Step {rank:1}) RETURN {n:n, nested:[[n,null]]} AS value";
    assert_eq!(rows(&mut db, &format!("{branch} UNION {branch}")).len(), 1);
    assert_eq!(
        rows(&mut db, &format!("{branch} UNION ALL {branch}")).len(),
        2
    );
}

#[test]
fn null_empty_and_missing_members_keep_their_collection_semantics() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "OPTIONAL MATCH (n:Absent) WITH {n:n, values:[[n]], empty:[]} AS box \
         RETURN box.n, box.values[0][0], box.missing, box.values[99], \
                box.values[null..], size(box.empty), [x IN box.empty | [x]], \
                all(x IN box.values[0] WHERE x IS NULL), box.values IS NULL"
        ),
        [vec![
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Integer(0),
            CypherValue::List(vec![]),
            CypherValue::Boolean(true),
            CypherValue::Boolean(false)
        ]]
    );
    assert_eq!(
        rows(&mut db, "MATCH (n:Absent) RETURN collect({node:n})"),
        [vec![CypherValue::List(vec![])]]
    );
}

#[test]
fn mixed_member_errors_are_structured_partial_and_roll_back_prior_writes() {
    let mut db = graph();
    let query =
        "MATCH (n:Step {rank:1}) WITH [n,42] AS values UNWIND [0,1] AS i RETURN values[i].rank";
    assert!(matches!(db.execute(query), Err(Error::Type(_))));
    {
        let mut read = db.prepare_read(query).unwrap();
        let mut cursor = read.cursor().unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap(),
            vec![CypherValue::Integer(1)]
        );
        assert!(matches!(cursor.next().unwrap(), Err(Error::Type(_))));
        assert!(cursor.next().is_none());
    }
    assert!(matches!(db.execute(
        "CREATE (n:Rollback {rank:1}) WITH [n,42] AS values UNWIND [0,1] AS i RETURN values[i].rank"
    ), Err(Error::Type(_))));
    assert_eq!(
        rows(&mut db, "MATCH (n:Rollback) RETURN count(n)"),
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn nested_collection_depth_is_checked_before_writes_and_transport_round_trips() {
    let mut db = graph();
    let mut property = json!(0);
    for _ in 0..100 {
        property = json!([property]);
    }
    db.execute_with(
        "MATCH (n:Step {rank:1}) SET n.deep=$value",
        &[("value".into(), property)].into(),
    )
    .unwrap();
    for depth in [1, 30, 60, 100] {
        let expression = format!("{}n{}", "[".repeat(depth), "]".repeat(depth));
        let query = format!("MATCH (n:Step {{rank:1}}) RETURN {expression}");
        crabnet::explain(&query).unwrap();
        let result = rows(&mut db, &query);
        let mut value = &result[0][0];
        for _ in 0..depth {
            value = &value.as_list().unwrap()[0];
        }
        assert!(value.as_node().is_some());
        if depth == 100 {
            let changed = db
                .execute(&format!(
                    "MATCH (n:Step {{rank:1}}) WITH n,{expression} AS box \
                 SET n.updated=true RETURN box"
                ))
                .unwrap();
            let mut value = &changed.rows[0][0];
            for _ in 0..depth {
                value = &value.as_list().unwrap()[0];
            }
            assert_eq!(
                value.as_node().unwrap().properties["updated"],
                CypherValue::Boolean(true)
            );
        }
    }
    let expression = format!("{}n{}", "[".repeat(101), "]".repeat(101));
    assert!(matches!(
        db.execute(&format!("CREATE (n:TooDeep) RETURN {expression}")),
        Err(Error::Semantic(_))
    ));
    let first = format!("{}n{}", "[".repeat(50), "]".repeat(50));
    let second = format!("{}box{}", "[".repeat(51), "]".repeat(51));
    assert!(matches!(
        db.execute(&format!(
            "CREATE (n:TooDeep) WITH {first} AS box RETURN {second}"
        )),
        Err(Error::Type(_))
    ));
    assert_eq!(
        rows(&mut db, "MATCH (n:TooDeep) RETURN count(n)"),
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn mixed_members_support_predicates_dynamic_keys_and_scalar_property_writes() {
    let mut db = graph();
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:1}) WITH [n,{rank:2},null] AS items \
         RETURN [x IN items WHERE x.rank > 0 | x.rank + 1], \
                any(x IN items WHERE x.rank = 2), \
                [x IN items WHERE x IS NOT NULL | x['rank']]"
        ),
        [vec![
            CypherValue::List(vec![CypherValue::Integer(2), CypherValue::Integer(3)]),
            CypherValue::Boolean(true),
            CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Integer(2)])
        ]]
    );
    assert_eq!(
        db.execute(
            "MATCH (n:Step {rank:1}) WITH n,{node:n, scalar:7} AS box \
         SET n.copied=box.node.rank, n.scalar=box.scalar \
         RETURN n.copied, n.scalar"
        )
        .unwrap()
        .rows,
        [vec![CypherValue::Integer(1), CypherValue::Integer(7)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "MATCH (n:Step {rank:2}) WITH [n,{rank:3,absent:null}] AS items \
         RETURN [x IN items | properties(x)], [x IN items | keys(x)], \
                [x IN items[..1] | x[0]], [x IN items[..1] | x:Step]"
        ),
        [vec![
            CypherValue::try_from(json!([{"rank":2},{"rank":3,"absent":null}])).unwrap(),
            CypherValue::try_from(json!([["rank"], ["absent", "rank"]])).unwrap(),
            CypherValue::List(vec![CypherValue::Null]),
            CypherValue::List(vec![CypherValue::Boolean(true)])
        ]]
    );
}

#[test]
fn known_path_and_entity_kind_errors_remain_planning_errors_after_nesting() {
    let mut db = graph();
    for expression in [
        "size(box.path)",
        "box.path[0]",
        "box.path[..1]",
        "box.path.key",
        "head(box.path)",
        "[x IN box.path | x]",
        "all(x IN box.path WHERE true)",
        "labels(box)",
        "length(box)",
        "box:Step",
    ] {
        let query = format!(
            "MATCH p = (:Step {{rank:1}})-[:NEXT]->() \
            WITH {{path:p}} AS box RETURN {expression}"
        );
        assert!(
            matches!(db.execute(&query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
    assert!(matches!(
        db.execute(
            "MATCH p = (:Step {rank:1})-[:NEXT]->() \
        WITH {path:p} AS box UNWIND box.path AS x RETURN x"
        ),
        Err(Error::Semantic(_))
    ));
}
