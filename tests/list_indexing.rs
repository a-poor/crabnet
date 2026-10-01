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

fn ints<const N: usize>(items: [i64; N]) -> CypherValue {
    CypherValue::List(items.iter().copied().map(int).collect())
}

fn text(value: &str) -> CypherValue {
    CypherValue::String(value.to_owned())
}

const NULL: CypherValue = CypherValue::Null;

#[test]
fn indexing_reads_list_positions_and_map_keys() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "RETURN [1, 2, 3][0] AS a, [[1]][0][0] AS b, [1, 2, 3][-1] AS c, [1, 2, 3][3] AS d, \
                [1, 2, 3][-4] AS e, {name: 'Apa'}['name'] AS h, {name: 'Apa'}['Name'] AS i, \
                null[0] AS k, [1][null] AS l, {name: 'Apa'}[null] AS m",
    );
    assert_eq!(
        row,
        [
            int(1),
            int(1),
            int(3),
            NULL,
            NULL,
            text("Apa"),
            NULL,
            NULL,
            NULL,
            NULL,
        ]
    );
    // A list takes only an integer index, a map only a string key, and
    // nothing else can be indexed.
    for query in [
        "RETURN [1, 2, 3][1.0] AS r",
        "RETURN [1, 2, 3]['x'] AS r",
        "RETURN {name: 'Apa'}[0] AS r",
        "RETURN 5[0] AS r",
        "RETURN 'abc'[0] AS r",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Type(_))),
            "{query}"
        );
    }

    // Indexes may be computed, come from parameters, or chain with other
    // list operations.
    let row = single(
        &mut database,
        "WITH [[1, 2], [3]] AS l, 1 AS i \
         RETURN l[i - 1][i] AS a, l[0][..1] AS b, size(l[1]) AS c, l[0] = [1, 2] AS d, \
                2 IN l[0] AS e, l[1..2][0] AS f, range(1, 3)[1] AS g",
    );
    assert_eq!(
        row,
        [
            int(2),
            ints([1]),
            int(1),
            CypherValue::Boolean(true),
            CypherValue::Boolean(true),
            ints([3]),
            int(2),
        ]
    );
    let result = database
        .execute_with(
            "WITH $expr AS expr, $idx AS idx RETURN expr[idx] AS value, $map[$key] AS keyed",
            &parameters([
                ("expr", json!(["Apa"])),
                ("idx", json!(0)),
                ("map", json!({"k": {"nested": true}})),
                ("key", json!("k")),
            ]),
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            text("Apa"),
            CypherValue::Map(BTreeMap::from([(
                "nested".to_owned(),
                CypherValue::Boolean(true)
            )])),
        ]]
    );
}

#[test]
fn dynamic_property_access_names_entity_properties_at_runtime() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:P {name: 'Ann', tags: ['a', 'b', 'c'], meta: {k: 1}})-[:R {w: 2}]->(:P {name: 'Bob'})",
        )
        .unwrap();
    let row = single(
        &mut database,
        "MATCH (p:P {name: 'Ann'})-[r]->() \
         RETURN p['name'] AS a, p['nope'] AS b, p[1] AS c, p.tags[1] AS d, p.tags[1..] AS e, \
                p.meta['k'] AS f, p['tags'][-1] AS g, r['w'] AS h, p[null] AS i",
    );
    assert_eq!(
        row,
        [
            text("Ann"),
            NULL,
            NULL,
            text("b"),
            CypherValue::List(vec![text("b"), text("c")]),
            int(1),
            text("c"),
            int(2),
            NULL,
        ]
    );
    let result = database
        .execute_with(
            "MATCH (p:P) WHERE p[$key] = 'Ann' RETURN p[$key] AS v, count(*) AS c",
            &parameters([("key", json!("name"))]),
        )
        .unwrap();
    assert_eq!(result.rows, [vec![text("Ann"), int(1)]]);

    let error = database
        .execute("MATCH p = (a)-->() RETURN p[0]")
        .unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
    let error = database
        .execute("MATCH p = (a)-->() RETURN nodes(p)[0]")
        .unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
}

#[test]
fn slices_normalize_negative_missing_and_exceeding_bounds() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "WITH [1, 2, 3, 4, 5] AS list \
         RETURN list[1..3] AS a, list[1..] AS b, list[..2] AS c, list[0..0] AS d, \
                list[-3..-1] AS e, list[3..1] AS f, list[-9..9] AS g, list[null..2] AS h, \
                list[1..null] AS i, list[1.5..2] AS j, list[..] AS k, list[0..1] AS l",
    );
    assert_eq!(
        row,
        [
            ints([2, 3]),
            ints([2, 3, 4, 5]),
            ints([1, 2]),
            ints([]),
            ints([3, 4]),
            ints([]),
            ints([1, 2, 3, 4, 5]),
            NULL,
            NULL,
            NULL,
            ints([1, 2, 3, 4, 5]),
            ints([1]),
        ]
    );
    let row = single(
        &mut database,
        "RETURN null[0..1] AS a, 'abc'[0..1] AS b, [[1], 's', null, {a: 1}][1..] AS c",
    );
    assert_eq!(
        row,
        [
            NULL,
            NULL,
            CypherValue::List(vec![
                text("s"),
                NULL,
                CypherValue::Map(BTreeMap::from([("a".to_owned(), int(1))])),
            ]),
        ]
    );
    let result = database
        .execute_with(
            "WITH [1, 2, 3] AS list RETURN list[$from..$to] AS r, list[$to..$from] AS inverted",
            &parameters([("from", json!(1)), ("to", json!(3))]),
        )
        .unwrap();
    assert_eq!(result.rows, [vec![ints([2, 3]), ints([])]]);
}

#[test]
fn range_builds_integer_lists_and_rejects_a_zero_step() {
    let mut database = Database::in_memory().unwrap();
    let row = single(
        &mut database,
        "RETURN range(1, 5) AS a, range(5, 1) AS b, range(1, 5, 2) AS c, range(5, 1, -2) AS d, \
                range(1, 5, -1) AS e, range(0, 0) AS f, range(-3412, 1381, 1298) AS g, \
                range(null, 1) AS h, range(1, null) AS i, range(1, 3, null) AS j, \
                size(range(1, 1000)) AS k, range(1, 1) AS l",
    );
    assert_eq!(
        row,
        [
            ints([1, 2, 3, 4, 5]),
            ints([]),
            ints([1, 3, 5]),
            ints([5, 3, 1]),
            ints([]),
            ints([0]),
            ints([-3412, -2114, -816, 482]),
            NULL,
            NULL,
            NULL,
            int(1000),
            ints([1]),
        ]
    );

    let result = database
        .execute("UNWIND range(1, 3) AS i RETURN i, [10, 20, 30][i - 1] AS v ORDER BY i DESC")
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![int(3), int(30)],
            vec![int(2), int(20)],
            vec![int(1), int(10)]
        ]
    );
    let result = database
        .execute(
            "WITH [0, 1, 2] AS stops UNWIND stops AS stop UNWIND [-1, 1] AS step \
             WITH stop, step, range(0, stop, step) AS list \
             WITH collect(size(list) = 0 OR step > 0 OR stop = 0) AS oks \
             RETURN all(ok IN oks WHERE ok) AS okay",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![CypherValue::Boolean(true)]]);

    for query in [
        "RETURN range(1, 2.0)",
        "RETURN range('1', 2)",
        "RETURN range(0, 1, true)",
    ] {
        let error = database.execute(query).unwrap_err();
        assert!(
            matches!(&error, Error::Argument(message) if message.contains("integers")),
            "{query}: {error:?}"
        );
    }
    let error = database.execute("RETURN range(2, 8, 0)").unwrap_err();
    assert!(
        matches!(&error, Error::Argument(message) if message.contains("step")),
        "{error:?}"
    );
    let error = database
        .execute("UNWIND [1, 0] AS s RETURN range(1, 3, s)")
        .unwrap_err();
    assert!(matches!(error, Error::Argument(_)), "{error:?}");
    let error = database.execute("RETURN range(1)").unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
}

#[test]
fn indexing_slicing_and_range_render_in_logical_plans() {
    let plan = explain(
        "MATCH (n) WITH n, [1, 2, 3] AS l RETURN l[0], l[1..], l[..2], n['k'], range(1, 3, 1)",
    )
    .unwrap();
    for expected in [
        "index(scalar(%s0), literal(0))",
        "slice(scalar(%s0), literal(1)..)",
        "slice(scalar(%s0), ..literal(2))",
        "dynamic-property(%0, literal(\"k\"))",
        "range(literal(1), literal(3), literal(1))",
    ] {
        assert!(plan.contains(expected), "missing {expected}:\n{plan}");
    }
}
