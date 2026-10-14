use crabnet::{CypherValue, Database, Error, Parameters, ReadMode, compile_read};
use serde_json::json;

fn text(value: &str) -> CypherValue {
    CypherValue::String(value.into())
}

#[test]
fn embedded_nul_is_a_character_in_literals_and_parameters() {
    let mut db = Database::in_memory().unwrap();
    let expected = vec![
        CypherValue::Integer(3),
        text("\0b"),
        text("a\0b"),
        text("b"),
    ];
    for query in [
        "RETURN size('a\0b'), substring('a\0b', 1), left('a\0b', 3), right('a\0b', 1)",
        "RETURN size($s), substring($s, 1), left($s, 3), right($s, 1)",
    ] {
        let parameters = Parameters::from([("s".into(), json!("a\0b"))]);
        assert_eq!(
            db.execute_with(query, &parameters).unwrap().rows,
            std::slice::from_ref(&expected)
        );
    }
}

#[test]
fn unicode_and_nul_slices_match_a_character_vector_oracle_with_large_bounds() {
    let mut db = Database::in_memory().unwrap();
    let plan = compile_read("RETURN size($s), substring($s, $start), substring($s, $start, $length), left($s, $length), right($s, $length)").unwrap();
    for s in [
        "",
        "\0",
        "\0\0",
        "a\0b",
        "\0é🙂\0",
        "e\u{301}\0界",
        "abc",
        "hé🙂",
    ] {
        let chars: Vec<char> = s.chars().collect();
        for start in [0, 1, 2, 3, 9, 1_i64 << 32, i64::MAX] {
            for length in [0, 1, 2, 4, 9, 1_i64 << 32, i64::MAX] {
                let start_index = usize::try_from(start)
                    .unwrap_or(usize::MAX)
                    .min(chars.len());
                let length_index = usize::try_from(length)
                    .unwrap_or(usize::MAX)
                    .min(chars.len());
                let to_string = |range: &[char]| CypherValue::String(range.iter().collect());
                let expected = vec![
                    CypherValue::Integer(chars.len() as i64),
                    to_string(&chars[start_index..]),
                    to_string(&chars[start_index..(start_index + length_index).min(chars.len())]),
                    to_string(&chars[..length_index]),
                    to_string(&chars[chars.len() - length_index..]),
                ];
                let params = Parameters::from([
                    ("s".into(), json!(s)),
                    ("start".into(), json!(start)),
                    ("length".into(), json!(length)),
                ]);
                assert_eq!(
                    db.execute_compiled_with(&plan, &params).unwrap().rows,
                    [expected],
                    "s={s:?}, start={start}, length={length}"
                );
            }
        }
    }
}

#[test]
fn string_predicates_compare_complete_values_including_nul() {
    let mut db = Database::in_memory().unwrap();
    let plan =
        compile_read("RETURN $s STARTS WITH $needle, $s ENDS WITH $needle, $s CONTAINS $needle")
            .unwrap();
    for s in ["", "\0", "a\0b", "a\0", "\0é🙂\0", "ab"] {
        for needle in ["", "\0", "a", "b", "a\0", "\0b", "é🙂\0", "a\0b\0", "abc"] {
            let params =
                Parameters::from([("s".into(), json!(s)), ("needle".into(), json!(needle))]);
            assert_eq!(
                db.execute_compiled_with(&plan, &params).unwrap().rows,
                [vec![
                    CypherValue::Boolean(s.starts_with(needle)),
                    CypherValue::Boolean(s.ends_with(needle)),
                    CypherValue::Boolean(s.contains(needle)),
                ]],
                "s={s:?}, needle={needle:?}"
            );
        }
    }
    for (s, needle) in [
        (json!(null), json!("\0")),
        (json!("\0"), json!(null)),
        (json!(1), json!("\0")),
        (json!("\0"), json!([])),
    ] {
        let params = Parameters::from([("s".into(), s), ("needle".into(), needle)]);
        assert_eq!(
            db.execute_compiled_with(&plan, &params).unwrap().rows,
            [vec![CypherValue::Null; 3]]
        );
    }
}

#[test]
fn stored_nul_strings_survive_mutation_composition_grouping_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("text.sqlite");
    let mut db = Database::open(&path).unwrap();
    let params = Parameters::from([("s".into(), json!("a\0é🙂\0"))]);
    db.execute_with("CREATE (:Text {s:$s})-[:R {s:$s}]->(:End)", &params)
        .unwrap();
    db.execute(
        "MATCH (n:Text)-[r:R]->() SET n.part = substring(n.s, 1, 3), r.part = right(r.s, 2)",
    )
    .unwrap();
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        db.execute(
            "MATCH (n:Text)-[r:R]->() RETURN n.s, n.part, r.s, r.part, size(n.part), size(r.part)"
        )
        .unwrap()
        .rows,
        [vec![
            text("a\0é🙂\0"),
            text("\0é🙂"),
            text("a\0é🙂\0"),
            text("🙂\0"),
            CypherValue::Integer(3),
            CypherValue::Integer(2)
        ]]
    );
    let params = Parameters::from([("pick".into(), json!(1))]);
    assert_eq!(db.execute_with("MATCH (n:Text) WITH [n, n.s] AS xs RETURN size(xs[$pick]), substring(xs[$pick], 1, 1), {value:left(xs[$pick], 2)}, [x IN [xs[$pick], xs[$pick]] | right(x, 1)]", &params).unwrap().rows,
        [vec![CypherValue::Integer(5), text("\0"), CypherValue::try_from(json!({"value":"a\0"})).unwrap(), CypherValue::try_from(json!(["\0", "\0"])).unwrap()]]);
    assert_eq!(db.execute("MATCH (n:Text) UNWIND [n.s, n.s, 'a'] AS s RETURN left(s, 2) AS part, count(*) AS copies ORDER BY part").unwrap().rows,
        [vec![text("a"), CypherValue::Integer(1)], vec![text("a\0"), CypherValue::Integer(2)]]);
    assert_eq!(db.execute("MATCH (n:Text) WHERE n.s ENDS WITH '🙂\0' RETURN right(n.s, 1) AS s UNION RETURN '\0' AS s").unwrap().rows,
        [vec![text("\0")]]);
}

#[test]
fn eager_and_both_cursor_modes_preserve_complete_text_and_duplicate_rows() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:R]->(:End)").unwrap();
    let params = Parameters::from([("s".into(), json!("\0é🙂\0"))]);
    for (prefix, mode) in [
        ("", ReadMode::Incremental),
        ("MATCH p = (:Start)-[:R*1..40]->() ", ReadMode::Incremental),
        (
            "MATCH p = shortestPath((:Start)-[:R*]->(:End)) ",
            ReadMode::Buffered,
        ),
    ] {
        let query = format!(
            "{prefix}UNWIND [$s, $s] AS s RETURN size(s), substring(s, 1), left(s, 2), right(s, 2), s ENDS WITH '🙂\0'"
        );
        let row = vec![
            CypherValue::Integer(4),
            text("é🙂\0"),
            text("\0é"),
            text("🙂\0"),
            CypherValue::Boolean(true),
        ];
        let expected = vec![row.clone(), row];
        assert_eq!(db.execute_with(&query, &params).unwrap().rows, expected);
        let compiled = compile_read(&query).unwrap();
        assert_eq!(
            db.execute_compiled_with(&compiled, &params).unwrap().rows,
            expected
        );
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
}

#[test]
fn bounds_nulls_and_rollback_still_apply_to_nul_strings() {
    let mut db = Database::in_memory().unwrap();
    let params = Parameters::from([("s".into(), json!("a\0b")), ("bound".into(), json!(-1))]);
    db.execute_with("CREATE (:Text {s:$s})", &params).unwrap();
    let before = db.execute("MATCH (n) RETURN n").unwrap().rows;
    for expression in [
        "substring($s, $bound)",
        "substring($s, 1, $bound)",
        "left($s, $bound)",
        "right($s, $bound)",
    ] {
        let plan = compile_read(&format!("RETURN {expression}")).unwrap();
        assert!(matches!(
            db.execute_compiled_with(&plan, &params),
            Err(Error::Argument(_))
        ));
        let query = format!("MATCH (n:Text) SET n.s = right(n.s, 2) RETURN {expression}");
        assert!(matches!(
            db.execute_with(&query, &params),
            Err(Error::Argument(_))
        ));
        assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, before);
        let mut tx = db.transaction().unwrap();
        tx.execute("MATCH (n:Text) SET n.s = left(n.s, 2)").unwrap();
        assert!(matches!(
            tx.execute_compiled_with(&plan, &params),
            Err(Error::Argument(_))
        ));
        assert!(matches!(tx.commit(), Err(Error::TransactionAborted)));
        assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, before);
    }
    assert_eq!(db.execute_with("RETURN substring($s, null), left($s, null), right($s, null), substring(null, -1), CASE WHEN false THEN left($s, -1) ELSE $s END", &params).unwrap().rows,
        [vec![CypherValue::Null, CypherValue::Null, CypherValue::Null, CypherValue::Null, text("a\0b")]]);
    assert_eq!(
        db.execute_with("RETURN size([]), size([$s, $s])", &params)
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(0), CypherValue::Integer(2)]]
    );
}

#[test]
fn other_string_operations_preserve_nul_and_text_after_it() {
    let mut db = Database::in_memory().unwrap();
    let s = " A\0BÉ🙂 ";
    let params = Parameters::from([("s".into(), json!(s))]);
    assert_eq!(db.execute_with("RETURN toLower($s), toUpper($s), trim($s), ltrim($s), rtrim($s), replace($s, '\0', 'x'), split($s, '\0'), reverse($s), $s + $s", &params).unwrap().rows,
        [vec![text(" a\0bÉ🙂 "), text(" A\0BÉ🙂 "), text("A\0BÉ🙂"), text("A\0BÉ🙂 "), text(" A\0BÉ🙂"), text(" AxBÉ🙂 "), CypherValue::try_from(json!([" A", "BÉ🙂 "])).unwrap(), text(" 🙂ÉB\0A "), text(&format!("{s}{s}"))]]);
    let plan = compile_read("RETURN replace($s, $search, $replacement)").unwrap();
    for (s, search, replacement, expected) in [
        ("a\0b\0", "\0", "🙂", "a🙂b🙂"),
        ("a\0b\0", "\0b", "X\0Y", "aX\0Y\0"),
        ("a\0b\0", "", "x", "a\0b\0"),
        ("\0\0\0", "\0\0", "X", "X\0"),
        ("a\0b\0", "b", "", "a\0\0"),
        ("a\0b\0", "\0c", "x", "a\0b\0"),
    ] {
        let params = Parameters::from([
            ("s".into(), json!(s)),
            ("search".into(), json!(search)),
            ("replacement".into(), json!(replacement)),
        ]);
        assert_eq!(
            db.execute_compiled_with(&plan, &params).unwrap().rows,
            [vec![text(expected)]],
            "s={s:?}, search={search:?}, replacement={replacement:?}"
        );
    }
}
