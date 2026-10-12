use std::time::Duration;

use crabnet::{CypherValue, Database, Error, OpenOptions, Parameters, ReadMode};
use serde_json::{Value, json};

const FUNCTIONS: [&str; 15] = [
    "abs", "sign", "ceil", "floor", "round", "sqrt", "exp", "log", "log10", "sin", "cos", "tan",
    "asin", "acos", "atan",
];

fn type_error(result: crabnet::Result<crabnet::QueryResult>, context: &str) {
    assert!(
        matches!(result, Err(Error::Type(_))),
        "{context}: {result:?}"
    );
}

#[test]
fn non_numeric_parameters_and_properties_raise_type_errors() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input)-[:R]->(:End)").unwrap();
    for input in [
        json!("1"),
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
        for function in FUNCTIONS {
            for query in [
                format!("RETURN {function}($input)"),
                format!("MATCH (n:Input) RETURN {function}(n.input)"),
                format!("MATCH ()-[r:R]->() RETURN {function}(r.input)"),
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
fn known_non_numeric_inputs_fail_during_planning() {
    let mut db = Database::in_memory().unwrap();
    for function in FUNCTIONS {
        for input in ["'1'", "true", "false", "[]", "[1]", "{}", "{x: 1}"] {
            let expression = format!("{function}({input})");
            for query in [
                format!("RETURN {expression}"),
                format!("RETURN CASE WHEN false THEN {expression} ELSE 1 END"),
                format!("RETURN coalesce(1, {expression})"),
                format!("MATCH (n:Absent) RETURN {expression}"),
                format!("WITH {input} AS x RETURN {function}(x)"),
                format!("UNWIND [{input}, {input}] AS x RETURN {function}(x)"),
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
fn nulls_valid_numbers_and_lazy_branches_keep_their_semantics() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input)").unwrap();
    for function in FUNCTIONS {
        for query in [
            format!("RETURN {function}(null)"),
            format!("MATCH (n:Input) RETURN {function}(n.missing)"),
        ] {
            assert_eq!(
                db.execute(&query).unwrap().rows,
                [vec![CypherValue::Null]],
                "{query}"
            );
        }
        for input in [Value::Null, json!(1), json!(1.0)] {
            let parameters = Parameters::from([("input".into(), input.clone())]);
            let query = format!("UNWIND [$input, $input] AS x RETURN {function}(x)");
            let rows = db.execute_with(&query, &parameters).unwrap().rows;
            assert_eq!(rows.len(), 2, "{query}");
            assert_eq!(rows[0], rows[1]);
            match (&input, function, &rows[0][0]) {
                (Value::Null, _, CypherValue::Null) => {}
                (_, "sign", CypherValue::Integer(1)) => {}
                (Value::Number(n), "abs", CypherValue::Integer(1)) if n.is_i64() => {}
                (_, _, CypherValue::Float(value)) if value.is_finite() => {}
                _ => panic!("{query}: {input}: {rows:?}"),
            }
        }
        let invalid = Parameters::from([("input".into(), json!("bad"))]);
        for query in [
            format!("RETURN CASE WHEN false THEN {function}($input) ELSE 1 END"),
            format!("RETURN CASE 1 WHEN 2 THEN {function}($input) ELSE 1 END"),
            format!("RETURN coalesce(1, {function}($input))"),
        ] {
            assert_eq!(
                db.execute_with(&query, &invalid).unwrap().rows,
                [vec![CypherValue::Integer(1)]],
                "{query}"
            );
            let mut read = db.prepare_read_with(&query, &invalid).unwrap();
            assert_eq!(
                read.cursor()
                    .unwrap()
                    .collect::<crabnet::Result<Vec<_>>>()
                    .unwrap(),
                [vec![CypherValue::Integer(1)]],
                "{query}"
            );
        }
    }
}

#[test]
fn composition_cannot_hide_numeric_input_errors() {
    let mut db = Database::in_memory().unwrap();
    let parameters = Parameters::from([("input".into(), json!("bad"))]);
    for function in FUNCTIONS {
        let expression = format!("{function}($input)");
        for output in [
            format!("{expression} IS NULL"),
            format!("coalesce({expression}, 1)"),
            format!("[{expression}]"),
            format!("{{value: {expression}}}"),
            format!("count({expression})"),
            format!("collect({expression})"),
            format!("size([{expression}]) IS NULL"),
            format!("abs({expression}) IS NULL"),
            format!("toString({expression})"),
            format!("[x IN [1] | {expression}]"),
        ] {
            let query = format!("RETURN {output}");
            type_error(db.execute_with(&query, &parameters), &query);
        }
    }
}

#[test]
fn mixed_graph_selections_validate_the_selected_numeric_input() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    for function in FUNCTIONS {
        for pick in 0..8 {
            let parameters = Parameters::from([("pick".into(), json!(pick))]);
            let query = format!(
                "MATCH p = (n:Start)-[r:R]->() WITH [n, r, p, {{node:n}}, [], '1', null, 1] AS xs RETURN {function}(xs[$pick])"
            );
            let result = db.execute_with(&query, &parameters);
            match pick {
                0..6 => type_error(result, &format!("{query}: {pick}")),
                6 => assert_eq!(result.unwrap().rows, [vec![CypherValue::Null]]),
                _ => {
                    result.unwrap();
                }
            }
        }
    }
}

#[test]
fn numeric_input_errors_fuse_cursors_and_allow_reuse() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let parameters = Parameters::from([("input".into(), json!("bad"))]);
    for function in FUNCTIONS {
        for (prefix, mode) in [
            ("", ReadMode::Incremental),
            ("MATCH p = (:Start)-[:R*1..40]->() ", ReadMode::Incremental),
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
                    assert_eq!(
                        cursor.next().unwrap().unwrap(),
                        [CypherValue::Integer(1)],
                        "{query}"
                    );
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
fn numeric_input_errors_roll_back_writes_and_release_locks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numeric-inputs.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.create_unique_constraint("Saved", "key").unwrap();
    db.execute("CREATE (:Saved {key:1, input:'bad'})").unwrap();
    let saved = db.execute("MATCH (n) RETURN n").unwrap().rows;
    let parameters = Parameters::from([("input".into(), json!("bad"))]);
    for function in FUNCTIONS {
        for suffix in [
            format!("RETURN {function}(n.input)"),
            format!("SET n.result = {function}(n.input)"),
        ] {
            let query = format!("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary) {suffix}");
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
