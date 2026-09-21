use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters, Value, explain};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn single(database: &mut Database, query: &str) -> Vec<CypherValue> {
    let result = database.execute(query).unwrap();
    assert_eq!(result.rows.len(), 1, "{query}");
    result.rows[0].clone()
}

fn int(value: i64) -> CypherValue {
    CypherValue::Integer(value)
}

fn list<const N: usize>(items: [CypherValue; N]) -> CypherValue {
    CypherValue::List(items.to_vec())
}

#[test]
fn size_counts_list_elements_and_string_characters() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "RETURN size([1, 2, 3]) AS a, size([]) AS b, size('héllo') AS c, size('') AS d, \
                size(null) AS e, size(1) AS f, size(true) AS g, size({a: 1}) AS h, \
                size([[1, 2], [3]]) AS i",
    );
    assert_eq!(
        row,
        [
            int(3),
            int(0),
            int(5),
            int(0),
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            int(2),
        ]
    );

    let result = database
        .execute_with(
            "RETURN size($list) AS j, size($text) AS k",
            &parameters([("list", json!([1, 2, 3, 4])), ("text", json!("ab"))]),
        )
        .unwrap();
    assert_eq!(result.rows, [vec![int(4), int(2)]]);

    // A stored list property and a projected null list.
    database
        .execute("CREATE (:Doc {tags: ['a', 'b', 'c'], name: 'doc'})")
        .unwrap();
    let row = single(
        &mut database,
        "MATCH (d:Doc) RETURN size(d.tags) AS tags, size(d.name) AS name, size(d.missing) AS missing",
    );
    assert_eq!(row, [int(3), int(3), CypherValue::Null]);
    let row = single(
        &mut database,
        "WITH null AS l RETURN size(l) AS a, size(null) AS b",
    );
    assert_eq!(row, [CypherValue::Null, CypherValue::Null]);

    // Size participates in predicates, ordering, and grouping.
    let result = database
        .execute(
            "UNWIND [[1, 2], [3], [], [4, 5, 6]] AS l \
             WITH l, size(l) AS s WHERE s > 0 \
             RETURN s, count(*) AS c ORDER BY s DESC",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![int(3), int(1)],
            vec![int(2), int(1)],
            vec![int(1), int(1)]
        ]
    );
}

#[test]
fn size_rejects_paths_and_entities_with_semantic_errors() {
    let mut database = Database::in_memory().unwrap();
    let error = database
        .execute("MATCH p = (a)-[*]->(b) RETURN size(p)")
        .unwrap_err();
    assert!(
        matches!(&error, Error::Semantic(message) if message.contains("not a path")),
        "{error:?}"
    );
    let error = database.execute("MATCH (n) RETURN size(n)").unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
    let error = database.execute("RETURN size(1, 2)").unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
}

#[test]
fn list_comprehensions_filter_and_project_in_list_order() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "RETURN [x IN [1, 2, 3, 4] WHERE x > 2 | x * 10] AS a, \
                [x IN [3, 1, 2] WHERE x > 1] AS b, \
                [x IN [1, 2] | x + 0.5] AS c, \
                [x IN [1, [2], {a: 3}, 's', null, true] | x] AS d, \
                [x IN [] | x] AS e, \
                [x IN null | x] AS f, \
                [x IN 'abc' | x] AS g, \
                [x IN [1, 2, null] WHERE x > 1 | x] AS h, \
                [x IN [1, 2, 3] WHERE x <> 2 | null] AS i",
    );
    assert_eq!(
        row,
        [
            list([int(30), int(40)]),
            list([int(3), int(2)]),
            list([CypherValue::Float(1.5), CypherValue::Float(2.5)]),
            list([
                int(1),
                list([int(2)]),
                CypherValue::Map(BTreeMap::from([("a".to_owned(), int(3))])),
                CypherValue::String("s".to_owned()),
                CypherValue::Null,
                CypherValue::Boolean(true),
            ]),
            list([]),
            CypherValue::Null,
            CypherValue::Null,
            list([int(2)]),
            list([CypherValue::Null, CypherValue::Null]),
        ]
    );
}

#[test]
fn list_comprehensions_nest_shadow_and_compose_with_other_list_operations() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "WITH 5 AS x, [1, 2] AS list \
         RETURN [x IN list | x + 1] AS shadowed, x AS outer, \
                [x IN list WHERE x > 1 | [y IN [x, x + 1] | y * 2]] AS nested, \
                size([x IN list WHERE x > 1]) AS counted, \
                all(x IN [m IN [{a: 1}, {a: 2}] | m.a] WHERE x > 0) AS quantified, \
                [m IN [{a: 1}, {a: 2}] WHERE m.a > 1 | m.a] AS keyed, \
                3 IN [x IN list | x + 1] AS member",
    );
    assert_eq!(
        row,
        [
            list([int(2), int(3)]),
            int(5),
            list([list([int(4), int(6)])]),
            int(1),
            CypherValue::Boolean(true),
            list([int(2)]),
            CypherValue::Boolean(true),
        ]
    );

    database
        .execute("CREATE (:Person {name: 'Ann', scores: [3, 9, 5]}), (:Person {name: 'Bob', scores: [1]})")
        .unwrap();
    let result = database
        .execute(
            "MATCH (p:Person) \
             WHERE size([s IN p.scores WHERE s > 4]) > 0 \
             RETURN p.name AS name, [s IN p.scores WHERE s > 4 | s * 2] AS high \
             ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("Ann".to_owned()),
            list([int(18), int(10)])
        ]]
    );
    let result = database
        .execute_with(
            "RETURN [x IN $list WHERE x > $min | x] AS kept",
            &parameters([("list", json!([1, 5, 10])), ("min", json!(2))]),
        )
        .unwrap();
    assert_eq!(result.rows, [vec![list([int(5), int(10)])]]);
}

#[test]
fn list_comprehensions_reject_aggregates_and_render_in_plans() {
    let mut database = Database::in_memory().unwrap();
    let error = database
        .execute("RETURN [x IN [1, 2, 3] | count(*)] AS bad")
        .unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
    let error = database
        .execute("RETURN [x IN [1, 2, 3] WHERE count(*) > 0] AS bad")
        .unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
    let error = database
        .execute("RETURN [x IN [1, 2] | y] AS bad")
        .unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");

    let plan = explain("RETURN size([x IN [1, 2] WHERE x > 1 | x + 1]) AS n").unwrap();
    assert!(
        plan.contains(
            "size([#l0 in list[literal(1), literal(2)] \
             where greater-than(#l0, literal(1)) | add(#l0, literal(1))])"
        ),
        "{plan}"
    );
}

#[test]
fn computed_list_and_map_literals_are_built_at_runtime() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "WITH 1 AS a \
         RETURN [a, a + 1, 'x', null, [a], {k: a}] AS l, \
                {n: a * 2, s: 'v', m: {d: a}, z: null} AS m, \
                {n: a}.n AS n, \
                [a, a] = [1, 1] AS eq, [a] IN [[1]] AS member, {k: a} = {k: 1} AS meq, \
                size([a, a]) AS s",
    );
    assert_eq!(
        row,
        [
            list([
                int(1),
                int(2),
                CypherValue::String("x".to_owned()),
                CypherValue::Null,
                list([int(1)]),
                CypherValue::Map(BTreeMap::from([("k".to_owned(), int(1))])),
            ]),
            CypherValue::Map(BTreeMap::from([
                ("n".to_owned(), int(2)),
                ("s".to_owned(), CypherValue::String("v".to_owned())),
                (
                    "m".to_owned(),
                    CypherValue::Map(BTreeMap::from([("d".to_owned(), int(1))]))
                ),
                ("z".to_owned(), CypherValue::Null),
            ])),
            int(1),
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            int(2),
        ]
    );

    database.execute("CREATE (:Item {a: 1, b: 'two'})").unwrap();
    let row = single(
        &mut database,
        "MATCH (i:Item) RETURN [i.a, i.b, i.missing] AS l, {a: i.a, m: i.missing} AS m",
    );
    assert_eq!(
        row,
        [
            list([
                int(1),
                CypherValue::String("two".to_owned()),
                CypherValue::Null
            ]),
            CypherValue::Map(BTreeMap::from([
                ("a".to_owned(), int(1)),
                ("m".to_owned(), CypherValue::Null),
            ])),
        ]
    );

    // Computed lists flow through WITH, UNWIND, ordering, and grouping.
    let result = database
        .execute(
            "UNWIND [1, 2] AS a WITH [a, a + 1] AS l \
             UNWIND l AS v RETURN v, count(*) AS c ORDER BY v",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![int(1), int(1)],
            vec![int(2), int(2)],
            vec![int(3), int(1)]
        ]
    );

    let plan = explain("WITH 1 AS a RETURN [a, {k: a + 1}] AS n").unwrap();
    assert!(
        plan.contains("list[scalar(%s0), map{\"k\": add(scalar(%s0), literal(1))}]"),
        "{plan}"
    );
}

#[test]
fn plus_concatenates_strings_and_lists() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "RETURN 'ab' + 'c' AS a, 'nam' + 'e' AS b, [1, 2] + [3] AS c, [1] + 2 AS d, 1 + [2] AS e, \
                [[], []] + [[]] AS f, [1] + null AS g, null + 'x' AS h, 'x' + 1 AS i, \
                1 + 2 AS j, 1 + 2.5 AS k, [1] + [] AS l, [] + 's' AS m, [1] + [[2]] AS n, \
                '' + '' AS o, size([1, 2] + [3, 4]) AS p, [1, 2] + [3] + 4 AS q",
    );
    assert_eq!(
        row,
        [
            CypherValue::String("abc".to_owned()),
            CypherValue::String("name".to_owned()),
            list([int(1), int(2), int(3)]),
            list([int(1), int(2)]),
            list([int(1), int(2)]),
            list([list([]), list([]), list([])]),
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            int(3),
            CypherValue::Float(3.5),
            list([int(1)]),
            list([CypherValue::String("s".to_owned())]),
            list([int(1), list([int(2)])]),
            CypherValue::String(String::new()),
            int(4),
            list([int(1), int(2), int(3), int(4)]),
        ]
    );

    database
        .execute("CREATE (:N {name: 'Apa', list: [2, -2]}), (:N {name: 'Bo', list: [1]})")
        .unwrap();
    let result = database
        .execute(
            "MATCH (n:N) WITH n.name + '!' AS shout, n.list + [0] AS padded \
             RETURN shout, padded, size(padded) AS s ORDER BY shout",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("Apa!".to_owned()),
                list([int(2), int(-2), int(0)]),
                int(3)
            ],
            vec![
                CypherValue::String("Bo!".to_owned()),
                list([int(1), int(0)]),
                int(2)
            ],
        ]
    );
    let result = database
        .execute(
            "MATCH (n:N) WHERE n['nam' + 'e'] = 'Apa' \
             UNWIND n.list + [7] AS v RETURN v ORDER BY v",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![int(-2)], vec![int(2)], vec![int(7)]]);
}
