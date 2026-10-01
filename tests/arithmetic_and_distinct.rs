use std::collections::{BTreeMap, BTreeSet};

use crabnet::{CypherValue, Database, Error, Parameters, QueryResult, Value};
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn string_rows<const N: usize>(values: [&str; N]) -> Vec<Vec<CypherValue>> {
    values
        .into_iter()
        .map(|value| vec![CypherValue::String(value.to_owned())])
        .collect()
}

fn named_rows(result: QueryResult) -> Vec<Vec<CypherValue>> {
    assert_eq!(result.columns, ["name"]);
    assert_eq!(result.summary, Default::default());
    result.rows
}

#[test]
fn arithmetic_return_preserves_numeric_types_precedence_and_associativity() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "RETURN 7 + 2 AS integer_add, 7 - 2 AS integer_subtract, \
                    7 * 2 AS integer_multiply, 7 / 2 AS integer_divide, \
                    7 % 2 AS integer_remainder, \
                    5 + 0.5 AS float_add, 5.5 - 2 AS float_subtract, \
                    2 * 1.5 AS float_multiply, 7 / 2.0 AS float_divide, \
                    5.5 % 2 AS float_remainder, \
                    2 + 3 * 4 AS precedence, (2 + 3) * 4 AS grouped, \
                    10 - 3 - 2 AS subtract_left_to_right, \
                    20 / 5 * 2 AS multiply_divide_left_to_right, \
                    -2 * 3 + 10 AS unary_precedence",
        )
        .unwrap();

    assert_eq!(
        result.columns,
        [
            "integer_add",
            "integer_subtract",
            "integer_multiply",
            "integer_divide",
            "integer_remainder",
            "float_add",
            "float_subtract",
            "float_multiply",
            "float_divide",
            "float_remainder",
            "precedence",
            "grouped",
            "subtract_left_to_right",
            "multiply_divide_left_to_right",
            "unary_precedence",
        ]
    );
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Integer(9),
            CypherValue::Integer(5),
            CypherValue::Integer(14),
            CypherValue::Integer(3),
            CypherValue::Integer(1),
            CypherValue::Float(5.5),
            CypherValue::Float(3.5),
            CypherValue::Float(3.0),
            CypherValue::Float(3.5),
            CypherValue::Float(1.5),
            CypherValue::Integer(14),
            CypherValue::Integer(20),
            CypherValue::Integer(5),
            CypherValue::Integer(8),
            CypherValue::Integer(4),
        ]]
    );
}

#[test]
fn dynamic_arithmetic_is_numeric_only_and_null_propagating() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Operand {name: 'valid', left: 8, right: 2}), \
                    (:Operand {name: 'valid-float', left: 1.5, right: 0.5}), \
                    (:Operand {name: 'text', left: '8', right: 2}), \
                    (:Operand {name: 'boolean', left: true, right: 2}), \
                    (:Operand {name: 'list', left: [8], right: 2}), \
                    (:Operand {name: 'map', left: {number: 8}, right: 2}), \
                    (:Operand {name: 'null', left: null, right: 2}), \
                    (:Operand {name: 'missing-left', right: 2}), \
                    (:Operand {name: 'missing-right', left: 8})",
        )
        .unwrap();

    let nullish = "['missing-left', 'missing-right', 'null']";
    let wrong_kinds = ["boolean", "list", "map", "text"];
    // `+` also concatenates strings and lists: a list operand appends, and
    // other mismatched kinds produce null rather than an error.
    let result = database
        .execute(
            "MATCH (operand:Operand) \
             WHERE (operand.left + operand.right) IS NULL \
             RETURN operand.name AS name ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        named_rows(result),
        string_rows([
            "boolean",
            "map",
            "missing-left",
            "missing-right",
            "null",
            "text"
        ])
    );
    for operator in ["-", "*", "/", "%"] {
        let result = database
            .execute(&format!(
                "MATCH (operand:Operand) WHERE operand.name IN {nullish} \
                 AND (operand.left {operator} operand.right) IS NULL \
                 RETURN operand.name AS name ORDER BY name"
            ))
            .unwrap();
        assert_eq!(
            named_rows(result),
            string_rows(["missing-left", "missing-right", "null"]),
            "operator {operator}"
        );
        // Every other operator is numeric only, and another kind is a type
        // error rather than a silent null.
        for name in wrong_kinds {
            let error = database
                .execute(&format!(
                    "MATCH (operand:Operand {{name: '{name}'}}) \
                     RETURN operand.left {operator} operand.right AS value"
                ))
                .unwrap_err();
            assert!(
                matches!(error, Error::Type(_)),
                "{name} {operator}: {error:?}"
            );
        }
    }

    let unary_nulls = database
        .execute(&format!(
            "MATCH (operand:Operand) WHERE operand.name IN {nullish} \
             AND (+operand.left) IS NULL AND (-operand.left) IS NULL \
             RETURN operand.name AS name ORDER BY name"
        ))
        .unwrap();
    assert_eq!(
        named_rows(unary_nulls),
        string_rows(["missing-left", "null"])
    );
    for name in wrong_kinds {
        for operator in ["+", "-"] {
            let error = database
                .execute(&format!(
                    "MATCH (operand:Operand {{name: '{name}'}}) \
                     RETURN {operator}operand.left AS value"
                ))
                .unwrap_err();
            assert!(
                matches!(error, Error::Type(_)),
                "{operator}{name}: {error:?}"
            );
        }
    }

    let unary_values = database
        .execute(
            "MATCH (operand:Operand) \
             WHERE operand.name = 'valid' OR operand.name = 'valid-float' \
             RETURN operand.name AS name, +operand.left AS positive, \
                    -operand.left AS negative ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        unary_values.rows,
        [
            vec![
                CypherValue::String("valid".into()),
                CypherValue::Integer(8),
                CypherValue::Integer(-8),
            ],
            vec![
                CypherValue::String("valid-float".into()),
                CypherValue::Float(1.5),
                CypherValue::Float(-1.5),
            ],
        ]
    );

    let filtered = database
        .execute(
            "MATCH (operand:Operand) \
             WHERE operand.left + operand.right * 3 = 14 \
             RETURN operand.name AS name",
        )
        .unwrap();
    assert_eq!(named_rows(filtered), string_rows(["valid"]));
}

#[test]
fn unary_arithmetic_rejects_strings_and_booleans() {
    let mut database = Database::in_memory().unwrap();
    let values = parameters([("operand", json!("8")), ("missing", json!(null))]);

    // A literal operand's kind is known while planning.
    for query in [
        "RETURN +'8' AS positive_text",
        "RETURN -true AS negative_boolean",
    ] {
        let error = database.execute(query).unwrap_err();
        assert!(matches!(error, Error::Semantic(_)), "{query}: {error:?}");
    }
    // A parameter's kind is only known at runtime. Text is never coerced.
    for query in [
        "RETURN +$operand AS positive_parameter",
        "RETURN -$operand AS negative_parameter",
        "RETURN $operand - 1 AS difference",
        "RETURN 2 * $operand AS product",
    ] {
        let error = database.execute_with(query, &values).unwrap_err();
        assert!(matches!(error, Error::Type(_)), "{query}: {error:?}");
    }
    // Null stays null.
    let result = database
        .execute_with("RETURN -$missing AS negated, $missing % 2 AS rest", &values)
        .unwrap();
    assert_eq!(result.rows, [vec![CypherValue::Null; 2]]);
}

#[test]
fn exponentiation_remains_an_explicitly_unsupported_operator() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute("RETURN 2 ^ 3 AS powered"),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn distinct_collapses_duplicate_scalar_and_multi_column_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:DistinctRow {first: 'a', second: 1}), \
                    (:DistinctRow {first: 'a', second: 1}), \
                    (:DistinctRow {first: 'a', second: 2}), \
                    (:DistinctRow {first: 'b', second: 1}), \
                    (:DistinctRow {first: 'b', second: 1})",
        )
        .unwrap();

    let scalar = database
        .execute(
            "MATCH (row:DistinctRow) RETURN DISTINCT row.first AS first \
             ORDER BY first",
        )
        .unwrap();
    assert_eq!(scalar.rows, string_rows(["a", "b"]));

    let tuples = database
        .execute(
            "MATCH (row:DistinctRow) \
             RETURN DISTINCT row.first AS first, row.second AS second \
             ORDER BY first, second",
        )
        .unwrap();
    assert_eq!(
        tuples.rows,
        [
            vec![CypherValue::String("a".into()), CypherValue::Integer(1)],
            vec![CypherValue::String("a".into()), CypherValue::Integer(2)],
            vec![CypherValue::String("b".into()), CypherValue::Integer(1)],
        ]
    );
}

#[test]
fn distinct_uses_cypher_null_boolean_and_numeric_equivalence() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:TypedDistinct {value: null}), (:TypedDistinct), \
                    (:TypedDistinct {value: true}), \
                    (:TypedDistinct {value: true}), \
                    (:TypedDistinct {value: false}), \
                    (:TypedDistinct {value: 1}), \
                    (:TypedDistinct {value: 1.0}), \
                    (:TypedDistinct {value: 0})",
        )
        .unwrap();

    let result = database
        .execute("MATCH (value:TypedDistinct) RETURN DISTINCT value.value AS value")
        .unwrap();
    assert_eq!(result.columns, ["value"]);
    assert_eq!(result.rows.len(), 5);

    let cells = result
        .rows
        .iter()
        .map(|row| row.as_slice())
        .collect::<Vec<_>>();
    assert_eq!(
        cells
            .iter()
            .filter(|row| matches!(row, [CypherValue::Null]))
            .count(),
        1
    );
    assert_eq!(
        cells
            .iter()
            .filter(|row| matches!(row, [CypherValue::Boolean(true)]))
            .count(),
        1
    );
    assert_eq!(
        cells
            .iter()
            .filter(|row| matches!(row, [CypherValue::Boolean(false)]))
            .count(),
        1
    );
    for expected in [0.0, 1.0] {
        assert_eq!(
            cells
                .iter()
                .filter(|row| match row {
                    [CypherValue::Integer(value)] => *value as f64 == expected,
                    [CypherValue::Float(value)] => *value == expected,
                    _ => false,
                })
                .count(),
            1,
            "numeric equivalence class for {expected}"
        );
    }
}

#[test]
fn distinct_equivalence_recurses_through_lists_and_maps() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:StructuredDistinct {value: [1, {nested: 2}]}), \
                    (:StructuredDistinct {value: [1.0, {nested: 2.0}]})",
        )
        .unwrap();

    let result = database
        .execute("MATCH (value:StructuredDistinct) RETURN DISTINCT value.value AS value")
        .unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn distinct_whole_entities_uses_entity_identity() {
    let mut database = Database::in_memory().unwrap();
    for _ in 0..2 {
        database
            .execute(
                "CREATE (person:Person {name: 'Ada'}), \
                        (person)-[:TAGGED]->(:Tag {name: 'one'}), \
                        (person)-[:TAGGED]->(:Tag {name: 'two'})",
            )
            .unwrap();
    }

    let result = database
        .execute(
            "MATCH (person:Person)-[:TAGGED]->(:Tag) \
             RETURN DISTINCT person",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 2);

    let mut ids = BTreeSet::new();
    for row in result.rows {
        let [CypherValue::Node(person)] = row.as_slice() else {
            panic!("expected one whole node value")
        };
        assert_eq!(person.labels, ["Person"]);
        assert_eq!(
            person.properties.get("name"),
            Some(&CypherValue::String("Ada".into()))
        );
        ids.insert(person.element_id.clone());
    }
    assert_eq!(
        ids.len(),
        2,
        "different nodes with equal properties remain distinct"
    );
}

#[test]
fn distinct_precedes_ordering_and_pagination_and_limits_ordering_scope() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Score {name: 'one', value: 1}), \
                    (:Score {name: 'two-a', value: 2}), \
                    (:Score {name: 'two-b', value: 2}), \
                    (:Score {name: 'three-a', value: 3}), \
                    (:Score {name: 'three-b', value: 3})",
        )
        .unwrap();

    let paged = database
        .execute(
            "MATCH (score:Score) RETURN DISTINCT score.value AS value \
             ORDER BY value DESC SKIP 1 LIMIT 1",
        )
        .unwrap();
    assert_eq!(paged.rows, [vec![CypherValue::Integer(2)]]);

    assert!(matches!(
        database.execute(
            "MATCH (score:Score) RETURN DISTINCT score.value AS value \
             ORDER BY score.name"
        ),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn distinct_post_pagination_resolves_and_validates_parameters() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Page {value: 1}), (:Page {value: 1}), \
                    (:Page {value: 2}), (:Page {value: 3})",
        )
        .unwrap();

    let values = parameters([("skip", json!(1)), ("limit", json!(1))]);
    let result = database
        .execute_with(
            "MATCH (page:Page) RETURN DISTINCT page.value AS value \
             ORDER BY value SKIP $skip LIMIT $limit",
            &values,
        )
        .unwrap();
    assert_eq!(result.rows, [vec![CypherValue::Integer(2)]]);

    let invalid = parameters([("skip", json!(-1)), ("limit", json!(1))]);
    assert!(matches!(
        database.execute_with(
            "MATCH (page:Page) RETURN DISTINCT page.value AS value \
             SKIP $skip LIMIT $limit",
            &invalid,
        ),
        Err(Error::Semantic(_))
    ));

    assert!(matches!(
        database.execute_with(
            "MATCH (page:Page) RETURN DISTINCT page.value AS value LIMIT $missing",
            &Parameters::new(),
        ),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));
}

#[test]
fn property_to_property_ordering_is_typed_and_null_when_incomparable() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OrderedPair {name: 'numeric-ascending', left: 2, right: 10}), \
                    (:OrderedPair {name: 'numeric-descending', left: 10, right: 2}), \
                    (:OrderedPair {name: 'numeric-mixed', left: 1, right: 1.5}), \
                    (:OrderedPair {name: 'text-ascending', left: 'apple', right: 'banana'}), \
                    (:OrderedPair {name: 'text-descending', left: 'zebra', right: 'apple'}), \
                    (:OrderedPair {name: 'boolean-ascending', left: false, right: true}), \
                    (:OrderedPair {name: 'boolean-descending', left: true, right: false}), \
                    (:OrderedPair {name: 'mismatch-number-text', left: 2, right: '10'}), \
                    (:OrderedPair {name: 'mismatch-bool-number', left: false, right: 0}), \
                    (:OrderedPair {name: 'null-left', left: null, right: 1}), \
                    (:OrderedPair {name: 'null-right', left: 1, right: null}), \
                    (:OrderedPair {name: 'missing-left', right: 1}), \
                    (:OrderedPair {name: 'missing-right', left: 1})",
        )
        .unwrap();

    for (operator, expected) in [
        (
            "<",
            string_rows([
                "boolean-ascending",
                "numeric-ascending",
                "numeric-mixed",
                "text-ascending",
            ]),
        ),
        (
            ">=",
            string_rows([
                "boolean-descending",
                "numeric-descending",
                "text-descending",
            ]),
        ),
    ] {
        let comparable = database
            .execute(&format!(
                "MATCH (pair:OrderedPair) WHERE pair.left {operator} pair.right \
                 RETURN pair.name AS name ORDER BY name"
            ))
            .unwrap();
        assert_eq!(named_rows(comparable), expected, "operator {operator}");

        let indeterminate = database
            .execute(&format!(
                "MATCH (pair:OrderedPair) \
                 WHERE (pair.left {operator} pair.right) IS NULL \
                 RETURN pair.name AS name ORDER BY name"
            ))
            .unwrap();
        assert_eq!(
            named_rows(indeterminate),
            string_rows([
                "mismatch-bool-number",
                "mismatch-number-text",
                "missing-left",
                "missing-right",
                "null-left",
                "null-right",
            ]),
            "operator {operator}"
        );
    }
}

#[test]
fn floats_round_trip_through_sql_without_losing_significant_digits() {
    let mut database = Database::in_memory().unwrap();
    let long = 1.263_541_865_238_126_4e305_f64;
    let small = 1e-305_f64;
    let dense = 123_456_789.123_456_79_f64;

    let result = database
        .execute_with(
            "RETURN 1.2635418652381264e305 AS a, 1e-305 AS b, \
             123456789.123456789 AS c, 0.1 + 0.2 AS d, 100.0 AS e",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            CypherValue::Float(long),
            CypherValue::Float(small),
            CypherValue::Float(dense),
            CypherValue::Float(0.1 + 0.2),
            CypherValue::Float(100.0),
        ]]
    );

    // Parameters, stored properties, and SQL-computed aggregates all cross
    // the same real-to-JSON boundary.
    let created = database
        .execute_with(
            "CREATE (n:N {stored: $value, sum: 0.30000000000000004}) \
             RETURN n.stored AS stored",
            &parameters([("value", json!(dense))]),
        )
        .unwrap();
    assert_eq!(created.rows, vec![vec![CypherValue::Float(dense)]]);
    let reread = database
        .execute(
            "MATCH (n:N) RETURN n.stored AS stored, n.sum = 0.1 + 0.2 AS same, \
             n.sum / 3 AS third",
        )
        .unwrap();
    assert_eq!(
        reread.rows,
        vec![vec![
            CypherValue::Float(dense),
            CypherValue::Boolean(true),
            CypherValue::Float(0.30000000000000004 / 3.0),
        ]]
    );
    let aggregated = database
        .execute_with(
            "UNWIND [0.1, 0.2] AS x \
             RETURN max(x + 1.2635418652381264e305) AS largest, sum(x) AS small",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(
        aggregated.rows,
        vec![vec![
            CypherValue::Float(long),
            CypherValue::Float(0.1 + 0.2)
        ]]
    );
    let sorted = database
        .execute_with(
            "UNWIND [1.0000000000000002, 1.0, 1.0000000000000004] AS x \
             RETURN x ORDER BY x DESC LIMIT 1",
            &Default::default(),
        )
        .unwrap();
    assert_eq!(
        sorted.rows,
        vec![vec![CypherValue::Float(1.000_000_000_000_000_4)]]
    );
}
