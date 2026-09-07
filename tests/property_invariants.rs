use std::{collections::BTreeMap, fmt::Write};

use crabnet::{CypherValue, Database, Error, Parameters, Value, explain_physical};
use serde_json::{Map, json};

fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect::<BTreeMap<_, _>>()
}

fn escaped_identifier(value: &str) -> String {
    let mut escaped = String::from('`');
    for character in value.chars() {
        match character {
            '`' => escaped.push_str("``"),
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\u{0008}' => escaped.push_str("\\b"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\u{000c}' => escaped.push_str("\\f"),
            character if character.is_control() => {
                write!(escaped, "\\U{:06X}", character as u32).unwrap();
            }
            character => escaped.push(character),
        }
    }
    escaped.push('`');
    escaped
}

fn cypher_map(value: &Value) -> BTreeMap<String, CypherValue> {
    let CypherValue::Map(value) = CypherValue::try_from(value).unwrap() else {
        panic!("expected a JSON object")
    };
    value
}

fn recursive_payload(marker: &str) -> Value {
    json!([
        {
            "marker": marker,
            "values": [
                null,
                false,
                true,
                i64::MIN,
                i64::MAX,
                1,
                1.0,
                "",
                "雪☃",
                {
                    "empty list": [],
                    "empty map": {},
                    "path.key": [
                        {"tick`key": ["leaf", {"explicit": null}]}
                    ]
                }
            ]
        },
        [[], [{}]]
    ])
}

fn assert_canonical_zero(value: &CypherValue) {
    let CypherValue::Float(value) = value else {
        panic!("expected a float, got {value:?}")
    };
    assert_eq!(value.to_bits(), 0.0_f64.to_bits());
}

fn nested_array(depth: usize) -> Value {
    nested_array_with_leaf(depth, json!("leaf"))
}

fn nested_array_with_leaf(depth: usize, leaf: Value) -> Value {
    (0..depth).fold(leaf, |value, _| Value::Array(vec![value]))
}

fn nested_list_literal(depth: usize) -> String {
    nested_list_literal_with_leaf(depth, "'leaf'")
}

fn nested_list_literal_with_leaf(depth: usize, leaf: &str) -> String {
    format!("{}{leaf}{}", "[".repeat(depth), "]".repeat(depth))
}

fn shallow_merge(base: &Value, patch: &Value) -> Value {
    let mut merged = base.as_object().unwrap().clone();
    for (key, value) in patch.as_object().unwrap() {
        if value.is_null() {
            merged.remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

#[test]
fn escaped_graph_identifiers_are_bound_across_fixed_and_traversal_reads() {
    const SOURCE_LABEL: &str = "Label'); DROP TABLE nodes; --";
    const TARGET_LABEL: &str = "終点 label";
    const EDGE_TYPE: &str = "TYPE\"); DROP TABLE edges; --";
    const SOURCE_VARIABLE: &str = "node` variable";
    const EDGE_VARIABLE: &str = "edge` variable";
    const TARGET_VARIABLE: &str = "target variable";
    const PAYLOAD_KEY: &str = "dot.key";
    const EDGE_KEY: &str = "edge space";
    const PARAMETER: &str = "payload` name";
    const COLUMN: &str = "column` name";

    let source_label = escaped_identifier(SOURCE_LABEL);
    let target_label = escaped_identifier(TARGET_LABEL);
    let edge_type = escaped_identifier(EDGE_TYPE);
    let source = escaped_identifier(SOURCE_VARIABLE);
    let edge = escaped_identifier(EDGE_VARIABLE);
    let target = escaped_identifier(TARGET_VARIABLE);
    let payload_key = escaped_identifier(PAYLOAD_KEY);
    let edge_key = escaped_identifier(EDGE_KEY);
    let parameter = escaped_identifier(PARAMETER);
    let column = escaped_identifier(COLUMN);
    let payload = recursive_payload("escaped identifier payload");

    let create = format!(
        "CREATE ({source}:{source_label} {{{payload_key}: ${parameter}}})-\
                [{edge}:{edge_type} {{{edge_key}: 7}}]->\
                ({target}:{target_label} {{name: 'target'}}) \
         RETURN {source}.{payload_key} AS {column}, \
                labels({source}) AS `label column`, \
                type({edge}) AS `type column`, \
                {edge}.{edge_key} AS `edge column`, \
                {source} AS `source entity`, {edge} AS `edge entity`"
    );
    let mut database = Database::in_memory().unwrap();
    let created = database
        .execute_with(&create, &parameters([(PARAMETER, payload.clone())]))
        .unwrap();

    assert_eq!(
        created.columns,
        [
            COLUMN,
            "label column",
            "type column",
            "edge column",
            "source entity",
            "edge entity",
        ]
    );
    assert_eq!(created.rows[0][0], payload);
    assert_eq!(created.rows[0][1], json!([SOURCE_LABEL]));
    assert_eq!(created.rows[0][2], json!(EDGE_TYPE));
    assert_eq!(created.rows[0][3], json!(7));
    let source_entity = created.rows[0][4].as_node().unwrap();
    let edge_entity = created.rows[0][5].as_relationship().unwrap();
    assert_eq!(source_entity.labels, [SOURCE_LABEL]);
    assert_eq!(source_entity.properties[PAYLOAD_KEY], payload);
    assert_eq!(edge_entity.relationship_type, EDGE_TYPE);
    assert_eq!(edge_entity.properties[EDGE_KEY], json!(7));

    let wrong_source_label = escaped_identifier(&format!("{SOURCE_LABEL} wrong"));
    let wrong_target_label = escaped_identifier(&format!("{TARGET_LABEL} wrong"));
    let wrong_edge_type = escaped_identifier(&format!("{EDGE_TYPE} wrong"));
    for quantifier in ["", "*1..1", "*"] {
        let relationship = format!("-[:{edge_type}{quantifier}]->");
        let query = format!(
            "MATCH (source:{source_label}){relationship}(target:{target_label}) \
             RETURN target.name AS name"
        );
        let result = database
            .execute(&query)
            .unwrap_or_else(|error| panic!("identifier traversal failed for {query}: {error}"));
        assert_eq!(result.rows, [vec![json!("target")]], "query: {query}");

        let wrong_source = format!(
            "MATCH (source:{wrong_source_label}){relationship}(target:{target_label}) \
             RETURN target.name AS name"
        );
        assert!(
            database.execute(&wrong_source).unwrap().rows.is_empty(),
            "source-label filter was not applied: {wrong_source}"
        );

        let wrong_target = format!(
            "MATCH (source:{source_label}){relationship}(target:{wrong_target_label}) \
             RETURN target.name AS name"
        );
        assert!(
            database.execute(&wrong_target).unwrap().rows.is_empty(),
            "target-label filter was not applied: {wrong_target}"
        );

        let wrong_relationship = format!("-[:{wrong_edge_type}{quantifier}]->");
        let wrong_type = format!(
            "MATCH (source:{source_label}){wrong_relationship}(target:{target_label}) \
             RETURN target.name AS name"
        );
        assert!(
            database.execute(&wrong_type).unwrap().rows.is_empty(),
            "relationship-type filter was not applied: {wrong_type}"
        );
    }

    let physical_query = format!(
        "MATCH ({source}:{source_label} {{{payload_key}: ${parameter}}})-\
               [:{edge_type}* {{{edge_key}: 7}}]->\
               ({target}:{target_label}) \
         RETURN {target}"
    );
    let physical = explain_physical(&physical_query).unwrap();
    for identifier in [
        SOURCE_LABEL,
        TARGET_LABEL,
        EDGE_TYPE,
        SOURCE_VARIABLE,
        TARGET_VARIABLE,
        PAYLOAD_KEY,
        EDGE_KEY,
        PARAMETER,
    ] {
        assert!(
            !physical.contains(identifier),
            "data-bearing identifier leaked into generated SQL: {identifier:?}\n{physical}"
        );
    }
    assert!(physical.contains("trail-expand"));
    assert!(physical.contains(" text"));
    assert!(physical.contains(" json"));
}

#[test]
fn contextual_names_and_unusual_named_parameters_remain_exact_and_case_sensitive() {
    let mut database = Database::in_memory().unwrap();
    let contextual = database
        .execute(
            "CREATE (match:Order {return: 1})-\
                    [where:contains {order: 2}]->(create:Thing) \
             RETURN match.return AS limit, where.order AS skip, \
                    type(where) AS union",
        )
        .unwrap();
    assert_eq!(contextual.columns, ["limit", "skip", "union"]);
    assert_eq!(
        contextual.rows,
        [vec![json!(1), json!(2), json!("contains")]]
    );

    database
        .execute("CREATE (:MiXeD {name: 'upper'}), (:mixed {name: 'lower'})")
        .unwrap();
    assert_eq!(
        database
            .execute("MATCH (value:MiXeD) RETURN value.name AS name")
            .unwrap()
            .rows,
        [vec![json!("upper")]]
    );
    assert_eq!(
        database
            .execute("MATCH (value:mixed) RETURN value.name AS name")
            .unwrap()
            .rows,
        [vec![json!("lower")]]
    );

    let values = parameters([
        ("odd.name", json!("dot")),
        ("1abc", json!("leading")),
        ("123", json!("escaped digits")),
        ("tick`name", json!("tick")),
        ("café", json!("unicode")),
        ("Name", json!("upper")),
        ("name", json!("lower")),
    ]);
    let parameters = database
        .execute_with(
            "RETURN $`odd.name` AS `odd column`, $1abc AS digit_leading, \
                    $`123` AS escaped_digits, $`tick``name` AS ticked, \
                    $café AS unicode, $Name AS upper, $name AS lower",
            &values,
        )
        .unwrap();
    assert_eq!(
        parameters.columns,
        [
            "odd column",
            "digit_leading",
            "escaped_digits",
            "ticked",
            "unicode",
            "upper",
            "lower",
        ]
    );
    assert_eq!(
        parameters.rows,
        [vec![
            json!("dot"),
            json!("leading"),
            json!("escaped digits"),
            json!("tick"),
            json!("unicode"),
            json!("upper"),
            json!("lower"),
        ]]
    );

    assert!(matches!(
        database.execute("RETURN $`missing`` name`"),
        Err(Error::MissingParameter(name)) if name == "missing` name"
    ));
    assert!(matches!(
        database.execute("RETURN $123"),
        Err(Error::Unsupported(message)) if message.contains("positional parameter")
    ));
}

#[test]
fn decoded_name_collisions_and_empty_relationship_types_fail_during_planning() {
    let mut database = Database::in_memory().unwrap();

    for query in [
        "RETURN 1 AS value, 2 AS `value` ORDER BY value",
        "RETURN 1, 1",
    ] {
        let error = database.execute(query).unwrap_err();
        assert!(
            matches!(error, Error::Semantic(ref message) if message.contains("duplicate")),
            "unexpected duplicate-column error for {query}: {error}"
        );
    }

    let duplicate_property = database
        .execute("CREATE (:Collision {foo: 1, `foo`: 2})")
        .unwrap_err();
    assert!(matches!(duplicate_property, Error::Semantic(_)));

    let duplicate_after_create = database
        .execute("CREATE (:Collision) RETURN 1 AS value, 2 AS `value`")
        .unwrap_err();
    assert!(
        matches!(duplicate_after_create, Error::Semantic(ref message) if message.contains("duplicate")),
        "unexpected post-CREATE duplicate-column error: {duplicate_after_create}"
    );

    let empty_type = database
        .execute("CREATE (:Collision), ()-[:``]->()")
        .unwrap_err();
    assert!(
        matches!(empty_type, Error::Semantic(ref message) if message.contains("relationship type") && message.contains("empty")),
        "unexpected empty-type error: {empty_type}"
    );

    assert!(
        database
            .execute("MATCH (value:Collision) RETURN value")
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn unusual_property_keys_survive_match_set_read_and_remove() {
    let keys = [
        "",
        "MATCH",
        "space key",
        "$.[0]",
        "double\"single'quote",
        "tick`slash\\",
        "café☃",
        "line\nkey",
        "sql'); DROP TABLE nodes; --",
        "case",
        "Case",
    ];
    let mut stored = Map::new();
    for (index, key) in keys.iter().enumerate() {
        stored.insert((*key).to_owned(), json!(format!("initial-{index}")));
    }
    let stored = Value::Object(stored);
    let mut database = Database::in_memory().unwrap();
    let created = database
        .execute_with(
            "CREATE (node:PropertyKeyNode $properties)-\
                    [edge:PROPERTY_KEY_EDGE $properties]->(:PropertyKeyTarget) \
             RETURN node, edge",
            &parameters([("properties", stored.clone())]),
        )
        .unwrap();
    assert_eq!(
        created.rows[0][0].as_node().unwrap().properties,
        cypher_map(&stored)
    );
    assert_eq!(
        created.rows[0][1].as_relationship().unwrap().properties,
        cypher_map(&stored)
    );

    for (index, key) in keys.iter().enumerate() {
        let identifier = escaped_identifier(key);
        let initial = json!(format!("initial-{index}"));
        let matched = format!(
            "MATCH (node:PropertyKeyNode)-[edge:PROPERTY_KEY_EDGE]->(:PropertyKeyTarget) \
             WHERE node.{identifier} = $expected AND edge.{identifier} = $expected \
             RETURN node.{identifier} AS node_value, edge.{identifier} AS edge_value"
        );
        let result = database
            .execute_with(&matched, &parameters([("expected", initial.clone())]))
            .unwrap_or_else(|error| panic!("MATCH failed for key {key:?}: {error}\n{matched}"));
        assert_eq!(
            result.rows,
            [vec![initial.clone(), initial]],
            "MATCH key {key:?}"
        );

        assert!(
            database
                .execute_with(&matched, &parameters([("expected", json!("not stored"))]))
                .unwrap()
                .rows
                .is_empty(),
            "mismatching WHERE value was not rejected for key {key:?}"
        );

        let updated = json!({
            "key": key,
            "index": index,
            "values": [null, false, {"nested": []}]
        });
        let set = format!(
            "MATCH (node:PropertyKeyNode)-[edge:PROPERTY_KEY_EDGE]->(:PropertyKeyTarget) \
             SET node.{identifier} = $updated, edge.{identifier} = $updated \
             RETURN node.{identifier} AS node_value, edge.{identifier} AS edge_value"
        );
        let result = database
            .execute_with(&set, &parameters([("updated", updated.clone())]))
            .unwrap_or_else(|error| panic!("SET failed for key {key:?}: {error}\n{set}"));
        assert_eq!(result.rows, [vec![updated.clone(), updated.clone()]]);
        assert_eq!(result.summary.properties_set, 2, "SET key {key:?}");

        let reread = format!(
            "MATCH (node:PropertyKeyNode)-[edge:PROPERTY_KEY_EDGE]->(:PropertyKeyTarget) \
             RETURN node.{identifier} AS node_value, edge.{identifier} AS edge_value"
        );
        assert_eq!(
            database.execute(&reread).unwrap().rows,
            [vec![updated.clone(), updated.clone()]],
            "persisted key {key:?}"
        );

        if key.contains("DROP TABLE") {
            let hybrid = format!(
                "MATCH (:PropertyKeyNode)-\
                       [:PROPERTY_KEY_EDGE* {{{identifier}: $updated}}]->\
                       (:PropertyKeyTarget) \
                 RETURN count(*) AS matches"
            );
            assert_eq!(
                database
                    .execute_with(&hybrid, &parameters([("updated", updated.clone())]))
                    .unwrap()
                    .rows,
                [vec![json!(1)]]
            );
            assert_eq!(
                database
                    .execute_with(&hybrid, &parameters([("updated", json!({"wrong": true}))]))
                    .unwrap()
                    .rows,
                [vec![json!(0)]],
                "hybrid edge-property filter was not applied for key {key:?}"
            );
        }

        let remove = format!(
            "MATCH (node:PropertyKeyNode)-[edge:PROPERTY_KEY_EDGE]->(:PropertyKeyTarget) \
             REMOVE node.{identifier}, edge.{identifier} \
             RETURN node.{identifier} AS node_value, edge.{identifier} AS edge_value, node, edge"
        );
        let result = database
            .execute(&remove)
            .unwrap_or_else(|error| panic!("REMOVE failed for key {key:?}: {error}\n{remove}"));
        assert_eq!(result.rows[0][0], CypherValue::Null);
        assert_eq!(result.rows[0][1], CypherValue::Null);
        assert!(
            !result.rows[0][2]
                .as_node()
                .unwrap()
                .properties
                .contains_key(*key)
        );
        assert!(
            !result.rows[0][3]
                .as_relationship()
                .unwrap()
                .properties
                .contains_key(*key)
        );
        assert_eq!(result.summary.properties_set, 2, "REMOVE key {key:?}");
    }

    assert_eq!(
        database
            .execute(
                "MATCH (:PropertyKeyNode)-[:PROPERTY_KEY_EDGE]->(:PropertyKeyTarget) \
                 RETURN count(*) AS matches"
            )
            .unwrap()
            .rows,
        [vec![json!(1)]]
    );
}

#[test]
fn recursive_parameter_properties_round_trip_through_mutation_and_path_hydration() {
    let source_properties = json!({
        "role": "source",
        "payload": recursive_payload("source-created"),
        "explicit_null": null,
        "empty_list": [],
        "empty_map": {},
        "negative_zero": -0.0
    });
    let edge_properties = json!({
        "payload": recursive_payload("edge-created"),
        "explicit_null": null,
        "empty_list": [],
        "empty_map": {},
        "negative_zero": -0.0
    });
    let target_properties = json!({
        "role": "target",
        "payload": recursive_payload("target-created")
    });
    let mut database = Database::in_memory().unwrap();
    let created = database
        .execute_with(
            "CREATE (source:RecursiveNode $source)-\
                    [edge:RECURSIVE_EDGE $edge]->\
                    (target:RecursiveTarget $target) \
             RETURN source, edge, target",
            &parameters([
                ("source", source_properties.clone()),
                ("edge", edge_properties.clone()),
                ("target", target_properties.clone()),
            ]),
        )
        .unwrap();
    assert_eq!(
        created.rows[0][0].as_node().unwrap().properties,
        cypher_map(&source_properties)
    );
    assert_eq!(
        created.rows[0][1].as_relationship().unwrap().properties,
        cypher_map(&edge_properties)
    );
    assert_eq!(
        created.rows[0][2].as_node().unwrap().properties,
        cypher_map(&target_properties)
    );
    assert_canonical_zero(&created.rows[0][0].as_node().unwrap().properties["negative_zero"]);
    assert_canonical_zero(
        &created.rows[0][1].as_relationship().unwrap().properties["negative_zero"],
    );

    let direct = database
        .execute_with(
            "MATCH (source:RecursiveNode {payload: $source_payload})-\
                   [edge:RECURSIVE_EDGE {payload: $edge_payload}]->\
                   (target:RecursiveTarget {payload: $target_payload}) \
             RETURN source, edge, target, source.payload, edge.payload, target.payload, \
                    source.negative_zero, edge.negative_zero",
            &parameters([
                ("source_payload", source_properties["payload"].clone()),
                ("edge_payload", edge_properties["payload"].clone()),
                ("target_payload", target_properties["payload"].clone()),
            ]),
        )
        .unwrap();
    assert_eq!(direct.rows[0][..3], created.rows[0][..3]);
    assert_eq!(direct.rows[0][3], source_properties["payload"]);
    assert_eq!(direct.rows[0][4], edge_properties["payload"]);
    assert_eq!(direct.rows[0][5], target_properties["payload"]);

    let direct_source = recursive_payload("source-direct-set");
    let direct_edge = recursive_payload("edge-direct-set");
    let assigned = database
        .execute_with(
            "MATCH (source:RecursiveNode)-[edge:RECURSIVE_EDGE]->(:RecursiveTarget) \
             SET source.`direct payload` = $source, edge.`direct payload` = $edge \
             RETURN source.`direct payload`, edge.`direct payload`",
            &parameters([
                ("source", direct_source.clone()),
                ("edge", direct_edge.clone()),
            ]),
        )
        .unwrap();
    assert_canonical_zero(&direct.rows[0][6]);
    assert_canonical_zero(&direct.rows[0][7]);
    assert_eq!(
        assigned.rows,
        [vec![direct_source.clone(), direct_edge.clone()]]
    );
    assert_eq!(assigned.summary.properties_set, 2);

    assert_eq!(
        database
            .execute(
                "MATCH (source:RecursiveNode)-[edge:RECURSIVE_EDGE]->(:RecursiveTarget) \
                 RETURN source.`direct payload`, edge.`direct payload`",
            )
            .unwrap()
            .rows,
        [vec![direct_source, direct_edge]]
    );

    let source_replacement = json!({
        "role": "source-replaced",
        "payload": recursive_payload("source-replaced"),
        "explicit_null": null,
        "negative_zero": -0.0
    });
    let edge_replacement = json!({
        "payload": recursive_payload("edge-replaced"),
        "explicit_null": null,
        "negative_zero": -0.0
    });
    database
        .execute_with(
            "MATCH (source:RecursiveNode)-[edge:RECURSIVE_EDGE]->(:RecursiveTarget) \
             SET source = $source, edge = $edge",
            &parameters([
                ("source", source_replacement.clone()),
                ("edge", edge_replacement.clone()),
            ]),
        )
        .unwrap();

    let source_patch = json!({
        "payload": recursive_payload("source-merged"),
        "explicit_null": null,
        "added": recursive_payload("source-added")
    });
    let edge_patch = json!({
        "payload": recursive_payload("edge-merged"),
        "explicit_null": null,
        "added": recursive_payload("edge-added")
    });
    let expected_source = shallow_merge(&source_replacement, &source_patch);
    let expected_edge = shallow_merge(&edge_replacement, &edge_patch);
    let merged = database
        .execute_with(
            "MATCH (source:RecursiveNode)-[edge:RECURSIVE_EDGE]->(target:RecursiveTarget) \
             SET source += $source, edge += $edge \
             RETURN source, edge, target",
            &parameters([("source", source_patch), ("edge", edge_patch)]),
        )
        .unwrap();
    assert_eq!(
        merged.rows[0][0].as_node().unwrap().properties,
        cypher_map(&expected_source)
    );
    assert_eq!(
        merged.rows[0][1].as_relationship().unwrap().properties,
        cypher_map(&expected_edge)
    );
    assert_eq!(merged.summary.properties_set, 6);

    let path_result = database
        .execute(
            "MATCH path = (source:RecursiveNode)-[:RECURSIVE_EDGE*]->\
                         (target:RecursiveTarget) \
             RETURN path",
        )
        .unwrap();
    assert_eq!(path_result.rows.len(), 1);
    let path = path_result.rows[0][0].as_path().unwrap();
    assert_eq!(path.nodes.len(), 2);
    assert_eq!(path.relationships.len(), 1);
    assert_eq!(path.nodes[0], *merged.rows[0][0].as_node().unwrap());
    assert_eq!(
        path.relationships[0],
        *merged.rows[0][1].as_relationship().unwrap()
    );
    assert_eq!(path.nodes[1], *merged.rows[0][2].as_node().unwrap());
    assert_canonical_zero(&path.nodes[0].properties["negative_zero"]);
    assert_canonical_zero(&path.relationships[0].properties["negative_zero"]);
}

#[test]
fn recursive_property_depth_is_preflighted_before_any_write() {
    const MAXIMUM_ACCEPTED_DEPTH: usize = 100;

    let accepted = nested_array(MAXIMUM_ACCEPTED_DEPTH);
    let rejected = nested_array(MAXIMUM_ACCEPTED_DEPTH + 1);
    let mut database = Database::in_memory().unwrap();
    database
        .execute_with(
            "CREATE (:DepthBoundary {name: 'accepted', value: $value})",
            &parameters([("value", accepted.clone())]),
        )
        .unwrap();

    let stored = database
        .execute(
            "MATCH (node:DepthBoundary {name: 'accepted'}) \
             RETURN node, node.value AS value",
        )
        .unwrap();
    let accepted_value = CypherValue::try_from(&accepted).unwrap();
    assert_eq!(stored.rows[0][1], accepted_value);
    assert_eq!(
        stored.rows[0][0].as_node().unwrap().properties["value"],
        accepted_value
    );

    assert_eq!(
        database
            .execute_with(
                "RETURN $value AS value",
                &parameters([("value", accepted.clone())]),
            )
            .unwrap()
            .rows,
        [vec![accepted_value.clone()]]
    );

    let direct_set = database
        .execute_with(
            "MATCH (node:DepthBoundary {name: 'accepted'}) \
             SET node.direct = $value RETURN node.direct AS value",
            &parameters([("value", accepted.clone())]),
        )
        .unwrap();
    assert_eq!(direct_set.rows, [vec![accepted_value.clone()]]);

    let accepted_literal = format!(
        "RETURN {} AS value",
        nested_list_literal(MAXIMUM_ACCEPTED_DEPTH)
    );
    assert_eq!(
        database.execute(&accepted_literal).unwrap().rows,
        [vec![accepted_value.clone()]]
    );

    let parenthesized_literal = format!(
        "RETURN ({}) AS value",
        nested_list_literal(MAXIMUM_ACCEPTED_DEPTH)
    );
    assert_eq!(
        database.execute(&parenthesized_literal).unwrap().rows,
        [vec![accepted_value.clone()]],
        "parentheses must not consume the collection-depth allowance"
    );

    let negative = nested_array_with_leaf(MAXIMUM_ACCEPTED_DEPTH, json!(-1.5));
    let negative_literal = format!(
        "RETURN {} AS value",
        nested_list_literal_with_leaf(MAXIMUM_ACCEPTED_DEPTH, "-1.5")
    );
    assert_eq!(
        database.execute(&negative_literal).unwrap().rows,
        [vec![CypherValue::try_from(negative).unwrap()]],
        "unary signs must not consume the collection-depth allowance"
    );
    let excessive_syntax_leaf = format!("{}1", "+".repeat(29));
    let excessive_syntax = format!(
        "RETURN {} AS value",
        nested_list_literal_with_leaf(MAXIMUM_ACCEPTED_DEPTH, &excessive_syntax_leaf)
    );
    let expression_error = database.execute(&excessive_syntax).unwrap_err();
    assert!(
        matches!(expression_error, Error::Semantic(ref message) if message.contains("expression nesting") && message.contains("128")),
        "unexpected expression-depth error: {expression_error}"
    );

    let excessive_scalar = format!("RETURN {}1 AS value", "+".repeat(257));
    let scalar_error = database.execute(&excessive_scalar).unwrap_err();
    assert!(
        matches!(scalar_error, Error::Semantic(ref message) if message.contains("scalar expression nesting") && message.contains("128")),
        "unexpected scalar-depth error: {scalar_error}"
    );

    let excessive_output = format!("RETURN {}1{} AS value", "(".repeat(257), ")".repeat(257));
    let output_error = database.execute(&excessive_output).unwrap_err();
    assert!(
        matches!(output_error, Error::Semantic(ref message) if message.contains("output expression nesting") && message.contains("128")),
        "unexpected output-depth error: {output_error}"
    );

    let rejected_literal = format!(
        "RETURN {} AS value",
        nested_list_literal(MAXIMUM_ACCEPTED_DEPTH + 1)
    );
    let literal_error = database.execute(&rejected_literal).unwrap_err();
    assert!(
        matches!(literal_error, Error::Semantic(ref message) if message.contains("nesting") && message.contains("100")),
        "unexpected literal depth error: {literal_error}"
    );
    for query in [
        "CREATE (:DepthBoundary {name: 'rejected', value: $value})",
        "MATCH (node:DepthBoundary {name: 'accepted'}) \
         SET node.changed = true, node.value = $value",
        "RETURN $value AS value",
    ] {
        let error = database
            .execute_with(query, &parameters([("value", rejected.clone())]))
            .unwrap_err();
        assert!(
            matches!(error, Error::Semantic(ref message) if message.contains("nesting") && message.contains("100")),
            "unexpected depth error for {query}: {error}"
        );
    }

    let unchanged = database
        .execute(
            "MATCH (node:DepthBoundary) \
             RETURN node.name AS name, node.changed AS changed, node.value AS value",
        )
        .unwrap();

    assert_eq!(unchanged.rows.len(), 1);
    assert_eq!(unchanged.rows[0][0], json!("accepted"));
    assert_eq!(unchanged.rows[0][1], CypherValue::Null);
    assert_eq!(unchanged.rows[0][2], accepted_value);
}

#[test]
fn property_map_depth_boundary_is_consistent_across_write_forms() {
    const MAXIMUM_ACCEPTED_DEPTH: usize = 100;

    let accepted = nested_array(MAXIMUM_ACCEPTED_DEPTH);
    let accepted_value = CypherValue::try_from(&accepted).unwrap();
    let whole_properties = json!({
        "route": "whole parameter",
        "value": accepted.clone()
    });
    let whole_patch = json!({"whole patch": accepted.clone()});
    let mut database = Database::in_memory().unwrap();

    let created = database
        .execute_with(
            "CREATE (node:DepthMap $properties)-\
                    [edge:DEPTH_MAP $properties]->(:DepthTarget) \
             RETURN node.value AS node_value, edge.value AS edge_value",
            &parameters([("properties", whole_properties.clone())]),
        )
        .unwrap();
    assert_eq!(
        created.rows,
        [vec![accepted_value.clone(), accepted_value.clone()]]
    );

    let accepted_literal = nested_list_literal(MAXIMUM_ACCEPTED_DEPTH);
    database
        .execute(&format!(
            "CREATE (:DepthLiteral {{value: {accepted_literal}}})"
        ))
        .unwrap();
    assert_eq!(
        database
            .execute("MATCH (node:DepthLiteral) RETURN node.value AS value")
            .unwrap()
            .rows,
        [vec![accepted_value.clone()]]
    );

    let whole_updates = database
        .execute_with(
            "MATCH (node:DepthMap)-[edge:DEPTH_MAP]->(:DepthTarget) \
             SET node = $properties, edge = $properties, \
                 node += $patch, edge += $patch \
             RETURN node.value, edge.value, node.`whole patch`, edge.`whole patch`",
            &parameters([
                ("properties", whole_properties.clone()),
                ("patch", whole_patch),
            ]),
        )
        .unwrap();
    assert_eq!(
        whole_updates.rows,
        [vec![
            accepted_value.clone(),
            accepted_value.clone(),
            accepted_value.clone(),
            accepted_value.clone(),
        ]]
    );

    let embedded_updates = database
        .execute_with(
            "MATCH (node:DepthMap)-[edge:DEPTH_MAP]->(:DepthTarget) \
             SET node = {value: $value, route: 'embedded parameter'}, \
                 edge += {embedded: $value} \
             RETURN node.value, edge.embedded",
            &parameters([("value", accepted.clone())]),
        )
        .unwrap();
    assert_eq!(
        embedded_updates.rows,
        [vec![accepted_value.clone(), accepted_value.clone()]]
    );

    let literal_updates = database
        .execute(&format!(
            "MATCH (node:DepthMap)-[edge:DEPTH_MAP]->(:DepthTarget) \
             SET node = {{value: {accepted_literal}, route: 'literal'}}, \
                 edge += {{literal: {accepted_literal}}} \
             RETURN node.value, edge.literal"
        ))
        .unwrap();
    assert_eq!(
        literal_updates.rows,
        [vec![accepted_value.clone(), accepted_value]]
    );

    let state_query = "MATCH (node:DepthMap)-[edge:DEPTH_MAP]->(:DepthTarget) RETURN node, edge";
    let baseline = database.execute(state_query).unwrap().rows;
    let rejected = nested_array(MAXIMUM_ACCEPTED_DEPTH + 1);
    let rejected_literal = nested_list_literal(MAXIMUM_ACCEPTED_DEPTH + 1);
    let rejected_parameters = parameters([
        ("value", rejected.clone()),
        ("properties", json!({"value": rejected.clone()})),
    ]);
    let rejected_queries = [
        "CREATE (:RejectedDepth $properties)-\
                [:REJECTED_DEPTH $properties]->()"
            .to_owned(),
        format!("CREATE (:RejectedDepth {{value: {rejected_literal}}})"),
        "MATCH (node:DepthMap)-[:DEPTH_MAP]->(:DepthTarget) \
         SET node = $properties"
            .to_owned(),
        "MATCH (:DepthMap)-[edge:DEPTH_MAP]->(:DepthTarget) \
         SET edge += $properties"
            .to_owned(),
        "MATCH (node:DepthMap)-[:DEPTH_MAP]->(:DepthTarget) \
         SET node = {value: $value}"
            .to_owned(),
        "MATCH (:DepthMap)-[edge:DEPTH_MAP]->(:DepthTarget) \
         SET edge += {value: $value}"
            .to_owned(),
        format!(
            "MATCH (node:DepthMap)-[:DEPTH_MAP]->(:DepthTarget) \
             SET node = {{value: {rejected_literal}}}"
        ),
        format!(
            "MATCH (:DepthMap)-[edge:DEPTH_MAP]->(:DepthTarget) \
             SET edge += {{value: {rejected_literal}}}"
        ),
    ];

    for query in rejected_queries {
        let error = database
            .execute_with(&query, &rejected_parameters)
            .unwrap_err();
        assert!(
            matches!(error, Error::Semantic(ref message) if message.contains("nesting") && message.contains("100")),
            "unexpected depth error for {query}: {error}"
        );
        assert_eq!(
            database.execute(state_query).unwrap().rows,
            baseline,
            "rejected write changed state: {query}"
        );
    }

    assert!(
        database
            .execute("MATCH (node:RejectedDepth) RETURN node")
            .unwrap()
            .rows
            .is_empty()
    );
}
