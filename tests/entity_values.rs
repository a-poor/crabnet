use crabnet::{CypherValue, Database, QueryResult};

fn assert_author_edge_project(result: &QueryResult) {
    assert_eq!(result.columns, ["author", "wrote", "project"]);
    assert_eq!(result.rows.len(), 1);

    let [
        CypherValue::Node(author),
        CypherValue::Relationship(wrote),
        CypherValue::Node(project),
    ] = result.rows[0].as_slice()
    else {
        panic!("expected a node, relationship, and node row")
    };

    assert!(!author.element_id.is_empty());
    assert_eq!(author.labels, ["Person"]);
    assert_eq!(
        author.properties.get("name"),
        Some(&CypherValue::String("Ada".into()))
    );

    assert!(!project.element_id.is_empty());
    assert_ne!(author.element_id, project.element_id);
    assert_eq!(project.labels, ["Project"]);
    assert_eq!(
        project.properties.get("name"),
        Some(&CypherValue::String("crabnet".into()))
    );

    assert!(!wrote.element_id.is_empty());
    assert_ne!(wrote.element_id, author.element_id);
    assert_ne!(wrote.element_id, project.element_id);
    assert_eq!(wrote.relationship_type, "WROTE");
    assert_eq!(wrote.start_node_element_id, author.element_id);
    assert_eq!(wrote.end_node_element_id, project.element_id);
    assert_eq!(
        wrote.properties.get("since"),
        Some(&CypherValue::Integer(2024))
    );
}

#[test]
fn match_returns_whole_nodes_and_relationships_with_endpoints() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada'})-[:WROTE {since: 2024}]->\
                    (:Project {name: 'crabnet'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (author:Person)-[wrote:WROTE]->(project:Project) \
             RETURN author, wrote, project",
        )
        .unwrap();

    assert_author_edge_project(&result);
    assert_eq!(result.summary.nodes_created, 0);
    assert_eq!(result.summary.relationships_created, 0);
}

#[test]
fn create_return_uses_the_same_whole_entity_values() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "CREATE (author:Person {name: 'Ada'})-\
                    [wrote:WROTE {since: 2024}]->\
                    (project:Project {name: 'crabnet'}) \
             RETURN author, wrote, project",
        )
        .unwrap();

    assert_author_edge_project(&result);
    assert_eq!(result.summary.nodes_created, 2);
    assert_eq!(result.summary.relationships_created, 1);
}

#[test]
fn an_entity_shaped_map_remains_a_map() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "RETURN {element_id: 'not-an-entity', labels: ['Person'], \
                     properties: {name: 'Ada'}} AS value",
        )
        .unwrap();

    assert_eq!(result.columns, ["value"]);
    assert_eq!(result.rows.len(), 1);
    let CypherValue::Map(fields) = &result.rows[0][0] else {
        panic!("an ordinary map must not be decoded as a graph entity")
    };
    assert_eq!(
        fields.get("element_id"),
        Some(&CypherValue::String("not-an-entity".into()))
    );
    assert_eq!(
        fields.get("labels"),
        Some(&CypherValue::List(vec![CypherValue::String(
            "Person".into()
        )]))
    );
    assert_eq!(
        fields.get("properties"),
        Some(&CypherValue::Map(
            [("name".into(), CypherValue::String("Ada".into()))]
                .into_iter()
                .collect()
        ))
    );
}

#[test]
fn whole_nodes_preserve_unlabeled_and_recursive_property_values() {
    let mut database = Database::in_memory().unwrap();

    let created = database
        .execute(
            "CREATE (item {nothing: null, active: true, count: 7, ratio: 1.5, \
                           tags: ['graph', {nested: false}]}) \
             RETURN item",
        )
        .unwrap();

    let CypherValue::Node(created_item) = &created.rows[0][0] else {
        panic!("expected a node")
    };
    assert!(created_item.labels.is_empty());
    assert_eq!(created_item.properties["nothing"], CypherValue::Null);
    assert_eq!(
        created_item.properties["active"],
        CypherValue::Boolean(true)
    );
    assert_eq!(created_item.properties["count"], CypherValue::Integer(7));
    assert_eq!(created_item.properties["ratio"], CypherValue::Float(1.5));
    assert_eq!(
        created_item.properties["tags"],
        CypherValue::List(vec![
            CypherValue::String("graph".into()),
            CypherValue::Map(
                [("nested".into(), CypherValue::Boolean(false))]
                    .into_iter()
                    .collect(),
            ),
        ])
    );

    let matched = database
        .execute("MATCH (item {count: 7}) RETURN item")
        .unwrap();
    assert_eq!(matched.rows, created.rows);
}

#[test]
fn relationship_endpoints_keep_storage_direction_in_an_undirected_match() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Point {name: 'start'})-[:LINK]->(:Point {name: 'end'})")
        .unwrap();

    let result = database
        .execute(
            "MATCH (right:Point {name: 'end'})-[link:LINK]-(left:Point {name: 'start'}) \
             RETURN right, link, left",
        )
        .unwrap();
    let [
        CypherValue::Node(right),
        CypherValue::Relationship(link),
        CypherValue::Node(left),
    ] = result.rows[0].as_slice()
    else {
        panic!("expected a node, relationship, and node row")
    };

    assert_eq!(link.start_node_element_id, left.element_id);
    assert_eq!(link.end_node_element_id, right.element_id);
}

#[test]
fn match_create_carries_matched_and_created_entities_through_rust_execution() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada'})-[:WROTE {since: 2024}]->\
                    (:Project {name: 'crabnet'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (author:Person)-[wrote:WROTE]->(project:Project) \
             CREATE (project)-[tagged:TAGGED]->(tag:Tag {name: 'graph'}) \
             RETURN author, wrote, project, tagged, tag",
        )
        .unwrap();
    let [
        CypherValue::Node(author),
        CypherValue::Relationship(wrote),
        CypherValue::Node(project),
        CypherValue::Relationship(tagged),
        CypherValue::Node(tag),
    ] = result.rows[0].as_slice()
    else {
        panic!("expected all matched and created graph entities")
    };

    assert_eq!(wrote.start_node_element_id, author.element_id);
    assert_eq!(wrote.end_node_element_id, project.element_id);
    assert_eq!(tagged.start_node_element_id, project.element_id);
    assert_eq!(tagged.end_node_element_id, tag.element_id);
    assert_eq!(tag.labels, ["Tag"]);
    assert_eq!(result.summary.nodes_created, 1);
    assert_eq!(result.summary.relationships_created, 1);
}
