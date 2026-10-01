use std::collections::BTreeMap;

use crabnet::{CypherValue, Database, Error, Parameters};
use serde_json::json;

fn count_matching(rows: &[Vec<CypherValue>], predicate: impl Fn(&CypherValue) -> bool) -> usize {
    rows.iter()
        .filter(|row| {
            let [value] = row.as_slice() else {
                panic!("expected one-column UNION row, got {row:?}");
            };
            predicate(value)
        })
        .count()
}

#[test]
fn union_all_preserves_branch_rows_and_duplicate_multiplicity() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "RETURN 1 AS value \
             UNION ALL RETURN 1.0 AS value \
             UNION ALL RETURN 1 AS value",
        )
        .unwrap();

    assert_eq!(result.columns, ["value"]);
    assert_eq!(result.rows.len(), 3);
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::Integer(1)
        )),
        2
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::Float(number) if *number == 1.0
        )),
        1
    );
    assert_eq!(result.summary, Default::default());
}

#[test]
fn union_and_union_distinct_use_recursive_cypher_row_equivalence() {
    for operator in ["UNION", "UNION DISTINCT"] {
        let mut database = Database::in_memory().unwrap();
        let query = format!(
            "RETURN [1, {{nested: 2}}] AS value, null AS marker \
             {operator} \
             RETURN [1.0, {{nested: 2.0}}] AS value, null AS marker"
        );

        let result = database.execute(&query).unwrap();

        assert_eq!(result.columns, ["value", "marker"]);
        assert_eq!(
            result.rows.len(),
            1,
            "{operator} should collapse recursively equivalent rows"
        );
        assert!(matches!(result.rows[0][0], CypherValue::List(_)));
        assert!(result.rows[0][1].is_null());
    }
}

#[test]
fn union_distinct_compares_the_complete_row_and_preserves_type_families() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "RETURN 1 AS value, 'same' AS marker \
             UNION RETURN 1.0 AS value, 'same' AS marker \
             UNION RETURN 1 AS value, 'different' AS marker \
             UNION RETURN true AS value, 'same' AS marker \
             UNION RETURN null AS value, 'same' AS marker \
             UNION RETURN null AS value, 'same' AS marker",
        )
        .unwrap();

    assert_eq!(result.columns, ["value", "marker"]);
    assert_eq!(result.rows.len(), 4);
    assert_eq!(
        result
            .rows
            .iter()
            .filter(|row| matches!(row.as_slice(), [CypherValue::Null, _]))
            .count(),
        1
    );
    assert_eq!(
        result
            .rows
            .iter()
            .filter(|row| matches!(row.as_slice(), [CypherValue::Boolean(true), _]))
            .count(),
        1
    );
}

#[test]
fn union_all_accepts_null_and_mixed_scalar_and_structured_values() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "RETURN null AS value \
             UNION ALL RETURN false AS value \
             UNION ALL RETURN 7 AS value \
             UNION ALL RETURN 1.5 AS value \
             UNION ALL RETURN 'seven' AS value \
             UNION ALL RETURN [7] AS value \
             UNION ALL RETURN {number: 7} AS value",
        )
        .unwrap();

    assert_eq!(result.columns, ["value"]);
    assert_eq!(result.rows.len(), 7);
    assert_eq!(count_matching(&result.rows, CypherValue::is_null), 1);
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::Boolean(false)
        )),
        1
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::Integer(7)
        )),
        1
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::Float(number) if *number == 1.5
        )),
        1
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::String(text) if text == "seven"
        )),
        1
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::List(values)
                if values == &[CypherValue::Integer(7)]
        )),
        1
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::Map(values)
                if values == &BTreeMap::from([(
                    "number".to_owned(),
                    CypherValue::Integer(7),
                )])
        )),
        1
    );
}

#[test]
fn match_where_order_and_pagination_are_local_to_each_union_branch() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:UnionItem {side: 'left', name: 'left-low', rank: 1}), \
                    (:UnionItem {side: 'left', name: 'left-high', rank: 3}), \
                    (:UnionItem {side: 'right', name: 'right-first', rank: 1}), \
                    (:UnionItem {side: 'right', name: 'right-second', rank: 2}), \
                    (:UnionItem {side: 'right', name: 'right-third', rank: 3})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (item:UnionItem) WHERE item.side = 'left' \
             RETURN item.name AS value ORDER BY item.rank DESC LIMIT 1 \
             UNION ALL \
             MATCH (item:UnionItem) WHERE item.side = 'right' \
             RETURN item.name AS value ORDER BY item.rank SKIP 1 LIMIT 1",
        )
        .unwrap();

    assert_eq!(result.columns, ["value"]);
    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::String(text) if text == "left-high"
        )),
        1
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::String(text) if text == "right-second"
        )),
        1
    );
}

#[test]
fn with_scope_and_filtering_are_local_to_each_union_branch() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:UnionPerson {name: 'Ada', team: 'compiler', rank: 2}), \
                    (:UnionPerson {name: 'Grace', team: 'compiler', rank: 1}), \
                    (:UnionPerson {name: 'Margaret', team: 'runtime', rank: 1})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:UnionPerson) \
             WITH person.name AS value, person.team AS team, person.rank AS rank \
             ORDER BY rank DESC LIMIT 1 WHERE team = 'compiler' \
             RETURN value \
             UNION ALL \
             MATCH (person:UnionPerson) \
             WITH person.name AS value, person.team AS team \
             WHERE team = 'runtime' \
             RETURN value",
        )
        .unwrap();

    assert_eq!(result.columns, ["value"]);
    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::String(text) if text == "Ada"
        )),
        1
    );
    assert_eq!(
        count_matching(&result.rows, |value| matches!(
            value,
            CypherValue::String(text) if text == "Margaret"
        )),
        1
    );
}

#[test]
fn union_distinct_uses_graph_entity_identity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:UnionPerson {name: 'Ada'}), (:UnionPerson {name: 'Ada'})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:UnionPerson) RETURN person AS value \
             UNION \
             MATCH (person:UnionPerson) RETURN person AS value",
        )
        .unwrap();

    assert_eq!(result.columns, ["value"]);
    assert_eq!(result.rows.len(), 2);
    let element_ids = result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [CypherValue::Node(node)] => node.element_id.as_str(),
            other => panic!("expected a whole-node UNION row, got {other:?}"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(element_ids.len(), 2);
}

#[test]
fn union_branches_require_the_same_column_names_in_the_same_positions() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "RETURN 1 AS value UNION ALL RETURN 2 AS other",
        "RETURN 1 AS first, 2 AS second \
         UNION RETURN 3 AS second, 4 AS first",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "incompatible UNION column names should be a semantic error: {query}"
        );
    }
}

#[test]
fn union_branches_require_the_same_column_arity() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute(
            "RETURN 1 AS first \
             UNION ALL RETURN 2 AS first, 3 AS second"
        ),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn every_union_branch_must_finish_with_a_projection() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute("RETURN 1 AS value UNION ALL MATCH (:UnionPerson)"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn writes_in_union_branches_are_rejected_before_any_side_effect() {
    let mut database = Database::in_memory().unwrap();

    assert!(matches!(
        database.execute(
            "CREATE (:UnionSideEffect) RETURN 1 AS value \
             UNION ALL RETURN 2 AS value"
        ),
        Err(Error::Unsupported(_))
    ));

    let remaining = database
        .execute("MATCH (node:UnionSideEffect) RETURN node")
        .unwrap();
    assert!(remaining.rows.is_empty());
}

#[test]
fn mixed_union_operators_are_rejected() {
    let mut database = Database::in_memory().unwrap();
    for query in [
        "RETURN 1 AS value UNION RETURN 1 AS value UNION ALL RETURN 1 AS value",
        "RETURN 1 AS value UNION ALL RETURN 1 AS value UNION RETURN 1 AS value",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "{query}"
        );
    }
}

#[test]
fn union_branches_resolve_parameters_independently_and_can_vary_result_shape() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:UnionShape)").unwrap();
    let parameters = Parameters::from([
        ("first".to_owned(), json!([1, {"nested": true}])),
        ("second".to_owned(), json!("plain")),
    ]);

    let result = database
        .execute_with(
            "RETURN $first AS value \
             UNION ALL RETURN $second AS value \
             UNION ALL MATCH (node:UnionShape) RETURN node AS value",
            &parameters,
        )
        .unwrap();

    assert_eq!(result.rows.len(), 3);
    assert!(matches!(result.rows[0][0], CypherValue::List(_)));
    assert_eq!(result.rows[1][0], CypherValue::String("plain".into()));
    assert!(matches!(result.rows[2][0], CypherValue::Node(_)));
}

#[test]
fn aggregate_and_order_bindings_remain_aligned_inside_union_branches() {
    let mut database = Database::in_memory().unwrap();
    let parameters = Parameters::from([("order".to_owned(), json!(3))]);

    let result = database
        .execute_with(
            "RETURN count(*) AS value \
             UNION ALL RETURN count(null) AS value \
             UNION ALL RETURN count(1) AS value ORDER BY $order",
            &parameters,
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![CypherValue::Integer(1)],
            vec![CypherValue::Integer(0)],
            vec![CypherValue::Integer(1)],
        ]
    );
}
