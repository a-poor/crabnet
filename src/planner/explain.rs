use serde_json::Value;

use super::logical::{
    AggregateKind, BinaryOp, Direction, EntityKind, Expr, Input, ListQuantifier, MatchOperator,
    Mutation, PathCollectionKind, PathFunctionKind, PathId, PathSegment, Projection, Query,
    ReadStage, RelationshipUse, ScalarSlotId, SlotId, SortDirection, StoredValue, UnaryOp,
    ValueExpr, WithBinding, WithStage,
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
    if query.path_slots > 0 {
        renderer.line(1, "path-slots:");
        for index in 0..query.path_slots {
            renderer.line(2, format!("{} path", path_slot_name(PathId(index))));
        }
    }

    renderer.line(1, "input:");
    match &query.input {
        Input::Unit => renderer.line(2, "unit"),
        Input::Rows {
            slots,
            scalars,
            stages,
        } => {
            renderer.line(
                2,
                format!("materialized-rows {}", render_bindings(slots, scalars)),
            );
            render_stages(&mut renderer, stages);
        }
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
            render_stages(&mut renderer, &pipeline.stages);
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

    if !query.segments.is_empty() {
        renderer.line(1, "segments:");
        for (index, segment) in query.segments.iter().enumerate() {
            renderer.line(
                2,
                format!(
                    "segment {index}: rows {}",
                    render_bindings(&segment.slots, &segment.scalars)
                ),
            );
            render_stages(&mut renderer, &segment.stages);
            renderer.line(3, "mutations:");
            if segment.mutations.is_empty() {
                renderer.line(4, "none");
            } else {
                for mutation in &segment.mutations {
                    renderer.line(4, render_mutation(mutation));
                }
            }
        }
    }

    renderer.line(1, "projection:");
    match &query.projection {
        None => renderer.line(2, "none"),
        Some(projection) => render_projection(&mut renderer, projection),
    }

    renderer.finish()
}

fn render_bindings(slots: &[(SlotId, EntityKind)], scalars: &[ScalarSlotId]) -> String {
    slots
        .iter()
        .map(|(slot, _)| slot_name(*slot))
        .chain(scalars.iter().map(|slot| scalar_slot_name(*slot)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_stages(renderer: &mut Renderer, stages: &[ReadStage]) {
    for (index, stage) in stages.iter().enumerate() {
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
                render_with_stage(renderer, stage);
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
            WithBinding::Path { name, path } => renderer.line(
                5,
                format!("{} = path({})", quote(name), path_slot_name(*path)),
            ),
            WithBinding::PathCollection { name, path, kind } => renderer.line(
                5,
                format!(
                    "{} = {}(path({}))",
                    quote(name),
                    path_collection_kind_name(*kind),
                    path_slot_name(*path)
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
        MatchOperator::ExpandVariableLength {
            traversal,
            left,
            right,
            direction,
            minimum,
            maximum,
            relationship_type,
            properties,
        } => format!(
            "expand-variable traversal=~{} left={} right={} direction={} bounds={}..{} type={} properties={}",
            traversal.0,
            slot_name(*left),
            slot_name(*right),
            direction_name(*direction),
            minimum,
            maximum,
            relationship_type
                .as_ref()
                .map_or_else(|| "any".to_owned(), |value| quote(value)),
            render_property_values(properties)
        ),
        MatchOperator::ExpandHybridVariableLength {
            traversal,
            left,
            right,
            direction,
            minimum,
            maximum,
            relationship_type,
            properties,
        } => format!(
            "expand-hybrid-variable executor=rust traversal=~{} left={} right={} direction={} bounds={}..{} type={} properties={}",
            traversal.0,
            slot_name(*left),
            slot_name(*right),
            direction_name(*direction),
            minimum,
            maximum.map_or_else(|| "unbounded".to_owned(), |value| value.to_string()),
            relationship_type
                .as_ref()
                .map_or_else(|| "any".to_owned(), |value| quote(value)),
            render_property_values(properties)
        ),
        MatchOperator::ExpandShortestPath {
            traversal,
            left,
            right,
            direction,
            minimum,
            maximum,
            all,
            relationship_type,
            properties,
        } => format!(
            "expand-shortest executor=rust traversal=~{} mode={} left={} right={} direction={} bounds={}..{} type={} properties={}",
            traversal.0,
            if *all { "all" } else { "one" },
            slot_name(*left),
            slot_name(*right),
            direction_name(*direction),
            minimum,
            maximum.map_or_else(|| "unbounded".to_owned(), |value| value.to_string()),
            relationship_type
                .as_ref()
                .map_or_else(|| "any".to_owned(), |value| quote(value)),
            render_property_values(properties)
        ),
        MatchOperator::DistinctRelationships(relationships) => format!(
            "distinct-relationships [{}]",
            relationships
                .iter()
                .map(render_relationship_use)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchOperator::BindPath {
            path,
            start,
            segments,
        } => format!(
            "bind-path path={} start={} segments=[{}]",
            path_slot_name(*path),
            slot_name(*start),
            segments
                .iter()
                .map(render_path_segment)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchOperator::MatchRelationshipList { traversal, path } => format!(
            "match-relationship-list traversal=~{} path={}",
            traversal.0,
            path_slot_name(*path)
        ),
        MatchOperator::Filter(expression) => format!("filter {}", render_expr(expression)),
    }
}

fn render_path_segment(segment: &PathSegment) -> String {
    format!(
        "segment(left={}, relationship={}, right={})",
        slot_name(segment.left),
        render_relationship_use(&segment.relationship),
        slot_name(segment.right)
    )
}

fn render_relationship_use(relationship: &RelationshipUse) -> String {
    match relationship {
        RelationshipUse::Single(slot) => slot_name(*slot),
        RelationshipUse::Traversal(traversal) => format!("~{}", traversal.0),
    }
}

fn render_property_values(properties: &[(String, ValueExpr)]) -> String {
    if properties.is_empty() {
        return "{}".to_owned();
    }
    let mut properties = properties.iter().collect::<Vec<_>>();
    properties.sort_by(|left, right| left.0.cmp(&right.0));
    format!(
        "{{{}}}",
        properties
            .into_iter()
            .map(|(key, value)| format!("{}: {}", quote(key), render_value_expr(value)))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn render_stored_value(value: &StoredValue) -> String {
    match value {
        StoredValue::Static(value) => render_value_expr(value),
        StoredValue::Computed(expression) => format!("computed({})", render_expr(expression)),
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
            render_stored_value(properties)
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
            render_stored_value(properties)
        ),
        Mutation::SetProperty { slot, key, value } => format!(
            "set-property slot={} key={} value={}",
            slot_name(*slot),
            quote(key),
            render_stored_value(value)
        ),
        Mutation::ReplaceProperties { slot, properties } => format!(
            "replace-properties slot={} properties={}",
            slot_name(*slot),
            render_stored_value(properties)
        ),
        Mutation::MergeProperties { slot, properties } => format!(
            "merge-properties slot={} properties={}",
            slot_name(*slot),
            render_stored_value(properties)
        ),
        Mutation::RemoveProperty { slot, key } => format!(
            "remove-property slot={} key={}",
            slot_name(*slot),
            quote(key)
        ),
        Mutation::Merge {
            fresh,
            creates,
            on_create,
            on_match,
            ..
        } => {
            let render_all = |mutations: &[Mutation]| {
                mutations
                    .iter()
                    .map(render_mutation)
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            format!(
                "merge fresh=[{}] creates=[{}] on-create=[{}] on-match=[{}]",
                fresh
                    .iter()
                    .map(|(slot, kind)| format!("{}:{}", slot_name(*slot), entity_kind_name(*kind)))
                    .collect::<Vec<_>>()
                    .join(", "),
                render_all(creates),
                render_all(on_create),
                render_all(on_match)
            )
        }
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
        Expr::Path(path) => format!("path({})", path_slot_name(*path)),
        Expr::PathFunction { kind, path } => format!(
            "{}(path({}))",
            path_function_kind_name(*kind),
            path_slot_name(*path)
        ),
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
        Expr::Quantified {
            kind,
            variable,
            list,
            predicate,
        } => format!(
            "{}(#l{} in {} where {})",
            quantifier_kind_name(*kind),
            variable.0,
            render_expr(list),
            render_expr(predicate)
        ),
        Expr::ListElement(variable) => format!("#l{}", variable.0),
        Expr::Size(operand) => format!("size({})", render_expr(operand)),
        Expr::Index { base, index } => {
            format!("index({}, {})", render_expr(base), render_expr(index))
        }
        Expr::DynamicProperty(slot, key) => {
            format!(
                "dynamic-property({}, {})",
                slot_name(*slot),
                render_expr(key)
            )
        }
        Expr::Slice { base, lower, upper } => format!(
            "slice({}, {}..{})",
            render_expr(base),
            lower.as_deref().map(render_expr).unwrap_or_default(),
            upper.as_deref().map(render_expr).unwrap_or_default()
        ),
        Expr::Range { start, end, step } => format!(
            "range({}, {}{})",
            render_expr(start),
            render_expr(end),
            step.as_deref()
                .map(|step| format!(", {}", render_expr(step)))
                .unwrap_or_default()
        ),
        Expr::List(items) => format!(
            "list[{}]",
            items.iter().map(render_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::Map(entries) => {
            let mut entries = entries.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            format!(
                "map{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!("{}: {}", quote(key), render_expr(value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Expr::ListComprehension {
            variable,
            list,
            predicate,
            projection,
        } => format!(
            "[#l{} in {}{}{}]",
            variable.0,
            render_expr(list),
            predicate
                .as_deref()
                .map(|predicate| format!(" where {}", render_expr(predicate)))
                .unwrap_or_default(),
            projection
                .as_deref()
                .map(|projection| format!(" | {}", render_expr(projection)))
                .unwrap_or_default(),
        ),
        Expr::MapProperty { base, key } => {
            format!("map-property({}, {})", render_expr(base), quote(key))
        }
        Expr::Case {
            operand,
            alternatives,
            otherwise,
        } => {
            let mut rendered = String::from("case");
            if let Some(operand) = operand {
                rendered.push(' ');
                rendered.push_str(&render_expr(operand));
            }
            for alternative in alternatives {
                let conditions = alternative
                    .conditions
                    .iter()
                    .map(render_expr)
                    .collect::<Vec<_>>()
                    .join(", ");
                rendered.push_str(&format!(
                    " when {conditions} then {}",
                    render_expr(&alternative.result)
                ));
            }
            if let Some(otherwise) = otherwise {
                rendered.push_str(&format!(" else {}", render_expr(otherwise)));
            }
            rendered.push_str(" end");
            rendered
        }
        Expr::Function {
            function,
            arguments,
        } => format!(
            "{}({})",
            function.name(),
            arguments
                .iter()
                .map(render_expr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::EntityKeys(slot) => format!("keys({})", slot_name(*slot)),
        Expr::EntityProperties(slot) => format!("properties({})", slot_name(*slot)),
    }
}

fn quantifier_kind_name(kind: ListQuantifier) -> &'static str {
    match kind {
        ListQuantifier::All => "all",
        ListQuantifier::Any => "any",
        ListQuantifier::None => "none",
        ListQuantifier::Single => "single",
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

fn path_slot_name(path: PathId) -> String {
    format!("%p{}", path.0)
}

fn entity_kind_name(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Node => "node",
        EntityKind::Relationship => "relationship",
    }
}

fn path_function_kind_name(kind: PathFunctionKind) -> &'static str {
    match kind {
        PathFunctionKind::Length => "length",
        PathFunctionKind::Nodes => "nodes",
        PathFunctionKind::Relationships => "relationships",
    }
}

fn path_collection_kind_name(kind: PathCollectionKind) -> &'static str {
    match kind {
        PathCollectionKind::Nodes => "nodes",
        PathCollectionKind::Relationships => "relationships",
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
        assert!(physical.contains("operator 0: sqlite-fragment"));
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
        assert!(physical.starts_with("physical-plan\n  branch 0:\n"));
        assert_eq!(physical.matches(": sqlite-fragment").count(), 3);
        assert!(physical.contains("operator 2: union mode=all"));
        assert!(physical.contains("operator 4: union mode=semantic-distinct"));
        assert!(!physical.contains("batch-hydrate-paths"));
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
    fn explains_bounded_traversal_as_a_recursive_edge_trail() {
        let query = "MATCH (start:Node)-[:NEXT*0..3 {open: true}]->(end:Node) RETURN end";

        let logical = explain_logical(query).unwrap();
        assert!(logical.contains("%0 node"));
        assert!(logical.contains("%1 node"));
        assert!(!logical.contains(" relationship\n"));
        assert!(logical.contains(
            "expand-variable traversal=~0 left=%0 right=%1 direction=right bounds=0..3 \
             type=\"NEXT\" properties={\"open\": literal(true)}"
        ));

        let physical = explain_physical(query).unwrap();
        assert!(physical.contains("WITH RECURSIVE _crabnet_walk_0"));
        assert!(physical.contains("UNION ALL"));
        assert!(physical.contains("json_each(walk0.used_edges)"));
        assert!(physical.contains("walk0.depth < 3"));
        assert!(physical.contains("walk_result0.depth >= 0"));
        assert!(physical.contains("walk_result0.depth <= 3"));
    }

    #[test]
    fn explains_prebound_relationship_list_rematches_outside_same_clause_uniqueness() {
        let same_clause = explain_logical(
            "MATCH (start)-[rels:NEXT*1..2]->(end), (start)-[rels:NEXT*1..2]->(other), \
                   (other)-[more:NEXT]->(tail) \
             RETURN tail",
        )
        .unwrap();
        assert!(same_clause.contains("bind-path path=%p0 start=%0"));
        assert!(same_clause.contains("match-relationship-list traversal=~1 path=%p0"));
        assert!(same_clause.contains("distinct-relationships [~0, %3]"));
        assert!(!same_clause.contains("~1, "));

        let later_clause = explain_logical(
            "MATCH (start)-[rels:NEXT*1..2]->() WITH rels \
             MATCH (x)-[rels:NEXT*]->(y)-[other:NEXT]->(z) RETURN z",
        )
        .unwrap();
        assert!(later_clause.contains("match-relationship-list traversal=~1 path=%p0"));
        assert!(later_clause.contains("distinct-relationships [~1, "));
    }

    #[test]
    fn explains_named_path_slots_segments_and_with_aliases() {
        let query = "MATCH route = (start:Node)-[first:NEXT]->(middle)-[:NEXT*0..2]->(end) \
                     WITH route AS journey \
                     RETURN journey";
        let logical = explain_logical(query).unwrap();

        assert!(logical.contains("path-slots:\n    %p0 path"));
        assert!(logical.contains(
            "bind-path path=%p0 start=%0 segments=[\
             segment(left=%0, relationship=%1, right=%2), \
             segment(left=%2, relationship=~0, right=%3)]"
        ));
        assert_eq!(logical.matches("\"journey\" = path(%p0)").count(), 2);

        let physical = explain_physical(query).unwrap();
        assert!(physical.contains(": batch-hydrate-paths"));
        assert_eq!(physical.matches("batch-hydrate-paths").count(), 1);
    }

    #[test]
    fn explains_path_functions_and_typed_collection_with_bindings() {
        let logical = explain_logical(
            "MATCH path = (start)-[:NEXT]->(end) \
             WITH length(path) AS hops, nodes(path) AS points, \
                  relationships(path) AS links \
             RETURN hops, points, links",
        )
        .unwrap();

        assert!(logical.contains("scalar-slots:\n    %s0 scalar"));
        assert!(logical.contains("path-slots:\n    %p0 path"));
        assert!(logical.contains("\"hops\" = scalar(%s0) <- length(path(%p0))"));
        assert!(logical.contains("\"points\" = nodes(path(%p0))"));
        assert!(logical.contains("\"links\" = relationships(path(%p0))"));
        assert!(logical.contains("\"hops\" = scalar(%s0)"));
    }

    #[test]
    fn explains_legacy_shortest_modes_bounds_filters_and_path_bindings() {
        let one = explain_logical(
            "MATCH route = shortestPath(\
                 (start:Node {name: $start})-[links:NEXT*0..4 {open: true}]->(end:Node)\
             ) RETURN route, links",
        )
        .unwrap();

        assert!(one.contains(
            "expand-shortest executor=rust traversal=~0 mode=one left=%0 right=%1 \
             direction=right bounds=0..4 type=\"NEXT\" \
             properties={\"open\": literal(true)}"
        ));
        assert!(one.contains("has-label %0 \"Node\""));
        assert!(one.contains("property-equals %0 \"name\" parameter(\"start\")"));
        assert!(one.contains("has-label %1 \"Node\""));
        assert!(one.contains(
            "bind-path path=%p0 start=%0 segments=[\
             segment(left=%0, relationship=~0, right=%1)]"
        ));
        assert!(one.contains(
            "bind-path path=%p1 start=%0 segments=[\
             segment(left=%0, relationship=~0, right=%1)]"
        ));
        assert!(one.contains("\"route\" = path(%p1)"));
        assert!(one.contains("\"links\" = relationships(path(%p0))"));

        let all = explain_logical(
            "MATCH route = allShortestPaths((start)-[:NEXT*..4]->(end)) RETURN route",
        )
        .unwrap();
        assert!(all.contains("expand-shortest executor=rust"));
        assert!(all.contains("mode=all"));
        assert!(all.contains("bounds=1..4"));
    }

    #[test]
    fn rejects_deferred_legacy_shortest_shapes_during_logical_planning() {
        for query in [
            "MATCH route = shortestPath((a)-[:NEXT*]->(b)), (marker) RETURN route",
            "MATCH route = shortestPath((a)-[:NEXT*2..]->(b)) RETURN route",
            "OPTIONAL MATCH route = shortestPath((a)-[:NEXT*]->(b)) RETURN route",
            "MATCH route = shortestPath((a)-[:NEXT*]->(b)) WHERE b.ready RETURN route",
            "MATCH route = shortestPath((a)-[:NEXT]->(b)) RETURN route",
            "MATCH route = shortestPath((a)-[:NEXT*]->(b)) CREATE (:SideEffect)",
        ] {
            assert!(
                matches!(explain_logical(query), Err(crate::Error::Unsupported(_))),
                "expected structured unsupported error for {query}"
            );
        }
    }

    #[test]
    fn physical_explain_orders_semantic_distinct_before_path_hydration() {
        let query = "MATCH path = (start)-[:NEXT]->(end) \
                     RETURN DISTINCT path, end.name AS name \
                     ORDER BY name SKIP 1 LIMIT 2";

        let physical = explain_physical(query).unwrap();
        let distinct = physical.find(": semantic-distinct").unwrap();
        let pagination = physical.find(": pagination").unwrap();
        let hydration = physical.find(": batch-hydrate-paths").unwrap();
        assert!(distinct < pagination && pagination < hydration);
    }

    #[test]
    fn physical_union_explain_hydrates_path_identities_once_after_union_operations() {
        let branch = "MATCH path = (start)-[:NEXT]->(end) RETURN path";
        let query = format!("{branch} UNION {branch} UNION ALL {branch}");

        let physical = explain_physical(&query).unwrap();
        assert_eq!(physical.matches(": sqlite-fragment").count(), 3);
        assert!(physical.contains("operator 2: union mode=semantic-distinct"));
        assert!(physical.contains("operator 4: union mode=all"));
        assert_eq!(physical.matches("batch-hydrate-paths").count(), 1);

        let last_union = physical
            .rfind("operator 4: union mode=all")
            .expect("physical plan should contain the final union");
        let hydration = physical
            .rfind(": batch-hydrate-paths")
            .expect("physical plan should contain shared path hydration");
        assert!(
            last_union < hydration,
            "path hydration must follow every union operation: {physical}"
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

        assert!(explanation.starts_with("physical-plan\n  branch 0:\n"));
        assert!(explanation.contains("WITH _crabnet_with_0 AS MATERIALIZED"));
        assert!(explanation.contains("_crabnet_with_1 AS MATERIALIZED"));
        assert!(explanation.contains("      ?1 json"));
        assert_eq!(explanation.matches("sqlite-fragment").count(), 1);
    }
}
