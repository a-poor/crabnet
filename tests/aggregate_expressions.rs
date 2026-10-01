use crabnet::{CypherValue, Database, Error, Parameters, explain_physical};
use serde_json::json;

#[test]
fn aggregate_arithmetic_and_scalar_functions_use_the_whole_input_bag() {
    let mut database = Database::in_memory().unwrap();
    let result = database
        .execute(
            "UNWIND [1, 2, 2, null] AS x \
             RETURN count(*) + 1 AS rows, sum(x) / count(x) AS mean, \
                    abs(min(x) - max(x)) AS spread, size(collect(DISTINCT x)) AS kinds",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Integer(5),
            CypherValue::Integer(1),
            CypherValue::Integer(1),
            CypherValue::Integer(2),
        ]]
    );
}

#[test]
fn scalar_list_operations_evaluate_after_aggregation() {
    let mut database = Database::in_memory().unwrap();
    let result = database
        .execute(
            "UNWIND [1, 2, 2, null] AS x WITH x ORDER BY x \
             RETURN [y IN collect(x) WHERE y > 1 | y * 2] AS doubled, \
                    all(y IN collect(x) WHERE y > 0) AS positive, \
                    collect(x)[count(x) - 1] AS last, \
                    {count: count(*), values: collect(x)[1..]} AS summary",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::List(vec![CypherValue::Integer(4), CypherValue::Integer(4)]),
            CypherValue::Boolean(true),
            CypherValue::Integer(2),
            CypherValue::Map(std::collections::BTreeMap::from([
                ("count".into(), CypherValue::Integer(4)),
                (
                    "values".into(),
                    CypherValue::List(vec![CypherValue::Integer(2), CypherValue::Integer(2)]),
                ),
            ])),
        ]]
    );
}

#[test]
fn empty_and_null_inputs_keep_global_and_grouped_cardinality() {
    let mut database = Database::in_memory().unwrap();
    for (input, count) in [("[]", 0), ("[null, null]", 2)] {
        let result = database
            .execute(&format!(
                "UNWIND {input} AS x RETURN count(*) + 1 AS rows, \
                 coalesce(avg(x), 42) AS mean, sum(x) + 2 AS total, \
                 min(x) + 1 AS smallest"
            ))
            .unwrap();
        assert_eq!(
            result.rows,
            [vec![
                CypherValue::Integer(count + 1),
                CypherValue::Integer(42),
                CypherValue::Integer(2),
                CypherValue::Null,
            ]]
        );
        for (expression, expected) in [
            ("size(collect(x))", CypherValue::Integer(0)),
            (
                "CASE count(x) WHEN 0 THEN 'empty' ELSE 'full' END",
                CypherValue::String("empty".into()),
            ),
            (
                "CASE WHEN count(*) > 0 THEN max(x) ELSE 7 END",
                if count == 0 {
                    CypherValue::Integer(7)
                } else {
                    CypherValue::Null
                },
            ),
            (
                "CASE WHEN false THEN count(*) ELSE 9 END",
                CypherValue::Integer(9),
            ),
        ] {
            let query = format!("UNWIND {input} AS x RETURN {expression} AS result");
            let result = database
                .execute(&query)
                .unwrap_or_else(|error| panic!("{query}: {error}"));
            assert_eq!(result.rows, [vec![expected]], "{query}");
        }
    }
    for query in [
        "UNWIND [] AS x RETURN x, count(*) + 1 AS c",
        "UNWIND [] AS x WITH x, count(*) + 1 AS c RETURN x, c",
    ] {
        assert!(database.execute(query).unwrap().rows.is_empty());
    }
    assert_eq!(
        database
            .execute("UNWIND [] AS x WITH count(*) + 1 AS c WHERE c > 0 RETURN c")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn aggregate_results_keep_dynamic_types_and_parameter_preflight() {
    let mut database = Database::in_memory().unwrap();
    let query = "UNWIND $values AS x RETURN sum(x) + $step AS total, \
                 coalesce(avg(x), $fallback) AS mean, \
                 toString(count(DISTINCT x)) AS kinds, min(x) IS NULL AS absent";
    for (values, total, mean, kinds, absent) in [
        (
            json!([1, 2.5, null]),
            CypherValue::Float(4.0),
            CypherValue::Float(1.75),
            "2",
            false,
        ),
        (
            json!([1, "2", null]),
            CypherValue::Null,
            CypherValue::Integer(99),
            "2",
            false,
        ),
        (
            json!([]),
            CypherValue::Float(0.5),
            CypherValue::Integer(99),
            "0",
            true,
        ),
    ] {
        let parameters = Parameters::from([
            ("values".into(), values),
            ("step".into(), json!(0.5)),
            ("fallback".into(), json!(99)),
        ]);
        let result = database.execute_with(query, &parameters).unwrap();
        assert_eq!(
            result.rows,
            [vec![
                total,
                mean,
                CypherValue::String(kinds.into()),
                CypherValue::Boolean(absent)
            ]]
        );
    }
    assert!(matches!(
        database.execute("UNWIND [] AS x RETURN CASE WHEN false THEN sum($missing) ELSE 1 END AS v"),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));
}

#[test]
fn grouped_expressions_reuse_keys_and_allow_modifier_only_aggregates() {
    let mut database = Database::in_memory().unwrap();
    let result = database
        .execute(
            "UNWIND [1, 2, 2, 3, 4] AS x WITH x, x % 2 AS parity \
         RETURN parity, parity + count(*) AS score \
         ORDER BY sum(x) + 1 DESC",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(0), CypherValue::Integer(3)],
            vec![CypherValue::Integer(1), CypherValue::Integer(3)],
        ]
    );
    let result = database
        .execute(
            "UNWIND [1, 2, 2, 3, 4] AS x \
         WITH x % 2 AS parity, count(*) + 1 AS c \
         WHERE sum(parity) = 0 AND c > 2 \
         RETURN parity, c",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(0), CypherValue::Integer(4)]]
    );
}

#[test]
fn with_distinct_order_pagination_and_union_shape_aggregate_rows() {
    let mut database = Database::in_memory().unwrap();
    // Different groups can produce the same computed aggregate value;
    // DISTINCT must remove those duplicates before pagination.
    for query in [
        "UNWIND [1, 1, 2, 2, 3, 4, 4, 4] AS x \
             WITH x, count(*) + 1 AS c RETURN DISTINCT c ORDER BY c SKIP 1 LIMIT 1",
        "UNWIND [1, 1, 2, 2, 3, 4, 4, 4] AS x \
             WITH x, count(*) + 1 AS c \
             WITH DISTINCT c ORDER BY c SKIP 1 LIMIT 1 WHERE c > 1 RETURN c",
    ] {
        assert_eq!(
            database.execute(query).unwrap().rows,
            [vec![CypherValue::Integer(3)]]
        );
    }
    let result = database
        .execute(
            "UNWIND [1, 1, 2] AS x WITH DISTINCT x, count(*) + 1 AS c \
         ORDER BY count(*) + 1 DESC WHERE c > 1 RETURN x, c",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(1), CypherValue::Integer(3)],
            vec![CypherValue::Integer(2), CypherValue::Integer(2)],
        ]
    );
    for (union, count) in [("UNION", 1), ("UNION ALL", 2)] {
        let result = database
            .execute(&format!(
                "UNWIND [1, 2] AS x RETURN count(*) + 1 AS c \
             {union} UNWIND [] AS x RETURN count(*) + 3 AS c"
            ))
            .unwrap();
        assert_eq!(result.rows, vec![vec![CypherValue::Integer(3)]; count]);
    }
}

#[test]
fn entity_groups_and_collections_preserve_identity_and_later_matches() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {value: 4}), (:Item {value: 4})")
        .unwrap();
    let result = database
        .execute(
            "MATCH (n:Item) UNWIND [1, 2] AS repeat \
         WITH n, n.value + count(*) AS score, collect(n) AS copies, \
              size(collect(DISTINCT n)) AS unique_count \
         MATCH (n) UNWIND copies AS copy \
         RETURN elementId(n) = elementId(copy) AS same, score, unique_count",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![
            vec![
                CypherValue::Boolean(true),
                CypherValue::Integer(6),
                CypherValue::Integer(1)
            ];
            4
        ]
    );
    assert_eq!(
        database
            .execute(
                "MATCH (n:Item) OPTIONAL MATCH (n)-[r:MISSING]->() \
         RETURN count(r) + 1 AS c, size(collect(r)) AS size"
            )
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1), CypherValue::Integer(0)]]
    );
    let result = database
        .execute("MATCH (n:Item) RETURN n.value AS value, n.value + count(*) AS score")
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![CypherValue::Integer(4), CypherValue::Integer(6)]]
    );
    let result = database
        .execute(
            "MATCH (n:Item) WITH collect(n) AS ns UNWIND [1, 2] AS repeat \
         RETURN ns, size(ns) + count(*) AS score",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert!(matches!(&result.rows[0][0], CypherValue::List(values) if values.len() == 2));
    assert_eq!(result.rows[0][1], CypherValue::Integer(4));
}

#[test]
fn random_expressions_run_per_group_and_distinct_modifiers_reuse_projected_values() {
    let mut database = Database::in_memory().unwrap();
    let result = database
        .execute(
            "UNWIND [1, 1, 2, 2] AS x \
         WITH DISTINCT x, count(*) + rand() AS draw ORDER BY draw WHERE draw >= 2 \
         RETURN draw, draw = draw AS stable",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    let mut previous = 2.0;
    for row in result.rows {
        let [CypherValue::Float(draw), CypherValue::Boolean(true)] = row.as_slice() else {
            panic!("unexpected draw row: {row:?}");
        };
        assert!((2.0..3.0).contains(draw));
        assert!(*draw >= previous);
        previous = *draw;
    }
    // This also exercises a source introduced only by ORDER BY after grouping.
    let result = database
        .execute("UNWIND [1, 1, 2] AS x RETURN x, count(*) + 1 AS c ORDER BY rand()")
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    let result = database
        .execute(
            "UNWIND [1, 1, 2, 2] AS x \
         RETURN x, count(*) AS c, [i IN range(1, 4) | rand()] AS draws ORDER BY x",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_ne!(result.rows[0][2], result.rows[1][2]);
    for row in result.rows {
        assert!(matches!(&row[2], CypherValue::List(draws) if draws.len() == 4));
    }
}

#[test]
fn aggregate_expressions_compose_with_runtime_paths_and_later_unwind() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Start)-[:NEXT]->()-[:NEXT]->()")
        .unwrap();
    let result = database
        .execute(
            "MATCH p = (:Start)-[:NEXT*]->() \
         WITH range(1, count(*)) AS indices, max(length(p)) + 1 AS nodes \
         UNWIND indices AS i RETURN i, nodes ORDER BY i",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(1), CypherValue::Integer(3)],
            vec![CypherValue::Integer(2), CypherValue::Integer(3)],
        ]
    );
}

#[test]
fn aggregate_write_pipelines_keep_multiplicity_and_roll_back_result_errors() {
    let mut database = Database::in_memory().unwrap();
    let result = database
        .execute(
            "UNWIND [1, 1, 2] AS x CREATE (:Item {value: x}) \
         WITH x, count(*) + 1 AS c CREATE (:Summary {value: x, count: c}) \
         RETURN sum(c) + 1 AS total",
        )
        .unwrap();
    assert_eq!(result.summary.nodes_created, 5);
    assert_eq!(result.rows, [vec![CypherValue::Integer(6)]]);
    assert_eq!(
        database
            .execute("MATCH (n:Summary) RETURN n.count ORDER BY n.value")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(3)], vec![CypherValue::Integer(2)]]
    );
    let error = database
        .execute(
            "CREATE (:RolledBack) WITH count(*) + 1 AS c \
         CREATE (:RolledBack {value: c}) RETURN (collect(c))[true] AS bad",
        )
        .unwrap_err();
    assert!(matches!(error, Error::Type(_)), "{error:?}");
    assert_eq!(
        database
            .execute("MATCH (n:RolledBack) RETURN count(*) + 1 AS c")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn invalid_aggregate_scopes_fail_before_mutation() {
    let mut database = Database::in_memory().unwrap();
    for query in [
        "RETURN sum(count(*) + 1) AS bad",
        "UNWIND [1, 2] AS x RETURN x + count(*) AS bad",
        "UNWIND [1, 2] AS x WITH x + count(*) AS bad RETURN bad",
        "UNWIND [1, 2] AS x RETURN x + 1 AS key, x + count(*) AS bad",
        "UNWIND [1, 2] AS x RETURN x + 1 AS key, (x + 1) + count(*) AS bad",
        "UNWIND [1, 2] AS x WITH x + 1 AS key, (x + 1) + count(*) AS bad RETURN bad",
        "UNWIND [1, 2] AS x RETURN x + 1 AS key, count(*) AS c ORDER BY (x + 1) + count(*)",
        "UNWIND [1, 2] AS x RETURN count(*) + 1 AS c ORDER BY x",
        "UNWIND [1, 2] AS x WITH DISTINCT count(*) + 1 AS c WHERE sum(x) > 0 RETURN c",
        "UNWIND [1, 2] AS x RETURN [y IN [1] | sum(y)] AS bad",
        "UNWIND [1, 2] AS x RETURN all(y IN [1] WHERE count(*) > y) AS bad",
        "MATCH (n) WHERE count(*) + 1 > 0 RETURN n",
    ] {
        let error = database
            .execute(&format!("CREATE (:ShouldNotExist) WITH 1 AS seed {query}"))
            .unwrap_err();
        assert!(matches!(error, Error::Semantic(_)), "{query}: {error:?}");
    }
    assert_eq!(
        database
            .execute("MATCH (n) RETURN count(*) AS c")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(0)]]
    );
}

#[test]
fn nested_aggregate_expressions_have_bounded_sql_and_correct_group_scope() {
    let query = |depth: usize| {
        let mut expression = "count(*)".to_owned();
        for _ in 0..depth {
            expression = format!("({expression} + 1)");
        }
        format!("UNWIND [1, 2, 3] AS x RETURN {expression} AS c")
    };
    let shallow = explain_physical(&query(4)).unwrap().len();
    let deep = explain_physical(&query(12)).unwrap().len();
    assert!(deep < shallow * 6 && deep < 64_000, "{shallow} -> {deep}");
    let mut database = Database::in_memory().unwrap();
    assert_eq!(
        database.execute(&query(12)).unwrap().rows,
        [vec![CypherValue::Integer(15)]]
    );
    let mut expression = "sum(x)".to_owned();
    for _ in 0..8 {
        expression = format!("CASE WHEN count(*) > 0 THEN {expression} ELSE 0 END");
    }
    let query = format!("UNWIND [1, 2, 3] AS x RETURN {expression} AS c");
    assert!(explain_physical(&query).unwrap().len() < 100_000);
    assert_eq!(
        database.execute(&query).unwrap().rows,
        [vec![CypherValue::Integer(6)]]
    );
}
