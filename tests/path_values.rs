use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crabnet::{CypherValue, Database, Error, PathValue, QueryResult, explain_physical};
use rusqlite::Connection;

static NEXT_DATABASE_PATH: AtomicU64 = AtomicU64::new(0);

fn path_at(result: &QueryResult, row: usize, column: usize) -> &PathValue {
    result.rows[row][column]
        .as_path()
        .unwrap_or_else(|| panic!("expected a path at row {row}, column {column}"))
}

fn property_string<'a>(value: &'a CypherValue, key: &str) -> &'a str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected string property `{key}`, got {value:?}"))
}

fn node_names(path: &PathValue) -> Vec<&str> {
    path.nodes
        .iter()
        .map(|node| {
            property_string(
                node.properties
                    .get("name")
                    .unwrap_or_else(|| panic!("node has no `name` property: {node:?}")),
                "name",
            )
        })
        .collect()
}

fn relationship_names(path: &PathValue) -> Vec<&str> {
    path.relationships
        .iter()
        .map(|relationship| {
            property_string(
                relationship.properties.get("name").unwrap_or_else(|| {
                    panic!("relationship has no `name` property: {relationship:?}")
                }),
                "name",
            )
        })
        .collect()
}

fn assert_path_shape(path: &PathValue, nodes: &[&str], relationships: &[&str]) {
    assert_eq!(path.nodes.len(), path.relationships.len() + 1);
    assert_eq!(node_names(path), nodes);
    assert_eq!(relationship_names(path), relationships);
}

fn path_nodes_value(path: &PathValue) -> CypherValue {
    CypherValue::List(path.nodes.iter().cloned().map(CypherValue::Node).collect())
}

fn path_relationships_value(path: &PathValue) -> CypherValue {
    CypherValue::List(
        path.relationships
            .iter()
            .cloned()
            .map(CypherValue::Relationship)
            .collect(),
    )
}

fn assert_path_function_row(row: &[CypherValue]) {
    let path = row[0].as_path().expect("first column must be a path");
    assert_eq!(
        row[1],
        CypherValue::Integer(path.relationships.len() as i64)
    );
    assert_eq!(row[2], path_nodes_value(path));
    assert_eq!(row[3], path_relationships_value(path));
}

fn node_list_names(value: &CypherValue) -> Vec<&str> {
    value
        .as_list()
        .expect("expected a node list")
        .iter()
        .map(|value| {
            let node = value.as_node().expect("list member must be a node");
            property_string(
                node.properties.get("name").expect("node must have a name"),
                "name",
            )
        })
        .collect()
}

fn relationship_list_names(value: &CypherValue) -> Vec<&str> {
    value
        .as_list()
        .expect("expected a relationship list")
        .iter()
        .map(|value| {
            let relationship = value
                .as_relationship()
                .expect("list member must be a relationship");
            property_string(
                relationship
                    .properties
                    .get("name")
                    .expect("relationship must have a name"),
                "name",
            )
        })
        .collect()
}

fn named_linear_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:PathNode {name: 'a'}), (:PathNode {name: 'b'}), \
                    (:PathNode {name: 'c'}), (:PathNode {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:PathNode {name: 'a'}), (b:PathNode {name: 'b'}), \
                   (c:PathNode {name: 'c'}), (d:PathNode {name: 'd'}) \
             CREATE (a)-[:NEXT {name: 'ab'}]->(b), \
                    (b)-[:NEXT {name: 'bc'}]->(c), \
                    (c)-[:NEXT {name: 'cd'}]->(d)",
        )
        .unwrap();
    database
}

struct TemporaryDatabasePath(PathBuf);

impl TemporaryDatabasePath {
    fn new() -> Self {
        let sequence = NEXT_DATABASE_PATH.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "crabnet-path-shaping-{}-{timestamp}-{sequence}.sqlite",
            std::process::id(),
        )))
    }
}

impl Drop for TemporaryDatabasePath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct PoisonedShapingGraph {
    // Fields drop in declaration order, so the connection closes before its
    // temporary database file is removed.
    database: Database,
    _path: TemporaryDatabasePath,
}

impl PoisonedShapingGraph {
    fn new() -> Self {
        let path = TemporaryDatabasePath::new();
        let mut database = Database::open(&path.0).unwrap();
        create_shaping_graph(&mut database);
        poison_discarded_nodes(&path.0);
        Self {
            database,
            _path: path,
        }
    }
}

fn create_shaping_graph(database: &mut Database) {
    database
        .execute(
            "CREATE (:ShapeStart {name: 'start'}), \
                    (:ShapeEnd {name: 'one'}), \
                    (:ShapeEnd {name: 'two'}), \
                    (:ShapeEnd {name: 'three'}), \
                    (:ShapeMarker), (:ShapeMarker)",
        )
        .unwrap();
    database
        .execute(
            "MATCH (start:ShapeStart), \
                   (one:ShapeEnd {name: 'one'}), \
                   (two:ShapeEnd {name: 'two'}), \
                   (three:ShapeEnd {name: 'three'}) \
             CREATE (start)-[:SHAPE {name: 'one', rank: 1}]->(one), \
                    (start)-[:SHAPE {name: 'two', rank: 2}]->(two), \
                    (start)-[:SHAPE {name: 'three', rank: 3}]->(three)",
        )
        .unwrap();
}

fn poison_discarded_nodes(path: &Path) {
    // Compact path rows need only IDs and the relationship rank. Hydration is
    // the first operation that evaluates endpoint property JSON. Poisoning
    // rows pagination must discard makes executor-stage ordering observable
    // without adding a test-only hook to Crabnet's public API.
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    let changed = connection
        .execute(
            "UPDATE nodes SET props = 'not-json'
             WHERE label = 'ShapeEnd'
               AND json_extract(props, '$.name') IN ('one', 'three')",
            [],
        )
        .unwrap();
    assert_eq!(changed, 2);
}

fn assert_middle_shaping_row(result: &QueryResult) {
    assert_eq!(result.columns, ["path", "rank"]);
    assert_eq!(result.rows.len(), 1);
    assert_path_shape(path_at(result, 0, 0), &["start", "two"], &["two"]);
    assert_eq!(result.rows[0][1], CypherValue::Integer(2));
}

fn assert_stage_precedes(explanation: &str, earlier: &str, later: &str) {
    let earlier_position = explanation
        .find(earlier)
        .unwrap_or_else(|| panic!("physical plan is missing `{earlier}`:\n{explanation}"));
    let later_position = explanation
        .find(later)
        .unwrap_or_else(|| panic!("physical plan is missing `{later}`:\n{explanation}"));
    assert!(
        earlier_position < later_position,
        "expected `{earlier}` before `{later}` in:\n{explanation}"
    );
}

#[test]
fn fixed_multi_hop_match_returns_a_public_typed_path() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (a:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->(b:PathNode)-
                           [:NEXT {name: 'bc'}]->(c:PathNode) \
             RETURN route AS journey",
        )
        .unwrap();

    assert_eq!(result.columns, ["journey"]);
    assert_eq!(result.rows.len(), 1);
    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a", "b", "c"], &["ab", "bc"]);
    assert_eq!(path.relationships[0].relationship_type, "NEXT");
    assert_eq!(
        path.relationships[0].start_node_element_id,
        path.nodes[0].element_id
    );
    assert_eq!(
        path.relationships[0].end_node_element_id,
        path.nodes[1].element_id
    );
    assert_eq!(
        path.relationships[1].start_node_element_id,
        path.nodes[1].element_id
    );
    assert_eq!(
        path.relationships[1].end_node_element_id,
        path.nodes[2].element_id
    );
}

#[test]
fn traversal_order_does_not_rewrite_stored_relationship_direction() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH reverse = (c:PathNode {name: 'c'})-
                            [edge:NEXT {name: 'bc'}]-
                            (b:PathNode {name: 'b'}) \
             RETURN reverse, edge",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["c", "b"], &["bc"]);
    let CypherValue::Relationship(projected_edge) = &result.rows[0][1] else {
        panic!("expected the relationship projection")
    };
    assert_eq!(&path.relationships[0], projected_edge);
    assert_eq!(
        path.relationships[0].start_node_element_id,
        path.nodes[1].element_id
    );
    assert_eq!(
        path.relationships[0].end_node_element_id,
        path.nodes[0].element_id
    );
}

#[test]
fn incoming_bounded_path_orders_entities_by_traversal_not_storage() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH reverse = (d:PathNode {name: 'd'})<-[:NEXT*2]-
                            (b:PathNode {name: 'b'}) \
             RETURN reverse",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["d", "c", "b"], &["cd", "bc"]);
    for (index, relationship) in path.relationships.iter().enumerate() {
        assert_eq!(
            relationship.start_node_element_id,
            path.nodes[index + 1].element_id
        );
        assert_eq!(
            relationship.end_node_element_id,
            path.nodes[index].element_id
        );
    }
}

#[test]
fn bounded_zero_hop_path_contains_one_node_and_no_relationships() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH identity = (start:PathNode {name: 'a'})-[:MISSING*0]->
                             (end:PathNode {name: 'a'}) \
             RETURN identity, start, end",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a"], &[]);
    let CypherValue::Node(start) = &result.rows[0][1] else {
        panic!("expected the start node")
    };
    let CypherValue::Node(end) = &result.rows[0][2] else {
        panic!("expected the end node")
    };
    assert_eq!(path.nodes[0], *start);
    assert_eq!(path.nodes[0], *end);
}

#[test]
fn a_named_node_pattern_is_a_zero_length_path_without_a_quantifier() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH singleton = (node:PathNode {name: 'b'}) \
             RETURN singleton, node",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["b"], &[]);
    let CypherValue::Node(node) = &result.rows[0][1] else {
        panic!("expected the projected node")
    };
    assert_eq!(&path.nodes[0], node);
}

#[test]
fn bounded_range_returns_one_ordered_path_per_edge_sequence_and_depth() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (:PathNode {name: 'a'})-[:NEXT*0..3]->(end:PathNode) \
             RETURN route, end.name AS endpoint ORDER BY endpoint",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 4);
    assert_path_shape(path_at(&result, 0, 0), &["a"], &[]);
    assert_path_shape(path_at(&result, 1, 0), &["a", "b"], &["ab"]);
    assert_path_shape(path_at(&result, 2, 0), &["a", "b", "c"], &["ab", "bc"]);
    assert_path_shape(
        path_at(&result, 3, 0),
        &["a", "b", "c", "d"],
        &["ab", "bc", "cd"],
    );
}

#[test]
fn mixed_fixed_and_bounded_segments_build_one_complete_path() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->
                           (:PathNode)-[:NEXT*2]->(end:PathNode {name: 'd'}) \
             RETURN route",
        )
        .unwrap();

    assert_path_shape(
        path_at(&result, 0, 0),
        &["a", "b", "c", "d"],
        &["ab", "bc", "cd"],
    );
}

#[test]
fn zero_hop_segment_inside_a_fixed_path_does_not_duplicate_its_boundary_node() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH route = (:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->(middle:PathNode)-
                           [:MISSING*0]->(end:PathNode {name: 'b'}) \
             RETURN route, middle, end",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a", "b"], &["ab"]);
    let [
        CypherValue::Path(_),
        CypherValue::Node(middle),
        CypherValue::Node(end),
    ] = result.rows[0].as_slice()
    else {
        panic!("expected a path and its two boundary-node projections")
    };
    assert_eq!(middle, end);
    assert_eq!(&path.nodes[1], middle);
}

#[test]
fn multiple_named_paths_keep_independent_ordered_entity_sequences() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH first = (:PathNode {name: 'a'})-
                           [:NEXT {name: 'ab'}]->(middle:PathNode), \
                   second = (middle)-[:NEXT*1..2]->(end:PathNode) \
             WHERE end.name = 'd' \
             RETURN first, second",
        )
        .unwrap();

    assert_eq!(result.columns, ["first", "second"]);
    assert_eq!(result.rows.len(), 1);
    assert_path_shape(path_at(&result, 0, 0), &["a", "b"], &["ab"]);
    assert_path_shape(path_at(&result, 0, 1), &["b", "c", "d"], &["bc", "cd"]);
}

#[test]
fn diamond_paths_retain_their_distinct_ordered_intermediate_nodes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:DiamondPath {name: 'a'}), (:DiamondPath {name: 'b'}), \
                    (:DiamondPath {name: 'c'}), (:DiamondPath {name: 'd'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:DiamondPath {name: 'a'}), (b:DiamondPath {name: 'b'}), \
                   (c:DiamondPath {name: 'c'}), (d:DiamondPath {name: 'd'}) \
             CREATE (a)-[:LINK {name: 'ab'}]->(b), \
                    (a)-[:LINK {name: 'ac'}]->(c), \
                    (b)-[:LINK {name: 'bd'}]->(d), \
                    (c)-[:LINK {name: 'cd'}]->(d)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:DiamondPath {name: 'a'})-[:LINK*2]->
                         (:DiamondPath {name: 'd'}) \
             RETURN path",
        )
        .unwrap();

    let mut paths = result
        .rows
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let path = path_at(&result, index, 0);
            (node_names(path), relationship_names(path))
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert_eq!(
        paths,
        [
            (vec!["a", "b", "d"], vec!["ab", "bd"]),
            (vec!["a", "c", "d"], vec!["ac", "cd"]),
        ]
    );
}

#[test]
fn parallel_relationship_ids_make_otherwise_identical_paths_distinct() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:ParallelPath {name: 'a'}), (:ParallelPath {name: 'b'})")
        .unwrap();
    database
        .execute(
            "MATCH (a:ParallelPath {name: 'a'}), (b:ParallelPath {name: 'b'}) \
             CREATE (a)-[:LINK {name: 'same'}]->(b), \
                    (a)-[:LINK {name: 'same'}]->(b)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:ParallelPath {name: 'a'})-[:LINK]->
                         (:ParallelPath {name: 'b'}) \
             RETURN DISTINCT path",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    let first = path_at(&result, 0, 0);
    let second = path_at(&result, 1, 0);
    assert_path_shape(first, &["a", "b"], &["same"]);
    assert_path_shape(second, &["a", "b"], &["same"]);
    assert_ne!(
        first.relationships[0].element_id,
        second.relationships[0].element_id
    );
    assert_ne!(first, second);
}

#[test]
fn a_cycle_path_may_repeat_a_node_but_not_a_relationship() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:CyclePath {name: 'a'}), (:CyclePath {name: 'b'}), \
                    (:CyclePath {name: 'c'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (a:CyclePath {name: 'a'}), (b:CyclePath {name: 'b'}), \
                   (c:CyclePath {name: 'c'}) \
             CREATE (a)-[:CYCLE {name: 'ab'}]->(b), \
                    (b)-[:CYCLE {name: 'bc'}]->(c), \
                    (c)-[:CYCLE {name: 'ca'}]->(a)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:CyclePath {name: 'a'})-[:CYCLE*3]->
                         (:CyclePath {name: 'a'}) \
             RETURN path",
        )
        .unwrap();

    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["a", "b", "c", "a"], &["ab", "bc", "ca"]);
    assert_eq!(path.nodes[0].element_id, path.nodes[3].element_id);
    let mut relationship_ids = path
        .relationships
        .iter()
        .map(|relationship| relationship.element_id.as_str())
        .collect::<Vec<_>>();
    relationship_ids.sort_unstable();
    relationship_ids.dedup();
    assert_eq!(relationship_ids.len(), 3);
}

#[test]
fn path_values_survive_with_aliases_without_losing_entity_types() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*1..2]->(end:PathNode) \
             WITH path AS journey, end \
             WHERE end.name = 'c' \
             RETURN journey AS path",
        )
        .unwrap();

    assert_eq!(result.columns, ["path"]);
    assert_path_shape(path_at(&result, 0, 0), &["a", "b", "c"], &["ab", "bc"]);
}

#[test]
fn union_distinct_uses_recursive_path_identity_while_union_all_keeps_duplicates() {
    let mut database = named_linear_graph();
    let branch = "MATCH path = (:PathNode {name: 'a'})-
                               [:NEXT {name: 'ab'}]->(:PathNode {name: 'b'}) \
                  RETURN path";

    let distinct = database
        .execute(&format!("{branch} UNION {branch}"))
        .unwrap();
    assert_eq!(distinct.rows.len(), 1);
    assert_path_shape(path_at(&distinct, 0, 0), &["a", "b"], &["ab"]);

    let all = database
        .execute(&format!("{branch} UNION ALL {branch}"))
        .unwrap();
    assert_eq!(all.rows.len(), 2);
    assert_eq!(all.rows[0], all.rows[1]);
}

#[test]
fn raw_path_operations_outside_projection_are_structured_rejections() {
    let mut database = named_linear_graph();

    for query in [
        "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN path.missing",
        "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN path ORDER BY path",
        "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN length(DISTINCT path)",
    ] {
        assert!(
            matches!(
                database.execute(query),
                Err(Error::Unsupported(_) | Error::Semantic(_))
            ),
            "unsupported path operation must fail as a structured query error: {query}"
        );
    }
}

#[test]
fn optional_and_write_fed_path_bindings_remain_explicitly_unsupported() {
    let mut database = named_linear_graph();

    assert!(matches!(
        database.execute("OPTIONAL MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) RETURN path"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        database.execute(
            "MATCH path = (a:PathNode)-[:NEXT]->(b:PathNode) \
             CREATE (:PathWriteSideEffect) RETURN path"
        ),
        Err(Error::Unsupported(_))
    ));

    let side_effects = database
        .execute("MATCH (node:PathWriteSideEffect) RETURN node")
        .unwrap();
    assert!(side_effects.rows.is_empty());
}

#[test]
fn skip_and_limit_discard_compact_rows_before_path_hydration() {
    let mut graph = PoisonedShapingGraph::new();

    let result = graph
        .database
        .execute(
            "MATCH path = (:ShapeStart)-[edge:SHAPE]->(:ShapeEnd) \
             RETURN path, edge.rank AS rank \
             ORDER BY rank SKIP 1 LIMIT 1",
        )
        .unwrap();

    assert_middle_shaping_row(&result);
}

#[test]
fn with_shapes_mixed_path_and_scalar_rows_before_final_hydration() {
    let mut graph = PoisonedShapingGraph::new();

    let result = graph
        .database
        .execute(
            "MATCH path = (:ShapeStart)-[edge:SHAPE]->(:ShapeEnd) \
             WITH path, edge.rank AS rank \
             ORDER BY rank SKIP 1 LIMIT 1 \
             RETURN path, rank",
        )
        .unwrap();

    assert_middle_shaping_row(&result);
}

#[test]
fn distinct_and_pagination_discard_duplicate_and_paged_rows_before_hydration() {
    let mut graph = PoisonedShapingGraph::new();

    let result = graph
        .database
        .execute(
            "MATCH path = (:ShapeStart)-[edge:SHAPE]->(:ShapeEnd), \
                   (:ShapeMarker) \
             RETURN DISTINCT path, edge.rank AS rank \
             ORDER BY rank SKIP 1 LIMIT 1",
        )
        .unwrap();

    assert_middle_shaping_row(&result);
}

#[test]
fn physical_plan_shapes_distinct_path_identities_before_batch_hydration() {
    let explanation = explain_physical(
        "MATCH path = (:ShapeStart)-[edge:SHAPE]->(:ShapeEnd) \
         RETURN DISTINCT path, edge.rank AS rank \
         ORDER BY rank SKIP 1 LIMIT 1",
    )
    .unwrap();

    assert_stage_precedes(&explanation, "semantic-distinct", "pagination");
    assert_stage_precedes(&explanation, "pagination", "batch-hydrate-paths");
}

#[test]
fn union_and_union_all_shape_mixed_path_and_scalar_rows_by_their_full_identity() {
    let mut database = named_linear_graph();
    let branch = "MATCH path = (:PathNode {name: 'a'})-
                               [:NEXT {name: 'ab'}]->(:PathNode {name: 'b'}) \
                  RETURN path, 7 AS marker";

    let distinct = database
        .execute(&format!("{branch} UNION {branch}"))
        .unwrap();
    assert_eq!(distinct.columns, ["path", "marker"]);
    assert_eq!(distinct.rows.len(), 1);
    assert_path_shape(path_at(&distinct, 0, 0), &["a", "b"], &["ab"]);
    assert_eq!(distinct.rows[0][1], CypherValue::Integer(7));

    let all = database
        .execute(&format!("{branch} UNION ALL {branch}"))
        .unwrap();
    assert_eq!(all.rows.len(), 2);
    assert_eq!(all.rows[0], all.rows[1]);
}

#[test]
fn physical_union_plans_share_one_batch_hydration_after_bag_shaping() {
    let branch = "MATCH path = (:PathNode {name: 'a'})-
                               [:NEXT {name: 'ab'}]->(:PathNode {name: 'b'}) \
                  RETURN path, 7 AS marker";

    for (operator, stage) in [
        ("UNION", "union mode=semantic-distinct"),
        ("UNION ALL", "union mode=all"),
    ] {
        let explanation = explain_physical(&format!("{branch} {operator} {branch}")).unwrap();
        assert_stage_precedes(&explanation, stage, "batch-hydrate-paths");
        assert_eq!(
            explanation.matches("batch-hydrate-paths").count(),
            1,
            "union branches must converge before one shared hydration stage:\n{explanation}"
        );
    }
}

#[test]
fn fixed_path_functions_return_public_typed_values_in_mixed_rows() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-
                         [:NEXT {name: 'ab'}]->(:PathNode)-
                         [:NEXT {name: 'bc'}]->(end:PathNode) \
             RETURN path, length(path) AS hops, nodes(path) AS points, \
                    relationships(path) AS links, end.name AS endpoint",
        )
        .unwrap();

    assert_eq!(
        result.columns,
        ["path", "hops", "points", "links", "endpoint"]
    );
    assert_eq!(result.rows.len(), 1);
    assert_path_function_row(&result.rows[0][..4]);
    assert_path_shape(path_at(&result, 0, 0), &["a", "b", "c"], &["ab", "bc"]);
    assert_eq!(result.rows[0][4], CypherValue::String("c".into()));
}

#[test]
fn bounded_and_zero_hop_path_functions_preserve_each_candidate_shape() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*0..2]->(end:PathNode) \
             RETURN path, length(path) AS hops, nodes(path) AS points, \
                    relationships(path) AS links, end.name AS endpoint \
             ORDER BY endpoint",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 3);
    for row in &result.rows {
        assert_path_function_row(&row[..4]);
    }
    assert_path_shape(path_at(&result, 0, 0), &["a"], &[]);
    assert_path_shape(path_at(&result, 1, 0), &["a", "b"], &["ab"]);
    assert_path_shape(path_at(&result, 2, 0), &["a", "b", "c"], &["ab", "bc"]);
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| row[1].clone())
            .collect::<Vec<_>>(),
        [
            CypherValue::Integer(0),
            CypherValue::Integer(1),
            CypherValue::Integer(2),
        ]
    );
}

#[test]
fn reverse_traversal_functions_keep_traversal_order_and_storage_direction() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'c'})-[:NEXT*2]-
                         (:PathNode {name: 'a'}) \
             RETURN path, length(path), nodes(path), relationships(path)",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    assert_path_function_row(&result.rows[0]);
    let path = path_at(&result, 0, 0);
    assert_path_shape(path, &["c", "b", "a"], &["bc", "ab"]);
    for (index, relationship) in path.relationships.iter().enumerate() {
        assert_eq!(
            relationship.start_node_element_id,
            path.nodes[index + 1].element_id
        );
        assert_eq!(
            relationship.end_node_element_id,
            path.nodes[index].element_id
        );
    }
}

#[test]
fn path_functions_resolve_a_path_carried_through_a_with_alias() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*2]->(:PathNode {name: 'c'}) \
             WITH path AS route \
             RETURN route, length(route) AS hops, nodes(route) AS points, \
                    relationships(route) AS links",
        )
        .unwrap();

    assert_eq!(result.columns, ["route", "hops", "points", "links"]);
    assert_eq!(result.rows.len(), 1);
    assert_path_function_row(&result.rows[0]);
    assert_path_shape(path_at(&result, 0, 0), &["a", "b", "c"], &["ab", "bc"]);
}

#[test]
fn distinct_and_pagination_shape_path_function_results_semantically() {
    let mut database = named_linear_graph();
    database
        .execute("CREATE (:PathFunctionMarker), (:PathFunctionMarker)")
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*0..3]->(:PathNode), \
                   (:PathFunctionMarker) \
             RETURN DISTINCT length(path) AS hops, nodes(path) AS points, \
                    relationships(path) AS links \
             ORDER BY hops SKIP 1 LIMIT 2",
        )
        .unwrap();

    assert_eq!(result.columns, ["hops", "points", "links"]);
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::Integer(1));
    assert_eq!(node_list_names(&result.rows[0][1]), ["a", "b"]);
    assert_eq!(relationship_list_names(&result.rows[0][2]), ["ab"]);
    assert_eq!(result.rows[1][0], CypherValue::Integer(2));
    assert_eq!(node_list_names(&result.rows[1][1]), ["a", "b", "c"]);
    assert_eq!(relationship_list_names(&result.rows[1][2]), ["ab", "bc"]);
}

#[test]
fn length_is_a_scalar_in_filters_arithmetic_ordering_and_with_pipelines() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*0..3]->(end:PathNode) \
             WHERE length(path) >= 1 \
             WITH path, end, length(path) AS hops \
             WHERE hops <= 2 \
             RETURN end.name AS endpoint, hops, hops + 10 AS score \
             ORDER BY length(path) DESC",
        )
        .unwrap();

    assert_eq!(result.columns, ["endpoint", "hops", "score"]);
    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("c".into()),
                CypherValue::Integer(2),
                CypherValue::Integer(12),
            ],
            vec![
                CypherValue::String("b".into()),
                CypherValue::Integer(1),
                CypherValue::Integer(11),
            ],
        ]
    );
}

#[test]
fn computed_path_function_values_survive_with_aliases() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*2]->(:PathNode {name: 'c'}) \
             WITH length(path) AS hops, nodes(path) AS points, \
                  relationships(path) AS links \
             RETURN hops, points, links",
        )
        .unwrap();

    assert_eq!(result.columns, ["hops", "points", "links"]);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(2));
    assert_eq!(node_list_names(&result.rows[0][1]), ["a", "b", "c"]);
    assert_eq!(relationship_list_names(&result.rows[0][2]), ["ab", "bc"]);
}

#[test]
fn distinct_nodes_ignore_edge_ids_while_relationships_keep_edge_identity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:FunctionParallel {name: 'a'}), (:FunctionParallel {name: 'b'})")
        .unwrap();
    database
        .execute(
            "MATCH (a:FunctionParallel {name: 'a'}), \
                   (b:FunctionParallel {name: 'b'}) \
             CREATE (a)-[:LINK {name: 'same'}]->(b), \
                    (a)-[:LINK {name: 'same'}]->(b)",
        )
        .unwrap();

    let nodes = database
        .execute(
            "MATCH path = (:FunctionParallel {name: 'a'})-[:LINK]->
                         (:FunctionParallel {name: 'b'}) \
             RETURN DISTINCT nodes(path) AS nodes",
        )
        .unwrap();
    assert_eq!(nodes.rows.len(), 1);
    assert_eq!(node_list_names(&nodes.rows[0][0]), ["a", "b"]);

    let relationships = database
        .execute(
            "MATCH path = (:FunctionParallel {name: 'a'})-[:LINK]->
                         (:FunctionParallel {name: 'b'}) \
             RETURN DISTINCT relationships(path) AS relationships",
        )
        .unwrap();
    assert_eq!(relationships.rows.len(), 2);
    let ids = relationships
        .rows
        .iter()
        .map(|row| {
            row[0].as_list().unwrap()[0]
                .as_relationship()
                .unwrap()
                .element_id
                .as_str()
        })
        .collect::<Vec<_>>();
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn union_and_union_all_apply_semantic_equality_to_path_function_rows() {
    let mut database = named_linear_graph();
    let branch = "MATCH path = (:PathNode {name: 'a'})-
                               [:NEXT {name: 'ab'}]->(:PathNode {name: 'b'}) \
                  RETURN length(path) AS hops, nodes(path) AS points, \
                         relationships(path) AS links";

    let distinct = database
        .execute(&format!("{branch} UNION {branch}"))
        .unwrap();
    assert_eq!(distinct.columns, ["hops", "points", "links"]);
    assert_eq!(distinct.rows.len(), 1);
    assert_eq!(distinct.rows[0][0], CypherValue::Integer(1));
    assert_eq!(node_list_names(&distinct.rows[0][1]), ["a", "b"]);
    assert_eq!(relationship_list_names(&distinct.rows[0][2]), ["ab"]);

    let all = database
        .execute(&format!("{branch} UNION ALL {branch}"))
        .unwrap();
    assert_eq!(all.rows.len(), 2);
    assert_eq!(all.rows[0], all.rows[1]);
}

#[test]
fn path_functions_validate_arity_at_plan_time() {
    let mut database = named_linear_graph();

    for function in ["length", "nodes", "relationships"] {
        for query in [
            format!("RETURN {function}()"),
            format!(
                "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
                 RETURN {function}(path, path)"
            ),
        ] {
            assert!(
                matches!(database.execute(&query), Err(Error::Semantic(_))),
                "bad arity must be a semantic planning error: {query}"
            );
        }
    }
}

#[test]
fn path_functions_reject_unknown_variables_and_non_path_kinds_semantically() {
    let mut database = named_linear_graph();

    for query in [
        "RETURN length(missing)",
        "MATCH (node:PathNode) RETURN length(node)",
        "MATCH (node:PathNode) RETURN nodes(node)",
        "MATCH ()-[edge:NEXT]->() RETURN relationships(edge)",
        "WITH 1 AS value RETURN length(value)",
        "WITH 1 AS value RETURN nodes(value)",
        "WITH 1 AS value RETURN relationships(value)",
        "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
         WITH path AS route RETURN length(path)",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "invalid path-function argument must fail semantically: {query}"
        );
    }
}

#[test]
fn typed_graph_lists_remain_rejected_in_scalar_and_expanding_contexts() {
    let mut database = named_linear_graph();

    for function in ["nodes", "relationships"] {
        let queries = [
            format!(
                "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
                 WHERE {function}(path) RETURN path"
            ),
            format!(
                "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
                 RETURN {function}(path) + []"
            ),
            format!(
                "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
                 RETURN {function}(path) AS values ORDER BY values"
            ),
            format!(
                "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
                 RETURN {function}(path) AS values, count(*) AS rows"
            ),
            format!(
                "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
                 RETURN collect({function}(path)) AS values"
            ),
            format!(
                "MATCH path = (:PathNode)-[:NEXT]->(:PathNode) \
                 UNWIND {function}(path) AS value RETURN value"
            ),
        ];
        for query in queries {
            assert!(
                matches!(
                    database.execute(&query),
                    Err(Error::Semantic(_) | Error::Unsupported(_))
                ),
                "typed graph list must not leak into a scalar/list executor context: {query}"
            );
        }
    }
}

#[test]
fn zero_hop_relationships_union_literal_empty_list_deduplicates_cross_transport() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:MISSING*0]->
                         (:PathNode {name: 'a'}) \
             RETURN relationships(path) AS relationships \
             UNION \
             RETURN [] AS relationships",
        )
        .unwrap();

    assert_eq!(result.columns, ["relationships"]);
    assert_eq!(result.rows, [vec![CypherValue::List(Vec::new())]]);
}

#[test]
fn distinct_relationships_over_multiple_zero_hop_paths_is_one_empty_list() {
    let mut database = named_linear_graph();

    let result = database
        .execute(
            "MATCH path = (:PathNode)-[:MISSING*0]->(:PathNode) \
             RETURN DISTINCT relationships(path) AS relationships",
        )
        .unwrap();

    assert_eq!(result.columns, ["relationships"]);
    assert_eq!(result.rows, [vec![CypherValue::List(Vec::new())]]);
}

#[test]
fn length_is_an_ordinary_scalar_for_aggregation_and_implicit_grouping() {
    let mut database = named_linear_graph();
    database
        .execute("CREATE (:LengthAggregateMarker), (:LengthAggregateMarker)")
        .unwrap();

    let result = database
        .execute(
            "MATCH path = (:PathNode {name: 'a'})-[:NEXT*0..3]->(:PathNode), \
                   (:LengthAggregateMarker) \
             RETURN length(path) AS hops, count(length(path)) AS paths, \
                    sum(length(path)) AS total_hops \
             ORDER BY hops",
        )
        .unwrap();

    assert_eq!(result.columns, ["hops", "paths", "total_hops"]);
    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::Integer(0),
                CypherValue::Integer(2),
                CypherValue::Integer(0),
            ],
            vec![
                CypherValue::Integer(1),
                CypherValue::Integer(2),
                CypherValue::Integer(2),
            ],
            vec![
                CypherValue::Integer(2),
                CypherValue::Integer(2),
                CypherValue::Integer(4),
            ],
            vec![
                CypherValue::Integer(3),
                CypherValue::Integer(2),
                CypherValue::Integer(6),
            ],
        ]
    );
}
