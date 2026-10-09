use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions};

/// Creates one node per pattern and `loops_per_component` `STEP` self-loops
/// on each, so every node is its own isolated traversal component.
fn self_loop_components(node_patterns: &[&str], loops_per_component: usize) -> Database {
    let mut database = Database::in_memory().unwrap();
    let created = database
        .execute(&format!("CREATE {}", node_patterns.join(", ")))
        .unwrap();
    assert_eq!(created.summary.nodes_created, node_patterns.len());

    let relationships = (0..loops_per_component)
        .map(|_| "(node)-[:STEP]->(node)")
        .collect::<Vec<_>>()
        .join(", ");
    let created = database
        .execute(&format!("MATCH (node) CREATE {relationships}"))
        .unwrap();
    assert_eq!(
        created.summary.relationships_created,
        node_patterns.len() * loops_per_component
    );
    database
}

// Each seven-loop component has sum(P(7, k), k=1..7) = 13,699 possible
// positive trails, so eight seeded components explore 109,592 states and
// exceed the default budget while one component stays well inside it.
const COMPONENTS: usize = 8;
const LOOPS: usize = 7;
const TRAILS_PER_COMPONENT: i64 = 13_699;

#[test]
fn default_traversal_state_budget_fails_closed_through_public_api() {
    // No component can reach the requested depth, so the payload budget
    // remains irrelevant to this state-budget regression.
    let mut database = self_loop_components(&["(:TraversalBudget)"; COMPONENTS], LOOPS);

    let error = database
        .execute(
            "MATCH (:TraversalBudget)-[:STEP*8..]->(end) \
             RETURN count(*) AS paths",
        )
        .unwrap_err();

    assert!(matches!(
        error,
        Error::ResourceLimit(message)
            if message.contains("trail expansion")
                && message.contains("100000-state budget")
    ));

    let usable = database
        .execute("MATCH (node:TraversalBudget) RETURN count(node) AS nodes")
        .unwrap();
    assert_eq!(usable.rows, [vec![CypherValue::Integer(8)]]);
}

#[test]
fn labeled_start_seeds_only_its_own_component() {
    let mut patterns = vec!["(:TraversalBudget)"; COMPONENTS - 1];
    patterns.push("(:SeedStart)");
    let mut database = self_loop_components(&patterns, LOOPS);

    // The same graph exceeds the budget when every node is seeded.
    assert!(matches!(
        database
            .execute("MATCH ()-[:STEP*8..]->(end) RETURN count(*) AS paths")
            .unwrap_err(),
        Error::ResourceLimit(_)
    ));

    // A start label restricts the seeds to one component's 13,699 states.
    let counted = database
        .execute("MATCH (:SeedStart)-[:STEP*1..]->(end) RETURN count(*) AS paths")
        .unwrap();
    assert_eq!(
        counted.rows,
        [vec![CypherValue::Integer(TRAILS_PER_COMPONENT)]]
    );

    let too_deep = database
        .execute("MATCH (:SeedStart)-[:STEP*8..]->(end) RETURN count(*) AS paths")
        .unwrap();
    assert_eq!(too_deep.rows, [vec![CypherValue::Integer(0)]]);
}

#[test]
fn start_property_and_parameter_constraints_seed_selectively() {
    let mut patterns = vec!["(:TraversalBudget {seed: false})"; COMPONENTS - 1];
    patterns.push("(:TraversalBudget {seed: true})");
    let mut database = self_loop_components(&patterns, LOOPS);

    let literal = database
        .execute(
            "MATCH (:TraversalBudget {seed: true})-[:STEP*1..]->(end) \
             RETURN count(*) AS paths",
        )
        .unwrap();
    assert_eq!(
        literal.rows,
        [vec![CypherValue::Integer(TRAILS_PER_COMPONENT)]]
    );

    let mut parameters = crabnet::Parameters::new();
    parameters.insert("seed".into(), serde_json::json!(true));
    let parameterized = database
        .execute_with(
            "MATCH (start:TraversalBudget {seed: $seed})-[:STEP*1..]->(end) \
             RETURN count(*) AS paths",
            &parameters,
        )
        .unwrap();
    assert_eq!(
        parameterized.rows,
        [vec![CypherValue::Integer(TRAILS_PER_COMPONENT)]]
    );

    // A pre-bound start variable also carries its later pattern constraints.
    let rebound = database
        .execute(
            "MATCH (start:TraversalBudget {seed: true}) \
             WITH start \
             MATCH (start)-[:STEP*1..]->(end) \
             RETURN count(*) AS paths",
        )
        .unwrap_err();
    assert!(matches!(rebound, Error::ResourceLimit(_)));
}

#[test]
fn shortest_paths_seed_from_the_constrained_start() {
    let mut patterns = vec!["(:TraversalBudget)"; COMPONENTS - 1];
    patterns.push("(:SeedStart)");
    let mut database = self_loop_components(&patterns, LOOPS);

    let shortest = database
        .execute(
            "MATCH route = shortestPath((:SeedStart)-[:STEP*1..]->(end)) \
             RETURN length(route) AS hops",
        )
        .unwrap();
    assert_eq!(shortest.rows, [vec![CypherValue::Integer(1)]]);

    let all = database
        .execute(
            "MATCH route = allShortestPaths((:SeedStart)-[:STEP*1..]->(end)) \
             RETURN count(*) AS routes",
        )
        .unwrap();
    assert_eq!(all.rows, [vec![CypherValue::Integer(LOOPS as i64)]]);
}

#[test]
fn constrained_shortest_paths_do_not_load_disconnected_edges() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Start)-[:STEP]->()-[:STEP]->(:End)")
        .unwrap();
    database
        .execute("UNWIND range(1, 100) AS i CREATE (n:Unrelated)-[:STEP]->(n)")
        .unwrap();
    let options = QueryOptions {
        max_materialized_rows: Some(8),
        ..QueryOptions::default()
    };
    for function in ["shortestPath", "allShortestPaths"] {
        for pattern in [
            "(:Start)-[:STEP*]->(:End)",
            "(:End)<-[:STEP*]-(:Start)",
            "(:Start)-[:STEP*]-(:End)",
        ] {
            let result = database
                .execute_with_options(
                    &format!("MATCH p = {function}({pattern}) RETURN length(p)"),
                    &Parameters::new(),
                    &options,
                )
                .unwrap();
            assert_eq!(result.rows, [vec![CypherValue::Integer(2)]]);
        }
    }
}

#[test]
fn shortest_neighbor_loading_stops_at_the_hop_bound() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Start)-[:STEP]->(:End)").unwrap();
    database
        .execute("MATCH (end:End) UNWIND range(1, 100) AS i CREATE (end)-[:STEP]->()")
        .unwrap();
    for (pattern, hops, rows) in [
        ("(:Start)-[:STEP*1..1]->(:End)", 1, 4),
        ("(:Start)-[:STEP*0..0]->(:Start)", 0, 3),
    ] {
        for function in ["shortestPath", "allShortestPaths"] {
            let options = QueryOptions {
                max_materialized_rows: Some(rows),
                ..QueryOptions::default()
            };
            let result = database
                .execute_with_options(
                    &format!("MATCH p = {function}({pattern}) RETURN length(p)"),
                    &Parameters::new(),
                    &options,
                )
                .unwrap();
            assert_eq!(result.rows, [vec![CypherValue::Integer(hops)]]);
        }
    }
}

#[test]
fn shortest_neighbor_budget_failure_aborts_prior_transaction_writes() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Start)").unwrap();
    database
        .execute("MATCH (start:Start) UNWIND range(1, 10) AS i CREATE (start)-[:STEP]->()")
        .unwrap();
    let mut transaction = database.transaction().unwrap();
    transaction.execute("CREATE (:Provisional)").unwrap();
    let options = QueryOptions {
        max_materialized_rows: Some(3),
        ..QueryOptions::default()
    };
    let error = transaction
        .execute_with_options(
            "MATCH p = shortestPath((:Start)-[:STEP*]->()) RETURN length(p)",
            &Parameters::new(),
            &options,
        )
        .unwrap_err();
    assert!(
        matches!(error, Error::ResourceLimit(message) if message.contains("materialized rows"))
    );
    assert!(transaction.commit().is_err());
    assert_eq!(
        database
            .execute("MATCH (:Provisional) RETURN count(*)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(0)]]
    );
    assert_eq!(
        database
            .execute("MATCH p = shortestPath((:Start)-[:STEP*]->()) RETURN count(p)")
            .unwrap()
            .rows,
        [vec![CypherValue::Integer(10)]]
    );
}
