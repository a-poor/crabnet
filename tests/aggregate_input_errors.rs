use std::time::Duration;

use crabnet::{CypherValue, Database, Error, OpenOptions, Parameters, ReadMode};
use serde_json::json;

fn type_error(result: crabnet::Result<crabnet::QueryResult>, context: &str) {
    assert!(
        matches!(result, Err(Error::Type(_))),
        "{context}: {result:?}"
    );
}

#[test]
fn non_numeric_aggregate_parameters_and_properties_raise_type_errors() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input)-[:R]->(:End)").unwrap();
    for input in [
        json!("2"),
        json!("bad"),
        json!(true),
        json!(false),
        json!([]),
        json!([1]),
        json!({}),
        json!({"x": 1}),
    ] {
        let parameters = Parameters::from([("input".into(), input.clone())]);
        db.execute_with(
            "MATCH (n:Input)-[r:R]->() SET n.input = $input, r.input = $input",
            &parameters,
        )
        .unwrap();
        for function in ["sum", "avg"] {
            for query in [
                format!("RETURN {function}($input)"),
                format!("MATCH (n:Input) RETURN {function}(n.input)"),
                format!("MATCH ()-[r:R]->() RETURN {function}(r.input)"),
                format!("UNWIND [1, null, $input, 2] AS x RETURN {function}(x)"),
                format!("UNWIND [$input, null, 1] AS x RETURN {function}(x)"),
                format!(
                    "UNWIND [{{g:1,v:1}}, {{g:2,v:$input}}] AS row RETURN row.g, {function}(row.v)"
                ),
            ] {
                type_error(
                    db.execute_with(&query, &parameters),
                    &format!("{query}: {input}"),
                );
            }
        }
    }
}

#[test]
fn known_non_numeric_aggregate_inputs_fail_during_planning() {
    let mut db = Database::in_memory().unwrap();
    for function in ["sum", "avg"] {
        for input in ["'1'", "true", "false", "[]", "[1]", "{}", "{x: 1}"] {
            for query in [
                format!("RETURN {function}({input})"),
                format!("RETURN CASE WHEN false THEN {function}({input}) ELSE 1 END"),
                format!("RETURN coalesce(1, {function}({input}))"),
                format!("MATCH (n:Absent) RETURN {function}({input})"),
                format!("WITH {input} AS x RETURN {function}(x)"),
                format!("UNWIND [{input}, null, {input}] AS x RETURN {function}(x)"),
            ] {
                let result = db.execute(&query);
                assert!(
                    matches!(result, Err(Error::Semantic(_))),
                    "{query}: {result:?}"
                );
            }
        }
        for input in [
            "n",
            "r",
            "p",
            "nodes(p)",
            "relationships(p)",
            "[n]",
            "{node:n}",
        ] {
            let query = format!("MATCH p = (n)-[r:R]->() RETURN {function}({input})");
            let result = db.execute(&query);
            assert!(
                matches!(result, Err(Error::Semantic(_))),
                "{query}: {result:?}"
            );
        }
    }
}

#[test]
fn aggregate_nulls_numbers_bags_and_lazy_input_branches_keep_their_semantics() {
    let mut db = Database::in_memory().unwrap();
    for (values, sum, average) in [
        (json!([]), CypherValue::Integer(0), CypherValue::Null),
        (
            json!([null, null]),
            CypherValue::Integer(0),
            CypherValue::Null,
        ),
        (
            json!([1, 1, 4, null]),
            CypherValue::Integer(6),
            CypherValue::Float(2.0),
        ),
        (
            json!([1, 1.0, 4, null]),
            CypherValue::Float(6.0),
            CypherValue::Float(2.0),
        ),
        (
            json!([0.0, -0.0, null]),
            CypherValue::Float(0.0),
            CypherValue::Float(0.0),
        ),
    ] {
        let parameters = Parameters::from([("values".into(), values)]);
        let query = "UNWIND $values AS x RETURN sum(x), avg(x), sum(ALL x), avg(ALL x)";
        let expected = vec![vec![sum.clone(), average.clone(), sum, average]];
        assert_eq!(db.execute_with(query, &parameters).unwrap().rows, expected);
        let mut read = db.prepare_read_with(query, &parameters).unwrap();
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        db.execute("UNWIND [2, 1, 2, null] AS x RETURN x, sum(x), avg(x) ORDER BY x")
            .unwrap()
            .rows,
        [
            vec![
                CypherValue::Integer(1),
                CypherValue::Integer(1),
                CypherValue::Float(1.0)
            ],
            vec![
                CypherValue::Integer(2),
                CypherValue::Integer(4),
                CypherValue::Float(2.0)
            ],
            vec![
                CypherValue::Null,
                CypherValue::Integer(0),
                CypherValue::Null
            ]
        ]
    );
    let invalid = Parameters::from([("input".into(), json!("bad"))]);
    for function in ["sum", "avg"] {
        for input in [
            "CASE WHEN false THEN $input ELSE 2 END",
            "coalesce(2, $input)",
        ] {
            let query = format!("RETURN {function}({input})");
            let value = if function == "sum" {
                CypherValue::Integer(2)
            } else {
                CypherValue::Float(2.0)
            };
            assert_eq!(
                db.execute_with(&query, &invalid).unwrap().rows,
                [vec![value]]
            );
        }
        for prefix in ["UNWIND [] AS x", "MATCH (n:Absent)"] {
            let query = format!("{prefix} RETURN {function}($input)");
            let value = if function == "sum" {
                CypherValue::Integer(0)
            } else {
                CypherValue::Null
            };
            assert_eq!(
                db.execute_with(&query, &invalid).unwrap().rows,
                [vec![value]]
            );
        }
        let query = format!("UNWIND [] AS x RETURN {function}($missing)");
        assert!(
            matches!(db.execute(&query), Err(Error::MissingParameter(_))),
            "{query}"
        );
    }
}

#[test]
fn composition_and_unselected_aggregate_results_preserve_input_errors() {
    let mut db = Database::in_memory().unwrap();
    let parameters = Parameters::from([("input".into(), json!("bad"))]);
    for function in ["sum", "avg"] {
        let expression = format!("{function}($input)");
        for output in [
            format!("{expression} IS NULL"),
            format!("coalesce({expression}, 1)"),
            format!("[{expression}]"),
            format!("{{value: {expression}}}"),
            format!("abs({expression}) IS NULL"),
            format!("toString({expression})"),
            format!("CASE WHEN false THEN {expression} ELSE 1 END"),
            format!("coalesce(1, {expression})"),
        ] {
            let query = format!("RETURN {output}");
            type_error(db.execute_with(&query, &parameters), &query);
        }
        for query in [
            format!("WITH {expression} AS total RETURN total IS NULL"),
            format!("WITH {expression} AS total RETURN count(total)"),
            format!("WITH {expression} AS total RETURN collect(total)"),
            format!("WITH {expression} AS total WHERE total IS NULL RETURN total"),
            format!("RETURN count(*) ORDER BY {expression}"),
        ] {
            type_error(db.execute_with(&query, &parameters), &query);
        }
    }
}

#[test]
fn mixed_graph_aggregate_selections_validate_the_selected_value() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    for function in ["sum", "avg"] {
        for pick in 0..8 {
            let parameters = Parameters::from([("pick".into(), json!(pick))]);
            let query = format!(
                "MATCH p = (n:Start)-[r:R]->() WITH [n, r, p, {{node:n}}, [], '1', null, 2] AS xs RETURN {function}(xs[$pick])"
            );
            let result = db.execute_with(&query, &parameters);
            match pick {
                0..6 => type_error(result, &format!("{query}: {pick}")),
                6 => assert_eq!(
                    result.unwrap().rows,
                    [vec![if function == "sum" {
                        CypherValue::Integer(0)
                    } else {
                        CypherValue::Null
                    }]]
                ),
                _ => assert_eq!(
                    result.unwrap().rows,
                    [vec![if function == "sum" {
                        CypherValue::Integer(2)
                    } else {
                        CypherValue::Float(2.0)
                    }]]
                ),
            }
        }
    }
}

#[test]
fn aggregate_input_errors_end_cursors_and_allow_prepared_reuse() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let parameters = Parameters::from([("input".into(), json!("bad"))]);
    for function in ["sum", "avg"] {
        for (prefix, mode) in [
            ("", ReadMode::Incremental),
            ("MATCH p = (:Start)-[:R*1..40]->() ", ReadMode::Buffered),
            (
                "MATCH p = shortestPath((:Start)-[:R*]->(:End)) ",
                ReadMode::Buffered,
            ),
        ] {
            let query =
                format!("RETURN 1 AS value UNION ALL {prefix}RETURN {function}($input) AS value");
            type_error(db.execute_with(&query, &parameters), &query);
            let mut read = db.prepare_read_with(&query, &parameters).unwrap();
            assert_eq!(read.mode(), mode, "{query}");
            for _ in 0..2 {
                if mode == ReadMode::Buffered {
                    assert!(matches!(read.cursor(), Err(Error::Type(_))), "{query}");
                } else {
                    let mut cursor = read.cursor().unwrap();
                    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
                    assert!(
                        matches!(cursor.next(), Some(Err(Error::Type(_)))),
                        "{query}"
                    );
                    assert!(cursor.next().is_none());
                }
            }
        }
    }
    db.execute("CREATE (:AfterError)").unwrap();
}

#[test]
fn aggregate_input_errors_roll_back_writes_and_release_locks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("aggregate-inputs.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.create_unique_constraint("Saved", "key").unwrap();
    db.execute("CREATE (:Saved {key:1, input:'bad'})").unwrap();
    let saved = db.execute("MATCH (n) RETURN n").unwrap().rows;
    let parameters = Parameters::from([("input".into(), json!("bad"))]);
    for function in ["sum", "avg"] {
        for query in [
            format!("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary) RETURN {function}(n.input)"),
            format!(
                "MATCH (n:Saved) SET n.key = 2 WITH {function}(n.input) AS total CREATE (:Temporary {{value:total}}) RETURN total"
            ),
        ] {
            type_error(db.execute(&query), &query);
            assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
        }
        for cursor_failure in [false, true] {
            let mut tx = db.transaction().unwrap();
            tx.execute("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary)")
                .unwrap();
            tx.create_unique_constraint("Temporary", "key").unwrap();
            if cursor_failure {
                let query =
                    format!("RETURN 1 AS value UNION ALL RETURN {function}($input) AS value");
                let mut read = tx.prepare_read_with(&query, &parameters).unwrap();
                let mut cursor = read.cursor().unwrap();
                assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
                assert!(
                    matches!(cursor.next(), Some(Err(Error::Type(_)))),
                    "{query}"
                );
                assert!(cursor.next().is_none());
            } else {
                let query = format!("RETURN {function}($input)");
                type_error(tx.execute_with(&query, &parameters), &query);
            }
            assert!(matches!(
                tx.execute("RETURN 1"),
                Err(Error::TransactionAborted)
            ));
            let mut other = Database::open_with_options(
                &path,
                &OpenOptions {
                    busy_timeout: Duration::ZERO,
                    ..OpenOptions::default()
                },
            )
            .unwrap();
            assert_eq!(other.execute("MATCH (n) RETURN n").unwrap().rows, saved);
            other.execute("CREATE (n:LockProbe) DELETE n").unwrap();
            assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
            assert_eq!(db.unique_constraints().unwrap().len(), 1);
            assert!(matches!(
                db.execute("CREATE (:Saved {key:1})"),
                Err(Error::Constraint(_))
            ));
        }
    }
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
    db.execute("MATCH (n:Saved) SET n.key = 2").unwrap();
}
