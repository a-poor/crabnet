use std::collections::{HashMap, HashSet};

use open_cypher::{
    Span,
    ast::{
        BinaryOperator, ClauseKind, CreateClause, DeleteClause, DeleteMode, Expr as AstExpr,
        ExprKind, FunctionInvocation, IntegerRadix, IsPredicate, LabelExpression,
        LabelExpressionKind, LiteralKind, MatchClause, NodePattern, ParameterName, PathFactor,
        PathFactorKind, Pattern, ProjectionClause, ProjectionItemKind, RelationshipDirection,
        RelationshipPattern, RemoveClause, RemoveItemKind, SetClause, SetItemKind, SetQuantifier,
        SortDirection as AstSortDirection, StatementKind, UnaryOperator as AstUnaryOperator,
        WithClause,
    },
    parse,
};
use serde_json::{Number, Value};

use crate::{Error, Result};

use super::logical::{
    BinaryOp, Direction, EntityKind, Expr, Input, MatchOperator, MatchPlan, Mutation, Projection,
    Query, ReadPipeline, ReadStage, ScalarSlotId, SlotId, SortDirection, SortItem, UnaryOp,
    ValueExpr, WithBinding, WithStage,
};

pub(crate) fn lower(source: &str) -> Result<Query> {
    let trimmed = source.trim();
    if trimmed.is_empty() || trimmed == ";" {
        return Err(Error::EmptyQuery);
    }

    let parsed = parse(source).map_err(|errors| Error::Parse(errors.render(source)))?;
    let [statement] = parsed.program.statements.as_slice() else {
        return Err(Error::EmptyQuery);
    };
    let StatementKind::Query(query_statement) = &statement.kind else {
        return Err(unsupported(statement.span, "query statement"));
    };
    if query_statement.mode.is_some() {
        return Err(unsupported(
            query_statement.query.span,
            "EXPLAIN and PROFILE",
        ));
    }
    let open_cypher::ast::QueryKind::Regular(query) = &query_statement.query.kind else {
        return Err(unsupported(query_statement.query.span, "query kind"));
    };
    if !query.unions.is_empty() {
        return Err(unsupported(query_statement.query.span, "UNION"));
    }

    let clauses = &query.head.kind.clauses;
    let contains_with = clauses
        .iter()
        .any(|clause| matches!(clause.kind, ClauseKind::With(_)));
    let repeated_read_match = clauses
        .iter()
        .filter(|clause| matches!(clause.kind, ClauseKind::Match(_)))
        .count()
        > 1
        && clauses
            .iter()
            .all(|clause| matches!(clause.kind, ClauseKind::Match(_) | ClauseKind::Return(_)));
    if contains_with || repeated_read_match {
        return lower_read_pipeline(source, clauses);
    }
    if clauses.iter().any(|clause| {
        matches!(
            clause.kind,
            ClauseKind::Set(_) | ClauseKind::Remove(_) | ClauseKind::Delete(_)
        )
    }) {
        return lower_matched_mutation_query(source, clauses);
    }
    let (match_clause, create_clause, return_clause) = match clauses.as_slice() {
        [only] => match &only.kind {
            ClauseKind::Create(create) => (None, Some(create), None),
            ClauseKind::Return(return_clause) => (None, None, Some(return_clause)),
            kind => return Err(unsupported(only.span, clause_name(kind))),
        },
        [first, second] => match (&first.kind, &second.kind) {
            (ClauseKind::Match(match_clause), ClauseKind::Return(return_clause)) => {
                (Some(match_clause), None, Some(return_clause))
            }
            (ClauseKind::Create(create), ClauseKind::Return(return_clause)) => {
                (None, Some(create), Some(return_clause))
            }
            (ClauseKind::Match(match_clause), ClauseKind::Create(create)) => {
                (Some(match_clause), Some(create), None)
            }
            (_, kind) => return Err(unsupported(second.span, clause_name(kind))),
        },
        [first, second, third] => match (&first.kind, &second.kind, &third.kind) {
            (
                ClauseKind::Match(match_clause),
                ClauseKind::Create(create),
                ClauseKind::Return(return_clause),
            ) => (Some(match_clause), Some(create), Some(return_clause)),
            (_, _, kind) => return Err(unsupported(third.span, clause_name(kind))),
        },
        [] => return Err(Error::EmptyQuery),
        _ => {
            let unsupported_clause = clauses
                .iter()
                .find(|clause| {
                    !matches!(
                        clause.kind,
                        ClauseKind::Match(_) | ClauseKind::Create(_) | ClauseKind::Return(_)
                    )
                })
                .unwrap_or(&clauses[1]);
            return Err(unsupported(
                unsupported_clause.span,
                clause_name(&unsupported_clause.kind),
            ));
        }
    };

    let mut lowerer = Lowerer::new(source);
    let input = match match_clause {
        Some(match_clause) => Input::Match(lowerer.match_clause(match_clause)?),
        None => Input::Unit,
    };
    if let Some(create) = create_clause {
        lowerer.create_clause(create)?;
    }
    let projection = return_clause
        .map(|return_clause| lowerer.projection(&return_clause.projection, create_clause.is_some()))
        .transpose()?;

    Ok(Query {
        slots: lowerer.slots,
        scalar_slots: lowerer.scalar_slots,
        input,
        mutations: lowerer.mutations,
        projection,
    })
}

fn lower_matched_mutation_query(
    source: &str,
    clauses: &[open_cypher::ast::Clause],
) -> Result<Query> {
    let mut lowerer = Lowerer::new(source);
    let mut input = MatchPlan::default();
    let mut saw_match = false;
    let mut saw_mutation = false;
    let mut saw_delete = false;
    let mut projection = None;

    for (index, clause) in clauses.iter().enumerate() {
        if projection.is_some() {
            return Err(unsupported(clause.span, "clauses after RETURN"));
        }
        match &clause.kind {
            ClauseKind::Match(match_clause) if !saw_mutation => {
                saw_match = true;
                input
                    .operators
                    .extend(lowerer.match_clause(match_clause)?.operators);
            }
            ClauseKind::Match(_) => {
                return Err(unsupported(clause.span, "MATCH after a mutation clause"));
            }
            ClauseKind::Set(set_clause) => {
                if saw_delete {
                    return Err(unsupported(clause.span, "mutation clause after DELETE"));
                }
                if !saw_match {
                    return Err(semantic(clause.span, "SET requires a MATCH input"));
                }
                saw_mutation = true;
                lowerer.set_clause(set_clause)?;
            }
            ClauseKind::Remove(remove_clause) => {
                if saw_delete {
                    return Err(unsupported(clause.span, "mutation clause after DELETE"));
                }
                if !saw_match {
                    return Err(semantic(clause.span, "REMOVE requires a MATCH input"));
                }
                saw_mutation = true;
                lowerer.remove_clause(remove_clause)?;
            }
            ClauseKind::Delete(delete_clause) => {
                if saw_delete {
                    return Err(unsupported(clause.span, "mutation clause after DELETE"));
                }
                if !saw_match {
                    return Err(semantic(clause.span, "DELETE requires a MATCH input"));
                }
                saw_mutation = true;
                saw_delete = true;
                lowerer.delete_clause(delete_clause)?;
            }
            ClauseKind::Return(return_clause) if index + 1 == clauses.len() => {
                projection = Some(lowerer.projection(&return_clause.projection, true)?);
            }
            ClauseKind::Return(_) => {
                return Err(unsupported(clause.span, "clauses after RETURN"));
            }
            kind => return Err(unsupported(clause.span, clause_name(kind))),
        }
    }

    if !saw_mutation {
        return Err(Error::Data(
            "matched-mutation lowering produced no mutation".into(),
        ));
    }

    Ok(Query {
        slots: lowerer.slots,
        scalar_slots: lowerer.scalar_slots,
        input: Input::Match(input),
        mutations: lowerer.mutations,
        projection,
    })
}

fn lower_read_pipeline(source: &str, clauses: &[open_cypher::ast::Clause]) -> Result<Query> {
    let mut lowerer = Lowerer::new(source);
    let mut stages = Vec::new();
    let mut projection = None;

    for (index, clause) in clauses.iter().enumerate() {
        if projection.is_some() {
            return Err(unsupported(clause.span, "clauses after RETURN"));
        }
        match &clause.kind {
            ClauseKind::Match(match_clause) => {
                stages.push(ReadStage::Match(lowerer.match_clause(match_clause)?));
            }
            ClauseKind::With(with_clause) => {
                stages.push(ReadStage::With(lowerer.with_clause(with_clause)?));
            }
            ClauseKind::Return(return_clause) if index + 1 == clauses.len() => {
                projection = Some(lowerer.projection(&return_clause.projection, false)?);
            }
            ClauseKind::Return(_) => {
                return Err(unsupported(clause.span, "clauses after RETURN"));
            }
            ClauseKind::Create(_) => {
                return Err(unsupported(clause.span, "writes in a WITH pipeline"));
            }
            kind => return Err(unsupported(clause.span, clause_name(kind))),
        }
    }

    let projection = projection.ok_or_else(|| {
        semantic(
            clauses.last().map_or(Span::new(0, 0), |clause| clause.span),
            "a read pipeline must end with RETURN",
        )
    })?;

    Ok(Query {
        slots: lowerer.slots,
        scalar_slots: lowerer.scalar_slots,
        input: Input::Pipeline(ReadPipeline { stages }),
        mutations: Vec::new(),
        projection: Some(projection),
    })
}

#[derive(Clone, Copy, Debug)]
enum Symbol {
    Entity(SlotId),
    Scalar(ScalarSlotId),
}

struct Lowerer<'source> {
    source: &'source str,
    slots: Vec<EntityKind>,
    scalar_slots: usize,
    symbols: HashMap<String, Symbol>,
    mutations: Vec<Mutation>,
}

impl<'source> Lowerer<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            source,
            slots: Vec::new(),
            scalar_slots: 0,
            symbols: HashMap::new(),
            mutations: Vec::new(),
        }
    }

    fn allocate(&mut self, kind: EntityKind) -> SlotId {
        let slot = SlotId(self.slots.len());
        self.slots.push(kind);
        slot
    }

    fn kind(&self, slot: SlotId) -> EntityKind {
        self.slots[slot.0]
    }

    fn allocate_scalar(&mut self) -> ScalarSlotId {
        let slot = ScalarSlotId(self.scalar_slots);
        self.scalar_slots += 1;
        slot
    }

    fn match_clause(&mut self, match_clause: &MatchClause) -> Result<MatchPlan> {
        if match_clause.optional || match_clause.mode.is_some() || !match_clause.hints.is_empty() {
            return Err(unsupported(
                match_clause.pattern.span,
                "OPTIONAL/path-mode/hinted MATCH",
            ));
        }
        let mut plan = MatchPlan::default();
        for part in &match_clause.pattern.parts {
            if part.kind.binding.is_some() || part.kind.selector.is_some() {
                return Err(unsupported(part.span, "path bindings and selectors"));
            }
            self.match_path(&mut plan, &part.kind.path.kind.factors, part.span)?;
        }
        if let Some(predicate) = &match_clause.where_clause {
            plan.operators
                .push(MatchOperator::Filter(self.scalar_expression(predicate)?));
        }
        Ok(plan)
    }

    fn match_path(
        &mut self,
        plan: &mut MatchPlan,
        factors: &[PathFactor],
        span: Span,
    ) -> Result<()> {
        let Some(first) = factors.first() else {
            return Err(semantic(span, "a MATCH path cannot be empty"));
        };
        let PathFactorKind::Node(first_node) = &first.kind else {
            return Err(unsupported(
                first.span,
                "a MATCH path not beginning with a node",
            ));
        };
        let mut left = self.match_node(plan, first_node, first.span)?;
        let mut relationships = Vec::new();
        let mut remaining = &factors[1..];
        while !remaining.is_empty() {
            let [relationship, node, rest @ ..] = remaining else {
                return Err(semantic(
                    span,
                    "MATCH paths must alternate relationships and nodes",
                ));
            };
            let PathFactorKind::Relationship(relationship_pattern) = &relationship.kind else {
                return Err(unsupported(
                    relationship.span,
                    "non-relationship path factor",
                ));
            };
            let PathFactorKind::Node(node_pattern) = &node.kind else {
                return Err(unsupported(node.span, "non-node path factor"));
            };
            let relationship_slot =
                self.match_relationship(plan, relationship_pattern, relationship.span)?;
            if relationships.contains(&relationship_slot) {
                return Err(semantic(
                    relationship.span,
                    "a relationship variable cannot be reused in one path",
                ));
            }
            relationships.push(relationship_slot);
            let right = self.match_node(plan, node_pattern, node.span)?;
            let direction = match relationship_pattern.direction.kind {
                RelationshipDirection::Right => Direction::Right,
                RelationshipDirection::Left => Direction::Left,
                RelationshipDirection::Undirected => Direction::Undirected,
                RelationshipDirection::Both => {
                    return Err(unsupported(
                        relationship_pattern.direction.span,
                        "bidirectional relationship patterns",
                    ));
                }
                _ => {
                    return Err(unsupported(
                        relationship_pattern.direction.span,
                        "relationship direction",
                    ));
                }
            };
            plan.operators.push(MatchOperator::ExpandRelationship {
                relationship: relationship_slot,
                left,
                right,
                direction,
            });
            left = right;
            remaining = rest;
        }
        if relationships.len() > 1 {
            plan.operators
                .push(MatchOperator::DistinctRelationships(relationships));
        }
        Ok(())
    }

    fn match_node(
        &mut self,
        plan: &mut MatchPlan,
        node: &NodePattern,
        span: Span,
    ) -> Result<SlotId> {
        let (slot, fresh) = self.bind_match_entity(
            node.variable.as_ref().map(|name| &name.kind.text),
            EntityKind::Node,
            span,
        )?;
        if fresh {
            plan.operators.push(MatchOperator::ScanNode { slot });
        }
        if let Some(label) = &node.labels {
            plan.operators.push(MatchOperator::HasLabel {
                slot,
                label: single_label(label, "multiple or boolean node labels")?,
            });
        }
        self.match_properties(plan, slot, node.properties.as_deref())?;
        if let Some(predicate) = &node.where_clause {
            plan.operators
                .push(MatchOperator::Filter(self.scalar_expression(predicate)?));
        }
        Ok(slot)
    }

    fn match_relationship(
        &mut self,
        plan: &mut MatchPlan,
        relationship: &RelationshipPattern,
        span: Span,
    ) -> Result<SlotId> {
        if relationship.legacy_quantifier.is_some() || relationship.graph_quantifier.is_some() {
            return Err(unsupported(span, "variable-length relationship patterns"));
        }
        let (slot, fresh) = self.bind_match_entity(
            relationship.variable.as_ref().map(|name| &name.kind.text),
            EntityKind::Relationship,
            span,
        )?;
        if fresh {
            plan.operators
                .push(MatchOperator::ScanRelationship { slot });
        }
        if let Some(label) = &relationship.labels {
            plan.operators.push(MatchOperator::HasRelationshipType {
                slot,
                relationship_type: single_label(label, "multiple or boolean relationship types")?,
            });
        }
        self.match_properties(plan, slot, relationship.properties.as_deref())?;
        if let Some(predicate) = &relationship.where_clause {
            plan.operators
                .push(MatchOperator::Filter(self.scalar_expression(predicate)?));
        }
        Ok(slot)
    }

    fn bind_match_entity(
        &mut self,
        variable: Option<&String>,
        kind: EntityKind,
        span: Span,
    ) -> Result<(SlotId, bool)> {
        if let Some(variable) = variable
            && let Some(symbol) = self.symbols.get(variable).copied()
        {
            let Symbol::Entity(slot) = symbol else {
                return Err(semantic(
                    span,
                    format!("scalar variable `{variable}` cannot be used in a graph pattern"),
                ));
            };
            if self.kind(slot) != kind {
                return Err(semantic(
                    span,
                    format!("variable `{variable}` changes entity kind"),
                ));
            }
            return Ok((slot, false));
        }

        let slot = self.allocate(kind);
        if let Some(variable) = variable {
            self.symbols.insert(variable.clone(), Symbol::Entity(slot));
        }
        Ok((slot, true))
    }

    fn match_properties(
        &mut self,
        plan: &mut MatchPlan,
        slot: SlotId,
        properties: Option<&AstExpr>,
    ) -> Result<()> {
        let Some(properties) = properties else {
            return Ok(());
        };
        let ExprKind::Map(entries) = &properties.kind else {
            return Err(unsupported(
                properties.span,
                "parameter-map pattern matching",
            ));
        };
        let mut keys = HashSet::with_capacity(entries.len());
        for entry in entries {
            if !keys.insert(&entry.key.kind.text) {
                return Err(semantic(
                    entry.key.span,
                    format!(
                        "property map contains duplicate key `{}`",
                        entry.key.kind.text
                    ),
                ));
            }
            plan.operators.push(MatchOperator::PropertyEquals {
                slot,
                key: entry.key.kind.text.clone(),
                value: input_value(&entry.value)?,
            });
        }
        Ok(())
    }

    fn create_clause(&mut self, create: &CreateClause) -> Result<()> {
        let mutation_count = self.mutations.len();
        self.create_pattern(&create.pattern)?;
        if self.mutations.len() == mutation_count {
            return Err(semantic(
                create.pattern.span,
                "CREATE must introduce at least one node or relationship",
            ));
        }
        Ok(())
    }

    fn create_pattern(&mut self, pattern: &Pattern) -> Result<()> {
        for part in &pattern.parts {
            if part.kind.binding.is_some() || part.kind.selector.is_some() {
                return Err(unsupported(
                    part.span,
                    "path bindings and selectors in CREATE",
                ));
            }
            self.create_path(&part.kind.path.kind.factors, part.span)?;
        }
        Ok(())
    }

    fn create_path(&mut self, factors: &[PathFactor], span: Span) -> Result<()> {
        let Some(first) = factors.first() else {
            return Err(semantic(span, "a CREATE path cannot be empty"));
        };
        let PathFactorKind::Node(first_node) = &first.kind else {
            return Err(unsupported(
                first.span,
                "a CREATE path not beginning with a node",
            ));
        };
        let mut left = self.create_node(first_node, first.span)?;
        let mut remaining = &factors[1..];
        while !remaining.is_empty() {
            let [relationship, node, rest @ ..] = remaining else {
                return Err(semantic(
                    span,
                    "CREATE paths must alternate relationships and nodes",
                ));
            };
            let PathFactorKind::Relationship(relationship_pattern) = &relationship.kind else {
                return Err(unsupported(
                    relationship.span,
                    "non-relationship path factor",
                ));
            };
            let PathFactorKind::Node(node_pattern) = &node.kind else {
                return Err(unsupported(node.span, "non-node path factor"));
            };
            let right = self.create_node(node_pattern, node.span)?;
            self.create_relationship(relationship_pattern, relationship.span, left, right)?;
            left = right;
            remaining = rest;
        }
        Ok(())
    }

    fn create_node(&mut self, node: &NodePattern, span: Span) -> Result<SlotId> {
        if node.where_clause.is_some() {
            return Err(unsupported(span, "element WHERE in CREATE"));
        }
        if let Some(variable) = &node.variable
            && let Some(symbol) = self.symbols.get(&variable.kind.text).copied()
        {
            let Symbol::Entity(slot) = symbol else {
                return Err(semantic(
                    variable.span,
                    "a scalar variable cannot be reused as a CREATE node",
                ));
            };
            if self.kind(slot) != EntityKind::Node {
                return Err(semantic(
                    variable.span,
                    "variable is already a relationship",
                ));
            }
            if node.labels.is_some() || node.properties.is_some() {
                return Err(semantic(
                    span,
                    "a repeated CREATE node variable must be a bare `(variable)` reference",
                ));
            }
            return Ok(slot);
        }

        let label = node
            .labels
            .as_ref()
            .map(|label| single_label(label, "multiple or boolean node labels"))
            .transpose()?;
        let properties = properties_input(node.properties.as_deref())?;
        let slot = self.allocate(EntityKind::Node);
        self.mutations.push(Mutation::CreateNode {
            slot,
            label,
            properties,
        });
        if let Some(variable) = &node.variable {
            self.symbols
                .insert(variable.kind.text.clone(), Symbol::Entity(slot));
        }
        Ok(slot)
    }

    fn create_relationship(
        &mut self,
        relationship: &RelationshipPattern,
        span: Span,
        left: SlotId,
        right: SlotId,
    ) -> Result<SlotId> {
        if relationship.legacy_quantifier.is_some()
            || relationship.graph_quantifier.is_some()
            || relationship.where_clause.is_some()
        {
            return Err(unsupported(
                span,
                "quantified or filtered relationships in CREATE",
            ));
        }
        let relationship_type = relationship
            .labels
            .as_ref()
            .ok_or_else(|| semantic(span, "a created relationship must have one type"))
            .and_then(|label| single_label(label, "multiple or boolean relationship types"))?;
        let (from, to) = match relationship.direction.kind {
            RelationshipDirection::Right => (left, right),
            RelationshipDirection::Left => (right, left),
            RelationshipDirection::Undirected | RelationshipDirection::Both => {
                return Err(unsupported(span, "undirected relationships in CREATE"));
            }
            _ => return Err(unsupported(span, "relationship direction")),
        };
        if let Some(variable) = &relationship.variable
            && self.symbols.contains_key(&variable.kind.text)
        {
            return Err(semantic(variable.span, "variable is already bound"));
        }
        let slot = self.allocate(EntityKind::Relationship);
        self.mutations.push(Mutation::CreateRelationship {
            slot,
            relationship_type,
            from,
            to,
            properties: properties_input(relationship.properties.as_deref())?,
        });
        if let Some(variable) = &relationship.variable {
            self.symbols
                .insert(variable.kind.text.clone(), Symbol::Entity(slot));
        }
        Ok(slot)
    }

    fn set_clause(&mut self, set_clause: &SetClause) -> Result<()> {
        for item in &set_clause.items {
            match &item.kind {
                SetItemKind::Property { target, value } => {
                    let (slot, key) = self.property_target(target)?;
                    self.mutations.push(Mutation::SetProperty {
                        slot,
                        key,
                        value: input_value(value)?,
                    });
                }
                SetItemKind::ReplaceProperties { target, value } => {
                    let slot = self.resolve_entity_name(target)?;
                    self.mutations.push(Mutation::ReplaceProperties {
                        slot,
                        properties: input_value(value)?,
                    });
                }
                SetItemKind::MergeProperties { target, value } => {
                    let slot = self.resolve_entity_name(target)?;
                    self.mutations.push(Mutation::MergeProperties {
                        slot,
                        properties: input_value(value)?,
                    });
                }
                SetItemKind::Labels { .. } => {
                    return Err(unsupported(item.span, "setting labels"));
                }
                _ => return Err(unsupported(item.span, "SET item")),
            }
        }
        Ok(())
    }

    fn remove_clause(&mut self, remove_clause: &RemoveClause) -> Result<()> {
        for item in &remove_clause.items {
            match &item.kind {
                RemoveItemKind::Property(target) => {
                    let (slot, key) = self.property_target(target)?;
                    self.mutations.push(Mutation::RemoveProperty { slot, key });
                }
                RemoveItemKind::Labels { .. } => {
                    return Err(unsupported(item.span, "removing labels"));
                }
                _ => return Err(unsupported(item.span, "REMOVE item")),
            }
        }
        Ok(())
    }

    fn delete_clause(&mut self, delete_clause: &DeleteClause) -> Result<()> {
        let detach = match delete_clause.mode.kind {
            DeleteMode::Normal | DeleteMode::Nodetach => false,
            DeleteMode::Detach => true,
            _ => return Err(unsupported(delete_clause.mode.span, "DELETE mode")),
        };
        let targets = delete_clause
            .expressions
            .iter()
            .map(|expression| {
                let mut target = expression;
                while let ExprKind::Parenthesized(inner) = &target.kind {
                    target = inner;
                }
                let ExprKind::Variable(variable) = &target.kind else {
                    return Err(semantic(
                        target.span,
                        "DELETE target must be a node or relationship variable",
                    ));
                };
                let slot = self.resolve_entity_name(variable)?;
                Ok((slot, self.kind(slot)))
            })
            .collect::<Result<Vec<_>>>()?;
        self.mutations.push(Mutation::Delete { targets, detach });
        Ok(())
    }

    fn property_target(&self, target: &AstExpr) -> Result<(SlotId, String)> {
        let ExprKind::Property { expression, key } = &target.kind else {
            return Err(semantic(
                target.span,
                "property mutation requires `variable.key`",
            ));
        };
        Ok((
            self.resolve_entity_variable(expression)?,
            key.kind.text.clone(),
        ))
    }

    fn resolve_entity_name(&self, name: &open_cypher::ast::Name) -> Result<SlotId> {
        match self.resolve_symbol(name)? {
            Symbol::Entity(slot) => Ok(slot),
            Symbol::Scalar(_) => Err(semantic(
                name.span,
                format!("scalar variable `{}` is not a graph entity", name.kind.text),
            )),
        }
    }

    fn with_clause(&mut self, with_clause: &WithClause) -> Result<WithStage> {
        let projection = &with_clause.projection;
        match projection.quantifier.as_ref().map(|value| value.kind) {
            None | Some(SetQuantifier::All) => {}
            Some(SetQuantifier::Distinct) => {
                return Err(unsupported(projection.span, "WITH DISTINCT"));
            }
            Some(_) => return Err(unsupported(projection.span, "WITH quantifier")),
        }

        let incoming_symbols = self.symbols.clone();
        let mut bindings = Vec::with_capacity(projection.items.len());
        let mut projected_symbols = HashMap::new();
        let mut aliases = HashMap::new();

        for item in &projection.items {
            let ProjectionItemKind::Expression { expression, alias } = &item.kind else {
                return Err(unsupported(item.span, "WITH *"));
            };
            let name = match (alias, &expression.kind) {
                (Some(alias), _) => alias.kind.text.clone(),
                (None, ExprKind::Variable(variable)) => variable.kind.text.clone(),
                (None, _) => {
                    return Err(unsupported(
                        item.span,
                        "computed WITH expressions without aliases",
                    ));
                }
            };
            if projected_symbols.contains_key(&name) {
                return Err(semantic(
                    item.span,
                    format!("WITH projects duplicate variable `{name}`"),
                ));
            }

            let expression = self.output_expression(expression)?;
            match expression {
                Expr::Entity(slot) => {
                    let kind = self.kind(slot);
                    bindings.push(WithBinding::Entity {
                        name: name.clone(),
                        slot,
                        kind,
                    });
                    projected_symbols.insert(name.clone(), Symbol::Entity(slot));
                    aliases.insert(name, Expr::Entity(slot));
                }
                expression => {
                    let slot = self.allocate_scalar();
                    bindings.push(WithBinding::Scalar {
                        name: name.clone(),
                        slot,
                        expression: expression.clone(),
                    });
                    projected_symbols.insert(name.clone(), Symbol::Scalar(slot));
                    aliases.insert(name, expression);
                }
            }
        }

        if bindings.is_empty() {
            return Err(semantic(
                projection.span,
                "WITH must contain at least one item",
            ));
        }

        // Cypher permits WITH's WHERE and ORDER BY to refer to projected
        // aliases. WHERE may also inspect incoming variables that are not
        // carried beyond this boundary. Projected names shadow incoming ones.
        self.symbols = incoming_symbols.clone();
        self.symbols.extend(projected_symbols.clone());
        let filter = with_clause
            .where_clause
            .as_ref()
            .map(|expression| self.scalar_expression_with_aliases(expression, &aliases))
            .transpose()?;
        let order_by = self.order_by(&projection.order_by, &aliases, false, &[])?;
        let (offset, limit) = projection_pagination(projection, "WITH")?;

        self.symbols = projected_symbols;
        Ok(WithStage {
            bindings,
            filter,
            order_by,
            offset,
            limit,
        })
    }

    fn projection(&self, projection: &ProjectionClause, after_write: bool) -> Result<Projection> {
        if after_write
            && (projection.quantifier.is_some()
                || projection.order_by.is_some()
                || projection.offset.is_some()
                || projection.skip.is_some()
                || projection.limit.is_some())
        {
            return Err(unsupported(
                projection.span,
                "projection modifiers after a mutation",
            ));
        }
        let distinct = match projection.quantifier.as_ref().map(|value| value.kind) {
            None | Some(SetQuantifier::All) => false,
            Some(SetQuantifier::Distinct) => true,
            Some(_) => return Err(unsupported(projection.span, "RETURN quantifier")),
        };

        let mut columns = Vec::with_capacity(projection.items.len());
        let mut expressions = Vec::with_capacity(projection.items.len());
        let mut aliases = HashMap::new();
        for item in &projection.items {
            let ProjectionItemKind::Expression { expression, alias } = &item.kind else {
                return Err(unsupported(item.span, "RETURN *"));
            };
            let lowered = self.output_expression(expression)?;
            if after_write
                && matches!(
                    lowered,
                    Expr::Binary { .. } | Expr::Unary { .. } | Expr::IsNull { .. }
                )
            {
                return Err(unsupported(
                    expression.span,
                    "computed projection after a mutation",
                ));
            }
            columns.push(column_name(self.source, expression, alias.as_ref()));
            expressions.push(lowered.clone());
            if let Some(alias) = alias {
                aliases.insert(alias.kind.text.clone(), lowered);
            }
        }
        if expressions.is_empty() {
            return Err(semantic(
                projection.span,
                "RETURN must contain at least one item",
            ));
        }

        let order_by = self.order_by(&projection.order_by, &aliases, distinct, &expressions)?;
        let (offset_expression, limit) = projection_pagination(projection, "RETURN")?;

        Ok(Projection {
            columns,
            expressions,
            distinct,
            order_by,
            offset: offset_expression,
            limit,
        })
    }

    fn order_by(
        &self,
        order: &Option<open_cypher::ast::OrderBy>,
        aliases: &HashMap<String, Expr>,
        distinct: bool,
        projected: &[Expr],
    ) -> Result<Vec<SortItem>> {
        let mut order_by = Vec::new();
        if let Some(order) = order {
            for item in &order.items {
                if item.nulls.is_some() {
                    return Err(unsupported(item.span, "ORDER BY NULLS FIRST/LAST"));
                }
                let expression = self.scalar_expression_with_aliases(&item.expression, aliases)?;
                if distinct && !projected.contains(&expression) {
                    return Err(semantic(
                        item.expression.span,
                        "ORDER BY expressions must be projected when RETURN DISTINCT is used",
                    ));
                }
                let direction = match item.direction.as_ref().map(|value| value.kind) {
                    None | Some(AstSortDirection::Ascending) => SortDirection::Ascending,
                    Some(AstSortDirection::Descending) => SortDirection::Descending,
                    _ => return Err(unsupported(item.span, "sort direction")),
                };
                order_by.push(SortItem {
                    expression,
                    direction,
                });
            }
        }
        Ok(order_by)
    }

    fn output_expression(&self, expression: &AstExpr) -> Result<Expr> {
        match &expression.kind {
            ExprKind::Property {
                expression: base,
                key,
            } => Ok(Expr::Property(
                self.resolve_entity_variable(base)?,
                key.kind.text.clone(),
            )),
            ExprKind::Variable(variable) => match self.resolve_symbol(variable)? {
                Symbol::Entity(slot) => Ok(Expr::Entity(slot)),
                Symbol::Scalar(slot) => Ok(Expr::Scalar(slot)),
            },
            ExprKind::Function(function) => self.function_expression(function, expression.span),
            ExprKind::Parenthesized(inner) => self.output_expression(inner),
            _ => self.scalar_expression(expression),
        }
    }

    fn scalar_expression(&self, expression: &AstExpr) -> Result<Expr> {
        self.scalar_expression_with_aliases(expression, &HashMap::new())
    }

    fn scalar_expression_with_aliases(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
    ) -> Result<Expr> {
        // The positive spelling of i64::MIN does not fit Crabnet's integer
        // domain, so preserve a directly negated integer literal as one value.
        // Other unary expressions stay in the scalar IR and receive the same
        // runtime type gates as property and parameter operands.
        if let ExprKind::Unary { operator, operand } = &expression.kind
            && operator.kind == AstUnaryOperator::Minus
            && matches!(operand.kind, ExprKind::Literal(_))
            && let ExprKind::Literal(literal) = &operand.kind
            && matches!(literal.kind, LiteralKind::Integer(_))
        {
            return input_value(expression).map(Expr::Value);
        }
        match &expression.kind {
            ExprKind::Literal(_)
            | ExprKind::Parameter(_)
            | ExprKind::List(_)
            | ExprKind::Map(_) => input_value(expression).map(Expr::Value),
            ExprKind::Property {
                expression: base,
                key,
            } => Ok(Expr::Property(
                self.resolve_entity_variable(base)?,
                key.kind.text.clone(),
            )),
            ExprKind::Binary {
                left,
                operator,
                right,
            } => {
                let operator = match operator.kind {
                    BinaryOperator::Or => BinaryOp::Or,
                    BinaryOperator::And => BinaryOp::And,
                    BinaryOperator::Equal => BinaryOp::Equal,
                    BinaryOperator::NotEqual => BinaryOp::NotEqual,
                    BinaryOperator::Less => BinaryOp::Less,
                    BinaryOperator::LessEqual => BinaryOp::LessEqual,
                    BinaryOperator::Greater => BinaryOp::Greater,
                    BinaryOperator::GreaterEqual => BinaryOp::GreaterEqual,
                    BinaryOperator::In => BinaryOp::In,
                    BinaryOperator::StartsWith => BinaryOp::StartsWith,
                    BinaryOperator::EndsWith => BinaryOp::EndsWith,
                    BinaryOperator::Contains => BinaryOp::Contains,
                    BinaryOperator::Add => BinaryOp::Add,
                    BinaryOperator::Subtract => BinaryOp::Subtract,
                    BinaryOperator::Multiply => BinaryOp::Multiply,
                    BinaryOperator::Divide => BinaryOp::Divide,
                    BinaryOperator::Remainder => BinaryOp::Remainder,
                    _ => return Err(unsupported(operator.span, "binary operator")),
                };
                Ok(Expr::Binary {
                    left: Box::new(self.scalar_expression_with_aliases(left, aliases)?),
                    operator,
                    right: Box::new(self.scalar_expression_with_aliases(right, aliases)?),
                })
            }
            ExprKind::Unary { operator, operand } => {
                let operator = match operator.kind {
                    AstUnaryOperator::Plus => UnaryOp::Plus,
                    AstUnaryOperator::Minus => UnaryOp::Minus,
                    AstUnaryOperator::Not => UnaryOp::Not,
                    _ => return Err(unsupported(operator.span, "unary operator")),
                };
                Ok(Expr::Unary {
                    operator,
                    operand: Box::new(self.scalar_expression_with_aliases(operand, aliases)?),
                })
            }
            ExprKind::Is {
                expression,
                negated,
                predicate: IsPredicate::Null,
            } => Ok(Expr::IsNull {
                expression: Box::new(self.scalar_expression_with_aliases(expression, aliases)?),
                negated: *negated,
            }),
            ExprKind::Is { .. } => Err(unsupported(expression.span, "IS predicate")),
            ExprKind::Function(function) => self.function_expression(function, expression.span),
            ExprKind::Parenthesized(inner) => self.scalar_expression_with_aliases(inner, aliases),
            ExprKind::Variable(variable) => {
                if let Some(expression) = aliases.get(&variable.kind.text) {
                    return match expression {
                        Expr::Entity(_) => Err(semantic(
                            variable.span,
                            format!(
                                "graph variable `{}` is not a scalar expression",
                                variable.kind.text
                            ),
                        )),
                        expression => Ok(expression.clone()),
                    };
                }
                match self.resolve_symbol(variable)? {
                    Symbol::Scalar(slot) => Ok(Expr::Scalar(slot)),
                    Symbol::Entity(_) => Err(semantic(
                        variable.span,
                        format!(
                            "graph variable `{}` is not a scalar expression",
                            variable.kind.text
                        ),
                    )),
                }
            }
            _ => Err(unsupported(expression.span, "expression")),
        }
    }

    fn function_expression(&self, function: &FunctionInvocation, span: Span) -> Result<Expr> {
        let name = simple_function_name(function, span)?;
        match name.as_str() {
            "elementid" => Ok(Expr::ElementId(
                self.single_entity_argument(function, span)?,
            )),
            "type" => {
                let slot = self.single_entity_argument(function, span)?;
                match self.kind(slot) {
                    EntityKind::Relationship => Ok(Expr::RelationshipType(slot)),
                    EntityKind::Node => Err(semantic(span, "type() requires a relationship")),
                }
            }
            "labels" => {
                let slot = self.single_entity_argument(function, span)?;
                match self.kind(slot) {
                    EntityKind::Node => Ok(Expr::Labels(slot)),
                    EntityKind::Relationship => Err(semantic(span, "labels() requires a node")),
                }
            }
            _ => Err(unsupported(span, format!("function `{name}`"))),
        }
    }

    fn single_entity_argument(&self, function: &FunctionInvocation, span: Span) -> Result<SlotId> {
        let [argument] = function.arguments.as_slice() else {
            return Err(semantic(
                span,
                "graph function requires exactly one argument",
            ));
        };
        let ExprKind::Variable(variable) = &argument.kind else {
            return Err(semantic(
                argument.span,
                "graph function argument must be a variable",
            ));
        };
        match self.resolve_symbol(variable)? {
            Symbol::Entity(slot) => Ok(slot),
            Symbol::Scalar(_) => Err(semantic(
                variable.span,
                format!(
                    "scalar variable `{}` cannot be used as a graph entity",
                    variable.kind.text
                ),
            )),
        }
    }

    fn resolve_entity_variable(&self, expression: &AstExpr) -> Result<SlotId> {
        let ExprKind::Variable(variable) = &expression.kind else {
            return Err(unsupported(expression.span, "nested property access"));
        };
        match self.resolve_symbol(variable)? {
            Symbol::Entity(slot) => Ok(slot),
            Symbol::Scalar(_) => Err(semantic(
                variable.span,
                format!(
                    "scalar variable `{}` does not have graph properties",
                    variable.kind.text
                ),
            )),
        }
    }

    fn resolve_symbol(&self, variable: &open_cypher::ast::Name) -> Result<Symbol> {
        self.symbols
            .get(&variable.kind.text)
            .copied()
            .ok_or_else(|| {
                semantic(
                    variable.span,
                    format!("unknown variable `{}`", variable.kind.text),
                )
            })
    }
}

fn projection_pagination(
    projection: &ProjectionClause,
    clause_name: &str,
) -> Result<(Option<ValueExpr>, Option<ValueExpr>)> {
    let offset = match (&projection.offset, &projection.skip) {
        (Some(_), Some(_)) => {
            return Err(semantic(
                projection.span,
                format!("{clause_name} cannot contain OFFSET and SKIP"),
            ));
        }
        (Some(offset), None) | (None, Some(offset)) => Some(page_value(offset)?),
        (None, None) => None,
    };
    let limit = projection.limit.as_ref().map(page_value).transpose()?;
    Ok((offset, limit))
}

fn properties_input(properties: Option<&AstExpr>) -> Result<ValueExpr> {
    match properties {
        None => Ok(ValueExpr::Map(Vec::new())),
        Some(expression) => match &expression.kind {
            ExprKind::Map(_) | ExprKind::Parameter(_) | ExprKind::Parenthesized(_) => {
                input_value(expression)
            }
            _ => Err(unsupported(expression.span, "non-map pattern properties")),
        },
    }
}

fn input_value(expression: &AstExpr) -> Result<ValueExpr> {
    match &expression.kind {
        ExprKind::Literal(literal) => literal_value(&literal.kind).map(ValueExpr::Literal),
        ExprKind::Parameter(parameter) => match &parameter.name {
            ParameterName::Named(name) => Ok(ValueExpr::Parameter(name.clone())),
            ParameterName::Positional(_) => {
                Err(unsupported(expression.span, "positional parameters"))
            }
            _ => Err(unsupported(expression.span, "parameter form")),
        },
        ExprKind::List(items) => items
            .iter()
            .map(input_value)
            .collect::<Result<Vec<_>>>()
            .map(ValueExpr::List),
        ExprKind::Map(entries) => entries
            .iter()
            .map(|entry| Ok((entry.key.kind.text.clone(), input_value(&entry.value)?)))
            .collect::<Result<Vec<_>>>()
            .map(ValueExpr::Map),
        ExprKind::Unary { operator, operand } => match operator.kind {
            AstUnaryOperator::Minus => {
                if let ExprKind::Literal(literal) = &operand.kind
                    && let LiteralKind::Integer(integer) = &literal.kind
                {
                    negative_integer_literal(integer).map(ValueExpr::Literal)
                } else {
                    input_value(operand).map(Box::new).map(ValueExpr::Negated)
                }
            }
            AstUnaryOperator::Plus => input_value(operand),
            AstUnaryOperator::Not => Err(unsupported(expression.span, "NOT in a stored value")),
            _ => Err(unsupported(expression.span, "unary value expression")),
        },
        ExprKind::Parenthesized(inner) => input_value(inner),
        _ => Err(unsupported(
            expression.span,
            "non-literal stored value expression",
        )),
    }
}

fn page_value(expression: &AstExpr) -> Result<ValueExpr> {
    match &expression.kind {
        ExprKind::Literal(literal) if matches!(literal.kind, LiteralKind::Integer(_)) => {
            input_value(expression)
        }
        ExprKind::Parameter(_) => input_value(expression),
        ExprKind::Unary { operator, operand }
            if matches!(
                operator.kind,
                AstUnaryOperator::Plus | AstUnaryOperator::Minus
            ) =>
        {
            page_value(operand)?;
            input_value(expression)
        }
        ExprKind::Parenthesized(inner) => page_value(inner),
        _ => Err(semantic(
            expression.span,
            "SKIP and LIMIT require an integer literal or named parameter",
        )),
    }
}

fn literal_value(literal: &LiteralKind) -> Result<Value> {
    match literal {
        LiteralKind::Null => Ok(Value::Null),
        LiteralKind::Boolean(value) => Ok(Value::Bool(*value)),
        LiteralKind::String(value) => Ok(Value::String(value.value.clone())),
        LiteralKind::Integer(integer) => {
            let (digits, radix) = integer_parts(integer)?;
            i64::from_str_radix(&digits, radix)
                .map(Value::from)
                .map_err(|_| {
                    Error::Semantic(format!(
                        "integer `{}` is outside the i64 range",
                        integer.text
                    ))
                })
        }
        LiteralKind::Float(float) => {
            let parsed = float.text.parse::<f64>().map_err(|_| {
                Error::Semantic(format!("invalid floating-point literal `{}`", float.text))
            })?;
            Number::from_f64(parsed)
                .map(Value::Number)
                .ok_or_else(|| Error::Semantic("non-finite floats are not supported".into()))
        }
        _ => Err(Error::Unsupported("literal type".into())),
    }
}

fn negative_integer_literal(integer: &open_cypher::ast::IntegerLiteral) -> Result<Value> {
    let (digits, radix) = integer_parts(integer)?;
    let magnitude = u64::from_str_radix(&digits, radix).map_err(|_| {
        Error::Semantic(format!(
            "integer `-{}` is outside the i64 range",
            integer.text
        ))
    })?;
    let minimum_magnitude = (i64::MAX as u64) + 1;
    if magnitude == minimum_magnitude {
        Ok(Value::from(i64::MIN))
    } else {
        i64::try_from(magnitude)
            .map(|value| Value::from(-value))
            .map_err(|_| {
                Error::Semantic(format!(
                    "integer `-{}` is outside the i64 range",
                    integer.text
                ))
            })
    }
}

fn integer_parts(integer: &open_cypher::ast::IntegerLiteral) -> Result<(String, u32)> {
    let text = integer.text.replace('_', "");
    match integer.radix {
        IntegerRadix::Decimal => Ok((text, 10)),
        IntegerRadix::Hexadecimal => Ok((
            text.strip_prefix("0x")
                .or_else(|| text.strip_prefix("0X"))
                .unwrap_or(&text)
                .to_owned(),
            16,
        )),
        IntegerRadix::Octal => Ok((
            text.strip_prefix("0o")
                .or_else(|| text.strip_prefix("0O"))
                .unwrap_or(&text)
                .to_owned(),
            8,
        )),
        _ => Err(Error::Unsupported("integer radix".into())),
    }
}

fn single_label(label: &LabelExpression, feature: &str) -> Result<String> {
    match &label.kind {
        LabelExpressionKind::Name(name) => Ok(name.kind.text.clone()),
        _ => Err(unsupported(label.span, feature)),
    }
}

fn simple_function_name(function: &FunctionInvocation, span: Span) -> Result<String> {
    if function.quantifier.is_some() || function.name.parts.len() != 1 {
        return Err(unsupported(span, "qualified or quantified function call"));
    }
    Ok(function.name.parts[0].kind.text.to_ascii_lowercase())
}

fn column_name(
    source: &str,
    expression: &AstExpr,
    alias: Option<&open_cypher::ast::Name>,
) -> String {
    alias.map_or_else(
        || {
            expression
                .span
                .text(source)
                .unwrap_or("<expression>")
                .to_owned()
        },
        |alias| alias.kind.text.clone(),
    )
}

fn clause_name(clause: &ClauseKind) -> &'static str {
    match clause {
        ClauseKind::Use(_) => "USE",
        ClauseKind::Match(_) => "this MATCH clause sequence",
        ClauseKind::Unwind(_) => "UNWIND",
        ClauseKind::Let(_) => "LET",
        ClauseKind::Create(_) => "this CREATE clause sequence",
        ClauseKind::Insert(_) => "INSERT",
        ClauseKind::Merge(_) => "MERGE",
        ClauseKind::Delete(_) => "DELETE",
        ClauseKind::Set(_) => "SET",
        ClauseKind::Remove(_) => "REMOVE",
        ClauseKind::Foreach(_) => "FOREACH",
        ClauseKind::LoadCsv(_) => "LOAD CSV",
        ClauseKind::Call(_) => "CALL",
        ClauseKind::With(_) => "WITH",
        ClauseKind::Return(_) => "this RETURN clause sequence",
        ClauseKind::Select(_) => "SELECT",
        ClauseKind::Next(_) => "NEXT",
        ClauseKind::Filter(_) => "FILTER",
        ClauseKind::Finish => "FINISH",
        ClauseKind::Error(_) => "recovered syntax",
        _ => "clause",
    }
}

fn unsupported(span: Span, feature: impl AsRef<str>) -> Error {
    Error::Unsupported(format!(
        "{} at bytes {}..{}",
        feature.as_ref(),
        span.start,
        span.end
    ))
}

fn semantic(span: Span, message: impl AsRef<str>) -> Error {
    Error::Semantic(format!(
        "{} at bytes {}..{}",
        message.as_ref(),
        span.start,
        span.end
    ))
}
