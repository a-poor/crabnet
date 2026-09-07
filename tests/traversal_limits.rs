use crabnet::{CypherValue, Database, Error};

fn self_loop_components(component_count: usize, loops_per_component: usize) -> Database {
    let mut database = Database::in_memory().unwrap();
    let nodes = (0..component_count)
        .map(|_| "(:TraversalBudget)")
        .collect::<Vec<_>>()
        .join(", ");
    let created = database.execute(&format!("CREATE {nodes}")).unwrap();
    assert_eq!(created.summary.nodes_created, component_count);

    let relationships = (0..loops_per_component)
        .map(|_| "(node)-[:STEP]->(node)")
        .collect::<Vec<_>>()
        .join(", ");
    let created = database
        .execute(&format!(
            "MATCH (node:TraversalBudget) CREATE {relationships}"
        ))
        .unwrap();
    assert_eq!(
        created.summary.relationships_created,
        component_count * loops_per_component
    );
    database
}

#[test]
fn default_traversal_state_budget_fails_closed_through_public_api() {
    // Each component has sum(P(7, k), k=1..7) = 13,699 possible positive
    // trails. Across eight components that is 109,592 explored states. No
    // component can reach the requested depth, so the payload budget remains
    // irrelevant to this state-budget regression.
    let mut database = self_loop_components(8, 7);

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
