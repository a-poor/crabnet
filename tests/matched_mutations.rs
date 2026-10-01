use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use crabnet::{CypherValue, Database, Error, Parameters};
use rusqlite::Connection;
use serde_json::json;

fn parameters<const N: usize>(values: [(&str, serde_json::Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

#[test]
fn set_updates_a_node_property_and_the_returned_entity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Ada', active: false})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (person:Person {name: 'Ada'}) \
             SET person.active = true \
             RETURN person.name AS name, person.active AS active, person",
        )
        .unwrap();

    assert_eq!(updated.columns, ["name", "active", "person"]);
    assert_eq!(updated.rows.len(), 1);
    assert_eq!(updated.rows[0][0], json!("Ada"));
    assert_eq!(updated.rows[0][1], json!(true));
    let person = updated.rows[0][2].as_node().unwrap();
    assert_eq!(
        person.properties.get("active"),
        Some(&CypherValue::Boolean(true))
    );
    assert_eq!(updated.summary.nodes_created, 0);
    assert_eq!(updated.summary.relationships_created, 0);
    assert_eq!(updated.summary.properties_set, 1);

    let stored = database
        .execute(
            "MATCH (person:Person {name: 'Ada'}) \
             RETURN person.active AS active",
        )
        .unwrap();
    assert_eq!(stored.rows, [vec![json!(true)]]);
}

#[test]
fn set_updates_relationship_properties_and_the_returned_entity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Node {name: 'from'})-\
             [:LINK {weight: 1, retained: true}]->(:Node {name: 'to'})",
        )
        .unwrap();

    let updated = database
        .execute(
            "MATCH ()-[link:LINK]->() \
             SET link.weight = 2, link.note = 'updated' \
             RETURN link.weight AS weight, link.note AS note, link",
        )
        .unwrap();

    assert_eq!(updated.rows.len(), 1);
    assert_eq!(updated.rows[0][0], json!(2));
    assert_eq!(updated.rows[0][1], json!("updated"));
    let link = updated.rows[0][2].as_relationship().unwrap();
    assert_eq!(
        link.properties.get("weight"),
        Some(&CypherValue::Integer(2))
    );
    assert_eq!(
        link.properties.get("note"),
        Some(&CypherValue::String("updated".into()))
    );
    assert_eq!(
        link.properties.get("retained"),
        Some(&CypherValue::Boolean(true))
    );
    assert_eq!(updated.summary.properties_set, 2);

    let stored = database
        .execute(
            "MATCH ()-[link:LINK]->() \
             RETURN link.weight AS weight, link.note AS note, link.retained AS retained",
        )
        .unwrap();
    assert_eq!(stored.rows, [vec![json!(2), json!("updated"), json!(true)]]);
}

#[test]
fn setting_a_property_to_null_removes_it() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Thing {name: 'kept', obsolete: 'remove me'})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (thing:Thing) \
             SET thing.obsolete = null \
             RETURN thing.obsolete AS obsolete, thing",
        )
        .unwrap();

    assert_eq!(updated.rows[0][0], CypherValue::Null);
    let thing = updated.rows[0][1].as_node().unwrap();
    assert!(!thing.properties.contains_key("obsolete"));
    assert_eq!(
        thing.properties.get("name"),
        Some(&CypherValue::String("kept".into()))
    );

    let stored = database
        .execute("MATCH (thing:Thing) RETURN thing")
        .unwrap();
    assert!(
        !stored.rows[0][0]
            .as_node()
            .unwrap()
            .properties
            .contains_key("obsolete")
    );
}

#[test]
fn set_plus_equals_merges_a_map_without_replacing_other_properties() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Ada', score: 1, retained: true})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (person:Person) \
             SET person += {score: 2, added: ['graph', 'sqlite']} \
             RETURN person",
        )
        .unwrap();

    let person = updated.rows[0][0].as_node().unwrap();
    assert_eq!(
        person.properties,
        [
            (
                "added".into(),
                CypherValue::List(vec![
                    CypherValue::String("graph".into()),
                    CypherValue::String("sqlite".into()),
                ]),
            ),
            ("name".into(), CypherValue::String("Ada".into())),
            ("retained".into(), CypherValue::Boolean(true)),
            ("score".into(), CypherValue::Integer(2)),
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(updated.summary.properties_set, 2);

    let stored = database
        .execute(
            "MATCH (person:Person) \
             RETURN person.name, person.score, person.retained, person.added",
        )
        .unwrap();
    assert_eq!(
        stored.rows,
        [vec![
            json!("Ada"),
            json!(2),
            json!(true),
            json!(["graph", "sqlite"]),
        ]]
    );
}

#[test]
fn remove_deletes_node_and_relationship_properties() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (from:Node {name: 'from', temporary: true})-\
             [link:LINK {weight: 3, temporary: true}]->\
             (:Node {name: 'to'})",
        )
        .unwrap();

    let updated = database
        .execute(
            "MATCH (from:Node {name: 'from'})-[link:LINK]->() \
             REMOVE from.temporary, link.temporary \
             RETURN from, link",
        )
        .unwrap();

    let from = updated.rows[0][0].as_node().unwrap();
    let link = updated.rows[0][1].as_relationship().unwrap();
    assert!(!from.properties.contains_key("temporary"));
    assert!(!link.properties.contains_key("temporary"));
    assert_eq!(
        from.properties.get("name"),
        Some(&CypherValue::String("from".into()))
    );
    assert_eq!(
        link.properties.get("weight"),
        Some(&CypherValue::Integer(3))
    );
    assert_eq!(updated.summary.properties_set, 2);

    let stored = database
        .execute(
            "MATCH (from:Node {name: 'from'})-[link:LINK]->() \
             RETURN from.temporary, link.temporary",
        )
        .unwrap();
    assert_eq!(stored.rows, [vec![CypherValue::Null, CypherValue::Null]]);
}

#[test]
fn matched_set_runs_once_for_every_input_row() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:Person {name: 'Ada'}), (:Person {name: 'Grace'}), \
                    (:Person {name: 'Margaret'})",
        )
        .unwrap();

    let updated = database
        .execute(
            "MATCH (person:Person) \
             SET person.cohort = 'updated' \
             RETURN person.name AS name, person.cohort AS cohort",
        )
        .unwrap();
    let mut rows = updated.rows;
    rows.sort_by(|left, right| left[0].as_str().unwrap().cmp(right[0].as_str().unwrap()));
    assert_eq!(
        rows,
        [
            vec![json!("Ada"), json!("updated")],
            vec![json!("Grace"), json!("updated")],
            vec![json!("Margaret"), json!("updated")],
        ]
    );

    let stored = database
        .execute(
            "MATCH (person:Person) \
             RETURN person.name AS name, person.cohort AS cohort ORDER BY name",
        )
        .unwrap();
    assert_eq!(stored.rows, rows);
}

#[test]
fn matched_set_and_remove_are_noops_when_match_has_no_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Person {name: 'Ada', retained: true})")
        .unwrap();

    let set = database
        .execute(
            "MATCH (person:Person {name: 'missing'}) \
             SET person.changed = true RETURN person",
        )
        .unwrap();
    assert!(set.rows.is_empty());
    assert_eq!(set.summary.nodes_created, 0);
    assert_eq!(set.summary.relationships_created, 0);
    assert_eq!(set.summary.properties_set, 0);

    let remove = database
        .execute(
            "MATCH (person:Person {name: 'missing'}) \
             REMOVE person.retained RETURN person",
        )
        .unwrap();
    assert!(remove.rows.is_empty());

    let stored = database
        .execute(
            "MATCH (person:Person {name: 'Ada'}) \
             RETURN person.changed, person.retained",
        )
        .unwrap();
    assert_eq!(
        stored.rows,
        [vec![CypherValue::Null, CypherValue::Boolean(true)]]
    );
}

#[test]
fn parameters_work_for_property_assignment_and_map_merge() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Document {name: 'plan', version: 1})")
        .unwrap();
    let values = parameters([
        ("payload", json!(["nodes", "edges"])),
        ("patch", json!({"version": 2, "published": true})),
    ]);

    let updated = database
        .execute_with(
            "MATCH (document:Document {name: 'plan'}) \
             SET document.payload = $payload, document += $patch \
             RETURN document.payload, document.version, document.published",
            &values,
        )
        .unwrap();

    assert_eq!(
        updated.rows,
        [vec![json!(["nodes", "edges"]), json!(2), json!(true)]]
    );

    let stored = database
        .execute(
            "MATCH (document:Document {name: 'plan'}) \
             RETURN document.payload, document.version, document.published",
        )
        .unwrap();
    assert_eq!(stored.rows, updated.rows);
}

#[test]
fn planning_and_parameter_errors_do_not_apply_earlier_set_items() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {name: 'original', retained: true})")
        .unwrap();

    assert!(matches!(
        database.execute(
            "MATCH (item:Item) \
             SET item.changed = true, missing.value = 1"
        ),
        Err(Error::Semantic(_))
    ));
    assert!(matches!(
        database.execute_with(
            "MATCH (item:Item) \
             SET item.changed = true, item.value = $missing",
            &Parameters::new(),
        ),
        Err(Error::MissingParameter(name)) if name == "missing"
    ));
    assert!(matches!(
        database.execute_with(
            "MATCH (item:Item) \
             SET item.changed = true, item += $not_a_map",
            &parameters([("not_a_map", json!([1, 2]))]),
        ),
        Err(Error::Semantic(_))
    ));

    let stored = database
        .execute(
            "MATCH (item:Item) \
             RETURN item.name, item.retained, item.changed, item.value",
        )
        .unwrap();
    assert_eq!(
        stored.rows,
        [vec![
            json!("original"),
            json!(true),
            json!(null),
            json!(null),
        ]]
    );
}

#[test]
fn label_set_and_remove_apply_alongside_property_changes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {name: 'original'})")
        .unwrap();

    let changed = database
        .execute(
            "MATCH (item:Item) \
             SET item.changed = true, item:Changed \
             RETURN labels(item) AS labels",
        )
        .unwrap();
    assert_eq!(changed.rows, [vec![json!(["Item", "Changed"])]]);
    assert_eq!(changed.summary.properties_set, 1);
    assert_eq!(changed.summary.labels_added, 1);

    let removed = database
        .execute(
            "MATCH (item:Item) \
             SET item.changed = false REMOVE item:Item \
             RETURN labels(item) AS labels, item.changed AS changed",
        )
        .unwrap();
    assert_eq!(removed.rows, [vec![json!(["Changed"]), json!(false)]]);
    assert_eq!(removed.summary.labels_removed, 1);

    let stored = database
        .execute("MATCH (item:Changed) RETURN item.name, item.changed")
        .unwrap();
    assert_eq!(stored.rows, [vec![json!("original"), json!(false)]]);
    assert!(
        database
            .execute("MATCH (item:Item) RETURN item")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn a_runtime_failure_rolls_back_property_updates_from_earlier_rows() {
    let path = TemporaryDatabasePath::new();
    let mut database = Database::open(path.as_ref()).unwrap();
    database
        .execute("CREATE (:Item {rank: 1}), (:Item {rank: 2})")
        .unwrap();
    drop(database);

    let connection = Connection::open(path.as_ref()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_second_property_update
             BEFORE UPDATE OF props ON nodes
             WHEN (
                 SELECT count(*) FROM nodes
                 WHERE json_extract(props, '$.status') = 'updated'
             ) = 1
             BEGIN
                 SELECT RAISE(ABORT, 'rejected second update for rollback test');
             END;",
        )
        .unwrap();
    drop(connection);

    let mut database = Database::open(path.as_ref()).unwrap();
    assert!(matches!(
        database.execute(
            "MATCH (item:Item) \
             SET item.status = 'updated'"
        ),
        Err(Error::Sqlite(_))
    ));

    let stored = database
        .execute(
            "MATCH (item:Item) \
             RETURN item.rank AS rank, item.status AS status ORDER BY rank",
        )
        .unwrap();
    assert_eq!(
        stored.rows,
        [vec![json!(1), json!(null)], vec![json!(2), json!(null)],]
    );
}

#[test]
fn set_equals_replaces_the_complete_property_map() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {old: 1, remove: true})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (item:Item) \
             SET item = {replacement: ['typed']} \
             RETURN item.old, item.remove, item.replacement, item",
        )
        .unwrap();

    assert_eq!(updated.rows[0][0], CypherValue::Null);
    assert_eq!(updated.rows[0][1], CypherValue::Null);
    assert_eq!(updated.rows[0][2], json!(["typed"]));
    assert_eq!(
        updated.rows[0][3].as_node().unwrap().properties,
        [(
            "replacement".into(),
            CypherValue::List(vec![CypherValue::String("typed".into())]),
        )]
        .into_iter()
        .collect()
    );
    assert_eq!(updated.summary.properties_set, 3);
}

#[test]
fn map_merge_is_shallow_and_null_entries_remove_properties() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {nested: {left: 1, retained: true}, gone: 2, keep: 3})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (item:Item) \
             SET item += {nested: {right: 4}, gone: null} \
             RETURN item.nested, item.gone, item.keep",
        )
        .unwrap();
    assert_eq!(
        updated.rows,
        [vec![json!({"right": 4}), json!(null), json!(3)]]
    );
    assert_eq!(updated.summary.properties_set, 2);
}

#[test]
fn property_updates_refresh_every_alias_of_the_same_entity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (node:Node {name: 'loop'})-[:SELF]->(node)")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (left:Node)-[:SELF]->(right:Node) \
             SET left.first = 1, right.second = 2 \
             RETURN left.second, right.first, left, right",
        )
        .unwrap();

    assert_eq!(updated.rows[0][0], json!(2));
    assert_eq!(updated.rows[0][1], json!(1));
    assert_eq!(updated.rows[0][2], updated.rows[0][3]);
    assert_eq!(updated.summary.properties_set, 2);
}

#[test]
fn property_updates_refresh_aliases_across_the_complete_input_bag() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {name: 'Ada'}), (:Item {name: 'Grace'})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (left:Item), (right:Item) \
             SET left.marked = true \
             RETURN left.name AS left, right.name AS right, right.marked AS marked",
        )
        .unwrap();
    let mut rows = updated.rows;
    rows.sort_by(|left, right| {
        left[0]
            .as_str()
            .unwrap()
            .cmp(right[0].as_str().unwrap())
            .then_with(|| left[1].as_str().unwrap().cmp(right[1].as_str().unwrap()))
    });

    assert_eq!(
        rows,
        [
            vec![json!("Ada"), json!("Ada"), json!(true)],
            vec![json!("Ada"), json!("Grace"), json!(true)],
            vec![json!("Grace"), json!("Ada"), json!(true)],
            vec![json!("Grace"), json!("Grace"), json!(true)],
        ]
    );
    assert_eq!(updated.summary.properties_set, 2);
}

#[test]
fn removing_an_absent_property_is_not_counted_as_a_change() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Item {name: 'kept'})").unwrap();

    let updated = database
        .execute("MATCH (item:Item) REMOVE item.missing RETURN item.name")
        .unwrap();
    assert_eq!(updated.rows, [vec![json!("kept")]]);
    assert_eq!(updated.summary.properties_set, 0);
}

#[test]
fn property_mutation_clauses_run_in_source_order() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {first: 1, removed: true})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (item:Item) \
             SET item.first = 2, item.temporary = 'discarded' \
             REMOVE item.removed \
             SET item += {temporary: null, final: ['kept']} \
             RETURN item.first, item.removed, item.temporary, item.final, item",
        )
        .unwrap();

    assert_eq!(
        updated.rows[0][..4],
        [json!(2), json!(null), json!(null), json!(["kept"])]
    );
    assert_eq!(
        updated.rows[0][4].as_node().unwrap().properties,
        [
            (
                "final".into(),
                CypherValue::List(vec![CypherValue::String("kept".into())]),
            ),
            ("first".into(), CypherValue::Integer(2)),
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(updated.summary.properties_set, 5);
}

#[test]
fn a_relationship_property_map_can_be_replaced_from_a_parameter() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE ()-[:LINK {weight: 1, old: true}]->()")
        .unwrap();

    let updated = database
        .execute_with(
            "MATCH ()-[link:LINK]->() \
             SET link = $properties \
             RETURN link.weight, link.note, link.old, link",
            &parameters([("properties", json!({"weight": 9, "note": "new"}))]),
        )
        .unwrap();

    assert_eq!(updated.rows[0][..3], [json!(9), json!("new"), json!(null)]);
    assert_eq!(
        updated.rows[0][3].as_relationship().unwrap().properties,
        [
            ("note".into(), CypherValue::String("new".into())),
            ("weight".into(), CypherValue::Integer(9)),
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(updated.summary.properties_set, 3);
}

#[test]
fn property_summary_counts_only_observable_storage_changes() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {name: 'same', score: 1})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (item:Item) \
             SET item.name = 'same', item += {score: 1} \
             RETURN item.name, item.score",
        )
        .unwrap();

    assert_eq!(updated.rows, [vec![json!("same"), json!(1)]]);
    assert_eq!(updated.summary.properties_set, 0);
}

#[test]
fn whole_map_replacement_drops_null_entries() {
    let mut database = Database::in_memory().unwrap();
    database.execute("CREATE (:Item {old: true})").unwrap();

    let updated = database
        .execute(
            "MATCH (item:Item) \
             SET item = {explicit: null, retained: 'yes'} \
             RETURN item",
        )
        .unwrap();
    let item = updated.rows[0][0].as_node().unwrap();

    assert!(!item.properties.contains_key("explicit"));
    assert_eq!(
        item.properties.get("retained"),
        Some(&CypherValue::String("yes".into()))
    );
    assert!(!item.properties.contains_key("old"));
    assert_eq!(updated.summary.properties_set, 2);
}

#[test]
fn computed_set_values_are_evaluated_per_row() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:Item {name: 'kept', n: 1}), (:Item {name: 'other', n: 2})")
        .unwrap();

    let updated = database
        .execute(
            "MATCH (item:Item) \
             SET item.changed = true, item.copy = item.name, item.n = item.n * 10 \
             RETURN item.name AS name, item.copy AS copy, item.n AS n ORDER BY name",
        )
        .unwrap();
    assert_eq!(
        updated.rows,
        [
            vec![json!("kept"), json!("kept"), json!(10)],
            vec![json!("other"), json!("other"), json!(20)],
        ]
    );
    assert_eq!(updated.summary.properties_set, 6);

    let stored = database
        .execute("MATCH (item:Item) RETURN item.name, item.changed, item.copy, item.n ORDER BY item.name")
        .unwrap();
    assert_eq!(
        stored.rows,
        [
            vec![json!("kept"), json!(true), json!("kept"), json!(10)],
            vec![json!("other"), json!(true), json!("other"), json!(20)],
        ]
    );
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
            "crabnet-matched-mutations-{}-{nonce}-{sequence}.sqlite",
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
