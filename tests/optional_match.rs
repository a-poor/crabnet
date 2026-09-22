use std::collections::BTreeSet;

use crabnet::{CypherValue, Database, Error};

#[test]
fn optional_match_null_extends_unmatched_input_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalPerson {name: 'Ada'}), \
                    (:OptionalPerson {name: 'Grace'}), \
                    (:OptionalProject {name: 'crabnet'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (person:OptionalPerson {name: 'Ada'}), \
                   (project:OptionalProject {name: 'crabnet'}) \
             CREATE (person)-[:WORKED_ON]->(project)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:OptionalPerson) \
             OPTIONAL MATCH (person)-[:WORKED_ON]->(project:OptionalProject) \
             RETURN person.name AS person, project.name AS project \
             ORDER BY person",
        )
        .unwrap();

    assert_eq!(result.columns, ["person", "project"]);
    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("Ada".into()),
                CypherValue::String("crabnet".into()),
            ],
            vec![CypherValue::String("Grace".into()), CypherValue::Null],
        ]
    );
    assert_eq!(result.summary, Default::default());
}

#[test]
fn optional_where_filters_candidates_without_removing_outer_rows() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalMember {name: 'Ada'}), \
                    (:OptionalMember {name: 'Grace'}), \
                    (:OptionalMember {name: 'Linus'}), \
                    (:OptionalTeam {name: 'compiler', active: true}), \
                    (:OptionalTeam {name: 'archive', active: false})",
        )
        .unwrap();
    for query in [
        "MATCH (person:OptionalMember {name: 'Ada'}), \
               (team:OptionalTeam {name: 'compiler'}) \
         CREATE (person)-[:MEMBER_OF {level: 'primary'}]->(team)",
        "MATCH (person:OptionalMember {name: 'Grace'}), \
               (team:OptionalTeam {name: 'archive'}) \
         CREATE (person)-[:MEMBER_OF {level: 'primary'}]->(team)",
        "MATCH (person:OptionalMember {name: 'Linus'}), \
               (team:OptionalTeam {name: 'compiler'}) \
         CREATE (person)-[:MEMBER_OF {level: 'secondary'}]->(team)",
    ] {
        database.execute(query).unwrap();
    }

    let result = database
        .execute(
            "MATCH (person:OptionalMember) \
             OPTIONAL MATCH (person)-[membership:MEMBER_OF]->(team:OptionalTeam) \
             WHERE team.active = true AND membership.level = 'primary' \
             RETURN person.name AS person, team.name AS team \
             ORDER BY person",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("Ada".into()),
                CypherValue::String("compiler".into()),
            ],
            vec![CypherValue::String("Grace".into()), CypherValue::Null],
            vec![CypherValue::String("Linus".into()), CypherValue::Null],
        ]
    );
}

#[test]
fn optional_relationship_patterns_respect_direction_type_and_properties() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalOwner {name: 'Ada'}), \
                    (:OptionalOwner {name: 'Grace'}), \
                    (:OptionalAsset {name: 'art'}), \
                    (:OptionalAsset {name: 'docs'}), \
                    (:OptionalAsset {name: 'music'})",
        )
        .unwrap();
    for query in [
        "MATCH (owner:OptionalOwner {name: 'Ada'}), \
               (asset:OptionalAsset {name: 'docs'}) \
         CREATE (owner)-[:OWNS {rank: 1}]->(asset)",
        "MATCH (owner:OptionalOwner {name: 'Grace'}), \
               (asset:OptionalAsset {name: 'art'}) \
         CREATE (owner)-[:OWNS {rank: 2}]->(asset)",
        "MATCH (owner:OptionalOwner {name: 'Ada'}), \
               (asset:OptionalAsset {name: 'music'}) \
         CREATE (owner)-[:WATCHES {rank: 1}]->(asset)",
    ] {
        database.execute(query).unwrap();
    }

    let result = database
        .execute(
            "MATCH (asset:OptionalAsset) \
             OPTIONAL MATCH (asset)<-[ownership:OWNS {rank: 1}]-(owner:OptionalOwner) \
             RETURN asset.name AS asset, owner.name AS owner, \
                    type(ownership) AS kind, ownership.rank AS rank \
             ORDER BY asset",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("art".into()),
                CypherValue::Null,
                CypherValue::Null,
                CypherValue::Null,
            ],
            vec![
                CypherValue::String("docs".into()),
                CypherValue::String("Ada".into()),
                CypherValue::String("OWNS".into()),
                CypherValue::Integer(1),
            ],
            vec![
                CypherValue::String("music".into()),
                CypherValue::Null,
                CypherValue::Null,
                CypherValue::Null,
            ],
        ]
    );
}

#[test]
fn chained_optional_matches_propagate_nullable_bindings() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalAuthor {name: 'Ada'}), \
                    (:OptionalAuthor {name: 'Grace'}), \
                    (:OptionalAuthor {name: 'Linus'}), \
                    (:OptionalContact {name: 'editor'}), \
                    (:OptionalContact {name: 'reviewer'}), \
                    (:OptionalPublication {name: 'manual'})",
        )
        .unwrap();
    for query in [
        "MATCH (author:OptionalAuthor {name: 'Ada'}), \
               (contact:OptionalContact {name: 'editor'}) \
         CREATE (author)-[:HAS_CONTACT]->(contact)",
        "MATCH (author:OptionalAuthor {name: 'Linus'}), \
               (contact:OptionalContact {name: 'reviewer'}) \
         CREATE (author)-[:HAS_CONTACT]->(contact)",
        "MATCH (contact:OptionalContact {name: 'editor'}), \
               (publication:OptionalPublication {name: 'manual'}) \
         CREATE (contact)-[:EDITED]->(publication)",
    ] {
        database.execute(query).unwrap();
    }

    let result = database
        .execute(
            "MATCH (author:OptionalAuthor) \
             OPTIONAL MATCH (author)-[:HAS_CONTACT]->(contact:OptionalContact) \
             OPTIONAL MATCH (contact)-[:EDITED]->(publication:OptionalPublication) \
             RETURN author.name AS author, contact.name AS contact, \
                    publication.name AS publication \
             ORDER BY author",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("Ada".into()),
                CypherValue::String("editor".into()),
                CypherValue::String("manual".into()),
            ],
            vec![
                CypherValue::String("Grace".into()),
                CypherValue::Null,
                CypherValue::Null,
            ],
            vec![
                CypherValue::String("Linus".into()),
                CypherValue::String("reviewer".into()),
                CypherValue::Null,
            ],
        ]
    );
}

#[test]
fn optional_match_composes_after_unwind_and_with_without_losing_duplicates() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute("CREATE (:OptionalLookup {name: 'Ada'}), (:OptionalLookup {name: 'Grace'})")
        .unwrap();

    let result = database
        .execute(
            "UNWIND ['Ada', 'missing', 'Ada'] AS wanted \
             WITH wanted \
             OPTIONAL MATCH (person:OptionalLookup) WHERE person.name = wanted \
             RETURN wanted, person.name AS found ORDER BY wanted",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [
            vec![
                CypherValue::String("Ada".into()),
                CypherValue::String("Ada".into()),
            ],
            vec![
                CypherValue::String("Ada".into()),
                CypherValue::String("Ada".into()),
            ],
            vec![CypherValue::String("missing".into()), CypherValue::Null,],
        ]
    );
}

#[test]
fn optional_match_over_unit_input_emits_one_null_row_when_nothing_matches() {
    let mut database = Database::in_memory().unwrap();

    let result = database
        .execute(
            "OPTIONAL MATCH (ghost:OptionalDefinitelyMissing) \
             RETURN ghost, ghost.name AS name, elementId(ghost) AS id, \
                    labels(ghost) AS labels",
        )
        .unwrap();

    assert_eq!(result.columns, ["ghost", "name", "id", "labels"]);
    assert_eq!(
        result.rows,
        [vec![
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
            CypherValue::Null,
        ]]
    );
}

#[test]
fn whole_optional_entities_and_graph_functions_are_null_extended() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalWholePerson {name: 'Ada'}), \
                    (:OptionalWholePerson {name: 'Grace'}), \
                    (:OptionalWholeProject {name: 'crabnet'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (person:OptionalWholePerson {name: 'Ada'}), \
                   (project:OptionalWholeProject {name: 'crabnet'}) \
             CREATE (person)-[:CONTRIBUTED {role: 'author'}]->(project)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (person:OptionalWholePerson) \
             OPTIONAL MATCH (person)-[contribution:CONTRIBUTED]->\
                            (project:OptionalWholeProject) \
             RETURN person.name AS person, project, contribution, project.name AS project_name, \
                    elementId(project) AS project_id, labels(project) AS project_labels, \
                    type(contribution) AS contribution_type \
             ORDER BY person",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::String("Ada".into()));
    assert!(matches!(result.rows[0][1], CypherValue::Node(_)));
    assert!(matches!(result.rows[0][2], CypherValue::Relationship(_)));
    assert_eq!(result.rows[0][3], CypherValue::String("crabnet".into()));
    assert!(matches!(result.rows[0][4], CypherValue::String(_)));
    assert_eq!(
        result.rows[0][5],
        CypherValue::List(vec![CypherValue::String("OptionalWholeProject".into())])
    );
    assert_eq!(result.rows[0][6], CypherValue::String("CONTRIBUTED".into()));

    assert_eq!(result.rows[1][0], CypherValue::String("Grace".into()));
    assert!(
        result.rows[1][1..]
            .iter()
            .all(|value| value == &CypherValue::Null)
    );
}

#[test]
fn optional_match_preserves_one_to_many_and_parallel_edge_multiplicity() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalBagSource {name: 'connected'}), \
                    (:OptionalBagSource {name: 'lonely'}), \
                    (:OptionalBagTarget {name: 'target'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (source:OptionalBagSource {name: 'connected'}), \
                   (target:OptionalBagTarget {name: 'target'}) \
             CREATE (source)-[:OPTIONAL_LINK]->(target), \
                    (source)-[:OPTIONAL_LINK]->(target)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (source:OptionalBagSource) \
             OPTIONAL MATCH (source)-[link:OPTIONAL_LINK]->(target:OptionalBagTarget) \
             RETURN source.name AS source, link, target.name AS target",
        )
        .unwrap();

    assert_eq!(result.rows.len(), 3);
    let connected = result
        .rows
        .iter()
        .filter(|row| row[0] == CypherValue::String("connected".into()))
        .collect::<Vec<_>>();
    assert_eq!(connected.len(), 2);
    assert!(connected.iter().all(|row| {
        matches!(row[1], CypherValue::Relationship(_))
            && row[2] == CypherValue::String("target".into())
    }));
    let relationship_ids = connected
        .iter()
        .map(|row| row[1].as_relationship().unwrap().element_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(relationship_ids.len(), 2);

    let lonely = result
        .rows
        .iter()
        .find(|row| row[0] == CypherValue::String("lonely".into()))
        .unwrap();
    assert_eq!(lonely[1], CypherValue::Null);
    assert_eq!(lonely[2], CypherValue::Null);
}

#[test]
fn a_partial_optional_path_nulls_every_entity_introduced_by_the_clause() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalPartialSource {name: 'source'}), \
                    (:OptionalWrongTarget {name: 'wrong'})",
        )
        .unwrap();
    database
        .execute(
            "MATCH (source:OptionalPartialSource), (target:OptionalWrongTarget) \
             CREATE (source)-[:PARTIAL_LINK]->(target)",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (source:OptionalPartialSource) \
             OPTIONAL MATCH (source)-[link:PARTIAL_LINK]->(target:OptionalWantedTarget) \
             RETURN source.name AS source, link, target",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("source".into()),
            CypherValue::Null,
            CypherValue::Null,
        ]]
    );
}

#[test]
fn comma_separated_optional_patterns_are_null_extended_as_one_clause() {
    let mut database = Database::in_memory().unwrap();
    database
        .execute(
            "CREATE (:OptionalCommaInput {name: 'input'}), \
                    (:OptionalCommaPresent {name: 'present'})",
        )
        .unwrap();

    let result = database
        .execute(
            "MATCH (input:OptionalCommaInput) \
             OPTIONAL MATCH (present:OptionalCommaPresent), \
                            (missing:OptionalCommaMissing) \
             RETURN input.name AS input, present, missing",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        [vec![
            CypherValue::String("input".into()),
            CypherValue::Null,
            CypherValue::Null,
        ]]
    );
}

#[test]
fn optional_patterns_reject_incompatible_rebindings_semantically() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "WITH 1 AS item OPTIONAL MATCH (item) RETURN item",
        "MATCH (item:OptionalKind) OPTIONAL MATCH ()-[item:LINK]->() RETURN item",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Semantic(_))),
            "incompatible rebinding should be a semantic error: {query}"
        );
    }
}

#[test]
fn unsupported_optional_pattern_shapes_fail_during_planning() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "OPTIONAL MATCH path = (left)-[:LINK]->(right) RETURN right",
        "OPTIONAL MATCH (left)-[:LINK*1..2]->(right) RETURN right",
    ] {
        assert!(
            matches!(database.execute(query), Err(Error::Unsupported(_))),
            "unsupported optional shape should be rejected explicitly: {query}"
        );
    }
}

#[test]
fn writes_after_optional_match_run_once_per_row_and_skip_null_targets() {
    let mut database = Database::in_memory().unwrap();

    // An OPTIONAL MATCH with no match still yields one row, so the CREATE
    // runs once.
    let created = database
        .execute(
            "OPTIONAL MATCH (missing:OptionalWriteInput) \
             CREATE (:OptionalWriteSideEffect)",
        )
        .unwrap();
    assert_eq!(created.summary.nodes_created, 1);

    // Updating clauses ignore rows whose target is null.
    let updated = database
        .execute(
            "OPTIONAL MATCH (missing:OptionalWriteInput) \
             SET missing.touched = true \
             REMOVE missing.touched \
             DELETE missing \
             RETURN missing",
        )
        .unwrap();
    assert_eq!(updated.rows, vec![vec![CypherValue::Null]]);
    assert_eq!(updated.summary.properties_set, 0);
    assert_eq!(updated.summary.nodes_deleted, 0);
}
