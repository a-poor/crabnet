use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use open_cypher::{
    Span,
    ast::{
        BinaryOperator, ClauseKind, CreateClause, DeleteClause, DeleteMode, Expr as AstExpr,
        ExprKind, FunctionInvocation, IntegerRadix, IsPredicate, LabelExpression,
        LabelExpressionKind, LiteralKind, MatchClause, MergeClause, MergeTrigger, NodePattern,
        ParameterName, PathFactor, PathFactorKind, Pattern, ProjectionClause, ProjectionItemKind,
        QuantifiedPredicateKind, Quantifier, QuantifierKind, RelationshipDirection,
        RelationshipPattern, RemoveClause, RemoveItemKind, SetClause, SetItemKind, SetQuantifier,
        SortDirection as AstSortDirection, StatementKind, UnaryOperator as AstUnaryOperator,
        UnwindClause, WithClause,
    },
    parse,
};
use serde_json::{Number, Value};

use crate::{Error, Result};

use super::UnionKind;
use super::logical::{
    AggregateKind, BinaryOp, CaseAlternative, Direction, EntityAccess, EntityKind, EntityTarget,
    Expr, Input, ListQuantifier, ListVariableId, MAX_USER_VALUE_NESTING, MatchOperator, MatchPlan,
    Mutation, PathCollectionKind, PathFunctionKind, PathId, PathSegment, Projection, Query,
    ReadPipeline, ReadStage, RelationshipUse, ScalarFunction, ScalarSlotId, Segment, SlotId,
    SortDirection, SortItem, StoredValue, TraversalId, UnaryOp, UnwindBinding, UnwindStage,
    ValueExpr, WithBinding, WithStage,
};
use super::nested::{self, Op as NestedOp, Shape as NestedShape};
use super::typecheck;

pub(super) const MAX_SQLITE_VARIABLE_LENGTH_DEPTH: usize = 32;

// This bounds recursive AST lowering independently of the JSON collection
// depth contract below.
const MAX_EXPRESSION_NESTING: usize = 128;

pub(super) struct UnionSource {
    pub(super) head: String,
    pub(super) branches: Vec<(UnionKind, String)>,
}

pub(super) fn split_union(source: &str) -> Result<Option<UnionSource>> {
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
    if query.unions.is_empty() {
        return Ok(None);
    }

    let mut single_queries = Vec::with_capacity(query.unions.len() + 1);
    single_queries.push(&query.head);
    single_queries.extend(query.unions.iter().map(|branch| &branch.query));
    for single_query in &single_queries {
        if single_query
            .kind
            .clauses
            .iter()
            .any(|clause| is_write_clause(&clause.kind))
        {
            return Err(unsupported(single_query.span, "writes in UNION branches"));
        }
        if !matches!(
            single_query.kind.clauses.last().map(|clause| &clause.kind),
            Some(ClauseKind::Return(_))
        ) {
            return Err(semantic(
                single_query.span,
                "every UNION branch must end with RETURN",
            ));
        }
    }

    let branch_source = |span: Span| {
        span.text(source)
            .map(str::to_owned)
            .ok_or_else(|| Error::Data("UNION branch span is outside its source query".into()))
    };
    let head = branch_source(query.head.span)?;
    let branches = query
        .unions
        .iter()
        .map(|branch| {
            let kind = match branch.operator.kind {
                open_cypher::ast::UnionOperator::All => UnionKind::All,
                open_cypher::ast::UnionOperator::Default
                | open_cypher::ast::UnionOperator::Distinct => UnionKind::Distinct,
                _ => return Err(unsupported(branch.operator.span, "UNION operator")),
            };
            Ok((kind, branch_source(branch.query.span)?))
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some((first, _)) = branches.first()
        && let Some(mixed) = branches.iter().position(|(kind, _)| kind != first)
    {
        return Err(semantic(
            query.unions[mixed].operator.span,
            "UNION and UNION ALL cannot be mixed in one query",
        ));
    }
    Ok(Some(UnionSource { head, branches }))
}

fn is_write_clause(clause: &ClauseKind) -> bool {
    matches!(
        clause,
        ClauseKind::Create(_)
            | ClauseKind::Insert(_)
            | ClauseKind::Merge(_)
            | ClauseKind::Delete(_)
            | ClauseKind::Set(_)
            | ClauseKind::Remove(_)
            | ClauseKind::Foreach(_)
    )
}

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
    if let Some(shortest) = clauses.iter().find_map(|clause| {
        let ClauseKind::Match(match_clause) = &clause.kind else {
            return None;
        };
        legacy_shortest_factor(match_clause)
    }) && clauses.iter().any(|clause| is_write_clause(&clause.kind))
    {
        return Err(unsupported(
            shortest.span,
            "writes with a legacy shortest-path MATCH",
        ));
    }
    let contains_write = clauses.iter().any(|clause| is_write_clause(&clause.kind));
    let contains_read_pipeline = clauses.iter().any(|clause| {
        matches!(clause.kind, ClauseKind::With(_) | ClauseKind::Unwind(_))
            || matches!(&clause.kind, ClauseKind::Match(match_clause) if match_clause.optional)
    });
    let repeated_read_match = clauses
        .iter()
        .filter(|clause| matches!(clause.kind, ClauseKind::Match(_)))
        .count()
        > 1;
    if !contains_write && (contains_read_pipeline || repeated_read_match) {
        return lower_read_pipeline(source, clauses);
    }
    lower_clause_sequence(source, clauses)
}

/// Lowers a general clause sequence: reading clauses (`MATCH`, `OPTIONAL
/// MATCH`, `WITH`, `UNWIND`) and updating clauses (`CREATE`, `MERGE`, `SET`,
/// `REMOVE`, `DELETE`) in any order, ending with an updating clause or
/// `RETURN`. The reading clauses before the first update form the query
/// input; every later run of reading clauses opens a segment that continues
/// from the rows the updates so far produced.
fn lower_clause_sequence(source: &str, clauses: &[open_cypher::ast::Clause]) -> Result<Query> {
    let Some(last) = clauses.last() else {
        return Err(Error::EmptyQuery);
    };
    if !matches!(last.kind, ClauseKind::Return(_)) && !is_write_clause(&last.kind) {
        return Err(semantic(
            last.span,
            "a query must end with RETURN or an updating clause",
        ));
    }

    let mut lowerer = Lowerer::new(source);
    let mut head_stages: Vec<ReadStage> = Vec::new();
    let mut head_mutations: Vec<Mutation> = Vec::new();
    let mut segments: Vec<Segment> = Vec::new();
    // `true` while the newest segment is still collecting reading clauses.
    let mut reading = false;
    let mut saw_mutation = false;
    let mut saw_delete = false;
    let mut projection = None;

    for (index, clause) in clauses.iter().enumerate() {
        if projection.is_some() {
            return Err(unsupported(clause.span, "clauses after RETURN"));
        }
        let is_read = matches!(
            clause.kind,
            ClauseKind::Match(_) | ClauseKind::With(_) | ClauseKind::Unwind(_)
        );
        if is_read && saw_mutation && !reading {
            // The updates collected so far belong to the run before this
            // segment; the segment sees the variables live at this point.
            let batch = std::mem::take(&mut lowerer.mutations);
            match segments.last_mut() {
                Some(segment) => segment.mutations = batch,
                None => head_mutations = batch,
            }
            let (slots, scalars) = lowerer.live_bindings();
            segments.push(Segment {
                slots,
                scalars,
                stages: Vec::new(),
                mutations: Vec::new(),
            });
            reading = true;
        }
        let mut push_stage = |stage: ReadStage| match segments.last_mut() {
            Some(segment) if saw_mutation => segment.stages.push(stage),
            _ => head_stages.push(stage),
        };
        match &clause.kind {
            ClauseKind::Match(match_clause) => {
                let plan = lowerer.match_clause(match_clause)?;
                push_stage(if match_clause.optional {
                    ReadStage::OptionalMatch(plan)
                } else {
                    ReadStage::Match(plan)
                });
            }
            ClauseKind::With(with_clause) => {
                push_stage(ReadStage::With(lowerer.with_clause(with_clause)?));
            }
            ClauseKind::Unwind(unwind_clause) => {
                push_stage(ReadStage::Unwind(lowerer.unwind_clause(unwind_clause)?));
            }
            ClauseKind::Create(_)
            | ClauseKind::Merge(_)
            | ClauseKind::Set(_)
            | ClauseKind::Remove(_)
            | ClauseKind::Delete(_) => {
                if saw_delete {
                    return Err(unsupported(clause.span, "mutation clause after DELETE"));
                }
                match &clause.kind {
                    ClauseKind::Create(create) => lowerer.create_clause(create)?,
                    ClauseKind::Merge(merge) => lowerer.merge_clause(merge)?,
                    kind => {
                        if index == 0 {
                            return Err(semantic(
                                clause.span,
                                format!("{} requires a MATCH input", clause_name(kind)),
                            ));
                        }
                        match kind {
                            ClauseKind::Set(set_clause) => lowerer.set_clause(set_clause)?,
                            ClauseKind::Remove(remove_clause) => {
                                lowerer.remove_clause(remove_clause)?;
                            }
                            ClauseKind::Delete(delete_clause) => {
                                saw_delete = true;
                                lowerer.delete_clause(delete_clause)?;
                            }
                            _ => unreachable!("updating clause kinds are matched above"),
                        }
                    }
                }
                saw_mutation = true;
                reading = false;
            }
            ClauseKind::Return(return_clause) if index + 1 == clauses.len() => {
                projection = Some(lowerer.projection(&return_clause.projection)?);
            }
            ClauseKind::Return(_) => {
                return Err(unsupported(clause.span, "clauses after RETURN"));
            }
            kind => return Err(unsupported(clause.span, clause_name(kind))),
        }
    }

    let batch = std::mem::take(&mut lowerer.mutations);
    match segments.last_mut() {
        Some(segment) => segment.mutations = batch,
        None => head_mutations = batch,
    }
    let input = if head_stages.is_empty() {
        Input::Unit
    } else if head_stages
        .iter()
        .all(|stage| matches!(stage, ReadStage::Match(_)))
    {
        let mut plan = MatchPlan::default();
        for stage in head_stages {
            let ReadStage::Match(stage_plan) = stage else {
                unreachable!("checked to be plain MATCH stages");
            };
            plan.operators.extend(stage_plan.operators);
        }
        Input::Match(plan)
    } else {
        Input::Pipeline(ReadPipeline {
            stages: head_stages,
        })
    };

    let nested_slots = lowerer
        .kinds
        .borrow()
        .scalars
        .iter()
        .filter_map(|(slot, kind)| matches!(kind, typecheck::Kind::Nested(_)).then_some(*slot))
        .collect();
    Ok(Query {
        slots: lowerer.slots.into_inner(),
        scalar_slots: lowerer.scalar_slots,
        nested_slots,
        entity_lists: lowerer
            .kinds
            .into_inner()
            .scalars
            .into_iter()
            .filter_map(|(slot, kind)| match kind {
                typecheck::Kind::EntityList(kind) => Some((slot, kind)),
                _ => None,
            })
            .collect(),
        path_slots: lowerer.path_slots,
        input,
        mutations: head_mutations,
        segments,
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
                let plan = lowerer.match_clause(match_clause)?;
                stages.push(if match_clause.optional {
                    ReadStage::OptionalMatch(plan)
                } else {
                    ReadStage::Match(plan)
                });
            }
            ClauseKind::With(with_clause) => {
                stages.push(ReadStage::With(lowerer.with_clause(with_clause)?));
            }
            ClauseKind::Unwind(unwind_clause) => {
                stages.push(ReadStage::Unwind(lowerer.unwind_clause(unwind_clause)?));
            }
            ClauseKind::Return(return_clause) if index + 1 == clauses.len() => {
                projection = Some(lowerer.projection(&return_clause.projection)?);
            }
            ClauseKind::Return(_) => {
                return Err(unsupported(clause.span, "clauses after RETURN"));
            }
            ClauseKind::Create(_) => {
                return Err(unsupported(clause.span, "writes in a read pipeline"));
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

    let nested_slots = lowerer
        .kinds
        .borrow()
        .scalars
        .iter()
        .filter_map(|(slot, kind)| matches!(kind, typecheck::Kind::Nested(_)).then_some(*slot))
        .collect();
    Ok(Query {
        slots: lowerer.slots.into_inner(),
        scalar_slots: lowerer.scalar_slots,
        nested_slots,
        entity_lists: lowerer
            .kinds
            .into_inner()
            .scalars
            .into_iter()
            .filter_map(|(slot, kind)| match kind {
                typecheck::Kind::EntityList(kind) => Some((slot, kind)),
                _ => None,
            })
            .collect(),
        path_slots: lowerer.path_slots,
        input: Input::Pipeline(ReadPipeline { stages }),
        mutations: Vec::new(),
        segments: Vec::new(),
        projection: Some(projection),
    })
}

#[derive(Clone, Copy, Debug)]
enum Symbol {
    Entity(SlotId),
    Scalar(ScalarSlotId),
    Path(PathId),
    PathCollection {
        path: PathId,
        kind: PathCollectionKind,
    },
}

struct MatchedPath {
    start: SlotId,
    segments: Vec<PathSegment>,
}

struct Lowerer<'source> {
    source: &'source str,
    // Pattern predicates allocate slots while expression lowering holds the
    // lowerer immutably, so the slot table sits behind a cell.
    slots: RefCell<Vec<EntityKind>>,
    scalar_slots: usize,
    traversal_slots: usize,
    path_slots: usize,
    symbols: HashMap<String, Symbol>,
    mutations: Vec<Mutation>,
    // Expression lowering borrows the lowerer immutably, so list-bound
    // variable ids come from a cell rather than a `&mut` counter.
    list_variables: Cell<usize>,
    // Kinds of scalar variables bound to expressions whose kind the query
    // text fixes, for planning-time type checks.
    kinds: RefCell<typecheck::Kinds>,
}

impl<'source> Lowerer<'source> {
    fn new(source: &'source str) -> Self {
        Self {
            source,
            slots: RefCell::new(Vec::new()),
            scalar_slots: 0,
            traversal_slots: 0,
            path_slots: 0,
            symbols: HashMap::new(),
            mutations: Vec::new(),
            list_variables: Cell::new(0),
            kinds: RefCell::default(),
        }
    }

    fn allocate_list_variable(&self) -> ListVariableId {
        let id = self.list_variables.get();
        self.list_variables.set(id + 1);
        ListVariableId(id)
    }

    fn allocate(&mut self, kind: EntityKind) -> SlotId {
        let mut slots = self.slots.borrow_mut();
        let slot = SlotId(slots.len());
        slots.push(kind);
        slot
    }

    fn kind(&self, slot: SlotId) -> EntityKind {
        self.slots.borrow()[slot.0]
    }

    fn allocate_scalar(&mut self) -> ScalarSlotId {
        let slot = ScalarSlotId(self.scalar_slots);
        self.scalar_slots += 1;
        slot
    }

    /// Restore graph-list types when a scalar slot is used as a value.
    fn symbol_expression(&self, symbol: Symbol) -> Expr {
        match symbol {
            Symbol::Entity(slot) => Expr::Entity(slot),
            Symbol::Scalar(slot) => match self.kinds.borrow().scalars.get(&slot) {
                Some(typecheck::Kind::EntityList(kind)) => Expr::EntityList {
                    kind: *kind,
                    expression: Box::new(Expr::Scalar(slot)),
                },
                Some(typecheck::Kind::Nested(shape)) => {
                    nested::restore(*shape.clone(), Expr::Scalar(slot))
                }
                _ => Expr::Scalar(slot),
            },
            Symbol::Path(path) => Expr::Path(path),
            Symbol::PathCollection { path, kind } => Expr::PathFunction {
                kind: path_function_kind(kind),
                path,
            },
        }
    }

    /// The variables `*` expands to: everything in scope, by name.
    fn wildcard_items(&self) -> Vec<(String, Symbol)> {
        let mut items = self
            .symbols
            .iter()
            .map(|(name, symbol)| (name.clone(), *symbol))
            .collect::<Vec<_>>();
        items.sort_by(|left, right| left.0.cmp(&right.0));
        items
    }

    /// The entity and scalar slots the variables in scope currently bind.
    fn live_bindings(&self) -> (Vec<(SlotId, EntityKind)>, Vec<ScalarSlotId>) {
        let mut slots = Vec::new();
        let mut scalars = Vec::new();
        for symbol in self.symbols.values() {
            match symbol {
                Symbol::Entity(slot) => slots.push((*slot, self.kind(*slot))),
                Symbol::Scalar(slot) => scalars.push(*slot),
                Symbol::Path(_) | Symbol::PathCollection { .. } => {}
            }
        }
        slots.sort_by_key(|(slot, _)| *slot);
        slots.dedup();
        scalars.sort();
        scalars.dedup();
        (slots, scalars)
    }

    fn allocate_traversal(&mut self) -> TraversalId {
        let traversal = TraversalId(self.traversal_slots);
        self.traversal_slots += 1;
        traversal
    }

    fn allocate_path(&mut self) -> PathId {
        let path = PathId(self.path_slots);
        self.path_slots += 1;
        path
    }

    fn match_clause(&mut self, match_clause: &MatchClause) -> Result<MatchPlan> {
        if match_clause.mode.is_some() || !match_clause.hints.is_empty() {
            return Err(unsupported(
                match_clause.pattern.span,
                "path-mode or hinted MATCH",
            ));
        }
        if let Some(shortest) = legacy_shortest_factor(match_clause) {
            if match_clause.optional {
                return Err(unsupported(
                    shortest.span,
                    "legacy shortest path in OPTIONAL MATCH",
                ));
            }
            if match_clause.pattern.parts.len() != 1 {
                return Err(unsupported(
                    match_clause.pattern.span,
                    "multiple pattern parts with a legacy shortest path",
                ));
            }
            if match_clause.pattern.parts[0].kind.path.kind.factors.len() != 1 {
                return Err(unsupported(
                    match_clause.pattern.parts[0].span,
                    "legacy shortest path combined with other factors",
                ));
            }
            if let Some(predicate) = &match_clause.where_clause {
                return Err(unsupported(
                    predicate.span,
                    "MATCH WHERE on a legacy shortest path",
                ));
            }
        }
        let mut plan = MatchPlan::default();
        let mut relationships = Vec::new();
        for part in &match_clause.pattern.parts {
            if part.kind.selector.is_some() {
                return Err(unsupported(part.span, "path selectors"));
            }
            let matched = if let [factor] = part.kind.path.kind.factors.as_slice() {
                match &factor.kind {
                    PathFactorKind::LegacyShortest { all, pattern } => {
                        self.match_shortest_path(&mut plan, *all, pattern, factor.span)?
                    }
                    _ => self.match_path(&mut plan, &part.kind.path.kind.factors, part.span)?,
                }
            } else {
                self.match_path(&mut plan, &part.kind.path.kind.factors, part.span)?
            };
            relationships.extend(matched.segments.iter().map(|segment| segment.relationship));
            if let Some(binding) = &part.kind.binding {
                let name = &binding.kind.text;
                if self.symbols.contains_key(name) {
                    return Err(semantic(
                        binding.span,
                        format!("path variable `{name}` is already bound"),
                    ));
                }
                let path = self.allocate_path();
                self.symbols.insert(name.clone(), Symbol::Path(path));
                plan.operators.push(MatchOperator::BindPath {
                    path,
                    start: matched.start,
                    segments: matched.segments,
                });
            }
        }
        // A traversal re-matched against a relationship list bound earlier in
        // this same clause denotes the same relationships, so it does not take
        // part in relationship uniqueness; the original binding already does.
        // A list bound by an earlier clause has no other occurrence here, so
        // its re-match stays in the uniqueness set like any other traversal.
        let same_clause_rematches = same_clause_rematches(&plan);
        relationships.retain(|relationship| {
            !matches!(
                relationship,
                RelationshipUse::Traversal(traversal) if same_clause_rematches.contains(traversal)
            )
        });
        if relationships.len() > 1 {
            plan.operators
                .push(MatchOperator::DistinctRelationships(relationships));
        }
        if let Some(predicate) = &match_clause.where_clause {
            plan.operators
                .push(MatchOperator::Filter(self.match_predicate(predicate)?));
        }
        Ok(plan)
    }

    fn match_path(
        &mut self,
        plan: &mut MatchPlan,
        factors: &[PathFactor],
        span: Span,
    ) -> Result<MatchedPath> {
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
        let start = left;
        let mut segments = Vec::new();
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
            let (right, relationship_use) = match (
                relationship_pattern.legacy_quantifier.as_ref(),
                relationship_pattern.graph_quantifier.as_ref(),
            ) {
                (None, None) => {
                    let relationship_slot =
                        self.match_relationship(plan, relationship_pattern, relationship.span)?;
                    let right = self.match_node(plan, node_pattern, node.span)?;
                    plan.operators.push(MatchOperator::ExpandRelationship {
                        relationship: relationship_slot,
                        left,
                        right,
                        direction,
                    });
                    (right, RelationshipUse::Single(relationship_slot))
                }
                (_, Some(_)) => {
                    return Err(unsupported(
                        relationship.span,
                        "graph-pattern relationship quantifiers",
                    ));
                }
                (Some(quantifier), None) => {
                    if relationship_pattern.where_clause.is_some() {
                        return Err(unsupported(
                            relationship.span,
                            "predicates on variable-length relationships",
                        ));
                    }
                    let (minimum, maximum) = legacy_quantifier_bounds(quantifier)?;
                    let traversal = self.allocate_traversal();
                    let relationship_type = relationship_pattern
                        .labels
                        .as_ref()
                        .map(|label| single_label(label, "multiple or boolean relationship types"))
                        .transpose()?;
                    let properties =
                        self.match_property_values(relationship_pattern.properties.as_deref())?;
                    let relationship_list = relationship_pattern
                        .variable
                        .as_ref()
                        .map(|variable| self.bind_relationship_list(variable))
                        .transpose()?;
                    let right = self.match_node(plan, node_pattern, node.span)?;
                    let expansion = match maximum {
                        Some(maximum) if maximum <= MAX_SQLITE_VARIABLE_LENGTH_DEPTH => {
                            MatchOperator::ExpandVariableLength {
                                traversal,
                                left,
                                right,
                                direction,
                                minimum,
                                maximum,
                                relationship_type,
                                properties,
                            }
                        }
                        maximum => MatchOperator::ExpandHybridVariableLength {
                            traversal,
                            left,
                            right,
                            direction,
                            minimum,
                            maximum,
                            relationship_type,
                            properties,
                        },
                    };
                    plan.operators.push(expansion);
                    Self::bind_relationship_list_result(
                        plan,
                        relationship_list,
                        traversal,
                        left,
                        right,
                    );
                    (right, RelationshipUse::Traversal(traversal))
                }
            };
            if relationships.contains(&relationship_use) {
                return Err(semantic(
                    relationship.span,
                    "a relationship variable cannot be reused in one path",
                ));
            }
            relationships.push(relationship_use);
            segments.push(PathSegment {
                left,
                relationship: relationship_use,
                right,
            });
            left = right;
            remaining = rest;
        }
        Ok(MatchedPath { start, segments })
    }

    fn match_shortest_path(
        &mut self,
        plan: &mut MatchPlan,
        all: bool,
        pattern: &open_cypher::ast::PathPattern,
        span: Span,
    ) -> Result<MatchedPath> {
        let [first, relationship, last] = pattern.kind.factors.as_slice() else {
            return Err(unsupported(
                span,
                "legacy shortest path with anything other than one relationship",
            ));
        };
        let PathFactorKind::Node(first_node) = &first.kind else {
            return Err(unsupported(
                first.span,
                "legacy shortest path not beginning with a node",
            ));
        };
        let PathFactorKind::Relationship(relationship_pattern) = &relationship.kind else {
            return Err(unsupported(
                relationship.span,
                "non-relationship legacy shortest-path factor",
            ));
        };
        let PathFactorKind::Node(last_node) = &last.kind else {
            return Err(unsupported(
                last.span,
                "legacy shortest path not ending with a node",
            ));
        };

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
        if relationship_pattern.graph_quantifier.is_some() {
            return Err(unsupported(
                relationship.span,
                "graph-pattern quantifier in a legacy shortest path",
            ));
        }
        let Some(quantifier) = relationship_pattern.legacy_quantifier.as_ref() else {
            return Err(unsupported(
                relationship.span,
                "legacy shortest path without a variable-length relationship",
            ));
        };
        if relationship_pattern.where_clause.is_some() {
            return Err(unsupported(
                relationship.span,
                "relationship predicate in a legacy shortest path",
            ));
        }

        let (minimum, maximum) = legacy_quantifier_bounds(quantifier)?;
        if minimum > 1 {
            return Err(unsupported(
                quantifier.span,
                "legacy shortest path with a minimum greater than one",
            ));
        }

        let left = self.match_node(plan, first_node, first.span)?;
        let traversal = self.allocate_traversal();
        let relationship_type = relationship_pattern
            .labels
            .as_ref()
            .map(|label| single_label(label, "multiple or boolean relationship types"))
            .transpose()?;
        let properties = self.match_property_values(relationship_pattern.properties.as_deref())?;
        let relationship_list = relationship_pattern
            .variable
            .as_ref()
            .map(|variable| self.bind_relationship_list(variable))
            .transpose()?;
        let right = self.match_node(plan, last_node, last.span)?;

        plan.operators.push(MatchOperator::ExpandShortestPath {
            traversal,
            left,
            right,
            direction,
            minimum,
            maximum,
            all,
            relationship_type,
            properties,
        });
        let segment = PathSegment {
            left,
            relationship: RelationshipUse::Traversal(traversal),
            right,
        };
        Self::bind_relationship_list_result(plan, relationship_list, traversal, left, right);

        Ok(MatchedPath {
            start: left,
            segments: vec![segment],
        })
    }

    // A fresh relationship variable binds the traversal as a new list; a
    // pre-bound one constrains the traversal to that exact sequence instead.
    fn bind_relationship_list_result(
        plan: &mut MatchPlan,
        binding: Option<RelationshipListBinding>,
        traversal: TraversalId,
        left: SlotId,
        right: SlotId,
    ) {
        match binding {
            Some(RelationshipListBinding::Fresh(path)) => {
                plan.operators.push(MatchOperator::BindPath {
                    path,
                    start: left,
                    segments: vec![PathSegment {
                        left,
                        relationship: RelationshipUse::Traversal(traversal),
                        right,
                    }],
                });
            }
            Some(RelationshipListBinding::Prebound(path)) => {
                plan.operators
                    .push(MatchOperator::MatchRelationshipList { traversal, path });
            }
            None => {}
        }
    }

    fn bind_relationship_list(
        &mut self,
        variable: &open_cypher::ast::Name,
    ) -> Result<RelationshipListBinding> {
        let name = &variable.kind.text;
        if let Some(symbol) = self.symbols.get(name).copied() {
            return match symbol {
                Symbol::PathCollection {
                    kind: PathCollectionKind::Relationships,
                    path,
                } => Ok(RelationshipListBinding::Prebound(path)),
                Symbol::Entity(_)
                | Symbol::Scalar(_)
                | Symbol::Path(_)
                | Symbol::PathCollection {
                    kind: PathCollectionKind::Nodes,
                    ..
                } => Err(semantic(
                    variable.span,
                    format!("variable `{name}` is already bound as a non-relationship-list value"),
                )),
            };
        }

        let path = self.allocate_path();
        self.symbols.insert(
            name.clone(),
            Symbol::PathCollection {
                path,
                kind: PathCollectionKind::Relationships,
            },
        );
        Ok(RelationshipListBinding::Fresh(path))
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
        } else {
            // An out-of-range index (or OPTIONAL MATCH) can bind a null node.
            // Even an otherwise unconstrained MATCH (n) must reject that row.
            plan.operators.push(MatchOperator::Filter(Expr::IsNull {
                expression: Box::new(Expr::ElementId(slot)),
                negated: true,
            }));
        }
        if let Some(label) = &node.labels {
            self.pattern_labels(plan, slot, label)?;
        }
        self.match_properties(plan, slot, node.properties.as_deref())?;
        if let Some(predicate) = &node.where_clause {
            plan.operators
                .push(MatchOperator::Filter(self.match_predicate(predicate)?));
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
                .push(MatchOperator::Filter(self.match_predicate(predicate)?));
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
                    format!("non-entity variable `{variable}` cannot be used in a graph pattern"),
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
        for (key, value) in self.match_property_values(properties)? {
            plan.operators
                .push(MatchOperator::PropertyEquals { slot, key, value });
        }
        Ok(())
    }

    fn match_property_values(
        &self,
        properties: Option<&AstExpr>,
    ) -> Result<Vec<(String, ValueExpr)>> {
        let Some(properties) = properties else {
            return Ok(Vec::new());
        };
        let ExprKind::Map(entries) = &properties.kind else {
            return Err(unsupported(
                properties.span,
                "parameter-map pattern matching",
            ));
        };
        let mut keys = HashSet::with_capacity(entries.len());
        let mut values = Vec::with_capacity(entries.len());
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
            values.push((entry.key.kind.text.clone(), input_value(&entry.value)?));
        }
        Ok(values)
    }

    /// Lowers `MERGE`. The pattern is walked once, producing both the match
    /// plan (bound variables stay fixed, fresh ones scan) and the create
    /// template for the fresh parts; `ON CREATE`/`ON MATCH` lower as SET
    /// mutations applied to the rows the merge created or matched.
    fn merge_clause(&mut self, merge: &MergeClause) -> Result<()> {
        let part = &merge.pattern;
        if part.kind.binding.is_some() || part.kind.selector.is_some() {
            return Err(unsupported(
                part.span,
                "path bindings and selectors in MERGE",
            ));
        }
        let factors = &part.kind.path.kind.factors;
        let Some(first) = factors.first() else {
            return Err(semantic(part.span, "a MERGE pattern cannot be empty"));
        };
        let PathFactorKind::Node(first_node) = &first.kind else {
            return Err(unsupported(
                first.span,
                "a MERGE pattern not beginning with a node",
            ));
        };

        let mut plan = MatchPlan::default();
        let mut fresh = Vec::new();
        let mut creates = Vec::new();
        let mut relationships = Vec::new();
        let mut left =
            self.merge_node(&mut plan, &mut fresh, &mut creates, first_node, first.span)?;
        let mut remaining = &factors[1..];
        while !remaining.is_empty() {
            let [relationship, node, rest @ ..] = remaining else {
                return Err(semantic(
                    part.span,
                    "MERGE patterns must alternate relationships and nodes",
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
            if relationship_pattern.legacy_quantifier.is_some()
                || relationship_pattern.graph_quantifier.is_some()
            {
                return Err(unsupported(
                    relationship.span,
                    "variable-length relationships in MERGE",
                ));
            }
            if relationship_pattern.where_clause.is_some() {
                return Err(unsupported(relationship.span, "element WHERE in MERGE"));
            }
            let direction = match relationship_pattern.direction.kind {
                RelationshipDirection::Right => Direction::Right,
                RelationshipDirection::Left => Direction::Left,
                RelationshipDirection::Undirected => Direction::Undirected,
                _ => {
                    return Err(unsupported(
                        relationship_pattern.direction.span,
                        "relationship direction",
                    ));
                }
            };
            let (relationship_slot, relationship_fresh) = self.bind_match_entity(
                relationship_pattern
                    .variable
                    .as_ref()
                    .map(|name| &name.kind.text),
                EntityKind::Relationship,
                relationship.span,
            )?;
            let relationship_type = relationship_pattern
                .labels
                .as_ref()
                .map(|label| single_label(label, "multiple or boolean relationship types"))
                .transpose()?;
            if relationship_fresh {
                plan.operators.push(MatchOperator::ScanRelationship {
                    slot: relationship_slot,
                });
            }
            if let Some(relationship_type) = &relationship_type {
                plan.operators.push(MatchOperator::HasRelationshipType {
                    slot: relationship_slot,
                    relationship_type: relationship_type.clone(),
                });
            }
            self.merge_properties(
                &mut plan,
                relationship_slot,
                relationship_pattern.properties.as_deref(),
            )?;
            let right =
                self.merge_node(&mut plan, &mut fresh, &mut creates, node_pattern, node.span)?;
            plan.operators.push(MatchOperator::ExpandRelationship {
                relationship: relationship_slot,
                left,
                right,
                direction,
            });
            relationships.push(RelationshipUse::Single(relationship_slot));
            if relationship_fresh {
                let Some(relationship_type) = relationship_type else {
                    return Err(semantic(
                        relationship.span,
                        "a relationship created by MERGE requires exactly one type",
                    ));
                };
                // An undirected relationship is created left to right.
                let (from, to) = match direction {
                    Direction::Left => (right, left),
                    Direction::Right | Direction::Undirected => (left, right),
                };
                fresh.push((relationship_slot, EntityKind::Relationship));
                creates.push(Mutation::CreateRelationship {
                    slot: relationship_slot,
                    relationship_type,
                    from,
                    to,
                    properties: self
                        .stored_properties(relationship_pattern.properties.as_deref())?,
                });
            }
            left = right;
            remaining = rest;
        }
        if relationships.len() > 1 {
            plan.operators
                .push(MatchOperator::DistinctRelationships(relationships));
        }
        if fresh.is_empty() {
            return Err(semantic(
                part.span,
                "MERGE pattern must introduce at least one new node or relationship; \
                 every variable in it is already bound",
            ));
        }

        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        for action in &merge.actions {
            let outer = std::mem::take(&mut self.mutations);
            let lowered = self.set_clause(&action.set);
            let lowered = lowered.map(|()| std::mem::replace(&mut self.mutations, outer));
            let lowered = lowered?;
            match action.trigger.kind {
                MergeTrigger::Create => on_create.extend(lowered),
                MergeTrigger::Match => on_match.extend(lowered),
                _ => return Err(unsupported(action.span, "MERGE action trigger")),
            }
        }

        self.mutations.push(Mutation::Merge {
            pattern: plan,
            fresh,
            creates,
            on_create,
            on_match,
        });
        Ok(())
    }

    fn merge_node(
        &mut self,
        plan: &mut MatchPlan,
        fresh: &mut Vec<(SlotId, EntityKind)>,
        creates: &mut Vec<Mutation>,
        node: &NodePattern,
        span: Span,
    ) -> Result<SlotId> {
        if node.where_clause.is_some() {
            return Err(unsupported(span, "element WHERE in MERGE"));
        }
        let (slot, is_fresh) = self.bind_match_entity(
            node.variable.as_ref().map(|name| &name.kind.text),
            EntityKind::Node,
            span,
        )?;
        let labels = node
            .labels
            .as_ref()
            .map(|label| label_names(label, "boolean label expressions in MERGE"))
            .transpose()?
            .unwrap_or_default();
        if is_fresh {
            plan.operators.push(MatchOperator::ScanNode { slot });
        }
        for label in &labels {
            plan.operators.push(MatchOperator::HasLabel {
                slot,
                label: label.clone(),
            });
        }
        self.merge_properties(plan, slot, node.properties.as_deref())?;
        if is_fresh {
            fresh.push((slot, EntityKind::Node));
            creates.push(Mutation::CreateNode {
                slot,
                labels,
                properties: self.stored_properties(node.properties.as_deref())?,
            });
        } else if node.labels.is_some() || node.properties.is_some() {
            return Err(semantic(
                span,
                "a variable bound before MERGE must be a bare `(variable)` reference",
            ));
        }
        Ok(slot)
    }

    /// Pattern properties in MERGE: literal and parameter values match as
    /// property equalities; computed values match through a filter.
    fn merge_properties(
        &mut self,
        plan: &mut MatchPlan,
        slot: SlotId,
        properties: Option<&AstExpr>,
    ) -> Result<()> {
        let Some(properties) = properties else {
            return Ok(());
        };
        // MERGE could never match a null property it would then create, so
        // a literal null is rejected.
        if let ExprKind::Map(entries) = &properties.kind
            && let Some(entry) = entries.iter().find(|entry| {
                matches!(
                    input_value(&entry.value),
                    Ok(ValueExpr::Literal(Value::Null))
                )
            })
        {
            return Err(semantic(
                entry.value.span,
                format!(
                    "MERGE cannot use null for property `{}`",
                    entry.key.kind.text
                ),
            ));
        }
        match self.match_property_values(Some(properties)) {
            Ok(values) => {
                for (key, value) in values {
                    plan.operators
                        .push(MatchOperator::PropertyEquals { slot, key, value });
                }
                Ok(())
            }
            Err(Error::Unsupported(_)) => {
                let ExprKind::Map(entries) = &properties.kind else {
                    return Err(unsupported(
                        properties.span,
                        "parameter-map pattern matching",
                    ));
                };
                let mut keys = HashSet::with_capacity(entries.len());
                for entry in entries {
                    let key = entry.key.kind.text.clone();
                    if !keys.insert(key.clone()) {
                        return Err(semantic(
                            entry.key.span,
                            format!("property map contains duplicate key `{key}`"),
                        ));
                    }
                    match input_value(&entry.value) {
                        Ok(value) => {
                            plan.operators
                                .push(MatchOperator::PropertyEquals { slot, key, value })
                        }
                        Err(Error::Unsupported(_)) => {
                            let value = self.scalar_expression(&entry.value)?;
                            plan.operators.push(MatchOperator::Filter(Expr::Binary {
                                left: Box::new(Expr::Property(slot, key)),
                                operator: BinaryOp::Equal,
                                right: Box::new(value),
                            }));
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
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

        let labels = node
            .labels
            .as_ref()
            .map(|label| label_names(label, "boolean label expressions in CREATE"))
            .transpose()?
            .unwrap_or_default();
        let properties = self.stored_properties(node.properties.as_deref())?;
        let slot = self.allocate(EntityKind::Node);
        self.mutations.push(Mutation::CreateNode {
            slot,
            labels,
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
        if relationship_type.is_empty() {
            return Err(semantic(
                span,
                "a created relationship type cannot be empty",
            ));
        }
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
            properties: self.stored_properties(relationship.properties.as_deref())?,
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
                    let (target, key) = self.property_target(target, "SET")?;
                    self.mutations.push(Mutation::SetProperty {
                        target,
                        key,
                        value: self.stored_value(value, input_value, false)?,
                    });
                }
                SetItemKind::ReplaceProperties { target, value } => {
                    let slot = self.resolve_entity_name(target)?;
                    self.mutations.push(Mutation::ReplaceProperties {
                        slot,
                        properties: self.stored_value(value, property_map_input, true)?,
                    });
                }
                SetItemKind::MergeProperties { target, value } => {
                    let slot = self.resolve_entity_name(target)?;
                    self.mutations.push(Mutation::MergeProperties {
                        slot,
                        properties: self.stored_value(value, property_map_input, true)?,
                    });
                }
                SetItemKind::Labels { target, labels } => {
                    let slot = self.label_target(target)?;
                    self.mutations.push(Mutation::SetLabels {
                        slot,
                        labels: label_names(labels, "boolean label expressions in SET")?,
                    });
                }
                _ => return Err(unsupported(item.span, "SET item")),
            }
        }
        Ok(())
    }

    /// A `CREATE`/`SET` value: literal and parameter shapes stay static and
    /// are resolved once per statement; anything else is lowered as an
    /// expression evaluated per input row.
    fn stored_value(
        &self,
        expression: &AstExpr,
        literal: fn(&AstExpr) -> Result<ValueExpr>,
        map: bool,
    ) -> Result<StoredValue> {
        match literal(expression) {
            Ok(value) => Ok(StoredValue::Static(value)),
            Err(Error::Unsupported(_)) => self.computed_stored_value(expression, map),
            Err(error) => Err(error),
        }
    }

    fn stored_properties(&self, properties: Option<&AstExpr>) -> Result<StoredValue> {
        match properties_input(properties) {
            Ok(value) => Ok(StoredValue::Static(value)),
            Err(Error::Unsupported(_)) if properties.is_some() => {
                self.computed_stored_value(properties.expect("checked above"), true)
            }
            Err(error) => Err(error),
        }
    }

    /// `map` marks whole-map contexts (`CREATE` properties, `SET =`,
    /// `SET +=`), where a bare entity means "its properties".
    fn computed_stored_value(&self, expression: &AstExpr, map: bool) -> Result<StoredValue> {
        let lowered = match self.output_expression(expression)? {
            Expr::Entity(slot) if map => Expr::EntityProperties(slot),
            entity @ Expr::EntityValue { .. } if map => {
                entity_access(entity, EntityAccess::Properties)
            }
            lowered if contains_graph_value(&lowered) => {
                return Err(semantic(
                    expression.span,
                    "graph entities and paths cannot be stored as property values",
                ));
            }
            lowered => lowered,
        };
        if lowered.contains_aggregate() {
            return Err(semantic(
                expression.span,
                "aggregate calls are not allowed in a stored value",
            ));
        }
        Ok(StoredValue::Computed(lowered))
    }

    fn remove_clause(&mut self, remove_clause: &RemoveClause) -> Result<()> {
        for item in &remove_clause.items {
            match &item.kind {
                RemoveItemKind::Property(target) => {
                    let (target, key) = self.property_target(target, "REMOVE")?;
                    self.mutations
                        .push(Mutation::RemoveProperty { target, key });
                }
                RemoveItemKind::Labels { target, labels } => {
                    let slot = self.label_target(target)?;
                    self.mutations.push(Mutation::RemoveLabels {
                        slot,
                        labels: label_names(labels, "boolean label expressions in REMOVE")?,
                    });
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
            .map(|expression| self.entity_target(expression, "DELETE"))
            .collect::<Result<Vec<_>>>()?;
        self.mutations.push(Mutation::Delete { targets, detach });
        Ok(())
    }

    fn entity_target(&self, expression: &AstExpr, clause: &str) -> Result<EntityTarget> {
        let lowered = self.output_expression(expression)?;
        if lowered.contains_aggregate() {
            return Err(semantic(
                expression.span,
                format!("aggregate calls are not allowed in a {clause} target"),
            ));
        }
        if let Expr::Entity(slot) = lowered {
            return Ok(EntityTarget::Bound(slot, self.kind(slot)));
        }
        let valid = match self.nested_shape(&lowered) {
            NestedShape::Entity(_) | NestedShape::Mixed | NestedShape::Null => true,
            NestedShape::Json => matches!(
                self.check_kinds(&lowered, expression.span)?,
                None | Some(typecheck::Kind::Null)
            ),
            _ => false,
        };
        if !valid {
            return Err(semantic(
                expression.span,
                format!("{clause} target must evaluate to a node, relationship, or null"),
            ));
        }
        Ok(EntityTarget::Computed(lowered))
    }

    fn property_target(&self, target: &AstExpr, clause: &str) -> Result<(EntityTarget, String)> {
        let ExprKind::Property { expression, key } = &target.kind else {
            return Err(semantic(
                target.span,
                "property mutation requires `expression.key`",
            ));
        };
        Ok((
            self.entity_target(expression, clause)?,
            key.kind.text.clone(),
        ))
    }

    /// `(n)-[:KNOWS]->()` as a boolean: the pattern is lowered by a scoped
    /// copy of this lowerer, so it sees the bound variables but its anonymous
    /// entities stay local to the predicate.
    fn pattern_predicate(
        &self,
        pattern: &open_cypher::ast::Pattern,
        span: Span,
        aliases: &HashMap<String, Expr>,
    ) -> Result<Expr> {
        let mut scoped = Lowerer::new(self.source);
        scoped.slots = RefCell::new(self.slots.borrow().clone());
        scoped.scalar_slots = self.scalar_slots;
        scoped.traversal_slots = self.traversal_slots;
        scoped.path_slots = self.path_slots;
        scoped.symbols = self.symbols.clone();
        scoped.kinds = RefCell::new(self.kinds.borrow().clone());
        scoped.list_variables.set(self.list_variables.get());

        let before = self.slots.borrow().len();
        let mut bindings = Vec::new();
        let mut seen = HashSet::new();
        // Pattern variables obey the same expression scope as property access.
        // Bind computed members to temporary entity slots; scalar locals must
        // never fall back to a shadowed outer graph variable.
        for name in pattern
            .parts
            .iter()
            .flat_map(|part| &part.kind.path.kind.factors)
            .filter_map(|factor| match &factor.kind {
                PathFactorKind::Node(node) => node.variable.as_ref(),
                PathFactorKind::Relationship(relationship) => relationship.variable.as_ref(),
                _ => None,
            })
        {
            if !seen.insert(&name.kind.text) {
                continue;
            }
            let Some(value) = aliases.get(&name.kind.text) else {
                continue;
            };
            let slot = match value {
                Expr::Entity(slot) => *slot,
                Expr::EntityValue { kind, .. } => {
                    let slot = scoped.allocate(*kind);
                    bindings.push((slot, *kind, value.clone()));
                    slot
                }
                _ => {
                    return Err(semantic(
                        name.span,
                        "a pattern variable must be a graph entity",
                    ));
                }
            };
            scoped
                .symbols
                .insert(name.kind.text.clone(), Symbol::Entity(slot));
        }
        let bound_names = scoped.symbols.keys().cloned().collect::<HashSet<_>>();
        let local_start = scoped.slots.borrow().len();

        let mut plan = MatchPlan::default();
        let mut relationships = Vec::new();
        for part in &pattern.parts {
            if part.kind.selector.is_some() {
                return Err(unsupported(
                    part.span,
                    "path selectors in a pattern predicate",
                ));
            }
            if let Some(binding) = &part.kind.binding {
                return Err(semantic(
                    binding.span,
                    "a pattern predicate cannot bind a path variable",
                ));
            }
            let factors = &part.kind.path.kind.factors;
            if factors.len() < 2 {
                return Err(semantic(
                    part.span,
                    "a pattern predicate must contain at least one relationship",
                ));
            }
            if factors
                .iter()
                .any(|factor| matches!(factor.kind, PathFactorKind::LegacyShortest { .. }))
            {
                return Err(unsupported(
                    part.span,
                    "shortest paths in a pattern predicate",
                ));
            }
            let matched = scoped.match_path(&mut plan, factors, part.span)?;
            relationships.extend(matched.segments.iter().map(|segment| segment.relationship));
        }
        if relationships.len() > 1 {
            plan.operators
                .push(MatchOperator::DistinctRelationships(relationships));
        }
        if plan.has_variable_length() {
            return Err(unsupported(span, "variable-length pattern predicates"));
        }
        if let Some(name) = scoped
            .symbols
            .keys()
            .find(|name| !bound_names.contains(*name))
        {
            return Err(semantic(
                span,
                format!("pattern predicate introduces the unbound variable `{name}`"),
            ));
        }

        let all = scoped.slots.into_inner();
        let local = all[local_start..]
            .iter()
            .enumerate()
            .map(|(offset, kind)| (SlotId(local_start + offset), *kind))
            .collect::<Vec<_>>();
        self.slots.borrow_mut().extend_from_slice(&all[before..]);
        self.list_variables.set(scoped.list_variables.get());
        Ok(Expr::Exists {
            pattern: Box::new(plan),
            local,
            bindings: bindings.into_boxed_slice(),
        })
    }

    fn expression_entity_kind(&self, expression: &Expr) -> Option<EntityKind> {
        match expression {
            Expr::Entity(slot) => Some(self.kind(*slot)),
            Expr::EntityValue { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    fn identity_expression(&self, expression: Expr) -> Expr {
        match expression {
            Expr::Entity(slot) => Expr::EntityIdentity(slot),
            entity @ Expr::EntityValue { kind, .. } => {
                entity_access(entity, EntityAccess::Identity(kind))
            }
            expression => expression,
        }
    }

    /// The node a `SET`/`REMOVE` label item targets.
    fn label_target(&self, name: &open_cypher::ast::Name) -> Result<SlotId> {
        let slot = self.resolve_entity_name(name)?;
        match self.kind(slot) {
            EntityKind::Node => Ok(slot),
            EntityKind::Relationship => Err(semantic(
                name.span,
                format!(
                    "`{}` is a relationship and cannot have labels",
                    name.kind.text
                ),
            )),
        }
    }

    /// Node labels in a pattern: a plain conjunction becomes one `HasLabel`
    /// operator per label, any other label expression becomes a filter.
    fn pattern_labels(
        &self,
        plan: &mut MatchPlan,
        slot: SlotId,
        label: &LabelExpression,
    ) -> Result<()> {
        match label_names(label, "label expression") {
            Ok(names) => {
                for label in names {
                    plan.operators.push(MatchOperator::HasLabel { slot, label });
                }
                Ok(())
            }
            Err(_) => {
                let predicate = self.label_predicate(slot, label)?;
                plan.operators.push(MatchOperator::Filter(predicate));
                Ok(())
            }
        }
    }

    /// A label expression as a boolean over the node's labels.
    fn label_predicate(&self, slot: SlotId, label: &LabelExpression) -> Result<Expr> {
        self.label_expression(label, &|name| Expr::HasLabel(slot, name))
    }

    fn label_expression(
        &self,
        label: &LabelExpression,
        leaf: &dyn Fn(String) -> Expr,
    ) -> Result<Expr> {
        let join = |parts: &[LabelExpression], operator: BinaryOp| -> Result<Expr> {
            let mut lowered = parts
                .iter()
                .map(|part| self.label_expression(part, leaf))
                .collect::<Result<Vec<_>>>()?;
            let first = lowered.remove(0);
            Ok(lowered.into_iter().fold(first, |left, right| Expr::Binary {
                left: Box::new(left),
                operator,
                right: Box::new(right),
            }))
        };
        match &label.kind {
            LabelExpressionKind::Name(name) => Ok(leaf(name.kind.text.clone())),
            LabelExpressionKind::And(parts) if !parts.is_empty() => join(parts, BinaryOp::And),
            LabelExpressionKind::Or(parts) if !parts.is_empty() => join(parts, BinaryOp::Or),
            LabelExpressionKind::Not(inner) => Ok(Expr::Unary {
                operator: UnaryOp::Not,
                operand: Box::new(self.label_expression(inner, leaf)?),
            }),
            LabelExpressionKind::Parenthesized(inner) => self.label_expression(inner, leaf),
            LabelExpressionKind::Wildcard => Err(unsupported(label.span, "label wildcard `%`")),
            _ => Err(unsupported(label.span, "label expression")),
        }
    }

    fn resolve_entity_name(&self, name: &open_cypher::ast::Name) -> Result<SlotId> {
        match self.resolve_symbol(name)? {
            Symbol::Entity(slot) => Ok(slot),
            Symbol::Scalar(_) => Err(semantic(
                name.span,
                format!("scalar variable `{}` is not a graph entity", name.kind.text),
            )),
            Symbol::Path(_) => Err(semantic(
                name.span,
                format!("path variable `{}` is not a graph entity", name.kind.text),
            )),
            Symbol::PathCollection { .. } => Err(semantic(
                name.span,
                format!(
                    "path collection variable `{}` is not a graph entity",
                    name.kind.text
                ),
            )),
        }
    }

    fn unwind_clause(&mut self, unwind_clause: &UnwindClause) -> Result<UnwindStage> {
        if self.symbols.contains_key(&unwind_clause.variable.kind.text) {
            return Err(semantic(
                unwind_clause.variable.span,
                format!(
                    "UNWIND variable `{}` is already bound",
                    unwind_clause.variable.kind.text
                ),
            ));
        }
        let mut expression = self.output_expression(&unwind_clause.expression)?;
        if expression.contains_aggregate() {
            return Err(semantic(
                unwind_clause.expression.span,
                "aggregate functions in UNWIND must cross a WITH boundary",
            ));
        }
        let name = unwind_clause.variable.kind.text.clone();
        if let Expr::Nested { shape, .. } = &expression {
            if **shape == NestedShape::Path {
                return Err(semantic(
                    unwind_clause.expression.span,
                    "UNWIND requires a list",
                ));
            }
            let element = shape.element();
            if let NestedShape::Entity(kind) = element {
                expression = Expr::EntityList {
                    kind,
                    expression: Box::new(nested::call(NestedOp::EntityList, vec![expression])),
                };
            } else {
                let slot = self.allocate_scalar();
                self.kinds
                    .borrow_mut()
                    .scalars
                    .insert(slot, typecheck::Kind::Nested(Box::new(element)));
                self.symbols.insert(name.clone(), Symbol::Scalar(slot));
                return Ok(UnwindStage {
                    name,
                    binding: UnwindBinding::Scalar(slot),
                    expression: nested::call(NestedOp::Elements, vec![expression]),
                });
            }
        }
        let entity_kind = match &expression {
            Expr::EntityList { kind, .. } => Some(*kind),
            Expr::PathFunction { kind, .. } => match kind {
                PathFunctionKind::Nodes => Some(EntityKind::Node),
                PathFunctionKind::Relationships => Some(EntityKind::Relationship),
                PathFunctionKind::Length => None,
            },
            _ => None,
        };
        let binding = if let Some(kind) = entity_kind {
            let slot = self.allocate(kind);
            self.symbols.insert(name.clone(), Symbol::Entity(slot));
            UnwindBinding::Entity(slot, kind)
        } else {
            if matches!(expression, Expr::EntityValue { .. }) {
                return Err(semantic(
                    unwind_clause.expression.span,
                    "UNWIND requires a list",
                ));
            }
            let expression =
                require_scalar_context(expression.clone(), unwind_clause.expression.span)?;
            if matches!(expression, Expr::Entity(_) | Expr::Path(_)) {
                return Err(semantic(
                    unwind_clause.expression.span,
                    "UNWIND requires a list",
                ));
            }
            let slot = self.allocate_scalar();
            if let Some(typecheck::Kind::List(Some(element))) =
                self.check_kinds(&expression, unwind_clause.expression.span)?
            {
                self.kinds.borrow_mut().scalars.insert(slot, *element);
            }
            self.symbols.insert(name.clone(), Symbol::Scalar(slot));
            UnwindBinding::Scalar(slot)
        };
        Ok(UnwindStage {
            name,
            binding,
            expression,
        })
    }

    fn with_clause(&mut self, with_clause: &WithClause) -> Result<WithStage> {
        let projection = &with_clause.projection;
        let distinct = match projection.quantifier.as_ref().map(|value| value.kind) {
            None | Some(SetQuantifier::All) => false,
            Some(SetQuantifier::Distinct) => true,
            Some(_) => return Err(unsupported(projection.span, "WITH quantifier")),
        };

        let incoming_symbols = self.symbols.clone();
        let mut bindings = Vec::with_capacity(projection.items.len());
        let mut projected_symbols = HashMap::new();
        let mut aliases = HashMap::new();

        let mut items = Vec::with_capacity(projection.items.len());
        let mut saw_wildcard = false;
        for item in &projection.items {
            match &item.kind {
                // `WITH *` carries every variable in scope under its own name;
                // over an empty scope it keeps the rows and binds nothing.
                ProjectionItemKind::Wildcard => {
                    saw_wildcard = true;
                    for (name, symbol) in self.wildcard_items() {
                        items.push((item.span, name, self.symbol_expression(symbol)));
                    }
                }
                ProjectionItemKind::Expression { expression, alias } => {
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
                    let expression = self.output_expression(expression)?;
                    items.push((item.span, name, expression));
                }
                _ => return Err(unsupported(item.span, "this WITH item")),
            }
        }
        let aggregating = items
            .iter()
            .any(|(_, _, expression)| expression.contains_aggregate());
        for (span, name, expression) in items {
            let expression = if aggregating {
                nested::materialize_path_value(expression)
            } else {
                expression
            };
            if projected_symbols.contains_key(&name) {
                return Err(semantic(
                    span,
                    format!("WITH projects duplicate variable `{name}`"),
                ));
            }
            match expression {
                Expr::EntityValue { kind, .. } => {
                    let slot = self.allocate(kind);
                    bindings.push(WithBinding::Entity {
                        name: name.clone(),
                        slot,
                        kind,
                        expression: Some(expression.clone()),
                    });
                    projected_symbols.insert(name.clone(), Symbol::Entity(slot));
                    aliases.insert(
                        name,
                        if distinct {
                            Expr::Entity(slot)
                        } else {
                            expression
                        },
                    );
                }
                Expr::Entity(slot) => {
                    let kind = self.kind(slot);
                    bindings.push(WithBinding::Entity {
                        name: name.clone(),
                        slot,
                        kind,
                        expression: None,
                    });
                    projected_symbols.insert(name.clone(), Symbol::Entity(slot));
                    aliases.insert(name, Expr::Entity(slot));
                }
                Expr::Path(path) => {
                    bindings.push(WithBinding::Path {
                        name: name.clone(),
                        path,
                    });
                    projected_symbols.insert(name.clone(), Symbol::Path(path));
                    aliases.insert(name, Expr::Path(path));
                }
                Expr::PathFunction { kind, path }
                    if let Some(collection_kind) = path_collection_kind(kind) =>
                {
                    bindings.push(WithBinding::PathCollection {
                        name: name.clone(),
                        path,
                        kind: collection_kind,
                    });
                    projected_symbols.insert(
                        name.clone(),
                        Symbol::PathCollection {
                            path,
                            kind: collection_kind,
                        },
                    );
                    aliases.insert(name, Expr::PathFunction { kind, path });
                }
                expression => {
                    let slot = self.allocate_scalar();
                    // Already checked when lowered; this only records it.
                    let kind = typecheck::check(&expression, &mut self.kinds.borrow_mut());
                    if let Ok(Some(kind)) = kind {
                        self.kinds.borrow_mut().scalars.insert(slot, kind);
                    }
                    bindings.push(WithBinding::Scalar {
                        name: name.clone(),
                        slot,
                        expression: expression.clone(),
                    });
                    projected_symbols.insert(name.clone(), Symbol::Scalar(slot));
                    aliases.insert(
                        name,
                        if distinct {
                            self.symbol_expression(Symbol::Scalar(slot))
                        } else {
                            expression
                        },
                    );
                }
            }
        }

        if bindings.is_empty() && !saw_wildcard {
            return Err(semantic(
                projection.span,
                "WITH must contain at least one item",
            ));
        }

        let binding_expressions = bindings
            .iter()
            .map(|binding| match binding {
                WithBinding::Entity {
                    slot, expression, ..
                } => expression.clone().unwrap_or(Expr::Entity(*slot)),
                WithBinding::Path { path, .. } => Expr::Path(*path),
                WithBinding::PathCollection { path, kind, .. } => Expr::PathFunction {
                    kind: path_function_kind(*kind),
                    path: *path,
                },
                WithBinding::Scalar { expression, .. } => expression.clone(),
            })
            .collect::<Vec<_>>();
        let mut grouping = implicit_grouping(&binding_expressions);
        if aggregating {
            validate_aggregate_scope(&binding_expressions, &grouping, projection.span)?;
        }
        let mut projected_values = binding_expressions.clone();
        if distinct {
            let projected_aliases = bindings
                .iter()
                .filter_map(|binding| match binding {
                    WithBinding::Scalar { slot, .. } => Some(Expr::Scalar(*slot)),
                    WithBinding::Entity { slot, .. } => Some(Expr::Entity(*slot)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            projected_values.extend(projected_aliases.clone());
            grouping.extend(projected_aliases);
        }

        // Cypher permits WITH's WHERE and ORDER BY to refer to projected
        // aliases. WHERE may also inspect incoming variables that are not
        // carried beyond a non-aggregating boundary. Aggregating WITH clauses
        // expose only grouping values and aggregate aliases to their filter.
        self.symbols = if aggregating {
            projected_symbols.clone()
        } else {
            let mut visible = incoming_symbols.clone();
            visible.extend(projected_symbols.clone());
            visible
        };
        let filter = with_clause
            .where_clause
            .as_ref()
            .map(|expression| {
                let filter = self.scalar_expression_with_aliases(expression, &aliases)?;
                self.check_predicate(&filter, expression.span)?;
                Ok::<_, Error>(filter)
            })
            .transpose()?;
        let order_by = self.order_by(&projection.order_by, &aliases, false, &[])?;
        if distinct
            && order_by.iter().any(|item| {
                !item.expression.uses_only_grouped_values(&projected_values)
                    || has_unprojected_aggregate(&item.expression, &projected_values)
            })
        {
            return Err(semantic(
                projection.span,
                "ORDER BY with WITH DISTINCT may only use projected values",
            ));
        }
        if !aggregating
            && (filter.as_ref().is_some_and(Expr::contains_aggregate)
                || order_by
                    .iter()
                    .any(|item| item.expression.contains_aggregate()))
        {
            return Err(semantic(
                projection.span,
                "aggregate functions in WITH WHERE or ORDER BY require an aggregating WITH projection",
            ));
        }
        if aggregating {
            if let Some(filter) = &filter {
                if distinct && has_unprojected_aggregate(filter, &projected_values) {
                    return Err(semantic(
                        projection.span,
                        "WITH DISTINCT filters may only use projected aggregate values",
                    ));
                }
                validate_aggregate_scope(std::slice::from_ref(filter), &grouping, projection.span)?;
            }
            validate_aggregate_scope(
                &order_by
                    .iter()
                    .map(|item| item.expression.clone())
                    .collect::<Vec<_>>(),
                &grouping,
                projection.span,
            )?;
        }
        let (offset, limit) = projection_pagination(projection, "WITH")?;

        self.symbols = projected_symbols;
        Ok(WithStage {
            bindings,
            distinct,
            filter,
            order_by,
            offset,
            limit,
        })
    }

    fn projection(&self, projection: &ProjectionClause) -> Result<Projection> {
        let distinct = match projection.quantifier.as_ref().map(|value| value.kind) {
            None | Some(SetQuantifier::All) => false,
            Some(SetQuantifier::Distinct) => true,
            Some(_) => return Err(unsupported(projection.span, "RETURN quantifier")),
        };

        let mut columns = Vec::with_capacity(projection.items.len());
        let mut expressions = Vec::with_capacity(projection.items.len());
        let mut aliases = HashMap::new();
        let mut column_names = HashSet::with_capacity(projection.items.len());
        let mut items = Vec::with_capacity(projection.items.len());
        for item in &projection.items {
            match &item.kind {
                // `RETURN *` projects every variable in scope as a column of
                // its own name.
                ProjectionItemKind::Wildcard => {
                    if self.symbols.is_empty() {
                        return Err(semantic(
                            item.span,
                            "RETURN * requires at least one variable in scope",
                        ));
                    }
                    for (name, symbol) in self.wildcard_items() {
                        items.push((
                            item.span,
                            name.clone(),
                            self.symbol_expression(symbol),
                            Some(name),
                        ));
                    }
                }
                ProjectionItemKind::Expression { expression, alias } => {
                    let lowered = self.output_expression(expression)?;
                    let column = column_name(self.source, expression, alias.as_ref());
                    let alias = alias.as_ref().map(|alias| alias.kind.text.clone());
                    items.push((item.span, column, lowered, alias));
                }
                _ => return Err(unsupported(item.span, "this RETURN item")),
            }
        }
        let aggregating = items
            .iter()
            .any(|(_, _, expression, _)| expression.contains_aggregate());
        for (span, column, lowered, alias) in items {
            let lowered = if aggregating {
                nested::materialize_path_value(lowered)
            } else {
                lowered
            };
            if !column_names.insert(column.clone()) {
                return Err(semantic(
                    span,
                    format!("RETURN projects duplicate column `{column}`"),
                ));
            }
            columns.push(column);
            expressions.push(lowered.clone());
            if let Some(alias) = alias {
                aliases.insert(alias, lowered);
            }
        }
        if expressions.is_empty() {
            return Err(semantic(
                projection.span,
                "RETURN must contain at least one item",
            ));
        }

        let grouping = implicit_grouping(&expressions);
        let order_by = self.order_by(&projection.order_by, &aliases, distinct, &expressions)?;
        if !aggregating
            && order_by
                .iter()
                .any(|item| item.expression.contains_aggregate())
        {
            return Err(semantic(
                projection.span,
                "aggregate functions in ORDER BY require an aggregating RETURN projection",
            ));
        }
        if aggregating {
            validate_aggregate_scope(&expressions, &grouping, projection.span)?;
            validate_aggregate_scope(
                &order_by
                    .iter()
                    .map(|item| item.expression.clone())
                    .collect::<Vec<_>>(),
                &grouping,
                projection.span,
            )?;
        }
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
        self.output_expression_with_aliases(expression, &HashMap::new())
    }

    fn output_expression_with_aliases(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
    ) -> Result<Expr> {
        let lowered = self.output_expression_with_aliases_at_depth(expression, aliases, 0)?;
        self.check_kinds(&lowered, expression.span)?;
        Ok(lowered)
    }

    fn output_expression_with_aliases_at_depth(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        if expression_depth > MAX_EXPRESSION_NESTING {
            return Err(semantic(
                expression.span,
                format!(
                    "output expression nesting exceeds the supported maximum of \
                     {MAX_EXPRESSION_NESTING}"
                ),
            ));
        }

        match &expression.kind {
            ExprKind::Binary { operator, .. } if operator.kind == BinaryOperator::Add => {
                self.add_expression(expression, aliases, expression_depth)
            }
            ExprKind::Case(_) => self.case_expression(expression, aliases, expression_depth),
            ExprKind::ListComprehension(comprehension) => {
                self.list_comprehension(comprehension, aliases, expression_depth)
            }
            ExprKind::List(_) | ExprKind::Map(_) => {
                self.collection_expression(expression, aliases, expression_depth)
            }
            ExprKind::DynamicProperty { .. } | ExprKind::Index { .. } | ExprKind::Slice { .. } => {
                self.subscript_expression(expression, aliases, expression_depth)
            }
            ExprKind::Property {
                expression: base,
                key,
            } => self.property_expression(base, &key.kind.text, aliases, expression_depth),
            ExprKind::Variable(variable) => {
                if let Some(expression) = aliases.get(&variable.kind.text) {
                    return Ok(expression.clone());
                }
                match self.resolve_symbol(variable)? {
                    Symbol::Entity(slot) => Ok(Expr::Entity(slot)),
                    Symbol::Scalar(slot) => Ok(self.symbol_expression(Symbol::Scalar(slot))),
                    Symbol::Path(path) => Ok(Expr::Path(path)),
                    Symbol::PathCollection { path, kind } => Ok(Expr::PathFunction {
                        kind: path_function_kind(kind),
                        path,
                    }),
                }
            }
            ExprKind::Function(function) => {
                self.function_expression(function, expression.span, aliases)
            }
            ExprKind::Parenthesized(inner) => {
                self.output_expression_with_aliases_at_depth(inner, aliases, expression_depth + 1)
            }
            _ => {
                self.scalar_expression_with_aliases_at_depth(expression, aliases, expression_depth)
            }
        }
    }

    /// Select values in their native transport. Graph results use private
    /// tags; agreeing branches retain their shape for later typed access.
    fn case_expression(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        let ExprKind::Case(case) = &expression.kind else {
            unreachable!("a CASE expression was matched");
        };
        let lower = |value: &AstExpr| {
            self.output_expression_with_aliases_at_depth(value, aliases, expression_depth + 1)
        };
        let mut operand = case.operand.as_deref().map(lower).transpose()?;
        let mut alternatives = Vec::with_capacity(case.alternatives.len());
        for alternative in &case.alternatives {
            let conditions = alternative
                .when
                .iter()
                .map(|condition| {
                    if operand.is_some() {
                        lower(condition)
                    } else {
                        self.scalar_expression_with_aliases_at_depth(
                            condition,
                            aliases,
                            expression_depth + 1,
                        )
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            alternatives.push(CaseAlternative {
                conditions,
                result: lower(&alternative.then)?,
            });
        }
        let mut otherwise = case.else_expression.as_deref().map(lower).transpose()?;
        // Mark simple CASE selectors as tagged whenever any selector can
        // contain a graph value. SQL compares these with graph-aware equality.
        if operand.is_some()
            && operand
                .iter()
                .chain(alternatives.iter().flat_map(|a| &a.conditions))
                .any(|value| self.nested_shape(value).contains_graph())
        {
            let pack = |value| nested::restore(NestedShape::Mixed, self.pack_nested(value));
            operand = operand.map(pack);
            for alternative in &mut alternatives {
                alternative.conditions = std::mem::take(&mut alternative.conditions)
                    .into_iter()
                    .map(pack)
                    .collect();
            }
        }
        let shapes = alternatives
            .iter()
            .map(|a| self.nested_shape(&a.result))
            .chain(otherwise.iter().map(|value| self.nested_shape(value)))
            .collect::<Vec<_>>();
        let graph = shapes.iter().any(NestedShape::contains_graph);
        if graph {
            for alternative in &mut alternatives {
                alternative.result = self.pack_nested(alternative.result.clone());
            }
            otherwise = otherwise.map(|value| self.pack_nested(value));
        }
        let result = Expr::Case {
            operand: operand.map(Box::new),
            alternatives,
            otherwise: otherwise.map(Box::new),
        };
        Ok(if graph {
            nested::restore(NestedShape::merge(shapes.iter()), result)
        } else {
            result
        })
    }

    fn collection_expression(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        match input_value(expression) {
            Ok(value) => return Ok(Expr::Value(value)),
            Err(Error::Unsupported(_)) => {}
            Err(error) => return Err(error),
        }
        enum Work<'a> {
            Value(&'a AstExpr, usize),
            List(usize),
            Map(Vec<String>),
        }
        let mut work = vec![Work::Value(expression, expression_depth)];
        let mut values = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Value(value, depth) => {
                    if depth > MAX_EXPRESSION_NESTING
                        || depth - expression_depth > MAX_USER_VALUE_NESTING
                    {
                        return Err(semantic(
                            value.span,
                            "collection nesting exceeds the supported maximum",
                        ));
                    }
                    match &value.kind {
                        ExprKind::List(items) => {
                            work.push(Work::List(items.len()));
                            work.extend(
                                items.iter().rev().map(|item| Work::Value(item, depth + 1)),
                            );
                        }
                        ExprKind::Map(entries) => {
                            let mut names = HashSet::new();
                            for entry in entries {
                                if !names.insert(&entry.key.kind.text) {
                                    return Err(semantic(value.span, "duplicate map key"));
                                }
                            }
                            work.push(Work::Map(
                                entries
                                    .iter()
                                    .map(|entry| entry.key.kind.text.clone())
                                    .collect(),
                            ));
                            work.extend(
                                entries
                                    .iter()
                                    .rev()
                                    .map(|entry| Work::Value(&entry.value, depth + 1)),
                            );
                        }
                        _ => values.push(
                            self.output_expression_with_aliases_at_depth(value, aliases, depth)?,
                        ),
                    }
                }
                Work::List(count) => {
                    let items = values.split_off(values.len() - count);
                    values.push(self.nested_collection(Expr::List(items)));
                }
                Work::Map(keys) => {
                    let items = values.split_off(values.len() - keys.len());
                    values.push(
                        self.nested_collection(Expr::Map(keys.into_iter().zip(items).collect())),
                    );
                }
            }
        }
        Ok(values.pop().expect("collection expression result"))
    }

    fn nested_shape(&self, expression: &Expr) -> NestedShape {
        match expression {
            Expr::Value(ValueExpr::Literal(Value::Null)) => NestedShape::Null,
            Expr::Nested { shape, .. } => *shape.clone(),
            Expr::Entity(slot) => NestedShape::Entity(self.kind(*slot)),
            Expr::EntityValue { kind, .. } => NestedShape::Entity(*kind),
            Expr::Path(_) => NestedShape::Path,
            value => match entity_list_kind(value) {
                Some(kind) => NestedShape::List(Box::new(NestedShape::Entity(kind))),
                None => NestedShape::Json,
            },
        }
    }

    fn pack_nested(&self, expression: Expr) -> Expr {
        let op = match self.nested_shape(&expression) {
            _ if matches!(expression, Expr::Nested { .. }) => return expression,
            NestedShape::Entity(EntityKind::Node) => NestedOp::Node,
            NestedShape::Entity(EntityKind::Relationship) => NestedOp::Relationship,
            NestedShape::Path => NestedOp::Path,
            NestedShape::List(element) if *element == NestedShape::Entity(EntityKind::Node) => {
                NestedOp::Nodes
            }
            NestedShape::List(_) => NestedOp::Relationships,
            _ => NestedOp::Json,
        };
        nested::call(op, vec![expression])
    }

    fn nested_collection(&self, expression: Expr) -> Expr {
        if !expression
            .children()
            .iter()
            .any(|value| self.nested_shape(value).contains_graph())
        {
            return expression;
        }
        let (shape, op, expression) = match expression {
            Expr::List(items) => {
                let shape = NestedShape::Tuple(
                    items.iter().map(|value| self.nested_shape(value)).collect(),
                );
                (
                    shape,
                    NestedOp::List,
                    Expr::List(
                        items
                            .into_iter()
                            .map(|value| self.pack_nested(value))
                            .collect(),
                    ),
                )
            }
            Expr::Map(entries) => {
                let shape = NestedShape::Map(
                    entries
                        .iter()
                        .map(|(key, value)| (key.clone(), self.nested_shape(value)))
                        .collect(),
                );
                (
                    shape,
                    NestedOp::Map,
                    Expr::Map(
                        entries
                            .into_iter()
                            .map(|(key, value)| (key, self.pack_nested(value)))
                            .collect(),
                    ),
                )
            }
            _ => unreachable!(),
        };
        nested::restore(shape, nested::call(op, vec![expression]))
    }

    fn list_member(
        &self,
        list: Expr,
        variable: ListVariableId,
    ) -> (Expr, Expr, Option<NestedShape>) {
        let member = Expr::ListElement(variable);
        if let Expr::Nested { shape, .. } = &list {
            let shape = shape.element();
            return (
                nested::call(NestedOp::Elements, vec![list_function_input(list)]),
                nested::restore(shape.clone(), member),
                Some(shape),
            );
        }
        let member = match entity_list_kind(&list) {
            Some(kind) => Expr::EntityValue {
                kind,
                expression: Box::new(member),
            },
            None => member,
        };
        (list, member, None)
    }

    /// The input uses the outer scope; the element shadows its name only in
    /// the predicate and projection. A graph-valued projection retains its
    /// kind so downstream clauses never mistake entity records for maps.
    fn list_comprehension(
        &self,
        comprehension: &open_cypher::ast::ListComprehension,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        let list = self.output_expression_with_aliases_at_depth(
            &comprehension.list,
            aliases,
            expression_depth + 1,
        )?;
        if is_non_list_graph_value(&list) {
            return Err(semantic(
                comprehension.list.span,
                "a list comprehension requires a list",
            ));
        }
        let input_kind = entity_list_kind(&list);
        let variable = self.allocate_list_variable();
        let (list, element, nested_input) = self.list_member(list, variable);
        let mut inner = aliases.clone();
        inner.insert(comprehension.variable.kind.text.clone(), element);
        let lower_inner = |part: &Option<Box<AstExpr>>,
                           output: bool|
         -> Result<Option<Box<Expr>>> {
            let Some(part) = part else { return Ok(None) };
            let lowered = if output {
                self.output_expression_with_aliases_at_depth(part, &inner, expression_depth + 1)?
            } else {
                self.scalar_expression_with_aliases_at_depth(part, &inner, expression_depth + 1)?
            };
            if lowered.contains_aggregate() {
                return Err(semantic(
                    part.span,
                    "aggregate calls are not allowed inside a list comprehension",
                ));
            }
            Ok(Some(Box::new(lowered)))
        };
        let predicate = lower_inner(&comprehension.predicate, false)?;
        let projection = lower_inner(&comprehension.projection, true)?;
        let projected_shape = projection.as_deref().map(|value| self.nested_shape(value));
        if (nested_input.is_some() && projection.is_none())
            || projected_shape.as_ref().is_some_and(|shape| {
                shape.contains_graph() && !matches!(shape, NestedShape::Entity(_))
            })
        {
            let shape = NestedShape::List(Box::new(projected_shape.or(nested_input).unwrap()));
            let expression = Expr::ListComprehension {
                variable,
                list: Box::new(list),
                predicate,
                projection: projection.map(|value| Box::new(self.pack_nested(*value))),
            };
            return Ok(nested::restore(
                shape,
                nested::call(NestedOp::List, vec![expression]),
            ));
        }
        let output_kind = match &projection {
            Some(projection) => self.expression_entity_kind(projection),
            None => input_kind,
        };
        let expression = Expr::ListComprehension {
            variable,
            list: Box::new(list),
            predicate,
            projection,
        };
        Ok(match output_kind {
            Some(kind) => Expr::EntityList {
                kind,
                expression: Box::new(expression),
            },
            None => expression,
        })
    }

    fn subscript_expression(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        match &expression.kind {
            ExprKind::DynamicProperty {
                expression: base,
                key,
            }
            | ExprKind::Index {
                expression: base,
                index: key,
            } => {
                let key = self.scalar_expression_with_aliases_at_depth(
                    key,
                    aliases,
                    expression_depth + 1,
                )?;
                let base = self.output_expression_with_aliases_at_depth(
                    base,
                    aliases,
                    expression_depth + 1,
                )?;
                match base {
                    Expr::Entity(slot) => Ok(Expr::DynamicProperty(slot, Box::new(key))),
                    entity @ Expr::EntityValue { .. } => Ok(entity_access(
                        entity,
                        EntityAccess::DynamicProperty(Box::new(key)),
                    )),
                    base if is_path_value(&base) => {
                        Err(semantic(expression.span, "paths cannot be indexed"))
                    }
                    base => Ok(index_expression(base, key)),
                }
            }
            ExprKind::Slice {
                expression: base,
                lower,
                upper,
            } => {
                let base = self.output_expression_with_aliases_at_depth(
                    base,
                    aliases,
                    expression_depth + 1,
                )?;
                let bound = |bound: &Option<Box<AstExpr>>| -> Result<Option<Box<Expr>>> {
                    bound
                        .as_deref()
                        .map(|bound| {
                            self.scalar_expression_with_aliases_at_depth(
                                bound,
                                aliases,
                                expression_depth + 1,
                            )
                            .map(Box::new)
                        })
                        .transpose()
                };
                if is_non_list_graph_value(&base) {
                    return Err(semantic(expression.span, "slicing requires a list"));
                }
                Ok(slice_expression(base, bound(lower)?, bound(upper)?))
            }
            _ => unreachable!("only subscripts use this lowering path"),
        }
    }

    /// Rejects a misuse of a value whose kind the query text fixes, such as
    /// `NOT 1`, and returns the expression's kind when it is known.
    fn check_kinds(&self, expression: &Expr, span: Span) -> Result<Option<typecheck::Kind>> {
        typecheck::check(expression, &mut self.kinds.borrow_mut())
            .map_err(|message| semantic(span, message))
    }

    fn check_predicate(&self, expression: &Expr, span: Span) -> Result<()> {
        typecheck::check_predicate(expression, &mut self.kinds.borrow_mut())
            .map_err(|message| semantic(span, message))
    }

    fn scalar_expression(&self, expression: &AstExpr) -> Result<Expr> {
        self.scalar_expression_with_aliases(expression, &HashMap::new())
    }

    fn match_predicate(&self, expression: &AstExpr) -> Result<Expr> {
        let span = expression.span;
        let expression = self.scalar_expression(expression)?;
        self.check_predicate(&expression, span)?;
        if expression.contains_aggregate() {
            return Err(semantic(
                span,
                "aggregate functions are not allowed in MATCH predicates",
            ));
        }
        Ok(expression)
    }

    fn scalar_expression_with_aliases(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
    ) -> Result<Expr> {
        let lowered = self.scalar_expression_with_aliases_at_depth(expression, aliases, 0)?;
        self.check_kinds(&lowered, expression.span)?;
        Ok(lowered)
    }

    fn scalar_expression_with_aliases_at_depth(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        if expression_depth > MAX_EXPRESSION_NESTING {
            return Err(semantic(
                expression.span,
                format!(
                    "scalar expression nesting exceeds the supported maximum of \
                     {MAX_EXPRESSION_NESTING}"
                ),
            ));
        }

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
            ExprKind::Literal(_) | ExprKind::Parameter(_) => {
                input_value(expression).map(Expr::Value)
            }
            ExprKind::List(_) | ExprKind::Map(_) => {
                let value = self.collection_expression(expression, aliases, expression_depth)?;
                require_scalar_context(value, expression.span)
            }
            ExprKind::Property {
                expression: base,
                key,
            } => {
                let value =
                    self.property_expression(base, &key.kind.text, aliases, expression_depth)?;
                require_scalar_context(value, expression.span)
            }
            ExprKind::Binary { .. } => {
                self.scalar_binary_expression(expression, aliases, expression_depth)
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
                    operand: Box::new(self.scalar_expression_with_aliases_at_depth(
                        operand,
                        aliases,
                        expression_depth + 1,
                    )?),
                })
            }
            ExprKind::Is {
                expression,
                negated,
                predicate: IsPredicate::Null,
            } => {
                // A node or relationship is null exactly when its id is.
                let operand = self.output_expression_with_aliases_at_depth(
                    expression,
                    aliases,
                    expression_depth + 1,
                )?;
                let operand = match nested::materialize_path_value(operand) {
                    operand @ (Expr::EntityList { .. } | Expr::Nested { .. }) => operand,
                    operand => {
                        require_scalar_context(self.identity_expression(operand), expression.span)?
                    }
                };
                Ok(Expr::IsNull {
                    expression: Box::new(operand),
                    negated: *negated,
                })
            }
            ExprKind::Is {
                expression: target,
                negated,
                predicate: IsPredicate::Label(label),
            } => {
                let entity = self.output_expression_with_aliases(target, aliases)?;
                if let Expr::Nested { shape, .. } = &entity {
                    if **shape != NestedShape::Mixed {
                        return Err(semantic(
                            target.span,
                            "a label predicate requires a node or relationship",
                        ));
                    }
                    let predicate = self.label_expression(label, &|name| {
                        nested::call(
                            NestedOp::HasLabel,
                            vec![
                                entity.clone(),
                                Expr::Value(ValueExpr::Literal(Value::String(name))),
                            ],
                        )
                    })?;
                    return Ok(if *negated {
                        Expr::Unary {
                            operator: UnaryOp::Not,
                            operand: Box::new(predicate),
                        }
                    } else {
                        predicate
                    });
                }
                let Some(kind) = self.expression_entity_kind(&entity) else {
                    return Err(semantic(
                        target.span,
                        "a label predicate requires a node or relationship",
                    ));
                };
                let predicate = match kind {
                    EntityKind::Node => self.label_expression(label, &|name| {
                        entity_access(entity.clone(), EntityAccess::HasLabel(name))
                    })?,
                    EntityKind::Relationship => {
                        self.label_expression(label, &|name| Expr::Binary {
                            left: Box::new(entity_access(
                                entity.clone(),
                                EntityAccess::RelationshipType,
                            )),
                            operator: BinaryOp::Equal,
                            right: Box::new(Expr::Value(ValueExpr::Literal(Value::String(name)))),
                        })?
                    }
                };
                Ok(if *negated {
                    Expr::Unary {
                        operator: UnaryOp::Not,
                        operand: Box::new(predicate),
                    }
                } else {
                    predicate
                })
            }
            ExprKind::Is { .. } => Err(unsupported(expression.span, "IS predicate")),
            ExprKind::Function(function) => self
                .function_expression(function, expression.span, aliases)
                .and_then(|lowered| require_scalar_context(lowered, expression.span)),
            ExprKind::Parenthesized(inner) => {
                self.scalar_expression_with_aliases_at_depth(inner, aliases, expression_depth + 1)
            }
            ExprKind::Variable(variable) => self.scalar_variable_expression(variable, aliases),
            // Name every remaining expression kind so unsupported-feature
            // reports rank real gaps instead of one opaque bucket.
            ExprKind::MapProjection(_) => Err(unsupported(expression.span, "map projection")),
            ExprKind::DynamicProperty { .. } | ExprKind::Index { .. } | ExprKind::Slice { .. } => {
                require_scalar_context(
                    self.subscript_expression(expression, aliases, expression_depth)?,
                    expression.span,
                )
            }
            ExprKind::Case(_) => require_scalar_context(
                self.case_expression(expression, aliases, expression_depth)?,
                expression.span,
            ),
            ExprKind::ListComprehension(comprehension) => {
                let lowered = self.list_comprehension(comprehension, aliases, expression_depth)?;
                require_scalar_context(lowered, expression.span)
            }
            ExprKind::PatternComprehension(_) => {
                Err(unsupported(expression.span, "pattern comprehension"))
            }
            ExprKind::Reduce(_) => Err(unsupported(expression.span, "reduce expression")),
            ExprKind::QuantifiedPredicate(quantified) => {
                let kind = match quantified.kind.kind {
                    QuantifiedPredicateKind::All => ListQuantifier::All,
                    QuantifiedPredicateKind::Any => ListQuantifier::Any,
                    QuantifiedPredicateKind::None => ListQuantifier::None,
                    QuantifiedPredicateKind::Single => ListQuantifier::Single,
                    _ => return Err(unsupported(quantified.kind.span, "list quantifier")),
                };
                // The list is evaluated in the enclosing scope; only the
                // predicate sees the bound variable, which shadows any outer
                // binding of the same name.
                let list = self.output_expression_with_aliases_at_depth(
                    &quantified.list,
                    aliases,
                    expression_depth + 1,
                )?;
                if is_non_list_graph_value(&list) {
                    return Err(semantic(
                        quantified.list.span,
                        "a list quantifier requires a list",
                    ));
                }
                let variable = self.allocate_list_variable();
                let (list, element, _) = self.list_member(list, variable);
                let mut inner = aliases.clone();
                inner.insert(quantified.variable.kind.text.clone(), element);
                let predicate = self.scalar_expression_with_aliases_at_depth(
                    &quantified.predicate,
                    &inner,
                    expression_depth + 1,
                )?;
                if predicate.contains_aggregate() {
                    return Err(semantic(
                        quantified.predicate.span,
                        "aggregate calls are not allowed inside a list quantifier predicate",
                    ));
                }
                Ok(Expr::Quantified {
                    kind,
                    variable,
                    list: Box::new(list),
                    predicate: Box::new(predicate),
                })
            }
            ExprKind::Subquery(_) => Err(unsupported(expression.span, "subquery expression")),
            ExprKind::Pattern(pattern) => self.pattern_predicate(pattern, expression.span, aliases),
            ExprKind::Wildcard => Err(unsupported(expression.span, "`*` in an expression")),
            ExprKind::Error(_) => Err(unsupported(expression.span, "malformed expression")),
            _ => Err(unsupported(expression.span, "expression")),
        }
    }

    fn add_expression(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        let ExprKind::Binary { left, right, .. } = &expression.kind else {
            unreachable!("addition is a binary expression");
        };
        let left =
            self.output_expression_with_aliases_at_depth(left, aliases, expression_depth + 1)?;
        let right =
            self.output_expression_with_aliases_at_depth(right, aliases, expression_depth + 1)?;
        if !self.nested_shape(&left).contains_graph() && !self.nested_shape(&right).contains_graph()
        {
            return Ok(Expr::Binary {
                left: Box::new(left),
                operator: BinaryOp::Add,
                right: Box::new(right),
            });
        }
        let shape = self
            .add_operand_shape(&left, expression.span)?
            .concatenate(self.add_operand_shape(&right, expression.span)?)
            .ok_or_else(|| {
                semantic(
                    expression.span,
                    "adding graph values requires a list operand",
                )
            })?;
        Ok(nested::restore(
            shape,
            nested::call(
                NestedOp::Add,
                vec![self.pack_nested(left), self.pack_nested(right)],
            ),
        ))
    }

    fn add_operand_shape(&self, expression: &Expr, span: Span) -> Result<NestedShape> {
        let shape = self.nested_shape(expression);
        if shape != NestedShape::Json {
            return Ok(shape);
        }
        Ok(match expression {
            Expr::Value(ValueExpr::List(items)) => NestedShape::Tuple(
                items
                    .iter()
                    .map(|value| {
                        if matches!(value, ValueExpr::Literal(Value::Null)) {
                            NestedShape::Null
                        } else {
                            NestedShape::Json
                        }
                    })
                    .collect(),
            ),
            Expr::Value(ValueExpr::Literal(Value::Array(items))) => NestedShape::Tuple(
                items
                    .iter()
                    .map(|value| {
                        if value.is_null() {
                            NestedShape::Null
                        } else {
                            NestedShape::Json
                        }
                    })
                    .collect(),
            ),
            Expr::List(items) => {
                NestedShape::Tuple(items.iter().map(|value| self.nested_shape(value)).collect())
            }
            _ => match self.check_kinds(expression, span)? {
                Some(typecheck::Kind::List(_)) => NestedShape::List(Box::new(NestedShape::Json)),
                Some(typecheck::Kind::Null) => NestedShape::Null,
                Some(_) => NestedShape::Json,
                None => NestedShape::Mixed,
            },
        })
    }

    // Keep binary branches out of the recursive unary-expression frame so the
    // syntax-depth guard remains reachable on a normal debug thread stack.
    fn scalar_binary_expression(
        &self,
        expression: &AstExpr,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        let ExprKind::Binary {
            left,
            operator,
            right,
        } = &expression.kind
        else {
            unreachable!("a binary expression was matched");
        };
        if operator.kind == BinaryOperator::Add {
            return require_scalar_context(
                self.add_expression(expression, aliases, expression_depth)?,
                expression.span,
            );
        }
        // `a < b < c` compares pairwise: `a < b AND b < c`.
        if let Some((operands, operators)) = comparison_chain(expression)
            && operators.len() > 1
        {
            let mut conjuncts = Vec::with_capacity(operators.len());
            for (index, operator) in operators.into_iter().enumerate() {
                if matches!(operator, BinaryOp::Equal | BinaryOp::NotEqual) {
                    let left = self.output_expression_with_aliases_at_depth(
                        operands[index],
                        aliases,
                        expression_depth + 1,
                    )?;
                    let right = self.output_expression_with_aliases_at_depth(
                        operands[index + 1],
                        aliases,
                        expression_depth + 1,
                    )?;
                    conjuncts.push(self.equality_expression(
                        left,
                        right,
                        operator,
                        expression.span,
                    )?);
                    continue;
                }
                conjuncts.push(Expr::Binary {
                    left: Box::new(self.scalar_expression_with_aliases_at_depth(
                        operands[index],
                        aliases,
                        expression_depth + 1,
                    )?),
                    operator,
                    right: Box::new(self.scalar_expression_with_aliases_at_depth(
                        operands[index + 1],
                        aliases,
                        expression_depth + 1,
                    )?),
                });
            }
            return Ok(conjuncts
                .into_iter()
                .reduce(|left, right| Expr::Binary {
                    left: Box::new(left),
                    operator: BinaryOp::And,
                    right: Box::new(right),
                })
                .expect("a chain has at least two comparisons"));
        }
        if matches!(
            operator.kind,
            BinaryOperator::Equal | BinaryOperator::NotEqual
        ) {
            let left =
                self.output_expression_with_aliases_at_depth(left, aliases, expression_depth + 1)?;
            let right =
                self.output_expression_with_aliases_at_depth(right, aliases, expression_depth + 1)?;
            return self.equality_expression(
                left,
                right,
                if operator.kind == BinaryOperator::Equal {
                    BinaryOp::Equal
                } else {
                    BinaryOp::NotEqual
                },
                expression.span,
            );
        }
        if operator.kind == BinaryOperator::In {
            let left =
                self.output_expression_with_aliases_at_depth(left, aliases, expression_depth + 1)?;
            let right =
                self.output_expression_with_aliases_at_depth(right, aliases, expression_depth + 1)?;
            if uses_nested_predicate(&left) || uses_nested_predicate(&right) {
                return Ok(nested::call(
                    NestedOp::In,
                    vec![self.pack_nested(left), self.pack_nested(right)],
                ));
            }
            return Ok(Expr::Binary {
                left: Box::new(require_scalar_context(left, expression.span)?),
                operator: BinaryOp::In,
                right: Box::new(require_scalar_context(right, expression.span)?),
            });
        }
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
            BinaryOperator::Xor => BinaryOp::Xor,
            BinaryOperator::RegexMatch => {
                return Err(unsupported(operator.span, "regular-expression match `=~`"));
            }
            BinaryOperator::Power => return Err(unsupported(operator.span, "`^`")),
            BinaryOperator::Concat => return Err(unsupported(operator.span, "`||`")),
            _ => return Err(unsupported(operator.span, "binary operator")),
        };
        Ok(Expr::Binary {
            left: Box::new(self.scalar_expression_with_aliases_at_depth(
                left,
                aliases,
                expression_depth + 1,
            )?),
            operator,
            right: Box::new(self.scalar_expression_with_aliases_at_depth(
                right,
                aliases,
                expression_depth + 1,
            )?),
        })
    }

    /// Graph collections compare recursively by typed identity, using the
    /// same representation for deferred paths and materialized selections.
    fn equality_expression(
        &self,
        left: Expr,
        right: Expr,
        operator: BinaryOp,
        span: Span,
    ) -> Result<Expr> {
        if uses_nested_predicate(&left) || uses_nested_predicate(&right) {
            let equal = nested::call(
                NestedOp::Equal,
                vec![self.pack_nested(left), self.pack_nested(right)],
            );
            return Ok(if operator == BinaryOp::Equal {
                equal
            } else {
                Expr::Unary {
                    operator: UnaryOp::Not,
                    operand: Box::new(equal),
                }
            });
        }
        Ok(Expr::Binary {
            left: Box::new(require_scalar_context(
                self.identity_expression(left),
                span,
            )?),
            operator,
            right: Box::new(require_scalar_context(
                self.identity_expression(right),
                span,
            )?),
        })
    }

    // Keep variable/type resolution out of the recursive scalar-expression
    // frame: deep unary expressions must reach the syntax-depth guard on a
    // normal thread stack even in debug builds.
    fn scalar_variable_expression(
        &self,
        variable: &open_cypher::ast::Name,
        aliases: &HashMap<String, Expr>,
    ) -> Result<Expr> {
        if let Some(expression) = aliases.get(&variable.kind.text) {
            return match expression {
                Expr::Entity(_) | Expr::Path(_) => Err(semantic(
                    variable.span,
                    format!(
                        "graph or path variable `{}` is not a scalar expression",
                        variable.kind.text
                    ),
                )),
                Expr::PathFunction { kind, .. } if path_collection_kind(*kind).is_some() => Err(
                    unsupported(variable.span, "path collections in scalar expressions"),
                ),
                expression => require_scalar_context(expression.clone(), variable.span),
            };
        }
        match self.resolve_symbol(variable)? {
            Symbol::Scalar(slot) => {
                require_scalar_context(self.symbol_expression(Symbol::Scalar(slot)), variable.span)
            }
            Symbol::Entity(_) => Err(semantic(
                variable.span,
                format!(
                    "graph variable `{}` is not a scalar expression",
                    variable.kind.text
                ),
            )),
            Symbol::Path(_) => Err(semantic(
                variable.span,
                format!(
                    "path variable `{}` is not a scalar expression",
                    variable.kind.text
                ),
            )),
            Symbol::PathCollection { .. } => Err(unsupported(
                variable.span,
                "path collections in scalar expressions",
            )),
        }
    }

    fn function_expression(
        &self,
        function: &FunctionInvocation,
        span: Span,
        aliases: &HashMap<String, Expr>,
    ) -> Result<Expr> {
        let name = function_name(function, span)?;
        let aggregate = match name.as_str() {
            "count" => Some(AggregateKind::Count),
            "sum" => Some(AggregateKind::Sum),
            "avg" => Some(AggregateKind::Average),
            "min" => Some(AggregateKind::Minimum),
            "max" => Some(AggregateKind::Maximum),
            "collect" => Some(AggregateKind::Collect),
            _ => None,
        };
        if let Some(kind) = aggregate {
            return self.aggregate_expression(function, kind, span, aliases);
        }
        if function.quantifier.is_some() {
            return Err(unsupported(span, "quantified scalar function call"));
        }
        let mut lowered_one = None;
        if let [argument] = function.arguments.as_slice()
            && matches!(
                name.as_str(),
                "elementid"
                    | "labels"
                    | "type"
                    | "properties"
                    | "keys"
                    | "size"
                    | "length"
                    | "nodes"
                    | "relationships"
            )
        {
            let value = self.output_expression_with_aliases(argument, aliases)?;
            if let Expr::Nested { shape, .. } = &value {
                let shape = *shape.clone();
                let valid = match name.as_str() {
                    "elementid" | "labels" | "type" => shape == NestedShape::Mixed,
                    "properties" | "keys" => {
                        matches!(shape, NestedShape::Map(_) | NestedShape::Mixed)
                    }
                    "length" | "nodes" | "relationships" => {
                        matches!(shape, NestedShape::Path | NestedShape::Mixed)
                    }
                    "size" => !matches!(
                        shape,
                        NestedShape::Path | NestedShape::Map(_) | NestedShape::Entity(_)
                    ),
                    _ => unreachable!(),
                };
                if !valid {
                    return Err(semantic(
                        span,
                        format!("{name}() received an incompatible graph value"),
                    ));
                }
                let op = match name.as_str() {
                    "elementid" => NestedOp::ElementId,
                    "labels" => NestedOp::Labels,
                    "type" => NestedOp::RelationshipType,
                    "properties" => NestedOp::Properties,
                    "keys" => NestedOp::Keys,
                    "size" => NestedOp::Size,
                    "length" => NestedOp::Length,
                    "nodes" => NestedOp::PathNodes,
                    "relationships" => NestedOp::PathRelationships,
                    _ => unreachable!(),
                };
                let call = nested::call(op, vec![value]);
                return Ok(match op {
                    NestedOp::PathNodes => nested::restore(
                        NestedShape::List(Box::new(NestedShape::Entity(EntityKind::Node))),
                        call,
                    ),
                    NestedOp::PathRelationships => nested::restore(
                        NestedShape::List(Box::new(NestedShape::Entity(EntityKind::Relationship))),
                        call,
                    ),
                    NestedOp::Properties => nested::restore(
                        if matches!(shape, NestedShape::Map(_) | NestedShape::Mixed) {
                            shape
                        } else {
                            NestedShape::Json
                        },
                        call,
                    ),
                    _ => call,
                });
            }
            lowered_one = Some(value);
        }
        let lower_one = |argument: &AstExpr| match &lowered_one {
            Some(value) => Ok(value.clone()),
            None => self.output_expression_with_aliases(argument, aliases),
        };
        match name.as_str() {
            "elementid" | "type" | "labels" => {
                let [argument] = function.arguments.as_slice() else {
                    return Err(semantic(
                        span,
                        "graph function requires exactly one argument",
                    ));
                };
                let entity = lower_one(argument)?;
                let Some(kind) = self.expression_entity_kind(&entity) else {
                    return Err(semantic(
                        span,
                        "graph function requires a node or relationship",
                    ));
                };
                let access = match name.as_str() {
                    "type" if kind == EntityKind::Relationship => EntityAccess::RelationshipType,
                    "labels" if kind == EntityKind::Node => EntityAccess::Labels,
                    "elementid" => EntityAccess::ElementId,
                    "type" => return Err(semantic(span, "type() requires a relationship")),
                    _ => return Err(semantic(span, "labels() requires a node")),
                };
                Ok(entity_access(entity, access))
            }
            "exists" => match function.arguments.as_slice() {
                [argument] if matches!(argument.kind, ExprKind::Pattern(_)) => {
                    let ExprKind::Pattern(pattern) = &argument.kind else {
                        unreachable!("matched a pattern argument");
                    };
                    self.pattern_predicate(pattern, span, aliases)
                }
                [argument] => Ok(Expr::IsNull {
                    expression: Box::new(self.scalar_expression_with_aliases(argument, aliases)?),
                    negated: true,
                }),
                _ => Err(semantic(span, "exists() takes exactly one argument")),
            },
            "length" => self.path_function_expression(
                function,
                PathFunctionKind::Length,
                span,
                aliases,
                lowered_one.as_ref(),
            ),
            "range" => {
                let (start, end, step) = match function.arguments.as_slice() {
                    [start, end] => (start, end, None),
                    [start, end, step] => (start, end, Some(step)),
                    _ => {
                        return Err(semantic(span, "range() requires two or three arguments"));
                    }
                };
                Ok(Expr::Range {
                    start: Box::new(self.scalar_expression_with_aliases(start, aliases)?),
                    end: Box::new(self.scalar_expression_with_aliases(end, aliases)?),
                    step: step
                        .map(|step| self.scalar_expression_with_aliases(step, aliases))
                        .transpose()?
                        .map(Box::new),
                })
            }
            "size" => {
                let [argument] = function.arguments.as_slice() else {
                    return Err(semantic(span, "size() requires exactly one argument"));
                };
                let argument = lower_one(argument)?;
                match argument {
                    Expr::Path(_) => Err(semantic(
                        span,
                        "size() requires a list or string, not a path; use length()",
                    )),
                    Expr::Entity(_) | Expr::EntityValue { .. } => Err(semantic(
                        span,
                        "size() requires a list or string, not a graph entity",
                    )),
                    argument => Ok(Expr::Size(Box::new(argument))),
                }
            }
            "nodes" => self.path_function_expression(
                function,
                PathFunctionKind::Nodes,
                span,
                aliases,
                lowered_one.as_ref(),
            ),
            "relationships" => self.path_function_expression(
                function,
                PathFunctionKind::Relationships,
                span,
                aliases,
                lowered_one.as_ref(),
            ),
            "keys" | "properties" => {
                let [argument] = function.arguments.as_slice() else {
                    return Err(semantic(
                        span,
                        format!("{name}() requires exactly one argument"),
                    ));
                };
                let argument = lower_one(argument)?;
                let function = if name == "keys" {
                    ScalarFunction::Keys
                } else {
                    ScalarFunction::Properties
                };
                match argument {
                    Expr::Entity(slot) if function == ScalarFunction::Keys => {
                        Ok(Expr::EntityKeys(slot))
                    }
                    Expr::Entity(slot) => Ok(Expr::EntityProperties(slot)),
                    entity @ Expr::EntityValue { .. } => Ok(entity_access(
                        entity,
                        if function == ScalarFunction::Keys {
                            EntityAccess::Keys
                        } else {
                            EntityAccess::Properties
                        },
                    )),
                    Expr::Path(_) | Expr::PathFunction { .. } => Err(semantic(
                        span,
                        format!("{name}() requires a node, relationship, or map, not a path"),
                    )),
                    argument => Ok(Expr::Function {
                        function,
                        arguments: vec![argument],
                    }),
                }
            }
            "coalesce" => {
                if function.arguments.is_empty() {
                    return Err(semantic(span, "coalesce() requires at least 1 argument(s)"));
                }
                let mut arguments = function
                    .arguments
                    .iter()
                    .map(lower_one)
                    .collect::<Result<Vec<_>>>()?;
                let shapes = arguments
                    .iter()
                    .map(|value| self.nested_shape(value))
                    .collect::<Vec<_>>();
                let graph = shapes.iter().any(NestedShape::contains_graph);
                if graph {
                    arguments = arguments
                        .into_iter()
                        .map(|value| self.pack_nested(value))
                        .collect();
                }
                let result = Expr::Function {
                    function: ScalarFunction::Coalesce,
                    arguments,
                };
                Ok(if graph {
                    nested::restore(NestedShape::merge(shapes.iter()), result)
                } else {
                    result
                })
            }
            "head" | "last" | "tail" | "reverse" => {
                let [argument] = function.arguments.as_slice() else {
                    return Err(semantic(
                        span,
                        format!("{name}() requires exactly one argument"),
                    ));
                };
                let base = lower_one(argument)?;
                if is_non_list_graph_value(&base)
                    || matches!(&base, Expr::Nested { shape, .. } if matches!(shape.as_ref(), NestedShape::Map(_)))
                {
                    return Err(semantic(span, "list function requires a list"));
                }
                let literal =
                    |value: i64| Box::new(Expr::Value(ValueExpr::Literal(Value::from(value))));
                let base = if name == "reverse" {
                    base
                } else {
                    list_function_input(base)
                };
                Ok(match name.as_str() {
                    "head" => index_expression(base, *literal(0)),
                    "last" => index_expression(base, *literal(-1)),
                    "reverse" => reverse_expression(base),
                    _ => tail_expression(base),
                })
            }
            _ => {
                let Some(scalar_function) = ScalarFunction::parse(&name) else {
                    return Err(unsupported(span, format!("function `{name}`")));
                };
                let (minimum, maximum) = scalar_function.arity();
                let count = function.arguments.len();
                if count < minimum || count > maximum {
                    let expected = match (minimum, maximum) {
                        (0, 0) => "no arguments".to_owned(),
                        (minimum, usize::MAX) => format!("at least {minimum} argument(s)"),
                        (minimum, maximum) if minimum == maximum => {
                            format!("exactly {minimum} argument(s)")
                        }
                        (minimum, maximum) => format!("{minimum} to {maximum} arguments"),
                    };
                    return Err(semantic(
                        span,
                        format!("{}() requires {expected}", scalar_function.name()),
                    ));
                }
                let arguments = function
                    .arguments
                    .iter()
                    .enumerate()
                    .map(|(index, argument)| {
                        if let Some(expected) = scalar_function.argument_kind(index) {
                            let value = self.output_expression_with_aliases(argument, aliases)?;
                            if !matches!(
                                self.nested_shape(&value),
                                NestedShape::Json | NestedShape::Null | NestedShape::Mixed
                            ) {
                                return Err(semantic(
                                    span,
                                    format!(
                                        "{name}() argument {} needs {}, not a graph value",
                                        index + 1,
                                        expected.name()
                                    ),
                                ));
                            }
                            require_scalar_context(value, span)
                        } else {
                            self.scalar_expression_with_aliases(argument, aliases)
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Expr::Function {
                    function: scalar_function,
                    arguments,
                })
            }
        }
    }

    fn path_function_expression(
        &self,
        function: &FunctionInvocation,
        kind: PathFunctionKind,
        span: Span,
        aliases: &HashMap<String, Expr>,
        lowered: Option<&Expr>,
    ) -> Result<Expr> {
        let [argument] = function.arguments.as_slice() else {
            return Err(semantic(
                span,
                format!(
                    "{}() requires exactly one argument",
                    path_function_name(kind)
                ),
            ));
        };
        let argument = match lowered {
            Some(value) => value.clone(),
            None => self.output_expression_with_aliases(argument, aliases)?,
        };
        let Expr::Path(path) = argument else {
            return Err(semantic(
                span,
                format!("{}() requires a path", path_function_name(kind)),
            ));
        };
        Ok(Expr::PathFunction { kind, path })
    }

    fn aggregate_expression(
        &self,
        function: &FunctionInvocation,
        kind: AggregateKind,
        span: Span,
        aliases: &HashMap<String, Expr>,
    ) -> Result<Expr> {
        let distinct = match function.quantifier.as_ref().map(|value| value.kind) {
            None | Some(SetQuantifier::All) => false,
            Some(SetQuantifier::Distinct) => true,
            Some(_) => return Err(unsupported(span, "aggregate quantifier")),
        };
        let [argument] = function.arguments.as_slice() else {
            return Err(semantic(
                span,
                "aggregate functions require exactly one argument",
            ));
        };

        let argument = if matches!(argument.kind, ExprKind::Wildcard) {
            if kind != AggregateKind::Count {
                return Err(semantic(span, "only count() accepts `*`"));
            }
            if distinct {
                return Err(semantic(span, "count(DISTINCT *) is not supported"));
            }
            None
        } else {
            let mut argument = self.output_expression_with_aliases(argument, aliases)?;
            if argument.contains_aggregate() {
                return Err(semantic(span, "aggregate functions cannot be nested"));
            }
            if argument.contains_rand() {
                return Err(semantic(
                    span,
                    "rand() is not allowed inside an aggregate function",
                ));
            }
            if matches!(argument, Expr::Entity(_) | Expr::EntityValue { .. })
                && !matches!(kind, AggregateKind::Count | AggregateKind::Collect)
            {
                return Err(unsupported(
                    span,
                    "whole entities in aggregates other than count()",
                ));
            }
            if matches!(kind, AggregateKind::Count | AggregateKind::Collect) {
                argument = nested::materialize_path_value(argument);
            }
            if matches!(argument, Expr::Nested { .. } | Expr::EntityList { .. }) {
                if !matches!(kind, AggregateKind::Count | AggregateKind::Collect) {
                    return Err(unsupported(
                        span,
                        "graph collections in numeric or extremum aggregates",
                    ));
                }
                let shape = self.nested_shape(&argument);
                argument = nested::restore(shape, self.pack_nested(argument));
            }
            if is_deferred_graph_value(&argument) {
                return Err(unsupported(
                    span,
                    "path values or path collections in aggregate functions",
                ));
            }
            if matches!(argument, Expr::EntityList { .. }) {
                return Err(unsupported(
                    span,
                    "collected entity lists in aggregate functions",
                ));
            }
            if distinct && !matches!(kind, AggregateKind::Count | AggregateKind::Collect) {
                return Err(unsupported(
                    span,
                    "DISTINCT for aggregates other than count()",
                ));
            }
            Some(Box::new(argument))
        };

        let entity_kind = match argument.as_deref() {
            Some(argument) if kind == AggregateKind::Collect => {
                self.expression_entity_kind(argument)
            }
            _ => None,
        };
        let nested_shape = if kind == AggregateKind::Collect {
            argument.as_deref().and_then(|argument| match argument {
                Expr::Nested { shape, .. } => Some(NestedShape::List(shape.clone())),
                _ => None,
            })
        } else {
            None
        };
        let aggregate = Expr::Aggregate {
            kind,
            argument,
            distinct,
        };
        if let Some(shape) = nested_shape {
            return Ok(nested::restore(
                shape,
                nested::call(NestedOp::List, vec![aggregate]),
            ));
        }
        Ok(match entity_kind {
            Some(kind) => Expr::EntityList {
                kind,
                expression: Box::new(aggregate),
            },
            None => aggregate,
        })
    }

    /// Lowers `base.key`. Graph entities keep the dedicated `Property`
    /// expression; any other base (a projected or parameter map, a quantified
    /// list element, a nested map, or an arbitrary scalar expression) becomes
    /// a `MapProperty` lookup evaluated against the base's JSON value.
    fn property_expression(
        &self,
        base: &AstExpr,
        key: &str,
        aliases: &HashMap<String, Expr>,
        expression_depth: usize,
    ) -> Result<Expr> {
        let lowered =
            self.output_expression_with_aliases_at_depth(base, aliases, expression_depth + 1)?;
        match lowered {
            value if is_path_value(&value) => Err(semantic(
                base.span,
                "path variables and path collections do not have graph properties",
            )),
            base @ Expr::Nested { .. } => {
                let shape = match self.nested_shape(&base) {
                    NestedShape::Map(fields) => {
                        fields.get(key).cloned().unwrap_or(NestedShape::Null)
                    }
                    _ => NestedShape::Mixed,
                };
                Ok(nested::restore(
                    shape,
                    nested::call(
                        NestedOp::Property,
                        vec![
                            base,
                            Expr::Value(ValueExpr::Literal(Value::String(key.to_owned()))),
                        ],
                    ),
                ))
            }
            Expr::Entity(_) | Expr::EntityValue { .. } => Ok(entity_access(
                lowered,
                EntityAccess::Property(key.to_owned()),
            )),
            Expr::Path(_) | Expr::PathFunction { .. } => Err(semantic(
                base.span,
                "path variables and path collections do not have graph properties",
            )),
            Expr::EntityList { .. } => Err(semantic(
                base.span,
                "property access requires a map, node, or relationship",
            )),
            base => Ok(Expr::MapProperty {
                base: Box::new(base),
                key: key.to_owned(),
            }),
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
                property_map_input(expression)
            }
            _ => Err(unsupported(expression.span, "non-map pattern properties")),
        },
    }
}

fn property_map_input(expression: &AstExpr) -> Result<ValueExpr> {
    property_map_input_at_depth(expression, 0)
}

fn property_map_input_at_depth(expression: &AstExpr, expression_depth: usize) -> Result<ValueExpr> {
    if expression_depth > MAX_EXPRESSION_NESTING {
        return Err(semantic(
            expression.span,
            format!(
                "property-map expression nesting exceeds the supported maximum of \
                 {MAX_EXPRESSION_NESTING}"
            ),
        ));
    }

    match &expression.kind {
        ExprKind::Map(entries) => entries
            .iter()
            .map(|entry| Ok((entry.key.kind.text.clone(), input_value(&entry.value)?)))
            .collect::<Result<Vec<_>>>()
            .map(ValueExpr::Map),
        ExprKind::Parenthesized(inner) => property_map_input_at_depth(inner, expression_depth + 1),
        _ => input_value(expression),
    }
}

fn input_value(expression: &AstExpr) -> Result<ValueExpr> {
    input_value_at_depth(expression, 0, 0)
}

fn input_value_at_depth(
    expression: &AstExpr,
    collection_depth: usize,
    expression_depth: usize,
) -> Result<ValueExpr> {
    if expression_depth > MAX_EXPRESSION_NESTING {
        return Err(semantic(
            expression.span,
            format!(
                "value expression nesting exceeds the supported maximum of \
                 {MAX_EXPRESSION_NESTING}"
            ),
        ));
    }
    match &expression.kind {
        ExprKind::Literal(literal) => literal_value(&literal.kind).map(ValueExpr::Literal),
        ExprKind::Parameter(parameter) => match &parameter.name {
            ParameterName::Named(name) => Ok(ValueExpr::Parameter(name.clone())),
            ParameterName::Positional(_) => {
                Err(unsupported(expression.span, "positional parameters"))
            }
            _ => Err(unsupported(expression.span, "parameter form")),
        },
        ExprKind::List(items) => {
            let collection_depth = collection_depth + 1;
            validate_collection_depth(expression.span, collection_depth)?;
            items
                .iter()
                .map(|item| input_value_at_depth(item, collection_depth, expression_depth + 1))
                .collect::<Result<Vec<_>>>()
                .map(ValueExpr::List)
        }
        ExprKind::Map(entries) => {
            let collection_depth = collection_depth + 1;
            validate_collection_depth(expression.span, collection_depth)?;
            entries
                .iter()
                .map(|entry| {
                    Ok((
                        entry.key.kind.text.clone(),
                        input_value_at_depth(&entry.value, collection_depth, expression_depth + 1)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()
                .map(ValueExpr::Map)
        }
        ExprKind::Unary { operator, operand } => match operator.kind {
            AstUnaryOperator::Minus => {
                if let ExprKind::Literal(literal) = &operand.kind
                    && let LiteralKind::Integer(integer) = &literal.kind
                {
                    negative_integer_literal(integer).map(ValueExpr::Literal)
                } else {
                    input_value_at_depth(operand, collection_depth, expression_depth + 1)
                        .map(Box::new)
                        .map(ValueExpr::Negated)
                }
            }
            AstUnaryOperator::Plus => {
                input_value_at_depth(operand, collection_depth, expression_depth + 1)
            }
            AstUnaryOperator::Not => Err(unsupported(expression.span, "NOT in a stored value")),
            _ => Err(unsupported(expression.span, "unary value expression")),
        },
        ExprKind::Parenthesized(inner) => {
            input_value_at_depth(inner, collection_depth, expression_depth + 1)
        }
        _ => Err(unsupported(
            expression.span,
            "non-literal stored value expression",
        )),
    }
}

fn validate_collection_depth(span: Span, depth: usize) -> Result<()> {
    if depth > MAX_USER_VALUE_NESTING {
        return Err(semantic(
            span,
            format!(
                "JSON value nesting exceeds the supported maximum of \
                 {MAX_USER_VALUE_NESTING} list/map containers"
            ),
        ));
    }
    Ok(())
}

fn page_value(expression: &AstExpr) -> Result<ValueExpr> {
    page_value_at_depth(expression, 0)
}

fn page_value_at_depth(expression: &AstExpr, expression_depth: usize) -> Result<ValueExpr> {
    if expression_depth > MAX_EXPRESSION_NESTING {
        return Err(semantic(
            expression.span,
            format!(
                "pagination expression nesting exceeds the supported maximum of \
                 {MAX_EXPRESSION_NESTING}"
            ),
        ));
    }
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
            page_value_at_depth(operand, expression_depth + 1)?;
            input_value(expression)
        }
        ExprKind::Parenthesized(inner) => page_value_at_depth(inner, expression_depth + 1),
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

fn legacy_shortest_factor(match_clause: &MatchClause) -> Option<&PathFactor> {
    match_clause
        .pattern
        .parts
        .iter()
        .flat_map(|part| &part.kind.path.kind.factors)
        .find(|factor| matches!(factor.kind, PathFactorKind::LegacyShortest { .. }))
}

fn legacy_quantifier_bounds(quantifier: &Quantifier) -> Result<(usize, Option<usize>)> {
    let parse_bound = |text: &str| {
        text.replace('_', "").parse::<usize>().map_err(|_| {
            semantic(
                quantifier.span,
                format!("invalid variable-length bound `{text}`"),
            )
        })
    };

    let (minimum, maximum) = match &quantifier.kind {
        QuantifierKind::Fixed(value) => {
            let value = parse_bound(value)?;
            (value, Some(value))
        }
        QuantifierKind::Range { lower, upper } => {
            // Legacy openCypher ranges default the omitted lower bound to one.
            // Zero-hop traversal must be requested explicitly with `*0...`.
            (
                lower.as_deref().map(parse_bound).transpose()?.unwrap_or(1),
                upper.as_deref().map(parse_bound).transpose()?,
            )
        }
        QuantifierKind::ZeroOrMore => (1, None),
        QuantifierKind::OneOrMore => {
            return Err(unsupported(
                quantifier.span,
                "legacy variable-length quantifier",
            ));
        }
        QuantifierKind::Optional => {
            return Err(unsupported(
                quantifier.span,
                "legacy variable-length quantifier",
            ));
        }
        _ => {
            return Err(unsupported(quantifier.span, "variable-length quantifier"));
        }
    };

    Ok((minimum, maximum))
}

/// The label names of a plain conjunction such as `:A:B` or `:A&B`, in
/// order and without repeats. Any other label expression is `feature`,
/// reported as unsupported.
fn label_names(label: &LabelExpression, feature: &str) -> Result<Vec<String>> {
    fn collect(label: &LabelExpression, names: &mut Vec<String>, feature: &str) -> Result<()> {
        match &label.kind {
            LabelExpressionKind::Name(name) => {
                if !names.contains(&name.kind.text) {
                    names.push(name.kind.text.clone());
                }
                Ok(())
            }
            LabelExpressionKind::And(parts) => parts
                .iter()
                .try_for_each(|part| collect(part, names, feature)),
            LabelExpressionKind::Parenthesized(inner) => collect(inner, names, feature),
            _ => Err(unsupported(label.span, feature)),
        }
    }
    let mut names = Vec::new();
    collect(label, &mut names, feature)?;
    Ok(names)
}

fn single_label(label: &LabelExpression, feature: &str) -> Result<String> {
    match &label.kind {
        LabelExpressionKind::Name(name) => Ok(name.kind.text.clone()),
        _ => Err(unsupported(label.span, feature)),
    }
}

fn function_name(function: &FunctionInvocation, span: Span) -> Result<String> {
    if function.name.parts.len() != 1 {
        return Err(unsupported(span, "qualified function call"));
    }
    Ok(function.name.parts[0].kind.text.to_ascii_lowercase())
}

#[derive(Clone, Copy, Debug)]
enum RelationshipListBinding {
    Fresh(PathId),
    Prebound(PathId),
}

// Traversals in `plan` that re-match a relationship list which `plan` itself
// binds.
fn same_clause_rematches(plan: &MatchPlan) -> Vec<TraversalId> {
    let bound_here = plan
        .operators
        .iter()
        .filter_map(|operator| match operator {
            MatchOperator::BindPath { path, .. } => Some(*path),
            _ => None,
        })
        .collect::<Vec<_>>();
    plan.operators
        .iter()
        .filter_map(|operator| match operator {
            MatchOperator::MatchRelationshipList { traversal, path }
                if bound_here.contains(path) =>
            {
                Some(*traversal)
            }
            _ => None,
        })
        .collect()
}

fn path_collection_kind(kind: PathFunctionKind) -> Option<PathCollectionKind> {
    match kind {
        PathFunctionKind::Length => None,
        PathFunctionKind::Nodes => Some(PathCollectionKind::Nodes),
        PathFunctionKind::Relationships => Some(PathCollectionKind::Relationships),
    }
}

fn path_function_kind(kind: PathCollectionKind) -> PathFunctionKind {
    match kind {
        PathCollectionKind::Nodes => PathFunctionKind::Nodes,
        PathCollectionKind::Relationships => PathFunctionKind::Relationships,
    }
}

fn path_function_name(kind: PathFunctionKind) -> &'static str {
    match kind {
        PathFunctionKind::Length => "length",
        PathFunctionKind::Nodes => "nodes",
        PathFunctionKind::Relationships => "relationships",
    }
}

fn is_deferred_graph_value(expression: &Expr) -> bool {
    matches!(
        expression,
        Expr::Path(_)
            | Expr::PathFunction {
                kind: PathFunctionKind::Nodes | PathFunctionKind::Relationships,
                ..
            }
    )
}

fn uses_nested_predicate(expression: &Expr) -> bool {
    is_deferred_graph_value(expression)
        || matches!(expression, Expr::Nested { .. } | Expr::EntityList { .. })
}

fn require_scalar_context(expression: Expr, span: Span) -> Result<Expr> {
    if let Expr::Nested { shape, .. } = &expression {
        return if matches!(shape.as_ref(), NestedShape::Mixed) {
            Ok(nested::call(NestedOp::ToJson, vec![expression]))
        } else {
            Err(unsupported(span, "graph collections in scalar expressions"))
        };
    }
    if matches!(expression, Expr::Path(_)) {
        Err(semantic(span, "path values are not scalar expressions"))
    } else if matches!(expression, Expr::EntityValue { .. }) {
        Err(unsupported(span, "graph entities in scalar expressions"))
    } else if matches!(expression, Expr::EntityList { .. }) {
        Err(unsupported(span, "entity lists in scalar expressions"))
    } else if is_deferred_graph_value(&expression) {
        Err(unsupported(span, "path collections in scalar expressions"))
    } else {
        Ok(expression)
    }
}

fn implicit_grouping(expressions: &[Expr]) -> Vec<Expr> {
    if !expressions.iter().any(Expr::contains_aggregate) {
        return Vec::new();
    }
    let mut grouping = Vec::new();
    for expression in expressions {
        if !expression.contains_aggregate()
            && expression.is_row_dependent()
            && !grouping.contains(expression)
        {
            grouping.push(expression.clone());
        }
    }
    grouping
}

fn validate_aggregate_scope(expressions: &[Expr], grouping: &[Expr], span: Span) -> Result<()> {
    // Inside an aggregate expression, openCypher permits grouped variables
    // and property accesses, not a repeated compound grouping expression.
    // Bind such a key with an earlier WITH to give it an unambiguous name.
    let aggregate_grouping = grouping
        .iter()
        .filter(|expression| match expression {
            Expr::Entity(_) | Expr::Scalar(_) | Expr::Property(_, _) | Expr::MapProperty { .. } => {
                true
            }
            Expr::EntityList { expression, .. } | Expr::Nested { expression, .. } => {
                matches!(expression.as_ref(), Expr::Scalar(_))
                    || matches!(expression.as_ref(), Expr::NestedCall { op: NestedOp::Path, arguments } if matches!(arguments.as_slice(), [Expr::Path(_)]))
            }
            _ => false,
        })
        .cloned()
        .collect::<Vec<_>>();
    if expressions.iter().all(|expression| {
        expression.uses_only_grouped_values(if expression.contains_aggregate() {
            &aggregate_grouping
        } else {
            grouping
        })
    }) {
        Ok(())
    } else {
        Err(semantic(
            span,
            "aggregate query references a value that is not a grouping key",
        ))
    }
}

fn has_unprojected_aggregate(expression: &Expr, projected: &[Expr]) -> bool {
    !projected.contains(expression)
        && (matches!(expression, Expr::Aggregate { .. })
            || expression
                .children()
                .into_iter()
                .any(|child| has_unprojected_aggregate(child, projected)))
}

fn comparison_operator(operator: BinaryOperator) -> Option<BinaryOp> {
    Some(match operator {
        BinaryOperator::Equal => BinaryOp::Equal,
        BinaryOperator::NotEqual => BinaryOp::NotEqual,
        BinaryOperator::Less => BinaryOp::Less,
        BinaryOperator::LessEqual => BinaryOp::LessEqual,
        BinaryOperator::Greater => BinaryOp::Greater,
        BinaryOperator::GreaterEqual => BinaryOp::GreaterEqual,
        _ => return None,
    })
}

/// The operands and operators of a left-nested run of comparisons such as
/// `a < b <= c`. Parentheses end a chain, so `(a < b) < c` is two operands.
fn comparison_chain(expression: &AstExpr) -> Option<(Vec<&AstExpr>, Vec<BinaryOp>)> {
    let ExprKind::Binary {
        left,
        operator,
        right,
    } = &expression.kind
    else {
        return None;
    };
    let operator = comparison_operator(operator.kind)?;
    let (mut operands, mut operators) =
        comparison_chain(left).unwrap_or_else(|| (vec![left.as_ref()], Vec::new()));
    operands.push(right.as_ref());
    operators.push(operator);
    Some((operands, operators))
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

/// Whether the resulting value can contain graph values, which cannot be stored.
fn contains_graph_value(expression: &Expr) -> bool {
    if let Expr::NestedCall { op, .. } = expression {
        return !matches!(
            op,
            NestedOp::ToJson
                | NestedOp::Size
                | NestedOp::Equal
                | NestedOp::In
                | NestedOp::ElementId
                | NestedOp::Labels
                | NestedOp::RelationshipType
                | NestedOp::Keys
                | NestedOp::HasLabel
                | NestedOp::Length
        );
    }
    // These operations return scalar/property values, not their input records.
    if matches!(
        expression,
        Expr::EntityAccess { .. }
            | Expr::Exists { .. }
            | Expr::Size(_)
            | Expr::IsNull { .. }
            | Expr::Quantified { .. }
    ) {
        return false;
    }
    if let Expr::Case {
        alternatives,
        otherwise,
        ..
    } = expression
    {
        return alternatives.iter().any(|a| contains_graph_value(&a.result))
            || otherwise.as_deref().is_some_and(contains_graph_value);
    }
    if let Expr::ListComprehension {
        list, projection, ..
    } = expression
    {
        return contains_graph_value(projection.as_deref().unwrap_or(list));
    }
    matches!(
        expression,
        Expr::Entity(_)
            | Expr::EntityValue { .. }
            | Expr::EntityList { .. }
            | Expr::Nested { .. }
            | Expr::Path(_)
            | Expr::PathFunction {
                kind: PathFunctionKind::Nodes | PathFunctionKind::Relationships,
                ..
            }
    ) || expression.children().into_iter().any(contains_graph_value)
}

fn is_path_value(expression: &Expr) -> bool {
    matches!(expression, Expr::Path(_))
        || matches!(expression, Expr::Nested { shape, .. } if **shape == NestedShape::Path)
}

fn is_non_list_graph_value(expression: &Expr) -> bool {
    matches!(expression, Expr::Entity(_) | Expr::EntityValue { .. }) || is_path_value(expression)
}

fn entity_list_kind(expression: &Expr) -> Option<EntityKind> {
    match expression {
        Expr::EntityList { kind, .. } => Some(*kind),
        Expr::PathFunction {
            kind: PathFunctionKind::Nodes,
            ..
        } => Some(EntityKind::Node),
        Expr::PathFunction {
            kind: PathFunctionKind::Relationships,
            ..
        } => Some(EntityKind::Relationship),
        _ => None,
    }
}

/// Keep record transport private while sharing the scalar bounds implementation.
fn index_expression(base: Expr, index: Expr) -> Expr {
    if let Expr::Nested { shape, .. } = &base {
        let selected = match (shape.as_ref(), &index) {
            (NestedShape::Map(fields), Expr::Value(ValueExpr::Literal(Value::String(key)))) => {
                fields.get(key).cloned().unwrap_or(NestedShape::Null)
            }
            (NestedShape::Tuple(items), Expr::Value(ValueExpr::Literal(Value::Number(index))))
                if index.as_i64().is_some() =>
            {
                let i = index.as_i64().unwrap() as i128;
                let i = if i < 0 { items.len() as i128 + i } else { i };
                usize::try_from(i)
                    .ok()
                    .and_then(|i| items.get(i))
                    .cloned()
                    .unwrap_or(NestedShape::Null)
            }
            (NestedShape::Map(fields), _) => NestedShape::merge(fields.values()),
            (shape, _) => shape.element(),
        };
        return nested::restore(selected, nested::call(NestedOp::Get, vec![base, index]));
    }
    let kind = entity_list_kind(&base);
    let expression = Expr::Index {
        base: Box::new(base),
        index: Box::new(index),
    };
    match kind {
        Some(kind) => Expr::EntityValue {
            kind,
            expression: Box::new(expression),
        },
        None => expression,
    }
}

fn slice_expression(base: Expr, lower: Option<Box<Expr>>, upper: Option<Box<Expr>>) -> Expr {
    if let Expr::Nested { shape, .. } = &base {
        let shape = NestedShape::List(Box::new(shape.element()));
        let slice = Expr::Slice {
            base: Box::new(nested::call(NestedOp::Elements, vec![base])),
            lower,
            upper,
        };
        return nested::restore(shape, nested::call(NestedOp::List, vec![slice]));
    }
    let kind = entity_list_kind(&base);
    let expression = Expr::Slice {
        base: Box::new(base),
        lower,
        upper,
    };
    match kind {
        Some(kind) => Expr::EntityList {
            kind,
            expression: Box::new(expression),
        },
        None => expression,
    }
}

// Keep list-function validation separate from the more general subscript rules.
// A mixed graph value may select a scalar, map, or entity at runtime.
fn list_function_input(base: Expr) -> Expr {
    if let Expr::Nested { shape, .. } = &base {
        return nested::restore(
            *shape.clone(),
            nested::call(NestedOp::ListInput, vec![base]),
        );
    }
    if entity_list_kind(&base).is_some() {
        return base;
    }
    Expr::Function {
        function: ScalarFunction::ListInput,
        arguments: vec![base],
    }
}

fn tail_expression(base: Expr) -> Expr {
    let lower = Some(Box::new(Expr::Value(ValueExpr::Literal(Value::from(1)))));
    // Unlike a general slice, tail preserves every known remaining position.
    if let Expr::Nested { shape, .. } = &base
        && let NestedShape::Tuple(items) = shape.as_ref()
    {
        let shape = NestedShape::Tuple(items.iter().skip(1).cloned().collect());
        let expression = Expr::Slice {
            base: Box::new(nested::call(NestedOp::Elements, vec![base])),
            lower,
            upper: None,
        };
        return nested::restore(shape, nested::call(NestedOp::List, vec![expression]));
    }
    slice_expression(base, lower, None)
}

fn reverse_expression(base: Expr) -> Expr {
    if let Expr::Nested { shape, .. } = &base {
        let mut shape = *shape.clone();
        if let NestedShape::Tuple(items) = &mut shape {
            items.reverse();
        }
        return nested::restore(shape, nested::call(NestedOp::Reverse, vec![base]));
    }
    let kind = entity_list_kind(&base);
    let expression = Expr::Function {
        function: ScalarFunction::Reverse,
        arguments: vec![base],
    };
    match kind {
        Some(kind) => Expr::EntityList {
            kind,
            expression: Box::new(expression),
        },
        None => expression,
    }
}

fn entity_access(entity: Expr, access: EntityAccess) -> Expr {
    if let Expr::Entity(slot) = entity {
        return match access {
            EntityAccess::ElementId => Expr::ElementId(slot),
            EntityAccess::Identity(_) => Expr::EntityIdentity(slot),
            EntityAccess::Labels => Expr::Labels(slot),
            EntityAccess::RelationshipType => Expr::RelationshipType(slot),
            EntityAccess::Keys => Expr::EntityKeys(slot),
            EntityAccess::Properties => Expr::EntityProperties(slot),
            EntityAccess::Property(key) => Expr::Property(slot, key),
            EntityAccess::DynamicProperty(key) => Expr::DynamicProperty(slot, key),
            EntityAccess::HasLabel(label) => Expr::HasLabel(slot, label),
        };
    }
    Expr::EntityAccess {
        entity: Box::new(entity),
        access,
    }
}
