use crabnet::explain_physical;

fn operator_names(explanation: &str) -> Vec<&str> {
    let operators = explanation
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let numbered = line.strip_prefix("operator ")?;
            let (index, operator) = numbered.split_once(": ")?;
            Some((index.parse::<usize>().ok()?, operator))
        })
        .collect::<Vec<_>>();

    for (expected, (actual, _)) in operators.iter().enumerate() {
        assert_eq!(
            *actual, expected,
            "physical operator numbers must be globally unique and contiguous:\n{explanation}"
        );
    }

    operators
        .into_iter()
        .map(|(_, operator)| operator)
        .collect()
}

fn assert_operator_sequence(explanation: &str, expected: &[&str]) {
    let operators = operator_names(explanation);
    assert_eq!(
        operators.len(),
        expected.len(),
        "unexpected physical operators:\n{explanation}"
    );
    for (actual, expected) in operators.iter().zip(expected) {
        assert!(
            actual.starts_with(expected),
            "expected operator `{expected}`, found `{actual}`:\n{explanation}"
        );
    }
}

fn assert_precedes(explanation: &str, earlier: &str, later: &str) {
    let earlier = explanation
        .find(earlier)
        .unwrap_or_else(|| panic!("physical plan is missing `{earlier}`:\n{explanation}"));
    let later = explanation
        .find(later)
        .unwrap_or_else(|| panic!("physical plan is missing `{later}`:\n{explanation}"));
    assert!(
        earlier < later,
        "expected `{earlier}` before `{later}`:\n{explanation}"
    );
}

fn sqlite_binding_descriptors(explanation: &str) -> Vec<&str> {
    let (_, bindings) = explanation
        .split_once("\n      bindings:\n")
        .unwrap_or_else(|| panic!("physical plan is missing SQLite bindings:\n{explanation}"));
    bindings
        .lines()
        .take_while(|line| line.starts_with("        "))
        .map(str::trim)
        .collect()
}

#[test]
fn pure_sql_is_one_explicit_sqlite_fragment() {
    let explanation = explain_physical(
        "MATCH (person:Person) \
         WHERE person.age >= $minimum \
         RETURN person.name AS name ORDER BY name LIMIT 5",
    )
    .unwrap();

    assert!(explanation.starts_with("physical-plan\n"));
    assert_operator_sequence(&explanation, &["sqlite-fragment"]);
    assert_precedes(&explanation, "operator 0: sqlite-fragment", "sql:");
    assert!(explanation.contains("FROM nodes"));
}

#[test]
fn deep_and_open_ended_trails_are_explicit_inputs_to_sqlite() {
    for query in [
        "MATCH (start)-[:NEXT*33]->(end) RETURN end.name AS name",
        "MATCH (start)-[:NEXT*]->(end) RETURN end.name AS name",
    ] {
        let explanation = explain_physical(query).unwrap();

        assert_operator_sequence(&explanation, &["trail-expand", "sqlite-fragment"]);
        assert_precedes(&explanation, "trail-expand", "sqlite-fragment");
        assert!(explanation.contains("edge-source:"));
        assert!(explanation.contains(
            "sql: SELECT trail_edge.id, trail_edge.from_node, trail_edge.to_node FROM edges AS trail_edge"
        ));
        assert!(explanation.contains("?1 text"));
        assert!(explanation.contains("start-domain: unrestricted"));
        assert!(explanation.contains("end-domain: unrestricted"));
        assert!(
            !explanation.contains("trail-input"),
            "trail expansion must be an operator, not an opaque SQL binding:\n{explanation}"
        );
    }
}

#[test]
fn constrained_trail_endpoints_expose_bound_node_domains() {
    let explanation = explain_physical(
        "MATCH (start:Person {name: $name})-[:NEXT*]->(end:City) RETURN end.name AS name",
    )
    .unwrap();

    assert_operator_sequence(&explanation, &["trail-expand", "sqlite-fragment"]);
    assert_precedes(&explanation, "edge-source:", "start-domain:");
    assert_precedes(&explanation, "start-domain:", "end-domain:");
    assert!(explanation.contains(
        "start-domain:\n        sql: SELECT trail_node.id FROM nodes AS trail_node WHERE trail_node.label = ?1"
    ));
    assert!(explanation.contains(
        "end-domain:\n        sql: SELECT trail_node.id FROM nodes AS trail_node WHERE trail_node.label = ?1 ORDER BY trail_node.id"
    ));
    assert!(
        !explanation.contains("unrestricted"),
        "both endpoints are constrained:\n{explanation}"
    );
}

#[test]
fn shortest_modes_are_distinct_explicit_bfs_operators() {
    for (function, mode) in [("shortestPath", "one"), ("allShortestPaths", "all")] {
        let query = format!(
            "MATCH route = {function}((start)-[:NEXT*..4]->(end)) \
             RETURN length(route) AS hops"
        );
        let explanation = explain_physical(&query).unwrap();
        let shortest = format!("shortest-expand mode={mode}");

        assert_operator_sequence(&explanation, &[&shortest, "sqlite-fragment"]);
        assert_precedes(&explanation, &shortest, "sqlite-fragment");
        assert!(
            !explanation.contains("trail-input"),
            "shortest-path BFS must be an operator, not an opaque SQL binding:\n{explanation}"
        );
    }
}

#[test]
fn semantic_shaping_and_hydration_are_separate_ordered_operators() {
    let explanation = explain_physical(
        "MATCH route = (start)-[edge:NEXT]->(end) \
         RETURN DISTINCT route, edge.rank AS rank \
         ORDER BY rank SKIP 1 LIMIT 2",
    )
    .unwrap();

    assert_operator_sequence(
        &explanation,
        &[
            "sqlite-fragment",
            "semantic-distinct",
            "pagination",
            "batch-hydrate-paths",
        ],
    );
    assert_precedes(&explanation, "sqlite-fragment", "semantic-distinct");
    assert_precedes(&explanation, "semantic-distinct", "pagination");
    assert_precedes(&explanation, "pagination", "batch-hydrate-paths");
}

#[test]
fn union_branches_converge_before_one_explicit_hydration_operator() {
    let branch = "MATCH route = (start)-[:NEXT]->(end) RETURN route";

    for (keyword, mode) in [("UNION ALL", "all"), ("UNION", "semantic-distinct")] {
        let explanation = explain_physical(&format!("{branch} {keyword} {branch}")).unwrap();
        let union = format!("union mode={mode}");

        assert_operator_sequence(
            &explanation,
            &[
                "sqlite-fragment",
                "sqlite-fragment",
                &union,
                "batch-hydrate-paths",
            ],
        );
    }
}
#[test]
fn mixed_three_branch_union_renders_its_left_fold_execution_order() {
    let branch = "MATCH (node) RETURN node.id AS value";
    let explanation =
        explain_physical(&format!("{branch} UNION {branch} UNION ALL {branch}")).unwrap();

    assert_operator_sequence(
        &explanation,
        &[
            "sqlite-fragment",
            "sqlite-fragment",
            "union mode=semantic-distinct",
            "sqlite-fragment",
            "union mode=all",
        ],
    );
}

#[test]
fn a_path_in_a_later_union_branch_triggers_one_root_hydration_pass() {
    let explanation = explain_physical(
        "MATCH (node) RETURN node.id AS value          UNION ALL          MATCH route = (start)-[:NEXT]->(end) RETURN route AS value",
    )
    .unwrap();

    assert_operator_sequence(
        &explanation,
        &[
            "sqlite-fragment",
            "sqlite-fragment",
            "union mode=all",
            "batch-hydrate-paths",
        ],
    );
}

#[test]
fn multiple_runtime_inputs_keep_distinct_slots_and_sql_bind_positions() {
    let explanation = explain_physical(
        "MATCH (start:Start)-[:FIRST*]->(middle:Middle)-[:SECOND*]->(end:End) \
         WHERE end.flag = $flag \
         RETURN end",
    )
    .unwrap();

    assert_operator_sequence(
        &explanation,
        &["trail-expand", "trail-expand", "sqlite-fragment"],
    );
    assert!(explanation.contains("output=%r0"));
    assert!(explanation.contains("output=%r1"));
    assert_eq!(
        sqlite_binding_descriptors(&explanation),
        [
            "?1 text",
            "?2 text",
            "?3 runtime(%r0)",
            "?4 text",
            "?5 runtime(%r1)",
            "?6 text",
            "?7 json",
        ]
    );
    assert_eq!(explanation.matches("runtime(%r0)").count(), 1);
    assert_eq!(explanation.matches("runtime(%r1)").count(), 1);
    assert_precedes(&explanation, "output=%r0", "output=%r1");
    assert_precedes(&explanation, "output=%r1", "sqlite-fragment");
}

#[test]
fn trail_paths_flow_through_sql_and_then_hydrate() {
    let explanation = explain_physical(
        "MATCH route = (start)-[:NEXT*]->(end) \
         RETURN route, nodes(route) AS points, relationships(route) AS links",
    )
    .unwrap();

    assert_operator_sequence(
        &explanation,
        &["trail-expand", "sqlite-fragment", "batch-hydrate-paths"],
    );
    assert_precedes(&explanation, "trail-expand", "sqlite-fragment");
    assert_precedes(&explanation, "sqlite-fragment", "batch-hydrate-paths");
    assert_eq!(
        explanation.matches("batch-hydrate-paths").count(),
        1,
        "path-derived values must use the same final hydration operator:\n{explanation}"
    );
}
