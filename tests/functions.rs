use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, explain};

fn single(database: &mut Database, query: &str) -> Vec<CypherValue> {
    let result = database.execute(query).unwrap();
    assert_eq!(result.rows.len(), 1, "{query}");
    result.rows[0].clone()
}

fn int(value: i64) -> CypherValue {
    CypherValue::Integer(value)
}

fn float(value: f64) -> CypherValue {
    CypherValue::Float(value)
}

fn text(value: &str) -> CypherValue {
    CypherValue::String(value.to_owned())
}

fn texts<const N: usize>(items: [&str; N]) -> CypherValue {
    CypherValue::List(items.iter().map(|item| text(item)).collect())
}

const NULL: CypherValue = CypherValue::Null;
const TRUE: CypherValue = CypherValue::Boolean(true);
const FALSE: CypherValue = CypherValue::Boolean(false);

#[test]
fn numeric_functions_follow_operand_types() {
    let mut database = Database::in_memory().unwrap();
    assert_eq!(
        single(
            &mut database,
            "RETURN abs(-3) AS a, abs(-1.5) AS b, abs(null) AS d, \
                    sign(-7) AS e, sign(2.5) AS f, sign(0) AS g, \
                    ceil(1.2) AS h, floor(1.8) AS i, round(2.5) AS j, ceil(3) AS k, \
                    sqrt(16) AS l, sqrt(null) AS m"
        ),
        [
            int(3),
            float(1.5),
            NULL,
            int(-1),
            int(1),
            int(0),
            float(2.0),
            float(1.0),
            float(3.0),
            float(3.0),
            float(4.0),
            NULL,
        ]
    );
    let row = single(
        &mut database,
        "RETURN exp(0) AS a, log(e()) AS b, log10(1000) AS c, sin(0) AS d, cos(0) AS e, \
                atan(0) AS f, pi() AS g",
    );
    assert_eq!(row[0], float(1.0));
    assert_eq!(row[1], float(1.0));
    assert_eq!(row[2], float(3.0));
    assert_eq!(row[3], float(0.0));
    assert_eq!(row[4], float(1.0));
    assert_eq!(row[5], float(0.0));
    assert_eq!(row[6], float(std::f64::consts::PI));

    // Function results compose with arithmetic and comparisons.
    assert_eq!(
        single(
            &mut database,
            "RETURN abs(-2) * 3 + floor(1.5) AS a, abs(-2) > 1 AS b"
        ),
        [float(7.0), TRUE]
    );
}

#[test]
fn rand_is_a_fresh_real_per_row_and_never_aggregated() {
    let mut database = Database::in_memory().unwrap();
    let result = database
        .execute("UNWIND range(1, 20) AS x RETURN rand() AS r, rand() < 0.5 AS half")
        .unwrap();
    let values = result
        .rows
        .iter()
        .map(|row| match &row[0] {
            CypherValue::Float(value) => *value,
            other => panic!("{other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        values.iter().all(|value| (0.0..1.0).contains(value)),
        "{values:?}"
    );
    let mut distinct = values.clone();
    distinct.sort_by(f64::total_cmp);
    distinct.dedup();
    assert!(distinct.len() > 1, "{values:?}");

    // One draw per call site per row: the value and its type agree, and a
    // WITH-bound draw is stable across later references.
    let result = database
        .execute(
            "UNWIND range(1, 40) AS x \
             WITH x, rand() AS r \
             RETURN r = r AS same, CASE WHEN r < 0.5 THEN 1 ELSE 'high' END AS mixed",
        )
        .unwrap();
    assert!(
        result
            .rows
            .iter()
            .all(|row| row[0] == CypherValue::Boolean(true))
    );
    let kinds = result
        .rows
        .iter()
        .map(|row| matches!(row[1], CypherValue::Integer(1)) || row[1] == text("high"))
        .collect::<Vec<_>>();
    assert!(kinds.iter().all(|kind| *kind), "{:?}", result.rows);
    let lows = result.rows.iter().filter(|row| row[1] == int(1)).count();
    assert!(lows > 0 && lows < 40, "{lows}");

    // Inside a comprehension the draw is per element and per outer row.
    let result = database
        .execute(
            "UNWIND range(1, 10) AS y \
             RETURN size([x IN range(1, 30) WHERE rand() < 0.5 | x]) AS kept",
        )
        .unwrap();
    let kept = result
        .rows
        .iter()
        .map(|row| match row[0] {
            CypherValue::Integer(value) => value,
            _ => panic!("{row:?}"),
        })
        .collect::<Vec<_>>();
    assert!(
        kept.iter().any(|count| *count != 0 && *count != 30),
        "{kept:?}"
    );
    let mut distinct_kept = kept.clone();
    distinct_kept.sort_unstable();
    distinct_kept.dedup();
    assert!(distinct_kept.len() > 1, "{kept:?}");

    let error = database.execute("RETURN count(rand()) AS c").unwrap_err();
    assert!(
        matches!(&error, Error::Semantic(message) if message.contains("rand()")),
        "{error:?}"
    );
    let error = database.execute("RETURN rand(1) AS c").unwrap_err();
    assert!(matches!(error, Error::Semantic(_)), "{error:?}");
}

#[test]
fn conversions_parse_text_and_pass_through_matching_kinds() {
    let mut database = Database::in_memory().unwrap();
    assert_eq!(
        single(
            &mut database,
            "RETURN toInteger(42) AS a, toInteger(4.9) AS b, toInteger(-4.9) AS c, \
                    toInteger('12') AS d, toInteger(' 7.8 ') AS e, toInteger('abc') AS f, \
                    toInteger(true) AS g, toInteger(false) AS h, toInteger(null) AS i"
        ),
        [
            int(42),
            int(4),
            int(-4),
            int(12),
            int(7),
            NULL,
            int(1),
            int(0),
            NULL
        ]
    );
    assert_eq!(
        single(
            &mut database,
            "RETURN toFloat(1) AS a, toFloat(1.5) AS b, toFloat('2.5') AS c, toFloat('x') AS d, \
                    toFloat(null) AS e, toFloat('1e2') AS f"
        ),
        [float(1.0), float(1.5), float(2.5), NULL, NULL, float(100.0)]
    );
    assert_eq!(
        single(
            &mut database,
            "RETURN toString(1) AS a, toString(1.5) AS b, toString(true) AS c, \
                    toString('s') AS d, toString(-2) AS e, toString(null) AS f, toString(1.0) AS g"
        ),
        [
            text("1"),
            text("1.5"),
            text("true"),
            text("s"),
            text("-2"),
            NULL,
            text("1.0")
        ]
    );
    assert_eq!(
        single(
            &mut database,
            "RETURN toBoolean(true) AS a, toBoolean('FALSE') AS b, toBoolean('yes') AS c, \
                    toBoolean(0) AS d, toBoolean(3) AS e, toBoolean('') AS f, toBoolean(null) AS g"
        ),
        [TRUE, FALSE, NULL, FALSE, TRUE, NULL, NULL]
    );
    // A kind a conversion does not accept is a runtime type error.
    for query in [
        "RETURN toInteger([1]) AS r",
        "RETURN toInteger({}) AS r",
        "RETURN toFloat(true) AS r",
        "RETURN toFloat([1.5]) AS r",
        "RETURN toString([1]) AS r",
        "RETURN toString({}) AS r",
        "RETURN toBoolean(1.0) AS r",
        "RETURN toBoolean([true]) AS r",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Type(_))),
            "{query}"
        );
    }
    // toBoolean() is a predicate, and toInteger() feeds arithmetic as an integer.
    let result = database
        .execute("UNWIND ['true', 'false', 'x'] AS s WITH s WHERE toBoolean(s) RETURN s")
        .unwrap();
    assert_eq!(result.rows, [vec![text("true")]]);
    assert_eq!(
        single(
            &mut database,
            "RETURN toInteger('2') + 1 AS a, toInteger('2') / 4 AS b"
        ),
        [int(3), int(0)]
    );
}

#[test]
fn string_functions_return_text_or_null() {
    let mut database = Database::in_memory().unwrap();
    assert_eq!(
        single(
            &mut database,
            "RETURN toLower('AbC') AS a, toUpper('AbC') AS b, trim('  x ') AS c, \
                    ltrim('  x ') AS d, rtrim('  x ') AS e, trim(null) AS g, \
                    reverse('abc') AS h, reverse([1, 'b', [3]]) AS i, reverse(null) AS j, \
                    substring('hello', 1) AS k, substring('hello', 1, 3) AS l, \
                    substring('hello', -1) AS m, left('hello', 2) AS n, right('hello', 2) AS o, \
                    right('hello', 0) AS p, right('hello', 9) AS q, \
                    replace('banana', 'an', 'AN') AS r, \
                    split('a,b,,c', ',') AS t, split('abc', '') AS u"
        ),
        [
            text("abc"),
            text("ABC"),
            text("x"),
            text("x "),
            text("  x"),
            NULL,
            text("cba"),
            CypherValue::List(vec![CypherValue::List(vec![int(3)]), text("b"), int(1)]),
            NULL,
            text("ello"),
            text("ell"),
            NULL,
            text("he"),
            text("lo"),
            text(""),
            text("hello"),
            text("bANANa"),
            texts(["a", "b", "", "c"]),
            texts(["a", "b", "c"]),
        ]
    );
    // Text results feed string predicates and size().
    assert_eq!(
        single(
            &mut database,
            "RETURN toUpper('ab') STARTS WITH 'A' AS a, size(reverse('abcd')) AS b, \
                    size(split('a b c', ' ')) AS c"
        ),
        [TRUE, int(4), int(3)]
    );
}

#[test]
fn list_and_map_functions() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:P {name: 'a', age: 1})-[:R {since: 2000}]->(:P)")
        .unwrap();
    assert_eq!(
        single(
            &mut database,
            "RETURN head([1, 2]) AS a, last([1, 2]) AS b, tail([1, 2, 3]) AS c, head([]) AS d, \
                    tail([]) AS e, head(null) AS f, tail(null) AS g, \
                    keys({b: 1, a: 2}) AS h, keys({}) AS i, keys(null) AS j, \
                    properties({a: 1}) AS k, properties(null) AS l, \
                    coalesce(null, 1, 2) AS m, coalesce(null, null) AS n, \
                    coalesce(null, 'x') AS o, coalesce([1], 2) AS p"
        ),
        [
            int(1),
            int(2),
            CypherValue::List(vec![int(2), int(3)]),
            NULL,
            CypherValue::List(vec![]),
            NULL,
            NULL,
            // Map literals are stored with sorted keys.
            texts(["a", "b"]),
            CypherValue::List(vec![]),
            NULL,
            CypherValue::Map([("a".to_owned(), int(1))].into_iter().collect()),
            NULL,
            int(1),
            NULL,
            text("x"),
            CypherValue::List(vec![int(1)]),
        ]
    );

    // Another kind is an error, while planning or at runtime.
    for function in ["keys", "properties"] {
        assert!(matches!(
            database.execute(&format!("RETURN {function}(1) AS r")),
            Err(Error::Semantic(_))
        ));
        assert!(matches!(
            database.execute(&format!("MATCH (n:P) RETURN {function}(n.name) AS r")),
            Err(Error::Type(_))
        ));
    }

    // keys() and properties() of entities, including a null optional match.
    let result = database
        .execute(
            "MATCH (n:P) OPTIONAL MATCH (n)-[r:R]->() \
             RETURN keys(n) AS nk, properties(n) AS np, keys(r) AS rk, properties(r) AS rp, \
                    coalesce(n.age, 'none') AS age \
             ORDER BY n.name",
        )
        .unwrap();
    let mut a_props = BTreeMap::new();
    a_props.insert("age".to_owned(), int(1));
    a_props.insert("name".to_owned(), text("a"));
    assert_eq!(result.rows.len(), 2);
    let mut a_keys = match &result.rows[0][0] {
        CypherValue::List(keys) => keys.clone(),
        other => panic!("{other:?}"),
    };
    a_keys.sort_by_key(|key| format!("{key:?}"));
    assert_eq!(a_keys, [text("age"), text("name")]);
    assert_eq!(result.rows[0][1], CypherValue::Map(a_props));
    assert_eq!(result.rows[0][2], texts(["since"]));
    assert_eq!(
        result.rows[0][3],
        CypherValue::Map([("since".to_owned(), int(2000))].into_iter().collect())
    );
    assert_eq!(result.rows[0][4], int(1));
    assert_eq!(
        result.rows[1],
        [
            CypherValue::List(vec![]),
            CypherValue::Map(BTreeMap::new()),
            NULL,
            NULL,
            text("none"),
        ]
    );

    // Explicit-null properties stay in the stored map (whole-entity values
    // show them) but do not exist as Cypher properties, so keys() and
    // properties() leave them out. Map values may hold null, so the map
    // forms keep them.
    database
        .execute("CREATE (:Q {present: 1, absent: null})")
        .unwrap();
    assert_eq!(
        single(
            &mut database,
            "MATCH (q:Q) RETURN keys(q) AS k, 'absent' IN keys(q) AS a, q.absent IS NULL AS b, \
                    properties(q) AS p, keys({x: null}) AS mk, properties({x: null}) AS mp"
        ),
        [
            texts(["present"]),
            FALSE,
            TRUE,
            CypherValue::Map([("present".to_owned(), int(1))].into_iter().collect()),
            texts(["x"]),
            CypherValue::Map([("x".to_owned(), NULL)].into_iter().collect()),
        ]
    );
    database
        .execute("MATCH (q:Q) SET q = {only: true, gone: null}")
        .unwrap();
    assert_eq!(
        single(&mut database, "MATCH (q:Q) RETURN keys(q) AS k"),
        [texts(["only"])]
    );

    // coalesce() picks the type of the value it returns, even when an
    // earlier statically typed argument is null.
    let result = database
        .execute("MATCH (n:P) OPTIONAL MATCH (n)-[r:R]->() RETURN coalesce(type(r), 5) AS v ORDER BY n.name")
        .unwrap();
    assert_eq!(result.rows, [vec![text("R")], vec![int(5)]]);
}

#[test]
fn functions_explain_and_reject_bad_arity() {
    let plan =
        explain("MATCH (n) RETURN toUpper(n.name) AS u, keys(n) AS k, coalesce(n.a, 1) AS c")
            .unwrap();
    assert!(plan.contains("toUpper(property(%0, \"name\"))"), "{plan}");
    assert!(plan.contains("keys(%0)"), "{plan}");
    assert!(
        plan.contains("coalesce(property(%0, \"a\"), literal(1))"),
        "{plan}"
    );

    for query in [
        "RETURN abs(1, 2)",
        "RETURN substring('a')",
        "RETURN coalesce()",
        "RETURN pi(1)",
        "MATCH p = (n) RETURN keys(p)",
    ] {
        let error = Database::in_memory().unwrap().execute(query).unwrap_err();
        assert!(matches!(error, Error::Semantic(_)), "{query}: {error:?}");
    }
    let error = Database::in_memory()
        .unwrap()
        .execute("RETURN foo(1)")
        .unwrap_err();
    assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
}
