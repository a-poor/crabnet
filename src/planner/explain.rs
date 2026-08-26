use serde_json::Value;

use super::logical::{
    AggregateKind, BinaryOp, Direction, EntityKind, Expr, Input, MatchOperator, Mutation,
    Projection, Query, ReadStage, ScalarSlotId, SlotId, SortDirection, UnaryOp, ValueExpr,
    WithBinding, WithStage,
};

/// Render a logical plan using canonical slots and semantic operator names.
///
/// The format intentionally does not reuse derived `Debug` output: enum and
/// field refactors should not silently redefine the explanation contract. Map
/// keys are sorted because their source order has no semantic meaning.
pub(super) fn render(query: &Query) -> String {
    let mut renderer = Renderer::default();
    renderer.line(0, "logical-query");

    renderer.line(1, "slots:");
    if query.slots.is_empty() {
        renderer.line(2, "none");
    } else {
        for (index, kind) in query.slots.iter().enumerate() {
            renderer.line(2, format!("%{index} {}", entity_kind_name(*kind)));
        }
    }
    if query.scalar_slots > 0 {
        renderer.line(1, "scalar-slots:");
        for index in 0..query.scalar_slots {
            renderer.line(
                2,
                format!("{} scalar", scalar_slot_name(ScalarSlotId(index))),
            );
        }
    }

    renderer.line(1, "input:");
    match &query.input {
        Input::Unit => renderer.line(2, "unit"),
        Input::Match(plan) => {
            renderer.line(2, "match");
            if plan.operators.is_empty() {
                renderer.line(3, "none");
            } else {
                for operator in &plan.operators {
                    renderer.line(3, render_match_operator(operator));
                }
            }
        }
        Input::Pipeline(pipeline) => {
            renderer.line(2, "pipeline");
            for (index, stage) in pipeline.stages.iter().enumerate() {
                match stage {
                    ReadStage::Match(plan) => {
                        renderer.line(3, format!("stage {index}: match"));
                        if plan.operators.is_empty() {
                            renderer.line(4, "none");
                        } else {
                            for operator in &plan.operators {
                                renderer.line(4, render_match_operator(operator));
                            }
                        }
                    }
                    ReadStage::OptionalMatch(plan) => {
                        renderer.line(3, format!("stage {index}: optional-match"));
                        if plan.operators.is_empty() {
                            renderer.line(4, "none");
                        } else {
                            for operator in &plan.operators {
                                renderer.line(4, render_match_operator(operator));
                            }
                        }
                    }
                    ReadStage::With(stage) => {
                        renderer.line(3, format!("stage {index}: with"));
                        render_with_stage(&mut renderer, stage);
                    }
                    ReadStage::Unwind(stage) => renderer.line(
                        3,
                        format!(
                            "stage {index}: unwind {} = scalar({}) <- {}",
                            quote(&stage.name),
                            scalar_slot_name(stage.slot),
                            render_expr(&stage.expression)
                        ),
                    ),
                }
            }
        }
    }

    renderer.line(1, "mutations:");
    if query.mutations.is_empty() {
        renderer.line(2, "none");
    } else {
        for mutation in &query.mutations {
            renderer.line(2, render_mutation(mutation));
        }
    }

    renderer.line(1, "projection:");
    match &query.projection {
        None => renderer.line(2, "none"),
        Some(projection) => render_projection(&mut renderer, projection),
    }

    renderer.finish()
}

fn render_with_stage(renderer: &mut Renderer, stage: &WithStage) {
    renderer.line(4, "scope:");
    for binding in &stage.bindings {
        match binding {
            WithBinding::Entity {
                name, slot, kind, ..
            } => renderer.line(
                5,
                format!(
                    "{} = entity({}) kind={}",
                    quote(name),
                    slot_name(*slot),
                    entity_kind_name(*kind)
                ),
            ),
            WithBinding::Scalar {
                name,
                slot,
                expression,
            } => renderer.line(
                5,
                format!(
                    "{} = scalar({}) <- {}",
                    quote(name),
                    scalar_slot_name(*slot),
                    render_expr(expression)
                ),
            ),
        }
    }
    renderer.line(
        4,
        format!(
            "filter: {}",
            stage
                .filter
                .as_ref()
                .map_or_else(|| "none".to_owned(), render_expr)
        ),
    );
    renderer.line(4, "order-by:");
    if stage.order_by.is_empty() {
        renderer.line(5, "none");
    } else {
        for item in &stage.order_by {
            renderer.line(
                5,
                format!(
                    "{} {}",
                    render_expr(&item.expression),
                    match item.direction {
                        SortDirection::Ascending => "ascending",
                        SortDirection::Descending => "descending",
                    }
                ),
            );
        }
    }
    renderer.line(
        4,
        format!(
            "offset: {}",
            stage
                .offset
                .as_ref()
                .map_or_else(|| "none".to_owned(), render_value_expr)
        ),
    );
    renderer.line(
        4,
        format!(
            "limit: {}",
            stage
                .limit
                .as_ref()
                .map_or_else(|| "none".to_owned(), render_value_expr)
        ),
    );
}

#[derive(Default)]
struct Renderer {
    output: String,
}

impl Renderer {
    fn line(&mut self, indentation: usize, content: impl AsRef<str>) {
        self.output.push_str(&"  ".repeat(indentation));
        self.output.push_str(content.as_ref());
        self.output.push('\n');
    }

    fn finish(self) -> String {
        self.output
    }
}

fn render_match_operator(operator: &MatchOperator) -> String {
    match operator {
        MatchOperator::ScanNode { slot } => format!("scan-node {}", slot_name(*slot)),
        MatchOperator::ScanRelationship { slot } => {
            format!("scan-relationship {}", slot_name(*slot))
        }
        MatchOperator::HasLabel { slot, label } => {
            format!("has-label {} {}", slot_name(*slot), quote(label))
        }
        MatchOperator::HasRelationshipType {
            slot,
            relationship_type,
        } => format!(
            "has-relationship-type {} {}",
            slot_name(*slot),
            quote(relationship_type)
        ),
        MatchOperator::PropertyEquals { slot, key, value } => format!(
            "property-equals {} {} {}",
            slot_name(*slot),
            quote(key),
            render_value_expr(value)
        ),
        MatchOperator::ExpandRelationship {
            relationship,
            left,
            right,
            direction,
        } => format!(
            "expand relationship={} left={} right={} direction={}",
            slot_name(*relationship),
            slot_name(*left),
            slot_name(*right),
            direction_name(*direction)
        ),
        MatchOperator::DistinctRelationships(slots) => format!(
            "distinct-relationships [{}]",
            slots
                .iter()
                .map(|slot| slot_name(*slot))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchOperator::Filter(expression) => format!("filter {}", render_expr(expression)),
    }
}

fn render_mutation(mutation: &Mutation) -> String {
    match mutation {
        Mutation::CreateNode {
            slot,
            label,
            properties,
        } => format!(
            "create-node slot={} label={} properties={}",
            slot_name(*slot),
            label.as_deref().map_or_else(|| "none".to_owned(), quote),
            render_value_expr(properties)
        ),
        Mutation::CreateRelationship {
            slot,
            relationship_type,
            from,
            to,
            properties,
        } => format!(
            "create-relationship slot={} type={} from={} to={} properties={}",
            slot_name(*slot),
            quote(relationship_type),
            slot_name(*from),
            slot_name(*to),
            render_value_expr(properties)
        ),
        Mutation::SetProperty { slot, key, value } => format!(
            "set-property slot={} key={} value={}",
            slot_name(*slot),
            quote(key),
            render_value_expr(value)
        ),
        Mutation::ReplaceProperties { slot, properties } => format!(
            "replace-properties slot={} properties={}",
            slot_name(*slot),
            render_value_expr(properties)
        ),
        Mutation::MergeProperties { slot, properties } => format!(
            "merge-properties slot={} properties={}",
            slot_name(*slot),
            render_value_expr(properties)
        ),
        Mutation::RemoveProperty { slot, key } => format!(
            "remove-property slot={} key={}",
            slot_name(*slot),
            quote(key)
        ),
        Mutation::Delete { targets, detach } => format!(
            "{} targets=[{}]",
            if *detach { "detach-delete" } else { "delete" },
            targets
                .iter()
                .map(|(slot, kind)| format!("{}:{}", slot_name(*slot), entity_kind_name(*kind)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn render_projection(renderer: &mut Renderer, projection: &Projection) {
    renderer.line(2, format!("distinct: {}", projection.distinct));
    renderer.line(2, "columns:");
    for (column, expression) in projection.columns.iter().zip(&projection.expressions) {
        renderer.line(
            3,
            format!("{} = {}", quote(column), render_expr(expression)),
        );
    }

    renderer.line(2, "order-by:");
    if projection.order_by.is_empty() {
        renderer.line(3, "none");
    } else {
        for item in &projection.order_by {
            renderer.line(
                3,
                format!(
                    "{} {}",
                    render_expr(&item.expression),
                    match item.direction {
                        SortDirection::Ascending => "ascending",
                        SortDirection::Descending => "descending",
                    }
                ),
            );
        }
    }

    renderer.line(
        2,
        format!(
            "offset: {}",
            projection
                .offset
                .as_ref()
                .map_or_else(|| "none".to_owned(), render_value_expr)
        ),
    );
    renderer.line(
        2,
        format!(
            "limit: {}",
            projection
                .limit
                .as_ref()
                .map_or_else(|| "none".to_owned(), render_value_expr)
        ),
    );
}

fn render_expr(expression: &Expr) -> String {
    match expression {
        Expr::Value(value) => render_value_expr(value),
        Expr::Entity(slot) => format!("entity({})", slot_name(*slot)),
        Expr::Scalar(slot) => format!("scalar({})", scalar_slot_name(*slot)),
        Expr::Property(slot, key) => {
            format!("property({}, {})", slot_name(*slot), quote(key))
        }
        Expr::ElementId(slot) => format!("element-id({})", slot_name(*slot)),
        Expr::Labels(slot) => format!("labels({})", slot_name(*slot)),
        Expr::RelationshipType(slot) => format!("relationship-type({})", slot_name(*slot)),
        Expr::Binary {
            left,
            operator,
            right,
        } => format!(
            "{}({}, {})",
            binary_op_name(*operator),
            render_expr(left),
            render_expr(right)
        ),
        Expr::Unary { operator, operand } => {
            format!("{}({})", unary_op_name(*operator), render_expr(operand))
        }
        Expr::IsNull {
            expression,
            negated,
        } => format!(
            "{}({})",
            if *negated { "is-not-null" } else { "is-null" },
            render_expr(expression)
        ),
        Expr::Aggregate {
            kind,
            argument,
            distinct,
        } => format!(
            "{}{}({})",
            aggregate_kind_name(*kind),
            if *distinct { "-distinct" } else { "" },
            argument
                .as_deref()
                .map_or_else(|| "*".to_owned(), render_expr)
        ),
    }
}

fn render_value_expr(expression: &ValueExpr) -> String {
    match expression {
        ValueExpr::Literal(value) => format!("literal({})", render_json(value)),
        ValueExpr::Parameter(name) => format!("parameter({})", quote(name)),
        ValueExpr::List(items) => format!(
            "list[{}]",
            items
                .iter()
                .map(render_value_expr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ValueExpr::Map(entries) => {
            let mut entries = entries.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            format!(
                "map{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!("{}: {}", quote(key), render_value_expr(value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        ValueExpr::Negated(value) => format!("negate({})", render_value_expr(value)),
    }
}

fn render_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => quote(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(render_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut values = values.iter().collect::<Vec<_>>();
            values.sort_by(|left, right| left.0.cmp(right.0));
            format!(
                "{{{}}}",
                values
                    .into_iter()
                    .map(|(key, value)| format!("{}: {}", quote(key), render_json(value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn quote(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a Rust string as JSON cannot fail")
}

fn slot_name(slot: SlotId) -> String {
    format!("%{}", slot.0)
}

fn scalar_slot_name(slot: ScalarSlotId) -> String {
    format!("%s{}", slot.0)
}

fn entity_kind_name(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Node => "node",
        EntityKind::Relationship => "relationship",
    }
}

fn aggregate_kind_name(kind: AggregateKind) -> &'static str {
    match kind {
        AggregateKind::Count => "count",
        AggregateKind::Sum => "sum",
        AggregateKind::Average => "avg",
        AggregateKind::Minimum => "min",
        AggregateKind::Maximum => "max",
        AggregateKind::Collect => "collect",
    }
}

fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Right => "right",
        Direction::Left => "left",
        Direction::Undirected => "undirected",
    }
}

fn binary_op_name(operator: BinaryOp) -> &'static str {
    match operator {
        BinaryOp::Or => "or",
        BinaryOp::And => "and",
        BinaryOp::Equal => "equal",
        BinaryOp::NotEqual => "not-equal",
        BinaryOp::Less => "less-than",
        BinaryOp::LessEqual => "less-than-or-equal",
        BinaryOp::Greater => "greater-than",
        BinaryOp::GreaterEqual => "greater-than-or-equal",
        BinaryOp::In => "in",
        BinaryOp::StartsWith => "starts-with",
        BinaryOp::EndsWith => "ends-with",
        BinaryOp::Contains => "contains",
        BinaryOp::Add => "add",
        BinaryOp::Subtract => "subtract",
        BinaryOp::Multiply => "multiply",
        BinaryOp::Divide => "divide",
        BinaryOp::Remainder => "remainder",
    }
}

fn unary_op_name(operator: UnaryOp) -> &'static str {
    match operator {
        UnaryOp::Plus => "positive",
        UnaryOp::Minus => "negative",
        UnaryOp::Not => "not",
    }
}

#[cfg(test)]
mod tests {
    use super::super::{explain_logical, explain_physical};

    #[test]
    fn explains_a_representative_read_as_a_normalized_logical_pipeline() {
        let explanation = explain_logical(
            "MATCH (a:Person {name: $name})-[r:KNOWS]->(b) \
             WHERE b.age >= $minimum AND r.active = true \
             RETURN b.name AS friend, type(r) AS kind \
             ORDER BY friend DESC SKIP $skip LIMIT 5",
        )
        .unwrap();

        assert_eq!(
            explanation,
            r#"logical-query
  slots:
    %0 node
    %1 relationship
    %2 node
  input:
    match
      scan-node %0
      has-label %0 "Person"
      property-equals %0 "name" parameter("name")
      scan-relationship %1
      has-relationship-type %1 "KNOWS"
      scan-node %2
      expand relationship=%1 left=%0 right=%2 direction=right
      filter and(greater-than-or-equal(property(%2, "age"), parameter("minimum")), equal(property(%1, "active"), literal(true)))
  mutations:
    none
  projection:
    distinct: false
    columns:
      "friend" = property(%2, "name")
      "kind" = relationship-type(%1)
    order-by:
      property(%2, "name") descending
    offset: parameter("skip")
    limit: literal(5)
"#
        );
    }

    #[test]
    fn explains_matched_create_and_normalizes_property_map_order() {
        let explanation = explain_logical(
            "MATCH (a:Person {id: $person_id}) \
             CREATE (a)-[r:KNOWS {since: 2024, metadata: {source: 'seed'}}]->\
                    (b:Person {name: $friend}) \
             RETURN elementId(r) AS relationship_id, b.name AS friend",
        )
        .unwrap();

        assert_eq!(
            explanation,
            r#"logical-query
  slots:
    %0 node
    %1 node
    %2 relationship
  input:
    match
      scan-node %0
      has-label %0 "Person"
      property-equals %0 "id" parameter("person_id")
  mutations:
    create-node slot=%1 label="Person" properties=map{"name": parameter("friend")}
    create-relationship slot=%2 type="KNOWS" from=%0 to=%1 properties=map{"metadata": map{"source": literal("seed")}, "since": literal(2024)}
  projection:
    distinct: false
    columns:
      "relationship_id" = element-id(%2)
      "friend" = property(%1, "name")
    order-by:
      none
    offset: none
    limit: none
"#
        );
    }

    #[test]
    fn explains_matched_property_and_delete_mutations() {
        let query = "MATCH (item:Item)-[edge:LINK]->() \
                     SET item.status = $status \
                     REMOVE edge.old \
                     DETACH DELETE item, edge \
                     RETURN item.status";
        let explanation = explain_logical(query).unwrap();

        assert!(
            explanation.contains("set-property slot=%0 key=\"status\" value=parameter(\"status\")")
        );
        assert!(explanation.contains("remove-property slot=%1 key=\"old\""));
        assert!(explanation.contains("detach-delete targets=[%0:node, %1:relationship]"));
        assert!(matches!(
            explain_physical(query),
            Err(crate::Error::Unsupported(message))
                if message.contains("physical explanations for mutation pipelines")
        ));
    }

    #[test]
    fn explains_whole_entity_projections() {
        let explanation = explain_logical("MATCH (node) RETURN node AS result").unwrap();

        assert!(explanation.contains("\"result\" = entity(%0)"));
    }

    #[test]
    fn explains_distinct_and_computed_projections() {
        let explanation = explain_logical(
            "MATCH (node) RETURN DISTINCT node.score + 1 AS adjusted ORDER BY adjusted",
        )
        .unwrap();

        assert!(explanation.contains("distinct: true"));
        assert!(explanation.contains("\"adjusted\" = add(property(%0, \"score\"), literal(1))"));
    }

    #[test]
    fn explains_with_boundaries_and_their_outgoing_scopes() {
        let explanation = explain_logical(
            "MATCH (person:Person) \
             WITH person.name AS name, person AS candidate \
             WHERE name STARTS WITH $prefix \
             WITH name AS result \
             RETURN result",
        )
        .unwrap();

        assert!(explanation.contains("stage 0: match"));
        assert!(explanation.contains("stage 1: with"));
        assert!(explanation.contains("\"name\" = scalar(%s0) <- property(%0, \"name\")"));
        assert!(explanation.contains("\"candidate\" = entity(%0) kind=node"));
        assert!(explanation.contains("stage 2: with"));
        assert!(explanation.contains("\"result\" = scalar(%s1) <- scalar(%s0)"));
    }

    #[test]
    fn explains_aggregate_grouping_across_a_with_boundary() {
        let query = "MATCH (member:Member) \
                     WITH member.team AS team, count(*) AS members \
                     WHERE members > 1 \
                     RETURN team, members";

        let logical = explain_logical(query).unwrap();
        assert!(logical.contains("\"team\" = scalar(%s0) <- property(%0, \"team\")"));
        assert!(logical.contains("\"members\" = scalar(%s1) <- count(*)"));
        assert!(logical.contains("filter: greater-than(count(*), literal(1))"));

        let physical = explain_physical(query).unwrap();
        assert!(physical.contains("COUNT(*)"));
        assert!(physical.contains(" GROUP BY "));
        assert!(physical.contains(" HAVING "));
    }

    #[test]
    fn explains_unwind_as_a_pipeline_row_expansion() {
        let query = "WITH [1, 2] AS values UNWIND values AS value RETURN value";

        let logical = explain_logical(query).unwrap();
        assert!(logical.contains("stage 0: with"));
        assert!(logical.contains("stage 1: unwind \"value\" = scalar(%s1) <- scalar(%s0)"));
        assert!(logical.contains("\"value\" = scalar(%s1)"));

        let physical = explain_physical(query).unwrap();
        assert!(physical.contains("_crabnet_unwind_1 AS MATERIALIZED"));
        assert!(physical.contains("json_each(CASE WHEN"));
        assert!(physical.contains("  rust-stage: none"));
    }

    #[test]
    fn explains_union_all_separately_from_semantic_union() {
        let query = "RETURN 1 AS value \
                     UNION ALL RETURN 1.0 AS value \
                     UNION RETURN 2 AS value";

        let logical = explain_logical(query).unwrap();
        assert!(logical.starts_with("logical-union\n  branch 0:\n"));
        assert_eq!(logical.matches("    logical-query\n").count(), 3);
        assert!(logical.contains("  union: all\n  branch 1:\n"));
        assert!(logical.contains("  union: semantic-distinct\n  branch 2:\n"));

        let physical = explain_physical(query).unwrap();
        assert!(physical.starts_with("physical-union\n  branch 0:\n"));
        assert_eq!(physical.matches("    physical-query\n").count(), 3);
        assert!(physical.contains("  union: all\n  branch 1:\n"));
        assert!(physical.contains("  union: semantic-distinct\n  branch 2:\n"));
    }

    #[test]
    fn explains_optional_predicates_inside_the_optional_stage() {
        let query = "MATCH (person:Person) \
                     OPTIONAL MATCH (person)-[:WORKS_ON]->(project:Project) \
                     WHERE project.active = true \
                     RETURN person.name AS person, project.name AS project";

        let logical = explain_logical(query).unwrap();
        let optional = logical
            .find("stage 1: optional-match")
            .expect("logical plan should contain an optional-match stage");
        let predicate = logical
            .find("filter equal(property(%2, \"active\"), literal(true))")
            .expect("optional predicate should remain inside the match plan");
        let projection = logical
            .find("  projection:")
            .expect("logical plan should contain a projection");
        assert!(optional < predicate && predicate < projection);

        let physical = explain_physical(query).unwrap();
        let optional_tail = physical
            .rsplit_once(" LEFT JOIN ")
            .map(|(_, tail)| tail)
            .expect("physical plan should null-extend with a left join");
        assert!(optional_tail.contains(" ON ("));
        assert!(
            !optional_tail.contains(" WHERE "),
            "the optional predicate must not become an outer WHERE filter: {physical}"
        );
    }

    #[test]
    fn physical_explain_shows_cte_boundaries_and_bind_positions() {
        let explanation = explain_physical(
            "WITH $start AS value \
             WITH value + $step AS result \
             RETURN result",
        )
        .unwrap();

        assert!(explanation.starts_with("physical-query\n  sqlite-fragment:\n"));
        assert!(explanation.contains("WITH _crabnet_with_0 AS MATERIALIZED"));
        assert!(explanation.contains("_crabnet_with_1 AS MATERIALIZED"));
        assert!(explanation.contains("      ?1 json"));
        assert!(explanation.contains("  rust-stage: none"));
    }
}
