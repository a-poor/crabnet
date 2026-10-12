use std::time::Duration;

use crabnet::{CypherValue, Database, Error, OpenOptions, Parameters, ReadMode};
use serde_json::{Value, json};

fn expressions(input: &str) -> Vec<String> {
    [
        format!("size({input})"),
        format!("head({input})"),
        format!("last({input})"),
        format!("tail({input})"),
        format!("reverse({input})"),
        format!("[x IN {input} | x]"),
        format!("[x IN {input} WHERE false | x]"),
        format!("all(x IN {input} WHERE true)"),
        format!("any(x IN {input} WHERE true)"),
        format!("none(x IN {input} WHERE true)"),
        format!("single(x IN {input} WHERE true)"),
    ]
    .into()
}

fn type_error(result: crabnet::Result<crabnet::QueryResult>, context: &str) {
    assert!(
        matches!(result, Err(Error::Type(_))),
        "{context}: {result:?}"
    );
}

#[test]
fn invalid_parameter_inputs_raise_type_errors_instead_of_null() {
    let mut db = Database::in_memory().unwrap();
    for input in [
        json!(1),
        json!(1.5),
        json!(true),
        json!({}),
        json!({"a": [1]}),
    ] {
        let parameters = Parameters::from([("input".into(), input.clone())]);
        for expression in expressions("$input") {
            let query = format!("RETURN {expression} AS value");
            type_error(
                db.execute_with(&query, &parameters),
                &format!("{query}: {input}"),
            );
        }
    }
    let parameters = Parameters::from([("input".into(), json!("text"))]);
    for expression in expressions("$input") {
        if expression.starts_with("size(") || expression.starts_with("reverse(") {
            continue;
        }
        let query = format!("RETURN {expression}");
        type_error(db.execute_with(&query, &parameters), &query);
    }
}

#[test]
fn known_invalid_kinds_are_rejected_before_execution_even_in_unused_branches() {
    let mut db = Database::in_memory().unwrap();
    for input in ["1", "1.5", "true", "{}", "{a: [1]}"] {
        for expression in expressions(input) {
            for query in [
                format!("RETURN {expression}"),
                format!("RETURN CASE WHEN false THEN {expression} ELSE null END"),
                format!("MATCH (n:Absent) RETURN {expression}"),
            ] {
                assert!(
                    matches!(db.execute(&query), Err(Error::Semantic(_))),
                    "{query}"
                );
            }
        }
    }
    for expression in expressions("input") {
        for prefix in ["WITH 1 AS input", "UNWIND [1, 2] AS input"] {
            let query = format!("{prefix} RETURN {expression}");
            assert!(
                matches!(db.execute(&query), Err(Error::Semantic(_))),
                "{query}"
            );
        }
    }
    for expression in expressions("{node:n}") {
        let query = format!("MATCH (n) RETURN {expression}");
        assert!(
            matches!(db.execute(&query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
}

#[test]
fn nulls_empty_lists_strings_and_lazy_runtime_branches_keep_their_semantics() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input)").unwrap();
    let null = Parameters::from([("input".into(), Value::Null)]);
    let invalid = Parameters::from([("input".into(), json!(7))]);
    for expression in expressions("$input") {
        let query = format!("RETURN {expression}");
        assert_eq!(
            db.execute_with(&query, &null).unwrap().rows,
            [vec![CypherValue::Null]],
            "{query}"
        );
        for query in [
            format!("RETURN CASE WHEN false THEN {expression} ELSE 1 END"),
            format!("RETURN coalesce(1, {expression})"),
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
    for expression in expressions("n.missing") {
        let query = format!("MATCH (n:Input) RETURN {expression}");
        assert_eq!(
            db.execute(&query).unwrap().rows,
            [vec![CypherValue::Null]],
            "{query}"
        );
    }
    assert_eq!(db.execute("RETURN size([]), head([]), last([]), tail([]), reverse([]), [x IN [] | x], all(x IN [] WHERE false), any(x IN [] WHERE true), none(x IN [] WHERE true), single(x IN [] WHERE true)").unwrap().rows,
        [vec![CypherValue::Integer(0), CypherValue::Null, CypherValue::Null, CypherValue::List(vec![]), CypherValue::List(vec![]), CypherValue::List(vec![]), CypherValue::Boolean(true), CypherValue::Boolean(false), CypherValue::Boolean(true), CypherValue::Boolean(false)]]);
    assert_eq!(
        db.execute(
            "RETURN size('hé🙂'), reverse('hé🙂'), tail([1, 1, null]), reverse([1, 1, null])"
        )
        .unwrap()
        .rows,
        [vec![
            CypherValue::Integer(3),
            CypherValue::String("🙂éh".into()),
            CypherValue::List(vec![CypherValue::Integer(1), CypherValue::Null]),
            CypherValue::List(vec![
                CypherValue::Null,
                CypherValue::Integer(1),
                CypherValue::Integer(1)
            ])
        ]]
    );
    // General subscript and UNWIND contracts are independent of list functions.
    assert_eq!(
        db.execute("RETURN 'abc'[1..], null[1..], [1, 2][1.5..]")
            .unwrap()
            .rows,
        [vec![CypherValue::Null; 3]]
    );
}

#[test]
fn composition_cannot_hide_invalid_list_inputs() {
    let mut db = Database::in_memory().unwrap();
    let parameters = Parameters::from([("input".into(), json!(7))]);
    for expression in expressions("$input") {
        for output in [
            format!("{expression} IS NULL"),
            format!("coalesce({expression}, 1)"),
            format!("[{expression}]"),
            format!("{{value: {expression}}}"),
            format!("count({expression})"),
            format!("collect({expression})"),
            format!("size([{expression}])"),
            format!("size([{expression}]) IS NULL"),
            format!("reverse([{expression}]) IS NULL"),
        ] {
            let query = format!("RETURN {output}");
            type_error(db.execute_with(&query, &parameters), &query);
        }
    }
}

#[test]
fn dynamically_selected_graph_values_obey_the_same_list_input_rules() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    for pick in 0..5 {
        let parameters =
            Parameters::from([("pick".into(), json!(pick)), ("input".into(), json!(7))]);
        for expression in expressions("xs[$pick]") {
            let query = format!(
                "MATCH p = (a:Start)-[r:R]->(b) WITH [a, r, p, {{node:a}}, $input] AS xs RETURN {expression}"
            );
            type_error(
                db.execute_with(&query, &parameters),
                &format!("{query}: {pick}"),
            );
        }
    }
    let parameters = Parameters::from([("pick".into(), json!(2))]);
    for expression in expressions("xs[$pick]") {
        let query = format!("MATCH (n:Start) WITH [n, [], null] AS xs RETURN {expression}");
        assert_eq!(
            db.execute_with(&query, &parameters).unwrap().rows,
            [vec![CypherValue::Null]],
            "{query}"
        );
    }
}

#[test]
fn cursor_errors_preserve_prior_rows_fuse_and_allow_reuse() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let parameters = Parameters::from([("input".into(), json!(7))]);
    for expression in expressions("$input") {
        for (prefix, mode) in [
            ("", ReadMode::Incremental),
            ("MATCH p = (:Start)-[:R*1..40]->() ", ReadMode::Incremental),
            (
                "MATCH p = shortestPath((:Start)-[:R*]->(:End)) ",
                ReadMode::Buffered,
            ),
        ] {
            let query = format!("RETURN 1 AS value UNION ALL {prefix}RETURN {expression} AS value");
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
fn invalid_list_inputs_roll_back_statement_and_transaction_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lists.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.create_unique_constraint("Saved", "key").unwrap();
    db.execute("CREATE (:Saved {key:1, input:7})").unwrap();
    let saved = db.execute("MATCH (n) RETURN n").unwrap().rows;
    let parameters = Parameters::from([("input".into(), json!(7))]);
    for expression in expressions("n.input") {
        for suffix in [
            format!("RETURN {expression}"),
            format!("SET n.result = {expression}"),
        ] {
            let query = format!("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary) {suffix}");
            type_error(db.execute(&query), &query);
            assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
        }
    }
    for expression in expressions("$input") {
        let mut tx = db.transaction().unwrap();
        tx.execute("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary)")
            .unwrap();
        tx.create_unique_constraint("Temporary", "key").unwrap();
        {
            let query = format!("RETURN 1 AS value UNION ALL RETURN {expression} AS value");
            let mut read = tx.prepare_read_with(&query, &parameters).unwrap();
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
            assert!(
                matches!(cursor.next(), Some(Err(Error::Type(_)))),
                "{query}"
            );
            assert!(cursor.next().is_none());
        }
        assert!(matches!(
            tx.execute("RETURN 1"),
            Err(Error::TransactionAborted)
        ));
        // The aborted transaction releases its writer lock before handle drop.
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
        assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
        assert_eq!(db.unique_constraints().unwrap().len(), 1);
        assert!(matches!(
            db.execute("CREATE (:Saved {key:1})"),
            Err(Error::Constraint(_))
        ));
    }
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
    db.execute("MATCH (n:Saved) SET n.key = 2").unwrap();
}
