use crabnet::{CypherValue, Database, Error, explain};

fn single(database: &mut Database, query: &str) -> Vec<CypherValue> {
    let result = database.execute(query).unwrap();
    assert_eq!(result.rows.len(), 1, "{query}");
    result.rows[0].clone()
}

fn int(value: i64) -> CypherValue {
    CypherValue::Integer(value)
}

fn text(value: &str) -> CypherValue {
    CypherValue::String(value.to_owned())
}

const NULL: CypherValue = CypherValue::Null;

#[test]
fn simple_case_matches_on_cypher_equality() {
    let mut database = Database::in_memory().unwrap();
    let query = |value: &str| {
        format!(
            "RETURN CASE {value} WHEN -10 THEN 'minus ten' WHEN 0 THEN 'zero' WHEN 1, 2 THEN 'small' \
                    WHEN 1.5 THEN 'real' WHEN 'a' THEN 'text' WHEN true THEN 'yes' \
                    WHEN [1, 2] THEN 'list' WHEN null THEN 'null' ELSE 'other' END AS result"
        )
    };
    let expectations = [
        ("-10", "minus ten"),
        ("0", "zero"),
        ("1", "small"),
        ("2", "small"),
        ("2.0", "small"),
        ("1.5", "real"),
        ("'a'", "text"),
        ("true", "yes"),
        ("[1, 2]", "list"),
        ("null", "other"),
        ("'0'", "other"),
        ("3", "other"),
        ("false", "other"),
    ];
    for (value, expected) in expectations {
        assert_eq!(
            single(&mut database, &query(value)),
            [text(expected)],
            "{value}"
        );
    }
}

#[test]
fn generic_case_uses_predicate_truth_and_null_defaults() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:P {age: 10, name: 'a'}), (:P {age: 30, name: 'b'}), (:P {name: 'c'})")
        .unwrap();
    let result = database
        .execute(
            "MATCH (p:P) RETURN p.name AS name, \
             CASE WHEN p.age < 18 THEN 'minor' WHEN p.age >= 18 THEN 'adult' ELSE 'unknown' END AS band, \
             CASE WHEN p.age > 20 THEN p.age * 2 END AS doubled \
             ORDER BY name",
        )
        .unwrap();
    assert_eq!(result.columns, ["name", "band", "doubled"]);
    assert_eq!(
        result.rows,
        [
            vec![text("a"), text("minor"), NULL],
            vec![text("b"), text("adult"), int(60)],
            vec![text("c"), text("unknown"), NULL],
        ]
    );

    // A null predicate is not true.
    assert_eq!(
        single(
            &mut database,
            "RETURN CASE WHEN null THEN 1 WHEN true THEN 4 END AS r"
        ),
        [int(4)]
    );
    // A predicate of another kind is an error: while planning when the query
    // text fixes its kind, and at runtime otherwise.
    assert!(matches!(
        database.execute("RETURN CASE WHEN 1 THEN 2 END AS r"),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute("MATCH (p:P) RETURN CASE WHEN p.name THEN 2 END AS r"),
        Err(Error::Type(_))
    ));
    // Without a matching branch or an ELSE the result is null.
    assert_eq!(
        single(&mut database, "RETURN CASE WHEN false THEN 1 END AS r"),
        [NULL]
    );
}

#[test]
fn case_results_keep_their_dynamic_types_and_compose() {
    let mut database = Database::in_memory().unwrap();
    // Branches of different types: the chosen branch decides the type.
    let result = database
        .execute(
            "UNWIND [1, 2, 3, 4] AS x \
             RETURN CASE x WHEN 1 THEN 1.5 WHEN 2 THEN 'two' WHEN 3 THEN [x, x] ELSE {k: x} END AS v",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Float(1.5)],
            vec![text("two")],
            vec![CypherValue::List(vec![int(3), int(3)])],
            vec![CypherValue::Map(
                [("k".to_owned(), int(4))].into_iter().collect()
            )],
        ]
    );

    // A CASE result feeds arithmetic, comparisons, size(), and predicates.
    // (A function call following a `CASE ... END AS x` in a WITH item does
    // not parse in open-cypher 0.2.1, so the CASE is aliased last.)
    let result = database
        .execute(
            "UNWIND [1, 2, 3] AS x \
             WITH x, x + CASE WHEN x > 1 THEN 10 ELSE 0 END AS shifted, \
                  CASE WHEN x % 2 = 0 THEN [x] ELSE [x, x] END AS l \
             WHERE size(l) = 2 \
             RETURN shifted ORDER BY shifted",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![int(1)], vec![int(13)]]);
    assert_eq!(
        single(
            &mut database,
            "UNWIND [1, 2, 3] AS x RETURN sum(CASE WHEN x = 3 THEN 1 ELSE 0 END) AS c"
        ),
        [int(1)]
    );

    // Aggregates are bound in WITH first, like every other nested aggregate.
    assert_eq!(
        single(
            &mut database,
            "UNWIND [1, 2, 3] AS x WITH count(x) AS c, sum(x) AS s \
             RETURN CASE WHEN c > 2 THEN 'many' ELSE 'few' END AS r, CASE c WHEN 3 THEN s END AS t"
        ),
        [text("many"), int(6)]
    );

    // A CASE in a WHERE clause is a plain predicate.
    let result = database
        .execute(
            "UNWIND [1, 2, 3] AS x WITH x WHERE CASE WHEN x = 2 THEN false ELSE x > 1 END RETURN x",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![int(3)]]);
}

#[test]
fn case_sql_stays_linear_and_explains() {
    let plan =
        explain("RETURN CASE 1 WHEN 1, 2 THEN 'a' WHEN 3 THEN 'b' ELSE 'c' END AS r").unwrap();
    assert!(
        plan.contains(
            "case literal(1) when literal(1), literal(2) then literal(\"a\") \
             when literal(3) then literal(\"b\") else literal(\"c\") end"
        ),
        "{plan}"
    );
    let plan = explain("MATCH (n) RETURN CASE WHEN n.a > 1 THEN n.b END AS r").unwrap();
    assert!(plan.contains("case when"), "{plan}");

    let nested = |depth: usize| {
        let mut expression = "n.a".to_owned();
        for _ in 0..depth {
            expression = format!("CASE WHEN n.b > 1 THEN {expression} + 1 ELSE 0 END");
        }
        crabnet::explain_physical(&format!("MATCH (n) RETURN {expression} AS r"))
            .unwrap()
            .len()
    };
    let (shallow, middle, deep) = (nested(2), nested(4), nested(6));
    assert!(
        deep - middle < 4 * (middle - shallow),
        "{shallow} {middle} {deep}"
    );

    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE ()").unwrap();
    assert_eq!(
        single(
            &mut database,
            "MATCH p = (n) RETURN CASE p WHEN 1 THEN 1 END AS r"
        ),
        [NULL]
    );
}
