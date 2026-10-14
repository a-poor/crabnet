use std::time::Duration;

use crabnet::{CypherValue, Database, Error, OpenOptions, Parameters, ReadMode};
use serde_json::{Value, json};

// Replace each argument in turn; unchanged positions contain valid inputs.
fn expressions(input: &str, integer: bool) -> Vec<String> {
    if integer {
        return vec![
            format!("substring('abc', {input})"),
            format!("substring('abc', {input}, 1)"),
            format!("substring('abc', 0, {input})"),
            format!("left('abc', {input})"),
            format!("right('abc', {input})"),
        ];
    }
    ["toLower", "toUpper", "trim", "ltrim", "rtrim"]
        .map(|function| format!("{function}({input})"))
        .into_iter()
        .chain([
            format!("substring({input}, 0)"),
            format!("substring({input}, 0, 1)"),
            format!("left({input}, 1)"),
            format!("right({input}, 1)"),
            format!("replace({input}, 'a', 'b')"),
            format!("replace('abc', {input}, 'b')"),
            format!("replace('abc', 'a', {input})"),
            format!("split({input}, ',')"),
            format!("split('a,b', {input})"),
        ])
        .collect()
}

fn all_expressions(input: &str) -> Vec<String> {
    expressions(input, false)
        .into_iter()
        .chain(expressions(input, true))
        .collect()
}

fn type_error(result: crabnet::Result<crabnet::QueryResult>, context: &str) {
    assert!(
        matches!(result, Err(Error::Type(_))),
        "{context}: {result:?}"
    );
}

#[test]
fn invalid_argument_kinds_fail_for_parameters_and_entity_properties() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input)-[:R]->(:End)").unwrap();
    for integer in [false, true] {
        let mut inputs = vec![
            json!(1.5),
            json!(true),
            json!(false),
            json!([]),
            json!([1]),
            json!({}),
            json!({"x":1}),
        ];
        inputs.push(if integer { json!("1") } else { json!(1) });
        if integer {
            inputs.push(json!(1.0));
        }
        for input in inputs {
            let parameters = Parameters::from([("input".into(), input.clone())]);
            db.execute_with(
                "MATCH (n:Input)-[r:R]->() SET n.input = $input, r.input = $input",
                &parameters,
            )
            .unwrap();
            for (prefix, argument) in [
                ("", "$input"),
                ("MATCH (n:Input) ", "n.input"),
                ("MATCH ()-[r:R]->() ", "r.input"),
            ] {
                for expression in expressions(argument, integer) {
                    let query = format!("{prefix}RETURN {expression}");
                    type_error(
                        db.execute_with(&query, &parameters),
                        &format!("{query}: {input}"),
                    );
                }
            }
        }
    }
}

#[test]
fn statically_invalid_arguments_are_rejected_including_unused_branches() {
    let mut db = Database::in_memory().unwrap();
    for integer in [false, true] {
        let mut inputs = vec!["1.5", "true", "false", "[]", "[1]", "{}", "{x:1}"];
        inputs.push(if integer { "'1'" } else { "1" });
        if integer {
            inputs.push("1.0");
        }
        for input in inputs {
            for expression in expressions(input, integer) {
                for query in [
                    format!("RETURN {expression}"),
                    format!("RETURN CASE WHEN false THEN {expression} ELSE 1 END"),
                    format!("RETURN coalesce(1, {expression})"),
                    format!("MATCH (n:Absent) RETURN {expression}"),
                ] {
                    let result = db.execute(&query);
                    assert!(
                        matches!(result, Err(Error::Semantic(_))),
                        "{query}: {result:?}"
                    );
                }
            }
            for expression in expressions("x", integer) {
                for prefix in [
                    format!("WITH {input} AS x"),
                    format!("UNWIND [{input}, {input}] AS x"),
                ] {
                    let query = format!("{prefix} RETURN {expression}");
                    let result = db.execute(&query);
                    assert!(
                        matches!(result, Err(Error::Semantic(_))),
                        "{query}: {result:?}"
                    );
                }
            }
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
        for expression in all_expressions(input) {
            let query = format!("MATCH p = (n)-[r:R]->() RETURN {expression}");
            let result = db.execute(&query);
            assert!(
                matches!(result, Err(Error::Semantic(_))),
                "{query}: {result:?}"
            );
        }
    }
}

#[test]
fn nulls_and_lazy_branches_preserve_existing_behavior() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Input)").unwrap();
    let null = Parameters::from([("input".into(), Value::Null)]);
    let invalid = Parameters::from([("input".into(), json!({}))]);
    for (prefix, argument) in [
        ("", "null"),
        ("", "$input"),
        ("MATCH (n:Input) ", "n.missing"),
    ] {
        for expression in all_expressions(argument) {
            let query = format!("{prefix}RETURN {expression}");
            assert_eq!(
                db.execute_with(&query, &null).unwrap().rows,
                [vec![CypherValue::Null]],
                "{query}"
            );
        }
    }
    for expression in all_expressions("$input") {
        for query in [
            format!("RETURN CASE WHEN false THEN {expression} ELSE 1 END"),
            format!("RETURN CASE 1 WHEN 2 THEN {expression} ELSE 1 END"),
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
    // A null argument continues to propagate even alongside an invalid dynamic
    // kind; known invalid kinds are still rejected during planning.
    for query in [
        "RETURN substring(null, $input)",
        "RETURN substring($input, null)",
        "RETURN substring('abc', null, $input)",
        "RETURN substring('abc', $input, null)",
        "RETURN left(null, $input)",
        "RETURN left($input, null)",
        "RETURN right(null, $input)",
        "RETURN right($input, null)",
        "RETURN replace(null, $input, 'x')",
        "RETURN replace($input, null, 'x')",
        "RETURN replace('x', $input, null)",
        "RETURN split(null, $input)",
        "RETURN split($input, null)",
    ] {
        assert_eq!(
            db.execute_with(query, &invalid).unwrap().rows,
            [vec![CypherValue::Null]],
            "{query}"
        );
    }
    for query in [
        "RETURN substring(null, 'bad')",
        "RETURN replace(null, 1, 'x')",
        "RETURN left(1, null)",
    ] {
        assert!(
            matches!(db.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
}

#[test]
fn unicode_empty_strings_duplicates_and_integer_boundaries_are_preserved() {
    let mut db = Database::in_memory().unwrap();
    let parameters = Parameters::from([("text".into(), json!("hé🙂")), ("count".into(), json!(2))]);
    let row = vec![
        json!("é🙂"),
        json!("é"),
        json!("hé"),
        json!("é🙂"),
        json!("hé🙂hé🙂"),
        json!(["h", "é", "🙂"]),
        json!("ABÉ🙂"),
    ]
    .into_iter()
    .map(|value| CypherValue::try_from(value).unwrap())
    .collect::<Vec<_>>();
    let query = "UNWIND [$text, $text] AS s RETURN substring(s, 1), substring(s, 1, 1), left(s, $count), right(s, $count), replace('xx', 'x', s), split(s, ''), toUpper('abÉ🙂')";
    assert_eq!(
        db.execute_with(query, &parameters).unwrap().rows,
        [row.clone(), row]
    );
    for expression in expressions("''", false) {
        // Every string position accepts the empty string, including delimiters.
        db.execute(&format!("RETURN {expression}")).unwrap();
    }
    assert_eq!(db.execute("RETURN substring('abc', 9), substring('abc', 1, 0), left('abc', 0), right('abc', 0), left('abc', 9), right('abc', 9)").unwrap().rows,
        [vec![json!(""), json!(""), json!(""), json!(""), json!("abc"), json!("abc")].into_iter().map(|value| CypherValue::try_from(value).unwrap()).collect::<Vec<_>>()]);
}

#[test]
fn composition_and_mixed_graph_selections_preserve_input_errors() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let parameters = Parameters::from([("input".into(), json!({}))]);
    for expression in all_expressions("$input") {
        for output in [
            format!("{expression} IS NULL"),
            format!("coalesce({expression}, 1)"),
            format!("[{expression}]"),
            format!("{{value: {expression}}}"),
            format!("count({expression})"),
            format!("collect({expression})"),
            format!("size([{expression}]) IS NULL"),
            format!("toString({expression})"),
            format!("[x IN [1] | {expression}]"),
        ] {
            let query = format!("RETURN {output}");
            type_error(db.execute_with(&query, &parameters), &query);
        }
    }
    for integer in [false, true] {
        for pick in 0..8 {
            let parameters = Parameters::from([
                ("pick".into(), json!(pick)),
                ("valid".into(), if integer { json!(1) } else { json!("a") }),
            ]);
            for expression in expressions("xs[$pick]", integer) {
                let query = format!(
                    "MATCH p = (n:Start)-[r:R]->() WITH [n, r, p, {{node:n}}, [], false, null, $valid] AS xs RETURN {expression}"
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
}

#[test]
fn cursor_errors_fuse_and_leave_prepared_reads_reusable() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let parameters = Parameters::from([("input".into(), json!({}))]);
    for expression in all_expressions("$input") {
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
fn string_input_errors_roll_back_writes_and_release_transaction_locks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("strings.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.create_unique_constraint("Saved", "key").unwrap();
    db.execute("CREATE (:Saved {key:1, input:{}})").unwrap();
    let saved = db.execute("MATCH (n) RETURN n").unwrap().rows;
    let parameters = Parameters::from([("input".into(), json!({}))]);
    for expression in all_expressions("n.input") {
        for suffix in [
            format!("RETURN {expression}"),
            format!("SET n.result = {expression}"),
        ] {
            let query = format!("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary) {suffix}");
            type_error(db.execute(&query), &query);
            assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
        }
    }
    for expression in all_expressions("$input") {
        for cursor_failure in [false, true] {
            let mut tx = db.transaction().unwrap();
            tx.execute("MATCH (n:Saved) SET n.key = 2 CREATE (:Temporary)")
                .unwrap();
            tx.create_unique_constraint("Temporary", "key").unwrap();
            if cursor_failure {
                let query = format!("RETURN 1 AS value UNION ALL RETURN {expression} AS value");
                let mut read = tx.prepare_read_with(&query, &parameters).unwrap();
                let mut cursor = read.cursor().unwrap();
                assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Integer(1)]);
                assert!(
                    matches!(cursor.next(), Some(Err(Error::Type(_)))),
                    "{query}"
                );
                assert!(cursor.next().is_none());
            } else {
                let query = format!("RETURN {expression}");
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
