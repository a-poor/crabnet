use std::time::Duration;

use crabnet::{CypherValue, Database, Error, OpenOptions, Parameters, ReadMode, compile_read};
use serde_json::{Value, json};

fn expressions(bound: &str) -> [String; 5] {
    [
        format!("substring('hé🙂', {bound})"),
        format!("substring('hé🙂', {bound}, 1)"),
        format!("substring('hé🙂', 1, {bound})"),
        format!("left('hé🙂', {bound})"),
        format!("right('hé🙂', {bound})"),
    ]
}

fn argument_error(result: crabnet::Result<crabnet::QueryResult>, context: &str) {
    assert!(
        matches!(result, Err(Error::Argument(_))),
        "{context}: {result:?}"
    );
}

#[test]
fn negative_bounds_fail_for_literals_parameters_properties_and_computed_values() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input)-[:R]->(:End)").unwrap();
    for bound in [-1, -9, i64::MIN] {
        let parameters = Parameters::from([("bound".into(), json!(bound))]);
        db.execute_with(
            "MATCH (n:Input)-[r:R]->() SET n.bound = $bound, r.bound = $bound",
            &parameters,
        )
        .unwrap();
        for (prefix, input) in [
            ("", "$bound"),
            ("MATCH (n:Input) ", "n.bound"),
            ("MATCH ()-[r:R]->() ", "r.bound"),
            ("UNWIND [$bound, $bound] AS b ", "b"),
        ] {
            for expression in expressions(input) {
                let query = format!("{prefix}RETURN {expression}");
                argument_error(db.execute_with(&query, &parameters), &query);
            }
        }
    }
    for bound in ["-1", "0 - 1", "CASE WHEN true THEN -1 ELSE 1 END"] {
        for expression in expressions(bound) {
            argument_error(db.execute(&format!("RETURN {expression}")), &expression);
        }
    }
    for expression in [
        "substring('', -1)",
        "substring('', 0, -1)",
        "substring('abc', 9, -1)",
        "substring('abc', -1, 0)",
        "left('', -1)",
        "right('', -1)",
    ] {
        argument_error(db.execute(&format!("RETURN {expression}")), expression);
    }
}

#[test]
fn null_and_lazy_evaluation_precede_bound_checks_but_not_argument_evaluation_errors() {
    let mut db = Database::in_memory().unwrap();
    let parameters = Parameters::from([("bound".into(), json!(-1))]);
    for expression in [
        "substring(null, -1)",
        "substring(null, 0, -1)",
        "substring('abc', null, -1)",
        "substring('abc', -1, null)",
        "left(null, -1)",
        "right(null, -1)",
        "substring(null, $bound)",
        "left(null, $bound)",
        "right(null, $bound)",
    ] {
        assert_eq!(
            db.execute_with(&format!("RETURN {expression}"), &parameters)
                .unwrap()
                .rows,
            [vec![CypherValue::Null]],
            "{expression}"
        );
    }
    for input in ["-1", "$bound"] {
        for expression in expressions(input) {
            for query in [
                format!("RETURN CASE WHEN false THEN {expression} ELSE 1 END"),
                format!("RETURN coalesce(1, {expression})"),
            ] {
                let read = compile_read(&query).unwrap();
                assert_eq!(
                    db.execute_compiled_with(&read, &parameters).unwrap().rows,
                    [vec![CypherValue::Integer(1)]],
                    "{query}"
                );
            }
            assert!(
                db.execute_with(
                    &format!("MATCH (n:Absent) RETURN {expression}"),
                    &parameters
                )
                .unwrap()
                .rows
                .is_empty()
            );
        }
    }
    argument_error(
        db.execute("RETURN left(null, size(substring('abc', -1)))"),
        "nested error",
    );
    let invalid = Parameters::from([("text".into(), json!(false))]);
    for query in [
        "RETURN substring($text, -1)",
        "RETURN left($text, -1)",
        "RETURN right($text, -1)",
    ] {
        assert!(
            matches!(db.execute_with(query, &invalid), Err(Error::Type(_))),
            "{query}"
        );
    }
    assert!(matches!(
        db.execute("RETURN substring('abc', -1, 'bad')"),
        Err(Error::Semantic(_))
    ));
    let invalid = Parameters::from([("length".into(), json!(false))]);
    assert!(matches!(
        db.execute_with("RETURN substring('abc', -1, $length)", &invalid),
        Err(Error::Type(_))
    ));
}

#[test]
fn composed_and_mixed_graph_bounds_preserve_errors() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let parameters = Parameters::from([("bound".into(), json!(-1)), ("pick".into(), json!(1))]);
    for expression in expressions("$bound") {
        for output in [
            format!("{expression} IS NULL"),
            format!("coalesce({expression}, '')"),
            format!("[{expression}]"),
            format!("{{value:{expression}}}"),
            format!("count({expression})"),
            format!("collect({expression})"),
            format!("size({expression})"),
            format!("[x IN [1] | {expression}]"),
        ] {
            let query = format!("RETURN {output}");
            argument_error(db.execute_with(&query, &parameters), &query);
        }
    }
    for expression in expressions("xs[$pick]") {
        let query = format!("MATCH (n:Start) WITH [n, $bound, null] AS xs RETURN {expression}");
        argument_error(db.execute_with(&query, &parameters), &query);
    }
}

#[test]
fn compiled_reads_rebind_after_failure_and_preserve_valid_unicode_bags() {
    let mut db = Database::in_memory().unwrap();
    let query = "UNWIND [$text, $text] AS s RETURN substring(s, $bound), substring(s, $bound, 1), substring(s, 1, $bound), left(s, $bound), right(s, $bound)";
    let read = compile_read(query).unwrap();
    for (bound, expected) in [
        (0, ["hé🙂", "h", "", "", ""]),
        (1, ["é🙂", "é", "é", "h", "🙂"]),
        (3, ["", "", "é🙂", "hé🙂", "hé🙂"]),
        (9, ["", "", "é🙂", "hé🙂", "hé🙂"]),
    ] {
        let mut parameters =
            Parameters::from([("text".into(), json!("hé🙂")), ("bound".into(), json!(-1))]);
        argument_error(
            db.execute_compiled_with(&read, &parameters),
            "compiled failure",
        );
        parameters.insert("bound".into(), json!(bound));
        let row = expected.map(|s| CypherValue::String(s.into())).to_vec();
        let result = db.execute_compiled_with(&read, &parameters).unwrap();
        assert_eq!(result.rows, [row.clone(), row]);
        assert_eq!(result, db.execute_with(query, &parameters).unwrap());
        parameters.insert("bound".into(), Value::Null);
        assert_eq!(
            db.execute_compiled_with(&read, &parameters).unwrap().rows,
            [vec![CypherValue::Null; 5], vec![CypherValue::Null; 5]]
        );
    }
}

#[test]
fn cursor_bound_errors_fuse_and_prepared_reads_can_restart() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let parameters = Parameters::from([("bound".into(), json!(-1))]);
    for expression in expressions("$bound") {
        for (prefix, mode) in [
            ("", ReadMode::Incremental),
            ("MATCH p = (:Start)-[:R*1..40]->() ", ReadMode::Incremental),
            (
                "MATCH p = shortestPath((:Start)-[:R*]->(:End)) ",
                ReadMode::Buffered,
            ),
        ] {
            let query = format!("RETURN 1 AS value UNION ALL {prefix}RETURN {expression} AS value");
            argument_error(db.execute_with(&query, &parameters), &query);
            let mut read = db.prepare_read_with(&query, &parameters).unwrap();
            assert_eq!(read.mode(), mode);
            for _ in 0..2 {
                if mode == ReadMode::Buffered {
                    assert!(matches!(read.cursor(), Err(Error::Argument(_))), "{query}");
                } else {
                    let mut cursor = read.cursor().unwrap();
                    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
                    assert!(
                        matches!(cursor.next(), Some(Err(Error::Argument(_)))),
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
fn bound_errors_roll_back_writes_and_compiled_or_cursor_transactions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bounds.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.create_unique_constraint("Saved", "key").unwrap();
    db.execute("CREATE (:Saved {key:1, bound:-1})").unwrap();
    let saved = db.execute("MATCH (n) RETURN n").unwrap().rows;
    for expression in expressions("n.bound") {
        for suffix in [
            format!("RETURN {expression}"),
            format!("SET n.result = {expression}"),
        ] {
            let query = format!("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary) {suffix}");
            argument_error(db.execute(&query), &query);
            assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
        }
    }
    let parameters = Parameters::from([("bound".into(), json!(-1))]);
    for expression in expressions("$bound") {
        let query = format!("RETURN 1 AS value UNION ALL RETURN {expression} AS value");
        let compiled = compile_read(&query).unwrap();
        for mode in 0..3 {
            let mut tx = db.transaction().unwrap();
            tx.execute("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary)")
                .unwrap();
            tx.create_unique_constraint("Temporary", "key").unwrap();
            match mode {
                0 => argument_error(tx.execute_with(&query, &parameters), &query),
                1 => argument_error(tx.execute_compiled_with(&compiled, &parameters), &query),
                _ => {
                    let mut read = tx.prepare_read_with(&query, &parameters).unwrap();
                    let mut cursor = read.cursor().unwrap();
                    assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
                    assert!(matches!(cursor.next(), Some(Err(Error::Argument(_)))));
                    assert!(cursor.next().is_none());
                }
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
