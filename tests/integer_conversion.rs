use crabnet::{CypherValue, Database, Error, Parameters, ReadMode, compile_read};
use serde_json::{Value, json};

fn converted(value: Option<i64>) -> CypherValue {
    value.map_or(CypherValue::Null, CypherValue::Integer)
}

#[test]
fn float_bounds_truncate_without_saturating_in_eager_and_compiled_reads() {
    let mut db = Database::in_memory().unwrap();
    let query = "RETURN toInteger($x)";
    let plan = compile_read(query).unwrap();
    // Explicit IEEE-754 neighbors around +/- 2^63, with independent integer
    // expectations. i64::MAX itself is not exactly representable as f64.
    for (input, expected) in [
        (9_223_372_036_854_775_808.0, None),
        (9_223_372_036_854_774_784.0, Some(9_223_372_036_854_774_784)),
        (-9_223_372_036_854_775_808.0, Some(i64::MIN)),
        (-9_223_372_036_854_777_856.0, None),
        (
            -9_223_372_036_854_774_784.0,
            Some(-9_223_372_036_854_774_784),
        ),
        (f64::MAX, None),
        (-f64::MAX, None),
        (4.9, Some(4)),
        (-4.9, Some(-4)),
        (-0.0, Some(0)),
        (f64::MIN_POSITIVE, Some(0)),
    ] {
        let params = Parameters::from([("x".into(), json!(input))]);
        let rows = vec![vec![converted(expected)]];
        assert_eq!(
            db.execute_with(query, &params).unwrap().rows,
            rows,
            "{input}"
        );
        assert_eq!(
            db.execute_compiled_with(&plan, &params).unwrap().rows,
            rows,
            "{input}"
        );
    }
    assert_eq!(
        db.execute(
            "RETURN toInteger(1e30), toInteger(-1e30), toInteger(2.0 * 4.611686018427388e18)"
        )
        .unwrap()
        .rows,
        [vec![CypherValue::Null; 3]]
    );
}

#[test]
fn integer_text_never_rounds_through_a_float_after_overflow() {
    let mut db = Database::in_memory().unwrap();
    let plan = compile_read("RETURN toInteger($x)").unwrap();
    for (input, expected) in [
        ("9223372036854775807", Some(i64::MAX)),
        ("-9223372036854775808", Some(i64::MIN)),
        ("9223372036854775808", None),
        ("-9223372036854775809", None),
        (" -0009223372036854775809 ", None),
        ("+0009223372036854775807", Some(i64::MAX)),
        ("99999999999999999999999999999999999999999", None),
        ("-99999999999999999999999999999999999999999", None),
        (" 7.8 ", Some(7)),
        ("-7.8e1", Some(-78)),
        ("9.223372036854776e18", None),
        ("-9.223372036854776e18", Some(i64::MIN)),
        ("1e309", None),
        ("NaN", None),
        ("inf", None),
        ("12\0", None),
        ("", None),
        ("+", None),
    ] {
        let params = Parameters::from([("x".into(), json!(input))]);
        assert_eq!(
            db.execute_compiled_with(&plan, &params).unwrap().rows,
            [vec![converted(expected)]],
            "{input:?}"
        );
    }
    for input in [i64::MIN, i64::MAX, 0] {
        let params = Parameters::from([("x".into(), json!(input))]);
        assert_eq!(
            db.execute_compiled_with(&plan, &params).unwrap().rows,
            [vec![CypherValue::Integer(input)]]
        );
    }
}

#[test]
fn stored_conversions_and_cursor_modes_preserve_nulls_and_duplicate_rows() {
    let mut db = Database::in_memory().unwrap();
    let params = Parameters::from([("x".into(), json!(1e30))]);
    db.execute_with(
        "CREATE (:Start {value:$x})-[:R {value:$x}]->(:End)",
        &params,
    )
    .unwrap();
    for (prefix, expression, mode) in [
        ("", "$x", ReadMode::Incremental),
        (
            "MATCH (n:Start)-[r:R]->() ",
            "r.value",
            ReadMode::Incremental,
        ),
        (
            "MATCH p = (n:Start)-[:R*1..40]->() ",
            "n.value",
            ReadMode::Incremental,
        ),
        (
            "MATCH p = shortestPath((n:Start)-[:R*]->(:End)) ",
            "n.value",
            ReadMode::Buffered,
        ),
    ] {
        let query = format!(
            "{prefix}UNWIND [1, 1] AS duplicate RETURN toInteger({expression}), coalesce(toInteger({expression}), 7)"
        );
        let expected = vec![vec![CypherValue::Null, CypherValue::Integer(7)]; 2];
        assert_eq!(db.execute_with(&query, &params).unwrap().rows, expected);
        let mut read = db.prepare_read_with(&query, &params).unwrap();
        assert_eq!(read.mode(), mode);
        for _ in 0..2 {
            assert_eq!(
                read.cursor()
                    .unwrap()
                    .collect::<crabnet::Result<Vec<_>>>()
                    .unwrap(),
                expected
            );
        }
    }
    assert_eq!(
        db.execute(
            "MATCH (n:Start) SET n.converted = toInteger(n.value) RETURN n.converted, keys(n)"
        )
        .unwrap()
        .rows,
        [vec![
            CypherValue::Null,
            CypherValue::List(vec![CypherValue::String("value".into())])
        ]]
    );
}

#[test]
fn null_wrong_kinds_lazy_branches_and_input_errors_keep_their_contracts() {
    let mut db = Database::in_memory().unwrap();
    for (input, expected) in [
        (Value::Null, None),
        (json!(true), Some(1)),
        (json!(false), Some(0)),
    ] {
        let params = Parameters::from([("x".into(), input)]);
        assert_eq!(
            db.execute_with("RETURN toInteger($x)", &params)
                .unwrap()
                .rows,
            [vec![converted(expected)]]
        );
    }
    for input in [json!([]), json!({})] {
        let params = Parameters::from([("x".into(), input)]);
        assert!(matches!(
            db.execute_with("RETURN toInteger($x)", &params),
            Err(Error::Type(_))
        ));
        assert_eq!(
            db.execute_with(
                "RETURN CASE WHEN false THEN toInteger($x) ELSE 5 END",
                &params
            )
            .unwrap()
            .rows,
            [vec![CypherValue::Integer(5)]]
        );
    }
    // Conversion failure is null, but an error evaluating its input must not
    // be swallowed, including after writes or inside an explicit transaction.
    db.execute("CREATE (:Saved {value:1})").unwrap();
    let before = db.execute("MATCH (n) RETURN n").unwrap().rows;
    for expression in ["toInteger(1e308 * 2)", "toInteger($bad)"] {
        let params = Parameters::from([("bad".into(), json!([]))]);
        let query = format!("MATCH (n:Saved) SET n.value = 2 RETURN {expression}");
        let result = db.execute_with(&query, &params);
        assert!(
            matches!(result, Err(Error::Argument(_)) | Err(Error::Type(_))),
            "{result:?}"
        );
        assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, before);
        let plan = compile_read(&format!("RETURN {expression}")).unwrap();
        let mut tx = db.transaction().unwrap();
        tx.execute("MATCH (n:Saved) SET n.value = 2").unwrap();
        assert!(matches!(
            tx.execute_compiled_with(&plan, &params),
            Err(Error::Argument(_)) | Err(Error::Type(_))
        ));
        assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
        assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, before);
    }
}
