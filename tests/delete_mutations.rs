use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crabnet::{CypherValue, Database, Error};
use rusqlite::Connection;
use serde_json::json;

#[test]
fn delete_removes_an_isolated_node() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Target {name: 'gone'}), (:Survivor {name: 'kept'})")
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (target:Target) \
             DELETE target \
             RETURN target.name AS name, target",
        )
        .unwrap();

    assert_eq!(deleted.columns, ["name", "target"]);
    assert_eq!(deleted.rows.len(), 1);
    assert_eq!(deleted.rows[0][0], json!("gone"));
    assert_eq!(
        deleted.rows[0][1].as_node().unwrap().properties.get("name"),
        Some(&CypherValue::String("gone".into()))
    );
    assert_eq!(deleted.summary.nodes_deleted, 1);
    assert_eq!(deleted.summary.relationships_deleted, 0);

    let remaining = database
        .execute("MATCH (node) RETURN node.name AS name")
        .unwrap();
    assert_eq!(remaining.rows, [vec![json!("kept")]]);
}

#[test]
fn deleting_a_relationship_retains_both_endpoint_nodes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Node {name: 'left'})-\
             [:LINK {weight: 3}]->(:Node {name: 'right'})",
        )
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (left)-[link:LINK]->(right) \
             DELETE link \
             RETURN left.name AS left, type(link) AS kind, \
                    link.weight AS weight, right.name AS right, link",
        )
        .unwrap();

    assert_eq!(
        deleted.rows[0][..4],
        [json!("left"), json!("LINK"), json!(3), json!("right")]
    );
    assert_eq!(
        deleted.rows[0][4]
            .as_relationship()
            .unwrap()
            .relationship_type,
        "LINK"
    );
    assert_eq!(deleted.summary.nodes_deleted, 0);
    assert_eq!(deleted.summary.relationships_deleted, 1);

    let nodes = database
        .execute("MATCH (node:Node) RETURN node.name AS name ORDER BY name")
        .unwrap();
    assert_eq!(nodes.rows, [vec![json!("left")], vec![json!("right")]]);
    assert!(
        database
            .execute("MATCH ()-[link]->() RETURN link")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn plain_delete_of_connected_nodes_fails_and_rolls_back_every_target() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Victim {name: 'isolated'}), \
                    (:Victim {name: 'connected'})-[:LINK]->(:Anchor {name: 'anchor'})",
        )
        .unwrap();

    assert!(matches!(
        database.execute("MATCH (victim:Victim) DELETE victim"),
        Err(Error::Constraint(_))
    ));

    let nodes = database
        .execute("MATCH (node) RETURN node.name AS name ORDER BY name")
        .unwrap();
    assert_eq!(
        nodes.rows,
        [
            vec![json!("anchor")],
            vec![json!("connected")],
            vec![json!("isolated")],
        ]
    );
    let edges = database
        .execute(
            "MATCH (from)-[edge:LINK]->(to) \
             RETURN from.name, type(edge), to.name",
        )
        .unwrap();
    assert_eq!(
        edges.rows,
        [vec![json!("connected"), json!("LINK"), json!("anchor")]]
    );
}

#[test]
fn detach_delete_removes_incoming_outgoing_and_self_loop_edges_only() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (center:Center {name: 'center'}), \
                    (:Leaf {name: 'incoming'})-[:IN]->(center), \
                    (center)-[:OUT]->(:Leaf {name: 'outgoing'}), \
                    (center)-[:SELF]->(center), \
                    (:Other {name: 'unrelated-left'})-[:KEPT]->\
                    (:Other {name: 'unrelated-right'})",
        )
        .unwrap();

    let deleted = database
        .execute("MATCH (center:Center) DETACH DELETE center RETURN center.name")
        .unwrap();
    assert_eq!(deleted.rows, [vec![json!("center")]]);
    assert_eq!(deleted.summary.nodes_deleted, 1);
    assert_eq!(deleted.summary.relationships_deleted, 3);

    let nodes = database
        .execute("MATCH (node) RETURN node.name AS name ORDER BY name")
        .unwrap();
    assert_eq!(
        nodes.rows,
        [
            vec![json!("incoming")],
            vec![json!("outgoing")],
            vec![json!("unrelated-left")],
            vec![json!("unrelated-right")],
        ]
    );
    let edges = database
        .execute(
            "MATCH (from)-[edge]->(to) \
             RETURN from.name, type(edge), to.name",
        )
        .unwrap();
    assert_eq!(
        edges.rows,
        [vec![
            json!("unrelated-left"),
            json!("KEPT"),
            json!("unrelated-right"),
        ]]
    );
}

#[test]
fn explicit_delete_target_order_does_not_change_the_result() {
    for query in [
        "MATCH (left:Left)-[edge:LINK]->(right:Right) \
         DELETE left, edge RETURN left.name, type(edge), right.name",
        "MATCH (left:Left)-[edge:LINK]->(right:Right) \
         DELETE edge, left RETURN left.name, type(edge), right.name",
    ] {
        let mut database = Database::in_memory().unwrap();
        database
            .execute(
                "CREATE (:Left {name: 'left'})-[:LINK]->(:Right {name: 'right'}), \
                        (:Kept {name: 'kept'})",
            )
            .unwrap();

        let deleted = database.execute(query).unwrap();
        assert_eq!(
            deleted.rows,
            [vec![json!("left"), json!("LINK"), json!("right")]]
        );
        assert_eq!(deleted.summary.nodes_deleted, 1);
        assert_eq!(deleted.summary.relationships_deleted, 1);

        let remaining = database
            .execute("MATCH (node) RETURN node.name AS name ORDER BY name")
            .unwrap();
        assert_eq!(remaining.rows, [vec![json!("kept")], vec![json!("right")]]);
        assert!(
            database
                .execute("MATCH ()-[edge]->() RETURN edge")
                .unwrap()
                .rows
                .is_empty()
        );
    }
}

#[test]
fn deletes_are_scheduled_globally_across_input_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (center:Center {name: 'center'})-[:LINK {rank: 1}]->\
                    (:Leaf {name: 'one'}), \
                    (center)-[:LINK {rank: 2}]->(:Leaf {name: 'two'})",
        )
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (center:Center)-[edge:LINK]->(leaf) \
             DELETE center, edge \
             RETURN center.name AS center, edge.rank AS rank, leaf.name AS leaf",
        )
        .unwrap();
    let mut rows = deleted.rows;
    rows.sort_by_key(|row| row[1].as_i64().unwrap());
    assert_eq!(
        rows,
        [
            vec![json!("center"), json!(1), json!("one")],
            vec![json!("center"), json!(2), json!("two")],
        ]
    );
    assert_eq!(deleted.summary.nodes_deleted, 1);
    assert_eq!(deleted.summary.relationships_deleted, 2);

    let leaves = database
        .execute("MATCH (leaf:Leaf) RETURN leaf.name AS name ORDER BY name")
        .unwrap();
    assert_eq!(leaves.rows, [vec![json!("one")], vec![json!("two")]]);
}

#[test]
fn duplicate_bag_targets_are_deleted_once_but_keep_return_snapshots() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Target {name: 'target'}), \
                    (:Endpoint {name: 'from'})-[:LINK {weight: 7}]->\
                    (:Endpoint {name: 'to'}), \
                    (:Copy {rank: 1}), (:Copy {rank: 2})",
        )
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (target:Target), ()-[edge:LINK]->(), (copy:Copy) \
             DELETE target, edge \
             RETURN target, target.name AS name, edge, edge.weight AS weight, copy.rank AS rank",
        )
        .unwrap();
    let mut rows = deleted.rows;
    rows.sort_by_key(|row| row[4].as_i64().unwrap());
    assert_eq!(rows.len(), 2);
    for (row, rank) in rows.iter().zip([1, 2]) {
        assert_eq!(
            row[0].as_node().unwrap().properties.get("name"),
            Some(&CypherValue::String("target".into()))
        );
        assert_eq!(row[1], json!("target"));
        assert_eq!(
            row[2].as_relationship().unwrap().properties.get("weight"),
            Some(&CypherValue::Integer(7))
        );
        assert_eq!(row[3], json!(7));
        assert_eq!(row[4], json!(rank));
    }
    assert_eq!(deleted.summary.nodes_deleted, 1);
    assert_eq!(deleted.summary.relationships_deleted, 1);

    assert!(
        database
            .execute("MATCH (target:Target) RETURN target")
            .unwrap()
            .rows
            .is_empty()
    );
    assert!(
        database
            .execute("MATCH ()-[edge:LINK]->() RETURN edge")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn detach_delete_deduplicates_an_edge_shared_by_deleted_nodes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Target {name: 'left'})-[:SHARED]->\
                    (:Target {name: 'right'})",
        )
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (target:Target) \
             DETACH DELETE target \
             RETURN target.name AS name",
        )
        .unwrap();
    let mut rows = deleted.rows;
    rows.sort_by(|left, right| left[0].as_str().unwrap().cmp(right[0].as_str().unwrap()));
    assert_eq!(rows, [vec![json!("left")], vec![json!("right")]]);
    assert_eq!(deleted.summary.nodes_deleted, 2);
    assert_eq!(deleted.summary.relationships_deleted, 1);

    assert!(
        database
            .execute("MATCH (node) RETURN node")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn detach_delete_deduplicates_an_explicit_relationship_target() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Target {name: 'left'})-[edge:LINK {weight: 4}]->\
                    (:Survivor {name: 'right'})",
        )
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (left:Target)-[edge:LINK]->(right:Survivor) \
             DETACH DELETE left, edge \
             RETURN left.name, edge.weight, right.name",
        )
        .unwrap();
    assert_eq!(
        deleted.rows,
        [vec![json!("left"), json!(4), json!("right")]]
    );
    assert_eq!(deleted.summary.nodes_deleted, 1);
    assert_eq!(deleted.summary.relationships_deleted, 1);

    let remaining = database.execute("MATCH (node) RETURN node.name").unwrap();
    assert_eq!(remaining.rows, [vec![json!("right")]]);
}

#[test]
fn delete_is_a_noop_when_match_has_no_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Kept {name: 'left'})-[:KEPT]->(:Kept {name: 'right'})")
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (missing:Missing) \
             DETACH DELETE missing RETURN missing",
        )
        .unwrap();
    assert!(deleted.rows.is_empty());
    assert_eq!(deleted.summary.nodes_deleted, 0);
    assert_eq!(deleted.summary.relationships_deleted, 0);

    let stored = database
        .execute(
            "MATCH (left:Kept)-[edge:KEPT]->(right:Kept) \
             RETURN left.name, type(edge), right.name",
        )
        .unwrap();
    assert_eq!(
        stored.rows,
        [vec![json!("left"), json!("KEPT"), json!("right")]]
    );
}

#[test]
fn property_updates_compose_with_delete_and_are_visible_in_return_snapshots() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Target {name: 'gone', status: 'old'})")
        .unwrap();

    let deleted = database
        .execute(
            "MATCH (target:Target) \
             SET target.status = 'deleted' \
             DELETE target \
             RETURN target.name, target.status, target",
        )
        .unwrap();

    assert_eq!(deleted.rows[0][..2], [json!("gone"), json!("deleted")]);
    assert_eq!(
        deleted.rows[0][2]
            .as_node()
            .unwrap()
            .properties
            .get("status"),
        Some(&CypherValue::String("deleted".into()))
    );
    assert_eq!(deleted.summary.properties_set, 1);
    assert_eq!(deleted.summary.nodes_deleted, 1);
    assert!(
        database
            .execute("MATCH (target:Target) RETURN target")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn failed_delete_rolls_back_property_mutations_from_an_earlier_clause() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Target {name: 'kept', status: 'original'})-[:LINK]->\
                    (:Anchor {name: 'anchor'})",
        )
        .unwrap();

    assert!(matches!(
        database.execute(
            "MATCH (target:Target) \
             SET target.status = 'temporary' \
             DELETE target"
        ),
        Err(Error::Constraint(_))
    ));

    let stored = database
        .execute(
            "MATCH (target:Target)-[edge:LINK]->(anchor:Anchor) \
             RETURN target.status, type(edge), anchor.name",
        )
        .unwrap();
    assert_eq!(
        stored.rows,
        [vec![json!("original"), json!("LINK"), json!("anchor")]]
    );
}

#[test]
fn delete_rejects_unknown_and_non_entity_targets_before_mutating() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Item {name: 'kept'})").unwrap();

    for query in [
        "MATCH (item:Item) DELETE missing",
        "MATCH (item:Item) DELETE item.name",
        "MATCH (item:Item) DELETE 1",
    ] {
        assert!(matches!(database.execute(query), Err(Error::Semantic(_))));
    }

    let stored = database
        .execute("MATCH (item:Item) RETURN item.name")
        .unwrap();
    assert_eq!(stored.rows, [vec![json!("kept")]]);
}

#[test]
fn delete_must_be_the_final_mutation_clause() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Item {name: 'kept'})").unwrap();

    for query in [
        "MATCH (item:Item) DELETE item SET item.changed = true",
        "MATCH (item:Item) DELETE item REMOVE item.name",
        "MATCH (item:Item) DELETE item DELETE item",
    ] {
        assert!(matches!(
            database.execute(query),
            Err(Error::Unsupported(_))
        ));
    }

    let stored = database
        .execute("MATCH (item:Item) RETURN item.name, item.changed")
        .unwrap();
    assert_eq!(stored.rows, [vec![json!("kept"), json!(null)]]);
}

#[test]
fn a_runtime_failure_rolls_back_detached_relationship_deletes() {
    let path = TemporaryDatabasePath::new();
    let mut database = Database::open(path.as_ref()).unwrap();
    database
        .execute(
            "CREATE (center:Center {name: 'center'})-[:LINK {rank: 1}]->\
                    (:Leaf {name: 'one'}), \
                    (center)-[:LINK {rank: 2}]->(:Leaf {name: 'two'})",
        )
        .unwrap();
    drop(database);

    let connection = Connection::open(path.as_ref()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_center_delete
             BEFORE DELETE ON nodes
             WHEN OLD.label = 'Center'
             BEGIN
                 SELECT RAISE(ABORT, 'rejected node delete for rollback test');
             END;",
        )
        .unwrap();
    drop(connection);

    let mut database = Database::open(path.as_ref()).unwrap();
    assert!(matches!(
        database.execute("MATCH (center:Center) DETACH DELETE center"),
        Err(Error::Sqlite(_))
    ));

    let nodes = database
        .execute("MATCH (node) RETURN node.name AS name ORDER BY name")
        .unwrap();
    assert_eq!(
        nodes.rows,
        [
            vec![json!("center")],
            vec![json!("one")],
            vec![json!("two")],
        ]
    );
    let edges = database
        .execute(
            "MATCH (center:Center)-[edge:LINK]->(leaf:Leaf) \
             RETURN edge.rank AS rank ORDER BY rank",
        )
        .unwrap();
    assert_eq!(edges.rows, [vec![json!(1)], vec![json!(2)]]);
}

static NEXT_TEMPORARY_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TemporaryDatabasePath(PathBuf);

impl TemporaryDatabasePath {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_TEMPORARY_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "crabnet-delete-mutations-{}-{nonce}-{sequence}.sqlite",
            std::process::id()
        )))
    }
}

impl AsRef<Path> for TemporaryDatabasePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDatabasePath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
