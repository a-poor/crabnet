use std::time::Duration;

use crabnet::{CypherValue, Database, Error, OpenOptions, Parameters, ReadMode};
use serde_json::json;

fn argument_error(error: Error, context: &str) {
    assert!(matches!(error, Error::Argument(_)), "{context}: {error:?}");
}

#[test]
fn evaluated_non_finite_arithmetic_is_an_argument_error() {
    let mut db = Database::in_memory().unwrap();
    for expression in [
        "0.0 / 0.0",
        "1 / 0",
        "1.0 / -0.0",
        "1 % 0",
        "1.0 % 0.0",
        "1.0e308 + 1.0e308",
        "-1.0e308 - 1.0e308",
        "1.0e308 * 2.0",
        "1.0e308 / 1.0e-308",
        "sqrt(-1)",
        "log(0)",
        "log(-1)",
        "log10(0)",
        "asin(2)",
        "acos(-2)",
        "exp(1000)",
    ] {
        let query = format!("RETURN {expression} AS value");
        argument_error(db.execute(&query).unwrap_err(), &query);
    }
}

#[test]
fn nan_comparisons_fail_before_producing_a_boolean_or_null() {
    let mut db = Database::in_memory().unwrap();
    // All seven recorded mismatches plus the formerly passing cross-kind
    // ordering case must now reject the unsupported arithmetic explicitly.
    for right in ["1", "1.0", "0.0 / 0.0", "'a'"] {
        for operator in ["=", "<>", "<", "<=", ">", ">="] {
            let query = format!("RETURN (0.0 / 0.0) {operator} ({right}) AS value");
            argument_error(db.execute(&query).unwrap_err(), &query);
        }
    }
}

#[test]
fn numeric_errors_survive_composition_and_parameterized_properties() {
    let mut db = Database::in_memory().unwrap();
    let parameters = Parameters::from([("large".into(), json!(1.0e308))]);
    db.execute_with("CREATE (:Number {value: $large})", &parameters)
        .unwrap();
    for query in [
        "RETURN ($large * 2) IS NULL",
        "RETURN toInteger($large * 2)",
        "RETURN toString($large * 2)",
        "RETURN coalesce($large * 2, 1)",
        "RETURN CASE WHEN true THEN $large * 2 ELSE 1 END",
        "RETURN [$large * 2]",
        "RETURN {value: $large * 2}",
        "RETURN [x IN [1, 2] | $large * x]",
        "WITH $large * 2 AS x RETURN x = x",
        "MATCH (n:Number) RETURN n.value + n.value",
        "MATCH (n:Number) WHERE n.value * 2 > 1 RETURN n",
        "UNWIND [$large, $large] AS x RETURN sum(x)",
        "UNWIND [$large, $large] AS x RETURN avg(x)",
        "UNWIND [$large, $large * 1.5] AS x WITH DISTINCT x RETURN sum(x)",
        "UNWIND [$large, $large * 1.5] AS x WITH DISTINCT x RETURN avg(x)",
        "UNWIND [$large, $large, -$large, -$large] AS x RETURN sum(x)",
        "UNWIND [$large, $large, -$large, -$large] AS x RETURN avg(x)",
    ] {
        argument_error(db.execute_with(query, &parameters).unwrap_err(), query);
    }
}

#[test]
fn finite_results_nulls_and_unselected_branches_keep_their_semantics() {
    let mut db = Database::in_memory().unwrap();
    for query in [
        "RETURN null / 0 AS x",
        "RETURN 1 / null AS x",
        "RETURN null % 0 AS x",
        "RETURN sqrt(null) AS x",
        "RETURN exp(null) AS x",
    ] {
        assert_eq!(
            db.execute(query).unwrap().rows,
            [vec![CypherValue::Null]],
            "{query}"
        );
    }
    for query in [
        "RETURN CASE WHEN true THEN 1 ELSE 0.0 / 0.0 END AS x",
        "RETURN CASE WHEN false THEN sqrt(-1) ELSE 1 END AS x",
        "RETURN CASE 2 WHEN 1 THEN exp(1000) ELSE 1 END AS x",
        "RETURN coalesce(1, 1.0e308 * 2) AS x",
        "UNWIND [0, 1] AS x RETURN CASE WHEN x = 0 THEN 1 ELSE 1 / x END AS value LIMIT 1",
    ] {
        assert_eq!(
            db.execute(query).unwrap().rows,
            [vec![CypherValue::Integer(1)]],
            "{query}"
        );
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(
            read.cursor()
                .unwrap()
                .collect::<crabnet::Result<Vec<_>>>()
                .unwrap(),
            [vec![CypherValue::Integer(1)]],
            "{query}"
        );
    }
    let parameters = Parameters::from([
        ("large".into(), json!(1.0e308)),
        ("small".into(), json!(1.0e-308)),
    ]);
    let result = db
        .execute_with(
            "RETURN $large / 2 AS a, $small / $large AS b, 7 / 2 AS c, \
         7 / 2.0 AS d, $large % 3 AS e, -5.5 % 2 AS f, 5.5 % -2 AS g",
            &parameters,
        )
        .unwrap();
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Float(5.0e307),
            CypherValue::Float(0.0),
            CypherValue::Integer(3),
            CypherValue::Float(3.5),
            // The exact integer represented by 1e308 has remainder 2 modulo 3.
            CypherValue::Float(2.0),
            CypherValue::Float(-1.5),
            CypherValue::Float(1.5),
        ]]
    );
    assert_eq!(
        db.execute("UNWIND [] AS x RETURN sum(x), avg(x)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(0), CypherValue::Null]]
    );
    assert_eq!(
        db.execute("UNWIND [null, null] AS x RETURN sum(x), avg(x)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(0), CypherValue::Null]]
    );
    assert_eq!(
        db.execute("UNWIND [1.0, 1.0, 3.0, null] AS x WITH DISTINCT x RETURN sum(x), avg(x)")
            .unwrap()
            .rows,
        [vec![CypherValue::Float(4.0), CypherValue::Float(2.0)]]
    );
}

#[test]
fn incremental_numeric_errors_end_the_cursor_and_allow_reuse() {
    let mut db = Database::in_memory().unwrap();
    let query = "UNWIND [2.0, 0.0] AS x RETURN 1 / x AS value";
    argument_error(db.execute(query).unwrap_err(), query);
    {
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), ReadMode::Incremental);
        for _ in 0..2 {
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Float(0.5)]);
            argument_error(cursor.next().unwrap().unwrap_err(), query);
            assert!(cursor.next().is_none());
        }
    }
    assert_eq!(
        db.execute("RETURN 1").unwrap().rows,
        [vec![CypherValue::Integer(1)]]
    );
}

#[test]
fn buffered_and_traversal_cursors_reject_non_finite_results() {
    let mut db = Database::in_memory().unwrap();
    db.execute("CREATE (:Start)-[:LINK]->(:End)").unwrap();
    for (query, mode) in [
        (
            "MATCH p = (:Start)-[:LINK*1..40]->() RETURN 1.0e308 * size(nodes(p))",
            ReadMode::Incremental,
        ),
        (
            "MATCH p = shortestPath((:Start)-[:LINK*]->(:End)) RETURN 1.0 / (size(nodes(p)) - 2)",
            ReadMode::Buffered,
        ),
    ] {
        argument_error(db.execute(query).unwrap_err(), query);
        let mut read = db.prepare_read(query).unwrap();
        assert_eq!(read.mode(), mode);
        let error = match read.cursor() {
            Err(error) => error,
            Ok(cursor) => cursor.collect::<crabnet::Result<Vec<_>>>().unwrap_err(),
        };
        argument_error(error, query);
    }
}

#[test]
fn numeric_failures_roll_back_statements_and_explicit_transactions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("numeric.sqlite");
    let mut db = Database::open(&path).unwrap();
    db.create_unique_constraint("Saved", "key").unwrap();
    db.execute("CREATE (:Saved {key: 1, value: 4})").unwrap();
    let saved = db.execute("MATCH (n) RETURN n").unwrap().rows;
    for query in [
        "CREATE (:Temporary) RETURN 0.0 / 0.0",
        "MATCH (n:Saved) SET n.key = 2 SET n.value = exp(1000)",
        "CREATE (:Temporary {value: 1.0e308 * 2})",
    ] {
        argument_error(db.execute(query).unwrap_err(), query);
        assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
    }
    for cursor_failure in [false, true] {
        let mut tx = db.transaction().unwrap();
        tx.execute("MATCH (n:Saved) SET n.key = 2").unwrap();
        tx.execute("CREATE (:Temporary)").unwrap();
        tx.create_unique_constraint("Temporary", "key").unwrap();
        if cursor_failure {
            let mut read = tx
                .prepare_read("UNWIND [4, -1] AS x RETURN sqrt(x)")
                .unwrap();
            let mut cursor = read.cursor().unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), [CypherValue::Float(2.0)]);
            argument_error(cursor.next().unwrap().unwrap_err(), "transaction cursor");
            assert!(cursor.next().is_none());
        } else {
            argument_error(tx.execute("RETURN 1 / 0").unwrap_err(), "transaction query");
        }
        assert!(matches!(
            tx.execute("RETURN 1"),
            Err(Error::TransactionAborted)
        ));
        // Rollback releases the writer lock before the aborted handle drops.
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
        assert_eq!(
            db.execute("MATCH (n:Saved {key: 1}) RETURN n")
                .unwrap()
                .rows,
            saved
        );
        assert!(matches!(
            db.execute("CREATE (:Saved {key: 1})"),
            Err(Error::Constraint(_))
        ));
    }
    drop(db);
    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.execute("MATCH (n) RETURN n").unwrap().rows, saved);
    db.execute("MATCH (n:Saved) SET n.value = sqrt(n.value)")
        .unwrap();
}
