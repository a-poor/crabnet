//! Internal execution, schema, and hydration regressions.

use super::*;
use rusqlite::functions::FunctionFlags;

fn install_version_four(connection: &Connection) {
    constraints::register(connection).unwrap();
    connection.execute_batch(INITIAL_SCHEMA).unwrap();
    connection.execute_batch(PROPERTY_INDEX_SCHEMA).unwrap();
    connection.execute_batch(constraints::SCHEMA).unwrap();
    connection
        .execute_batch(
            "INSERT INTO nodes (id, labels, props)
         VALUES ('saved', '[\"Package\"]', '{\"key\":\"a\",\"generation\":1}');
         PRAGMA user_version = 4;",
        )
        .unwrap();
    constraints::create(connection, "Package", "key").unwrap();
}

#[test]
fn selective_indexes_follow_identity_and_label_changes() {
    let mut database = Database::in_memory().unwrap();
    database.create_unique_constraint("A", "key").unwrap();
    database.create_unique_constraint("B", "other").unwrap();
    let created = database
        .execute("CREATE (n:A:B {key:[1,null], other:1.0, kept:true}) RETURN elementId(n)")
        .unwrap();
    let old = created.rows[0][0].as_str().unwrap();
    database.execute("MATCH (n:A) REMOVE n:B").unwrap();
    database.execute("CREATE (:B {other:1})").unwrap();
    assert!(matches!(
        database.execute("MATCH (n:A) SET n:B"),
        Err(crate::Error::Constraint(_))
    ));
    assert_eq!(
        database
            .execute("MATCH (n:A) RETURN elementId(n), labels(n), n.key, n.kept")
            .unwrap()
            .rows,
        [vec![
            json!(old),
            json!(["A"]),
            json!([1, null]),
            json!(true)
        ]]
    );
    // Cypher does not expose ID updates. Exercise the ID branch of these
    // two triggers directly, independently of the label lookup index.
    database
        .connection
        .execute("UPDATE nodes SET id = 'renamed' WHERE id = ?1", [old])
        .unwrap();
    for (table, count) in [("node_properties", 3), ("node_unique_values", 1)] {
        let old_count: i64 = database
            .connection
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE node_id=?1"),
                [old],
                |row| row.get(0),
            )
            .unwrap();
        let new_count: i64 = database
            .connection
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE node_id='renamed'"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);
        assert_eq!(new_count, count);
    }
}

#[test]
fn version_four_upgrade_preserves_data_and_installs_selective_maintenance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("version-four.sqlite");
    let connection = Connection::open(&path).unwrap();
    install_version_four(&connection);
    drop(connection);
    let readonly = OpenOptions {
        mode: OpenMode::ReadOnly,
        ..OpenOptions::default()
    };
    assert!(
        matches!(Database::open_with_options(&path, &readonly), Err(crate::Error::Data(message)) if message.contains("version 4 requires migration"))
    );
    let mut database = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadWrite,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let version: i64 = database
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(database.unique_constraints().unwrap().len(), 1);
    assert_eq!(
        database
            .execute("MATCH (p:Package {key:'a'}) RETURN elementId(p), p.generation")
            .unwrap()
            .rows,
        [vec![json!("saved"), json!(1)]]
    );
    let before = database.connection.total_changes();
    database
        .execute("MATCH (p:Package {key:'a'}) SET p.generation = 2")
        .unwrap();
    // One stored-node change and one property-index change. The unique
    // key and unchanged property entry are retained after migration.
    assert_eq!(database.connection.total_changes() - before, 2);
    assert!(matches!(
        database.execute("CREATE (:Package {key:'a'})"),
        Err(crate::Error::Constraint(_))
    ));
    drop(database);
    let mut readonly = Database::open_with_options(&path, &readonly).unwrap();
    assert_eq!(
        readonly
            .execute("MATCH (p:Package {key:'a'}) RETURN elementId(p), p.generation")
            .unwrap()
            .rows,
        [vec![json!("saved"), json!(2)]]
    );
}

#[test]
fn failed_version_five_upgrade_restores_both_triggers_and_version() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let mut connection = Connection::open_in_memory().unwrap();
    install_version_four(&connection);
    let triggers = |connection: &Connection| {
        connection
            .prepare(
                "SELECT sql FROM sqlite_schema
                 WHERE name IN ('node_properties_update','node_unique_update') ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = triggers(&connection);
    connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::CreateTrigger {
                    trigger_name: "node_unique_update",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    assert!(initialize(&mut connection).is_err());
    assert!(connection.is_autocommit());
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    assert_eq!(triggers(&connection), before);
    assert_eq!(constraints::list(&connection).unwrap().len(), 1);
    initialize(&mut connection).unwrap();
    let mut database = Database::from_connection(connection).unwrap();
    assert_eq!(
        database
            .execute("MATCH (p:Package) RETURN elementId(p), p.key, p.generation")
            .unwrap()
            .rows,
        [vec![json!("saved"), json!("a"), json!(1)]]
    );
}

#[test]
fn property_updates_leave_unchanged_index_entries_alone() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mut database = Database::in_memory().unwrap();
    database.create_unique_constraint("Package", "key").unwrap();
    database
        .execute(
            "CREATE (:Package {key: 'saved', generation: 1, name: 'kept', nested: {x:[1,true]}})",
        )
        .unwrap();
    let writes = Arc::new(std::array::from_fn::<_, 6, _>(|_| AtomicUsize::new(0)));
    let observed = writes.clone();
    database
        .connection
        .create_scalar_function(
            "record_index_write",
            1,
            FunctionFlags::SQLITE_UTF8,
            move |context| {
                observed[usize::from(context.get::<u8>(0)?)].fetch_add(1, Ordering::Relaxed);
                Ok(0)
            },
        )
        .unwrap();
    for (table_index, table) in ["node_properties", "node_unique_values"].iter().enumerate() {
        for (action_index, action) in ["DELETE", "INSERT", "UPDATE"].iter().enumerate() {
            let index = table_index * 3 + action_index;
            database
                .connection
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER observe_{index} AFTER {action} ON main.{table}
                 BEGIN SELECT record_index_write({index}); END;"
                ))
                .unwrap();
        }
    }
    let result = database
        .execute("MATCH (p:Package) SET p.generation = 2 RETURN p.key, p.generation")
        .unwrap();
    assert_eq!(result.rows, [vec![json!("saved"), json!(2)]]);
    assert_eq!(result.summary.properties_set, 1);
    assert_eq!(
        writes.each_ref().map(|n| n.load(Ordering::Relaxed)),
        [0, 0, 1, 0, 0, 0]
    );
}

#[test]
fn bulk_writes_reuse_statements_within_a_stable_schema() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mut database = Database::in_memory().unwrap();
    database.create_unique_constraint("Item", "id").unwrap();
    let prepares = Arc::new(std::array::from_fn::<_, 4, _>(|_| AtomicUsize::new(0)));
    let observed = prepares.clone();
    database
        .connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            let index = match context.action {
                AuthAction::Insert {
                    table_name: "nodes",
                } => Some(0),
                AuthAction::Insert {
                    table_name: "edges",
                } => Some(1),
                AuthAction::Update {
                    table_name: "nodes",
                    column_name: "props",
                } => Some(2),
                AuthAction::Update {
                    table_name: "edges",
                    column_name: "props",
                } => Some(3),
                _ => None,
            };
            if let Some(index) = index {
                observed[index].fetch_add(1, Ordering::Relaxed);
            }
            Authorization::Allow
        }))
        .unwrap();
    let result = database
        .execute(
            "UNWIND range(1, 16) AS i CREATE (a:Item {id: i})-[r:R {key: i}]->(b)
         SET a.x = 1, a.y = 2, r.x = 1, r.y = 2 RETURN count(*)",
        )
        .unwrap();
    assert_eq!(result.rows, [vec![json!(16)]]);
    assert_eq!(result.summary.nodes_created, 32);
    assert_eq!(result.summary.relationships_created, 16);
    assert_eq!(result.summary.properties_set, 64);
    assert_eq!(
        prepares.each_ref().map(|n| n.load(Ordering::Relaxed)),
        [1; 4]
    );
}

#[test]
fn cancellation_after_a_write_aborts_the_transaction_and_restores_indexes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let mut database = Database::in_memory().unwrap();
    database.create_unique_constraint("Item", "key").unwrap();
    database.execute("CREATE (:Item {key: 'saved'})").unwrap();
    let token = crate::CancellationToken::new();
    let from_trigger = token.clone();
    let wrote = Arc::new(AtomicBool::new(false));
    let trigger_wrote = wrote.clone();
    database
        .connection
        .create_scalar_function("cancel_query", 0, FunctionFlags::SQLITE_UTF8, move |_| {
            trigger_wrote.store(true, Ordering::Relaxed);
            from_trigger.cancel();
            Ok(0)
        })
        .unwrap();
    database.connection.execute_batch("CREATE TRIGGER cancel_insert AFTER INSERT ON nodes WHEN json_extract(NEW.props, '$.cancel') = 1 BEGIN SELECT cancel_query(); END;").unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction
        .execute("MATCH (n:Item) SET n.key = 'changed'")
        .unwrap();
    let options = QueryOptions {
        cancellation: Some(token),
        ..QueryOptions::default()
    };
    let error = transaction
        .execute_with_options(
            "CREATE (:Item {key: 'new', cancel: true}), (:Item {key: 'later'})",
            &Parameters::new(),
            &options,
        )
        .unwrap_err();
    assert!(
        wrote.load(Ordering::Relaxed),
        "the cancellation must happen after a write"
    );
    assert!(matches!(error, crate::Error::Cancelled), "{error}");
    assert!(matches!(
        transaction.commit(),
        Err(crate::Error::TransactionAborted)
    ));
    assert_eq!(
        database
            .execute("MATCH (n:Item {key: 'saved'}) RETURN n.key")
            .unwrap()
            .rows,
        [vec![crate::Value::from("saved")]]
    );
    database
        .execute("CREATE (:Item {key: 'changed'}), (:Item {key: 'new'}), (:Item {key: 'later'})")
        .unwrap();
}

#[test]
fn rust_distinct_checks_cancellation_without_running_sqlite() {
    let token = crate::CancellationToken::new();
    let context = ExecutionContext::new(&QueryOptions {
        cancellation: Some(token.clone()),
        ..QueryOptions::default()
    });
    token.cancel();
    let result = distinct_pending_rows(
        vec![vec![PendingValue::Materialized(CypherValue::Integer(1))]],
        &context,
    );
    assert!(matches!(result, Err(crate::Error::Cancelled)));
}
use rusqlite::OptionalExtension;
use serde_json::json;

#[test]
fn generated_read_fragments_prepare_with_exact_dense_bindings() {
    #[derive(Default)]
    struct Inspected {
        fragments: usize,
        edge_sources: usize,
        node_domains: usize,
    }

    fn assert_dense_bindings(statement: &rusqlite::Statement<'_>, expected: usize, context: &str) {
        assert_eq!(
            statement.parameter_count(),
            expected,
            "{context}: SQLite parameter arity differs from the physical arguments"
        );
        for index in 1..=expected {
            let expected_name = format!("?{index}");
            assert_eq!(
                statement.parameter_name(index),
                Some(expected_name.as_str()),
                "{context}: physical placeholders must be dense and numbered from ?1"
            );
        }
    }

    fn inspect_operator(
        connection: &Connection,
        operator: &PreparedPhysicalOperator,
        expected_columns: &[String],
        context: &str,
        inspected: &mut Inspected,
    ) {
        match operator {
            PreparedPhysicalOperator::Sqlite(fragment) => {
                inspected.fragments += 1;
                let statement = connection.prepare(&fragment.sql).unwrap_or_else(|error| {
                    panic!("{context}: generated SQLite fragment did not prepare: {error}")
                });
                assert!(statement.readonly(), "{context}: read fragment writes");
                assert_dense_bindings(&statement, fragment.arguments.len(), context);
                assert_eq!(
                    fragment.columns, expected_columns,
                    "{context}: fragment columns differ from the read plan"
                );
                assert_eq!(
                    statement.column_count(),
                    fragment.columns.len(),
                    "{context}: SQLite result width differs from the planned columns"
                );
                assert_eq!(
                    fragment.columns.len(),
                    fragment.output_shapes.len(),
                    "{context}: every result column needs one output shape"
                );

                for (index, input) in fragment.inputs.iter().enumerate() {
                    inspected.edge_sources += 1;
                    let edge_context = format!("{context}/edge-source-{index}");
                    let spec = match input {
                        PreparedValueOperator::TrailExpand { spec, .. }
                        | PreparedValueOperator::ShortestExpand { spec, .. } => spec,
                    };
                    let statement = connection.prepare(&spec.edge_sql).unwrap_or_else(|error| {
                        panic!("{edge_context}: generated edge SQL did not prepare: {error}")
                    });
                    assert!(statement.readonly(), "{edge_context}: edge source writes");
                    assert_dense_bindings(&statement, spec.edge_bindings.len(), &edge_context);
                    assert_eq!(
                        statement.column_count(),
                        3,
                        "{edge_context}: traversal edge sources must return id/from/to"
                    );
                    for (name, domain) in [
                        ("start-domain", spec.start_domain.as_ref()),
                        ("end-domain", spec.end_domain.as_ref()),
                    ] {
                        let Some(domain) = domain else { continue };
                        inspected.node_domains += 1;
                        let domain_context = format!("{edge_context}/{name}");
                        let statement = connection.prepare(&domain.sql).unwrap_or_else(|error| {
                            panic!(
                                "{domain_context}: generated node-domain SQL did not \
                                     prepare: {error}"
                            )
                        });
                        assert!(statement.readonly(), "{domain_context}: domain writes");
                        assert_dense_bindings(&statement, domain.bindings.len(), &domain_context);
                        assert_eq!(
                            statement.column_count(),
                            1,
                            "{domain_context}: node domains must return only ids"
                        );
                    }
                }
            }
            PreparedPhysicalOperator::SemanticDistinct { input }
            | PreparedPhysicalOperator::Paginate { input, .. }
            | PreparedPhysicalOperator::BatchHydrate { input } => {
                inspect_operator(connection, input, expected_columns, context, inspected)
            }
            PreparedPhysicalOperator::Union { left, right, .. } => {
                inspect_operator(
                    connection,
                    left,
                    expected_columns,
                    &format!("{context}/union-left"),
                    inspected,
                );
                inspect_operator(
                    connection,
                    right,
                    expected_columns,
                    &format!("{context}/union-right"),
                    inspected,
                );
            }
        }
    }

    let database = Database::in_memory().unwrap();
    let cases = vec![
        (
            "literal-and-parameter-projections",
            "RETURN 1 AS literal, $value AS parameter, [$value] AS nested",
            Parameters::from([("value".to_owned(), json!({"items": [true, 2]}))]),
            1,
            0,
        ),
        (
            "typed-predicates",
            r#"MATCH (node:Thing {kind: $kind})
               WHERE node.score >= $floor AND node.name CONTAINS $piece
               RETURN node.value AS value ORDER BY value"#,
            Parameters::from([
                ("kind".to_owned(), json!("sample")),
                ("floor".to_owned(), json!(2)),
                ("piece".to_owned(), json!("am")),
            ]),
            1,
            0,
        ),
        (
            "with-aggregate-and-ordering",
            r#"WITH $seed AS seed WHERE seed >= $floor
               WITH seed + $step AS adjusted
               RETURN count(adjusted) AS total ORDER BY $order"#,
            Parameters::from([
                ("seed".to_owned(), json!(4)),
                ("floor".to_owned(), json!(1)),
                ("step".to_owned(), json!(3)),
                ("order".to_owned(), json!(0)),
            ]),
            1,
            0,
        ),
        (
            "semantic-distinct-pagination",
            r#"MATCH (node:Thing {kind: $kind})
               RETURN DISTINCT node.value AS value ORDER BY value
               SKIP $skip LIMIT $limit"#,
            Parameters::from([
                ("kind".to_owned(), json!("sample")),
                ("skip".to_owned(), json!(1)),
                ("limit".to_owned(), json!(2)),
            ]),
            1,
            0,
        ),
        (
            "with-distinct-typed-keys-and-modifiers",
            r#"UNWIND $values AS value
               WITH DISTINCT value ORDER BY value + $step SKIP $skip LIMIT $limit
               WHERE value > $floor
               RETURN value"#,
            Parameters::from([
                ("values".to_owned(), json!([1, 1.0, 2])),
                ("step".to_owned(), json!(1)),
                ("skip".to_owned(), json!(0)),
                ("limit".to_owned(), json!(2)),
                ("floor".to_owned(), json!(0)),
            ]),
            1,
            0,
        ),
        (
            "with-distinct-graph-list-keys",
            r#"MATCH p = (a)-[:STEP*0..]->(b)
               WITH DISTINCT nodes(p) AS ns, relationships(p) AS rs
               RETURN ns, rs"#,
            Parameters::new(),
            1,
            1,
        ),
        (
            "collected-entities-and-distinct-scalars",
            r#"MATCH (node:Thing {kind: $kind})
               WITH node ORDER BY node.value
               WITH collect(DISTINCT node) AS entities,
                    collect(DISTINCT node.value + $step) AS values
               WITH DISTINCT entities, values WHERE size(entities) > $floor
               UNWIND entities AS entity
               RETURN entity, entities, values"#,
            Parameters::from([
                ("kind".to_owned(), json!("sample")),
                ("step".to_owned(), json!(1)),
                ("floor".to_owned(), json!(0)),
            ]),
            1,
            0,
        ),
        (
            "collected-entities-seed-runtime-traversal",
            r#"MATCH (node:Start {key: $key})
               WITH collect(node) AS entities UNWIND entities AS entity
               MATCH (entity)-[:STEP*1..]->(target)
               RETURN collect(DISTINCT target) AS targets"#,
            Parameters::from([("key".to_owned(), json!(1))]),
            1,
            1,
        ),
        (
            "path-lists-unwind-into-entities",
            r#"MATCH p = (start:Start {key: $key})-[:STEP*0..2]->()-[:STEP]->()
               WITH DISTINCT nodes(p) AS ns, relationships(p) AS rs LIMIT $limit
               UNWIND ns AS node UNWIND rs AS edge
               OPTIONAL MATCH (node)-[:EXTRA]->(other)
               RETURN node, edge, other"#,
            Parameters::from([("key".to_owned(), json!(1)), ("limit".to_owned(), json!(2))]),
            1,
            0,
        ),
        (
            "runtime-path-unwind-seeds-another-runtime-traversal",
            r#"MATCH p = (start:Start {key: $key})-[rels:STEP*]->()
               UNWIND nodes(p) AS node UNWIND rels AS edge
               MATCH (node)-[:MORE* {enabled: $enabled}]->(target)
               RETURN p, node, edge, target"#,
            Parameters::from([
                ("key".to_owned(), json!(1)),
                ("enabled".to_owned(), json!(true)),
            ]),
            1,
            2,
        ),
        (
            "aggregate-expressions-group-before-scalar-composition",
            r#"UNWIND $values AS value
               WITH value % 2 AS parity, value
               WITH DISTINCT parity, count(*) + $step AS tally,
                    [x IN collect(value) WHERE x > $floor | x + $step] AS kept
               ORDER BY tally DESC LIMIT $limit
               WHERE tally > $floor
               RETURN parity, kept, {count: tally, first: kept[0]} AS report"#,
            Parameters::from([
                ("values".to_owned(), json!([1, 2, 2, 3])),
                ("step".to_owned(), json!(1)),
                ("floor".to_owned(), json!(0)),
                ("limit".to_owned(), json!(2)),
            ]),
            1,
            0,
        ),
        (
            "three-branch-union",
            r#"RETURN $left AS value
               UNION RETURN $middle AS value
               UNION RETURN $right AS value"#,
            Parameters::from([
                ("left".to_owned(), json!(1)),
                ("middle".to_owned(), json!(1.0)),
                ("right".to_owned(), json!([2])),
            ]),
            3,
            0,
        ),
        (
            "two-runtime-trail-inputs",
            r#"MATCH (start:Start)-[:FIRST* {enabled: $enabled}]->(middle:Middle)-
                     [:SECOND* {weight: $weight}]->(end:End)
               WHERE end.flag = $flag RETURN end"#,
            Parameters::from([
                ("enabled".to_owned(), json!(true)),
                ("weight".to_owned(), json!(2)),
                ("flag".to_owned(), json!("kept")),
            ]),
            1,
            2,
        ),
        (
            "shortest-runtime-input",
            r#"MATCH route = allShortestPaths(
                     (start)-[:ROAD*..4 {open: $open}]->(end))
               RETURN length(route) AS hops"#,
            Parameters::from([("open".to_owned(), json!(true))]),
            1,
            1,
        ),
        (
            "same-clause-relationship-list-rematch",
            r#"MATCH (start:Start {name: $name})-[rels:LINK*1..2]->(end),
                     (start)-[rels:LINK*1..2]->(other)
               RETURN end, other"#,
            Parameters::from([("name".to_owned(), json!("seed"))]),
            1,
            0,
        ),
        (
            "prebound-relationship-list-rematch-through-with",
            r#"MATCH (:Start {name: $name})-[rels:LINK*1..2]->() WITH rels
               MATCH (x)-[rels:LINK* {enabled: $enabled}]->(y)
               RETURN x, y"#,
            Parameters::from([
                ("name".to_owned(), json!("seed")),
                ("enabled".to_owned(), json!(true)),
            ]),
            1,
            1,
        ),
    ];

    let mut prepared_node_domains = 0;
    for (name, query, parameters, expected_fragments, expected_edge_sources) in cases {
        let Plan::Read(plan) = planner::compile(query)
            .unwrap_or_else(|error| panic!("{name}: query did not compile: {error}"))
        else {
            panic!("{name}: expected a read plan")
        };
        let plan = plan
            .prepare(&parameters)
            .unwrap_or_else(|error| panic!("{name}: physical plan did not prepare: {error}"));
        let mut inspected = Inspected::default();
        inspect_operator(
            &database.connection,
            &plan.root,
            &plan.columns,
            name,
            &mut inspected,
        );
        assert_eq!(
            inspected.fragments, expected_fragments,
            "{name}: unexpected number of SQLite fragments"
        );
        assert_eq!(
            inspected.edge_sources, expected_edge_sources,
            "{name}: unexpected number of runtime edge sources"
        );
        prepared_node_domains += inspected.node_domains;
    }
    assert!(
        prepared_node_domains > 0,
        "the corpus must prepare at least one constrained traversal endpoint domain"
    );
}

#[test]
fn distinct_pagination_parameters_preflight_before_trail_materialization() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Preflight)-[:LINK]->(:Preflight)")
        .unwrap();
    let Plan::Read(plan) =
        planner::compile("MATCH (:Preflight)-[:LINK*]->(end) RETURN DISTINCT end LIMIT $missing")
            .unwrap()
    else {
        panic!("expected a read plan");
    };
    let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(0));

    let error =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &budget).unwrap_err();

    assert!(matches!(error, crate::Error::MissingParameter(name) if name == "missing"));
}

#[test]
fn all_union_parameters_preflight_before_branch_materialization() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Preflight)-[:LINK]->(:Preflight)")
        .unwrap();
    let Plan::Read(plan) = planner::compile(
        "MATCH (:Preflight)-[:LINK*]->(end) RETURN end AS value UNION ALL MATCH (node) RETURN $missing AS value",
    )
    .unwrap()
    else {
        panic!("expected a read plan");
    };
    let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(0));

    let error =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &budget).unwrap_err();

    assert!(matches!(error, crate::Error::MissingParameter(name) if name == "missing"));
}

#[test]
fn trail_resource_limit_propagates_through_the_executor() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:BudgetStart)-[:LINK]->(:BudgetEnd)")
        .unwrap();
    let Plan::Read(plan) =
        planner::compile("MATCH (:BudgetStart)-[:LINK*]->(end) RETURN end").unwrap()
    else {
        panic!("expected a read plan");
    };
    let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(0));

    let error =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &budget).unwrap_err();

    assert!(matches!(
        error,
        crate::Error::ResourceLimit(message)
            if message.contains("trail expansion") && message.contains("0-state budget")
    ));
}

#[test]
fn union_branches_share_one_trail_budget() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:BudgetStart)-[:LINK]->(:BudgetEnd)")
        .unwrap();
    let branch = "MATCH (:BudgetStart)-[:LINK*]->(end) RETURN end AS value";
    let Plan::Read(plan) = planner::compile(&format!("{branch} UNION ALL {branch}")).unwrap()
    else {
        panic!("expected a read plan");
    };
    let budget = ExecutionContext::with_trails(traversal::TrailBudget::new(1));

    let error =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &budget).unwrap_err();

    assert!(matches!(
        error,
        crate::Error::ResourceLimit(message)
            if message.contains("trail expansion") && message.contains("1-state budget")
    ));
}

#[test]
fn union_branches_share_one_serialized_trail_payload_budget() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:PayloadStart)-[:LINK]->(:PayloadEnd)")
        .unwrap();
    let branch = "MATCH (:PayloadStart)-[:LINK*]->(end) RETURN end AS value";
    let Plan::Read(single_plan) = planner::compile(branch).unwrap() else {
        panic!("expected a read plan");
    };
    let measurement =
        ExecutionContext::with_trails(traversal::TrailBudget::with_limits(100, usize::MAX));
    execute_read_plan(
        &database.connection,
        &single_plan,
        &Parameters::new(),
        &measurement,
    )
    .unwrap();
    let one_branch_payload = measurement.trails.borrow().payload_used();
    assert!(one_branch_payload > 0);

    let Plan::Read(plan) = planner::compile(&format!("{branch} UNION ALL {branch}")).unwrap()
    else {
        panic!("expected a read plan");
    };
    let budget =
        ExecutionContext::with_trails(traversal::TrailBudget::with_limits(100, one_branch_payload));

    let error =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &budget).unwrap_err();

    assert!(matches!(
        error,
        crate::Error::ResourceLimit(message)
            if message.contains("trail expansion")
                && message.contains(&format!(
                    "{one_branch_payload}-byte serialized-payload budget"
                ))
    ));
}

#[test]
fn retained_memory_limit_propagates_through_the_executor() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:MemoryStart)-[:LINK]->(:MemoryEnd)")
        .unwrap();
    let Plan::Read(plan) =
        planner::compile("MATCH (:MemoryStart)-[:LINK*]->(end) RETURN end").unwrap()
    else {
        panic!("expected a read plan");
    };
    let budget =
        ExecutionContext::with_trails(traversal::TrailBudget::new(100).with_memory_limit(0));

    let error =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &budget).unwrap_err();

    assert!(matches!(
        error,
        crate::Error::ResourceLimit(message)
            if message.contains("trail expansion")
                && message.contains("0-byte retained-memory budget")
    ));
}

#[test]
fn retained_memory_is_released_between_producers_rather_than_accumulated() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:MemoryStart)-[:LINK]->(:MemoryEnd)")
        .unwrap();
    let branch = "MATCH (:MemoryStart)-[:LINK*]->(end) RETURN end AS value";
    let Plan::Read(single_plan) = planner::compile(branch).unwrap() else {
        panic!("expected a read plan");
    };
    let measurement =
        ExecutionContext::with_trails(traversal::TrailBudget::with_limits(100, usize::MAX));
    execute_read_plan(
        &database.connection,
        &single_plan,
        &Parameters::new(),
        &measurement,
    )
    .unwrap();
    let one_branch_peak = measurement.trails.borrow().memory_peak();
    assert!(one_branch_peak > 0);
    assert_eq!(measurement.trails.borrow().memory_used(), 0);

    // Unlike the cumulative state and payload counters, each producer
    // returns its inputs when it finishes, so two branches fit inside the
    // peak of one.
    let Plan::Read(plan) = planner::compile(&format!("{branch} UNION ALL {branch}")).unwrap()
    else {
        panic!("expected a read plan");
    };
    let exact = ExecutionContext::with_trails(
        traversal::TrailBudget::with_limits(100, usize::MAX).with_memory_limit(one_branch_peak),
    );
    let result =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &exact).unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(exact.trails.borrow().memory_peak(), one_branch_peak);
    assert_eq!(exact.trails.borrow().memory_used(), 0);

    let Plan::Read(plan) = planner::compile(branch).unwrap() else {
        panic!("expected a read plan");
    };
    let short = ExecutionContext::with_trails(
        traversal::TrailBudget::with_limits(100, usize::MAX).with_memory_limit(one_branch_peak - 1),
    );
    let error =
        execute_read_plan(&database.connection, &plan, &Parameters::new(), &short).unwrap_err();
    assert!(matches!(
        error,
        crate::Error::ResourceLimit(message)
            if message.contains(&format!(
                "{}-byte retained-memory budget",
                one_branch_peak - 1
            ))
    ));
}

#[test]
fn distinct_numeric_equivalence_does_not_round_large_integers_through_f64() {
    assert!(cypher_values_equivalent(
        &CypherValue::Integer(1),
        &CypherValue::Float(1.0)
    ));
    assert!(!cypher_values_equivalent(
        &CypherValue::Integer(9_007_199_254_740_993),
        &CypherValue::Float(9_007_199_254_740_992.0)
    ));
}

#[test]
fn pending_distinct_uses_typed_values_and_ordered_path_identity() {
    let first = PathIdentity {
        node_ids: vec!["a".into(), "b".into()],
        relationship_ids: vec!["edge".into()],
    };
    let same = first.clone();
    let reversed = PathIdentity {
        node_ids: vec!["b".into(), "a".into()],
        relationship_ids: vec!["edge".into()],
    };
    let different_edge = PathIdentity {
        node_ids: vec!["a".into(), "b".into()],
        relationship_ids: vec!["other".into()],
    };

    assert!(pending_values_equivalent(
        &PendingValue::Materialized(CypherValue::Integer(1)),
        &PendingValue::Materialized(CypherValue::Float(1.0)),
    ));
    assert!(pending_values_equivalent(
        &PendingValue::Path(first.clone()),
        &PendingValue::Path(same),
    ));
    assert!(!pending_values_equivalent(
        &PendingValue::Path(first.clone()),
        &PendingValue::Path(reversed),
    ));
    assert!(!pending_values_equivalent(
        &PendingValue::Path(first.clone()),
        &PendingValue::Path(different_edge),
    ));
    assert!(!pending_values_equivalent(
        &PendingValue::Path(first),
        &PendingValue::Materialized(CypherValue::String("path".into())),
    ));
}

#[test]
fn pending_path_collections_use_entity_identity_and_bridge_typed_lists() {
    let node = |id: &str| {
        CypherValue::Node(NodeValue {
            element_id: id.into(),
            labels: Vec::new(),
            properties: Default::default(),
        })
    };
    let relationship = |id: &str| {
        CypherValue::Relationship(RelationshipValue {
            element_id: id.into(),
            relationship_type: "LINK".into(),
            start_node_element_id: "a".into(),
            end_node_element_id: "b".into(),
            properties: Default::default(),
        })
    };

    let nodes = PendingValue::PathNodes(vec!["a".into(), "b".into()]);
    let materialized_nodes =
        PendingValue::Materialized(CypherValue::List(vec![node("a"), node("b")]));
    assert!(pending_values_equivalent(&nodes, &materialized_nodes));
    assert!(pending_values_equivalent(&materialized_nodes, &nodes));

    let relationships = PendingValue::PathRelationships(vec!["edge".into()]);
    let materialized_relationships =
        PendingValue::Materialized(CypherValue::List(vec![relationship("edge")]));
    assert!(pending_values_equivalent(
        &relationships,
        &materialized_relationships,
    ));
    assert!(!pending_values_equivalent(&nodes, &relationships));

    let empty_relationships = PendingValue::PathRelationships(Vec::new());
    let materialized_empty = PendingValue::Materialized(CypherValue::List(Vec::new()));
    assert!(pending_values_equivalent(
        &empty_relationships,
        &materialized_empty,
    ));
    assert!(pending_values_equivalent(
        &materialized_empty,
        &empty_relationships,
    ));
}

#[test]
fn pending_path_id_collection_is_unique_across_rows_and_columns() {
    let rows = vec![
        vec![
            PendingValue::Path(PathIdentity {
                node_ids: vec!["a".into(), "b".into()],
                relationship_ids: vec!["ab".into()],
            }),
            PendingValue::Path(PathIdentity {
                node_ids: vec!["b".into(), "c".into()],
                relationship_ids: vec!["bc".into()],
            }),
        ],
        vec![
            PendingValue::Path(PathIdentity {
                node_ids: vec!["a".into(), "b".into()],
                relationship_ids: vec!["ab".into()],
            }),
            PendingValue::PathNodes(vec!["node-only".into(), "b".into()]),
            PendingValue::PathRelationships(vec!["edge-only".into(), "bc".into()]),
        ],
    ];

    let (nodes, relationships) =
        collect_path_ids(&rows, &ExecutionContext::new(&QueryOptions::default())).unwrap();
    assert_eq!(
        nodes,
        BTreeSet::from([
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "node-only".to_owned()
        ])
    );
    assert_eq!(
        relationships,
        BTreeSet::from(["ab".to_owned(), "bc".to_owned(), "edge-only".to_owned()])
    );
}

#[test]
fn directly_materialized_entities_seed_path_hydration() {
    let context = ExecutionContext::new(&QueryOptions::default());
    let node = NodeValue {
        element_id: "seeded".into(),
        labels: vec!["Seed".into()],
        properties: Default::default(),
    };
    let rows = vec![vec![
        PendingValue::Materialized(CypherValue::Node(node.clone())),
        PendingValue::Path(PathIdentity {
            node_ids: vec![node.element_id.clone()],
            relationship_ids: Vec::new(),
        }),
    ]];

    // This connection intentionally has no graph schema. Loading can only
    // succeed if the directly materialized node satisfies the path cache.
    let connection = Connection::open_in_memory().unwrap();
    let hydrator = PathHydrator::load(
        &connection,
        &rows,
        &ExecutionContext::new(&QueryOptions::default()),
    )
    .unwrap();
    let path = hydrator
        .hydrate(
            PathIdentity {
                node_ids: vec![node.element_id.clone()],
                relationship_ids: Vec::new(),
            },
            &context,
        )
        .unwrap();
    assert_eq!(path.nodes, [node]);
    assert!(path.relationships.is_empty());
}

#[test]
fn empty_and_seeded_relationship_lists_need_no_storage_query() {
    let context = ExecutionContext::new(&QueryOptions::default());
    let relationship = RelationshipValue {
        element_id: "seeded-edge".into(),
        relationship_type: "LINK".into(),
        start_node_element_id: "a".into(),
        end_node_element_id: "b".into(),
        properties: Default::default(),
    };
    let rows = vec![
        vec![PendingValue::PathRelationships(Vec::new())],
        vec![
            PendingValue::Materialized(CypherValue::Relationship(relationship.clone())),
            PendingValue::PathRelationships(vec![relationship.element_id.clone()]),
        ],
    ];

    // No graph schema exists: both the empty zero-hop list and the seeded
    // relationship list must avoid a storage lookup entirely.
    let connection = Connection::open_in_memory().unwrap();
    let hydrator = PathHydrator::load(
        &connection,
        &rows,
        &ExecutionContext::new(&QueryOptions::default()),
    )
    .unwrap();
    assert_eq!(
        hydrator
            .hydrate_relationship_list(Vec::new(), &context)
            .unwrap(),
        CypherValue::List(Vec::new())
    );
    assert_eq!(
        hydrator
            .hydrate_relationship_list(vec![relationship.element_id.clone()], &context)
            .unwrap(),
        CypherValue::List(vec![CypherValue::Relationship(relationship)])
    );
}

#[test]
fn schema_enables_foreign_keys_and_checks_both_endpoints() {
    let database = Database::in_memory().unwrap();
    let enabled: i64 = database
        .connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(enabled, 1);
    let application_id: i64 = database
        .connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .unwrap();
    let schema_version: i64 = database
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(application_id, APPLICATION_ID);
    assert_eq!(schema_version, SCHEMA_VERSION);

    database
        .connection
        .execute(
            "INSERT INTO nodes (id, labels) VALUES ('one', '[\"Thing\"]'), ('two', '[\"Thing\"]')",
            [],
        )
        .unwrap();
    database
        .connection
        .execute(
            "INSERT INTO edges (id, edge_type, from_node, to_node)
             VALUES ('valid', 'ARBITRARY_TYPE', 'one', 'two')",
            [],
        )
        .unwrap();
    assert!(
        database
            .connection
            .execute("DELETE FROM nodes WHERE id = 'one'", [])
            .is_err()
    );
    let error = database
        .connection
        .execute(
            "INSERT INTO edges (id, edge_type, from_node, to_node)
             VALUES ('bad', 'ANY_TYPE', 'one', 'missing')",
            [],
        )
        .unwrap_err();
    assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));

    let error = database
        .connection
        .execute(
            "INSERT INTO edges (id, edge_type, from_node, to_node)
             VALUES ('also-bad', 'ANY_TYPE', 'missing', 'one')",
            [],
        )
        .unwrap_err();
    assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));
}

#[test]
fn schema_requires_object_properties() {
    let database = Database::in_memory().unwrap();
    assert!(
        database
            .connection
            .execute(
                "INSERT INTO nodes (id, props) VALUES ('bad-json', '[]')",
                [],
            )
            .is_err()
    );
    let props: String = database
        .connection
        .query_row(
            "INSERT INTO nodes (id) VALUES ('default-props') RETURNING props",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
        .unwrap();
    assert_eq!(props, "{}");
}

#[test]
fn version_one_files_migrate_to_the_label_list_schema() {
    let mut connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE nodes (
                 id TEXT PRIMARY KEY NOT NULL,
                 label TEXT,
                 props TEXT NOT NULL DEFAULT '{{}}'
                     CHECK (json_valid(props) AND json_type(props) = 'object')
             ) STRICT;
             CREATE TABLE edges (
                 id TEXT PRIMARY KEY NOT NULL,
                 edge_type TEXT NOT NULL CHECK (edge_type <> ''),
                 from_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
                 to_node TEXT NOT NULL REFERENCES nodes(id) ON DELETE RESTRICT,
                 props TEXT NOT NULL DEFAULT '{{}}'
                     CHECK (json_valid(props) AND json_type(props) = 'object')
             ) STRICT;
             CREATE INDEX nodes_by_label ON nodes(label);
             INSERT INTO nodes (id, label, props)
                 VALUES ('labeled', 'Thing', '{{\"n\": 1}}'), ('bare', NULL, '{{}}');
             INSERT INTO edges (id, edge_type, from_node, to_node)
                 VALUES ('link', 'LINK', 'labeled', 'bare');
             PRAGMA application_id = {APPLICATION_ID};
             PRAGMA user_version = 1;"
        ))
        .unwrap();

    initialize(&mut connection).unwrap();

    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let labels: Vec<(String, String)> = connection
        .prepare("SELECT id, labels FROM nodes ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        labels,
        vec![
            ("bare".to_owned(), "[]".to_owned()),
            ("labeled".to_owned(), "[\"Thing\"]".to_owned()),
        ]
    );
    let indexed: Vec<(String, String)> = connection
        .prepare("SELECT label, node_id FROM node_labels")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(indexed, vec![("Thing".to_owned(), "labeled".to_owned())]);
    let edges: i64 = connection
        .query_row("SELECT count(*) FROM edges", [], |row| row.get(0))
        .unwrap();
    assert_eq!(edges, 1);

    // Opening again is a no-op on the migrated file, and the triggers
    // maintain the index table from here on.
    initialize(&mut connection).unwrap();
    connection
        .execute(
            "UPDATE nodes SET labels = '[\"Thing\", \"Other\"]' WHERE id = 'bare'",
            [],
        )
        .unwrap();
    let things: i64 = connection
        .query_row(
            "SELECT count(*) FROM node_labels WHERE label = 'Thing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(things, 2);
}

#[test]
fn version_two_upgrade_backfills_properties_and_preserves_labels() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(INITIAL_SCHEMA).unwrap();
    connection
        .execute_batch(
            r#"
        PRAGMA user_version = 2;
        INSERT INTO nodes (id, labels, props)
            VALUES ('one', '["Person", "Member"]', '{"id":7,"missing":null,"tags":[1,2]}');
    "#,
        )
        .unwrap();
    let mut database = Database::from_connection(connection).unwrap();
    let result = database
        .execute("MATCH (n:Person:Member {id: 7}) RETURN n.id")
        .unwrap();
    assert_eq!(result.rows, vec![vec![CypherValue::Integer(7)]]);
    let count: i64 = database
        .connection
        .query_row("SELECT count(*) FROM node_properties", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    initialize(&mut database.connection).unwrap();
    database
        .execute("MATCH (n:Member) SET n.id = 8 REMOVE n:Person")
        .unwrap();
    let result = database
        .execute("MATCH (n:Member {id: 8}) RETURN n.id")
        .unwrap();
    assert_eq!(result.rows, vec![vec![CypherValue::Integer(8)]]);
    let version: i64 = database
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn failed_property_index_upgrade_rolls_back_schema_and_version() {
    let mut connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(INITIAL_SCHEMA).unwrap();
    connection
        .execute_batch(
            r#"
        PRAGMA user_version = 2;
        PRAGMA ignore_check_constraints = ON;
        INSERT INTO nodes (id, props) VALUES ('bad', 'invalid json');
        PRAGMA ignore_check_constraints = OFF;
    "#,
        )
        .unwrap();
    assert!(initialize(&mut connection).is_err());
    assert!(connection.is_autocommit());
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    let objects: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'node_properties%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(objects, 0);
}

#[test]
fn version_three_upgrade_preserves_data_and_supports_unique_constraints() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(INITIAL_SCHEMA).unwrap();
    connection.execute_batch(PROPERTY_INDEX_SCHEMA).unwrap();
    connection
        .execute_batch(
            r#"PRAGMA user_version = 3;
           INSERT INTO nodes (id, labels, props) VALUES
               ('a', '["Person", "Member"]', '{"email":"a"}'),
               ('b', '["Person"]', '{"email":"a"}');
           INSERT INTO edges (id, edge_type, from_node, to_node)
               VALUES ('edge', 'LINK', 'a', 'b');"#,
        )
        .unwrap();
    let mut database = Database::from_connection(connection).unwrap();
    assert!(database.unique_constraints().unwrap().is_empty());
    assert!(matches!(
        database.create_unique_constraint("Person", "email"),
        Err(crate::Error::Constraint(_))
    ));
    assert_eq!(
        database
            .execute("MATCH (a:Member)-[:LINK]->(b:Person) RETURN a.email, b.email")
            .unwrap()
            .rows,
        [vec![json!("a"), json!("a")]]
    );
    database
        .execute("MATCH (a:Member) SET a.email = 'changed'")
        .unwrap();
    database
        .create_unique_constraint("Person", "email")
        .unwrap();
    assert!(matches!(
        database.execute("CREATE (:Person {email: 'changed'})"),
        Err(crate::Error::Constraint(_))
    ));
    let version: i64 = database
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    initialize(&mut database.connection).unwrap();
    assert_eq!(database.unique_constraints().unwrap().len(), 1);
}

#[test]
fn failed_uniqueness_upgrade_rolls_back_ddl_and_version() {
    let mut connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(INITIAL_SCHEMA).unwrap();
    connection.execute_batch(PROPERTY_INDEX_SCHEMA).unwrap();
    // Make a later DDL statement fail after the new tables were created.
    connection
        .execute_batch(
            "PRAGMA user_version = 3;
         CREATE TRIGGER node_unique_insert AFTER INSERT ON nodes BEGIN SELECT 1; END;
         INSERT INTO nodes (id) VALUES ('saved');",
        )
        .unwrap();
    assert!(initialize(&mut connection).is_err());
    assert!(connection.is_autocommit());
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    let objects: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_schema WHERE name LIKE 'node_unique%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(objects, ["node_unique_insert"]);
    let nodes: i64 = connection
        .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(nodes, 1);
    connection
        .execute_batch("DROP TRIGGER node_unique_insert")
        .unwrap();
    initialize(&mut connection).unwrap();
    assert_eq!(constraints::list(&connection).unwrap().len(), 0);
}

#[test]
fn entities_with_the_same_id_but_different_kinds_are_not_equal() {
    let mut database = Database::in_memory().unwrap();
    database
        .connection
        .execute_batch(
            "INSERT INTO nodes (id) VALUES ('same');
        INSERT INTO edges (id, edge_type, from_node, to_node)
            VALUES ('same', 'SELF', 'same', 'same');",
        )
        .unwrap();
    let result = database
        .execute("MATCH (n)-[r]->(n) RETURN n = r AS same, n <> r AS different")
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            CypherValue::Boolean(false),
            CypherValue::Boolean(true)
        ]]
    );
}

#[test]
fn indexed_property_lookups_probe_each_candidates_labels() {
    let database = Database::in_memory().unwrap();
    let Plan::Read(plan) = planner::compile("MATCH (n:Person:Member {id: 7}) RETURN n.id").unwrap()
    else {
        panic!("expected a read");
    };
    let PreparedPhysicalOperator::Sqlite(fragment) = plan.prepare(&Parameters::new()).unwrap().root
    else {
        panic!("expected a SQLite fragment");
    };
    let values = fragment
        .arguments
        .into_iter()
        .map(|argument| match argument {
            PreparedFragmentArgument::Value(value) => value,
            _ => panic!("unexpected runtime input"),
        })
        .collect::<Vec<_>>();
    let details = database
        .connection
        .prepare(&format!("EXPLAIN QUERY PLAN {}", fragment.sql))
        .unwrap()
        .query_map(params_from_iter(values.iter()), |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(details.contains("node_properties_by_value"), "{details}");
    assert_eq!(
        details.matches("label=? AND node_id=?").count(),
        2,
        "{details}"
    );
    assert!(details.contains("label=? AND node_id=?"), "{details}");
}

#[test]
fn initialization_does_not_downstamp_a_future_schema() {
    let mut connection = Connection::open_in_memory().unwrap();
    let future_version = SCHEMA_VERSION + 1;
    connection
        .execute_batch(&format!(
            "PRAGMA application_id = {APPLICATION_ID}; \
             PRAGMA user_version = {future_version};"
        ))
        .unwrap();

    let error = initialize(&mut connection).unwrap_err();
    assert!(matches!(error, crate::Error::Data(_)));
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, future_version);
}

#[test]
fn initialization_is_idempotent_for_the_same_database() {
    let mut database = Database::in_memory().unwrap();
    database
        .connection
        .execute("INSERT INTO nodes (id) VALUES ('preserved')", [])
        .unwrap();

    initialize(&mut database.connection).unwrap();

    let count: i64 = database
        .connection
        .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn a_late_create_failure_rolls_back_earlier_nodes() {
    let mut database = Database::in_memory().unwrap();
    database
        .connection
        .execute_batch(
            "CREATE TRIGGER reject_flagged_node
             BEFORE INSERT ON nodes
             WHEN json_extract(NEW.props, '$.reject') = 1
             BEGIN
               SELECT RAISE(ABORT, 'rejected for rollback test');
             END;",
        )
        .unwrap();

    assert!(
        database
            .execute("CREATE (:Thing {ok: true}), (:Thing {reject: true})")
            .is_err()
    );
    let count: i64 = database
        .connection
        .query_row("SELECT count(*) FROM nodes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn a_late_matched_row_failure_rolls_back_every_rows_writes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Source {number: 1}), (:Source {number: 2})")
        .unwrap();
    database
        .connection
        .execute_batch(
            "CREATE TRIGGER reject_second_edge
             BEFORE INSERT ON edges
             WHEN (SELECT count(*) FROM edges) = 1
             BEGIN
               SELECT RAISE(ABORT, 'rejected second edge for rollback test');
             END;",
        )
        .unwrap();

    assert!(
        database
            .execute(
                "MATCH (source:Source) \
                 CREATE (source)-[:GENERATED]->(:Sink)"
            )
            .is_err()
    );

    let edges: i64 = database
        .connection
        .query_row("SELECT count(*) FROM edges", [], |row| row.get(0))
        .unwrap();
    let sinks: i64 = database
        .connection
        .query_row(
            "SELECT count(*) FROM node_labels WHERE label = 'Sink'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let sources: i64 = database
        .connection
        .query_row(
            "SELECT count(*) FROM node_labels WHERE label = 'Source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(edges, 0);
    assert_eq!(sinks, 0);
    assert_eq!(sources, 2);
}
