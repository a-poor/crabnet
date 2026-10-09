mod explain;
mod logical;
mod lower;
mod nested;
mod physical;
mod sqlite;
mod typecheck;

use crate::{Error, Parameters, Result};

pub(crate) use logical::{
    EntityKind, SlotId, require_object, validate_storable_properties, validate_storable_value,
};
use logical::{
    EntityTarget, Expr, Input, MatchPlan, Mutation, Projection, Query, ROW_INDEX_SLOT,
    ScalarSlotId, Segment, StoredValue,
};
pub(crate) use physical::{
    OutputShape, PhysicalOperator, PreparedFragmentArgument, PreparedPhysicalOperator,
    PreparedSqlFragment, PreparedValueOperator, ResolvedNodeDomain, ResolvedTrailExpansion,
    ShortestMode, TrailDirection,
};
pub(crate) use sqlite::{ROWS_TABLE, ResolvedRowSource};

pub(crate) enum Plan {
    Read(ReadPlan),
    Mutate(Box<MutationPlan>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnionKind {
    All,
    Distinct,
}

pub(crate) struct ReadPlan {
    pub(crate) columns: Vec<String>,
    pub(crate) root: PhysicalOperator,
}

#[derive(Clone)]
pub(crate) struct PreparedReadPlan {
    pub(crate) columns: Vec<String>,
    pub(crate) root: PreparedPhysicalOperator,
}

impl ReadPlan {
    fn single(query: sqlite::CompiledRead) -> Self {
        let columns = query.columns.clone();
        let hydrate = query.has_path_identity_output();
        let mut root = query.into_operator();
        if hydrate {
            root = PhysicalOperator::BatchHydrate {
                input: Box::new(root),
            };
        }
        Self { columns, root }
    }

    pub(crate) fn prepare(self, parameters: &Parameters) -> Result<PreparedReadPlan> {
        Ok(PreparedReadPlan {
            columns: self.columns,
            root: self.root.prepare(parameters)?,
        })
    }

    fn render_physical(&self) -> String {
        self.root.render()
    }
}

pub(crate) fn compile(source: &str) -> Result<Plan> {
    if let Some(union) = lower::split_union(source)? {
        return compile_union(union);
    }
    let query = lower::lower(source)?;
    compile_logical(query)
}

/// Render the normalized logical plan without exposing backend SQL details.
///
/// The crate root exposes this through the experimental `crabnet::explain`
/// helper. Keeping the renderer here avoids making logical-plan types public.
pub(crate) fn explain_logical(source: &str) -> Result<String> {
    let Some(union) = lower::split_union(source)? else {
        return lower::lower(source).map(|query| explain::render(&query));
    };

    let mut sources = Vec::with_capacity(union.branches.len() + 1);
    sources.push((None, union.head));
    sources.extend(
        union
            .branches
            .into_iter()
            .map(|(kind, source)| (Some(kind), source)),
    );
    let mut output = String::from("logical-union\n");
    for (index, (kind, source)) in sources.into_iter().enumerate() {
        if let Some(kind) = kind {
            output.push_str(match kind {
                UnionKind::All => "  union: all\n",
                UnionKind::Distinct => "  union: semantic-distinct\n",
            });
        }
        output.push_str(&format!("  branch {index}:\n"));
        for line in explain::render(&lower::lower(&source)?).lines() {
            output.push_str("    ");
            output.push_str(line);
            output.push('\n');
        }
    }
    Ok(output)
}

/// Render the backend plan, including generated SQL and bind positions.
pub(crate) fn explain_physical(source: &str) -> Result<String> {
    match compile(source)? {
        Plan::Read(plan) => Ok(plan.render_physical()),
        Plan::Mutate(_) => Err(Error::Unsupported(
            "physical explanations for mutation pipelines".into(),
        )),
    }
}

fn compile_logical(query: Query) -> Result<Plan> {
    if query.mutations.is_empty() && query.segments.is_empty() {
        return compile_read_query(query)
            .map(ReadPlan::single)
            .map(Plan::Read);
    }

    if query.path_slots > 0 {
        return Err(Error::Unsupported(
            "writes fed by named paths or relationship-list bindings".into(),
        ));
    }

    let input = match &query.input {
        Input::Unit => None,
        Input::Match(_) | Input::Pipeline(_) => Some(sqlite::compile_row_source(&query.input)?),
        Input::Rows { .. } => {
            return Err(Error::Data(
                "a mutation plan cannot start from materialized rows".into(),
            ));
        }
    };
    let layout = RowLayout {
        slots: query.slots.clone(),
        scalar_slots: query.scalar_slots,
        entity_lists: query.entity_lists.clone(),
        nested_slots: query.nested_slots.clone(),
    };
    // Every later read segment continues from the rows the mutations before
    // it leave behind, seeing the variables live at that point.
    let segments = query
        .segments
        .into_iter()
        .map(|segment| {
            let Segment {
                slots,
                scalars,
                stages,
                mutations,
            } = segment;
            let source = sqlite::compile_row_source(&Input::Rows {
                slots,
                scalars,
                stages,
            })?;
            Ok(CompiledSegment { source, mutations })
        })
        .collect::<Result<Vec<_>>>()?;
    // The post-mutation RETURN is an ordinary read over the rows the
    // mutations leave behind, so it supports everything a read projection
    // does: computed expressions, aggregates, DISTINCT, ordering, and paging.
    let result = query
        .projection
        .as_ref()
        .map(|projection| {
            sqlite::compile_read(&layout.rows_input(Vec::new()), projection).map(ReadPlan::single)
        })
        .transpose()?;
    Ok(Plan::Mutate(Box::new(MutationPlan {
        input,
        layout,
        mutations: query.mutations,
        segments,
        result,
    })))
}

fn compile_union(union: lower::UnionSource) -> Result<Plan> {
    let mut sources = Vec::with_capacity(union.branches.len() + 1);
    sources.push(union.head);
    let mut unions = Vec::with_capacity(union.branches.len());
    for (kind, source) in union.branches {
        unions.push(kind);
        sources.push(source);
    }

    let mut branches = Vec::with_capacity(sources.len());
    for source in sources {
        let query = lower::lower(&source)?;
        if !query.mutations.is_empty() {
            return Err(Error::Unsupported("writes in UNION branches".into()));
        }
        branches.push(compile_read_query(query)?);
    }

    let expected_columns = &branches[0].columns;
    for branch in &branches[1..] {
        if branch.columns.len() != expected_columns.len() {
            return Err(Error::Semantic(
                "UNION branches must return the same number of columns".into(),
            ));
        }
        if branch.columns != *expected_columns {
            return Err(Error::Semantic(
                "UNION branches must use the same column names in the same positions".into(),
            ));
        }
    }

    let columns = expected_columns.clone();
    let hydrate = branches
        .iter()
        .any(sqlite::CompiledRead::has_path_identity_output);
    let mut branches = branches.into_iter();
    let mut root = branches
        .next()
        .expect("UNION always has a head branch")
        .into_operator();
    for (kind, branch) in unions.into_iter().zip(branches) {
        root = PhysicalOperator::Union {
            left: Box::new(root),
            right: Box::new(branch.into_operator()),
            kind,
        };
    }
    if hydrate {
        root = PhysicalOperator::BatchHydrate {
            input: Box::new(root),
        };
    }
    Ok(Plan::Read(ReadPlan { columns, root }))
}

fn compile_read_query(query: Query) -> Result<sqlite::CompiledRead> {
    let projection = query
        .projection
        .as_ref()
        .ok_or_else(|| Error::Semantic("a read-only query branch must end with RETURN".into()))?;
    sqlite::compile_read(&query.input, projection)
}

pub(crate) struct MutationPlan {
    input: Option<sqlite::RowSource>,
    layout: RowLayout,
    mutations: Vec<Mutation>,
    segments: Vec<CompiledSegment>,
    result: Option<ReadPlan>,
}

/// The shape of a mutation pipeline's rows: the kind of every entity slot
/// and the number of scalar slots, whether or not a given row binds them.
#[derive(Clone, Debug)]
pub(crate) struct RowLayout {
    pub(crate) slots: Vec<EntityKind>,
    pub(crate) scalar_slots: usize,
    pub(crate) entity_lists: Vec<(ScalarSlotId, EntityKind)>,
    pub(crate) nested_slots: Vec<ScalarSlotId>,
}

impl RowLayout {
    fn entity_slots(&self) -> Vec<(SlotId, EntityKind)> {
        self.slots
            .iter()
            .enumerate()
            .map(|(index, kind)| (SlotId(index), *kind))
            .collect()
    }

    fn scalar_slot_ids(&self) -> Vec<ScalarSlotId> {
        (0..self.scalar_slots).map(ScalarSlotId).collect()
    }

    /// A read over the materialized rows with every slot exposed, continuing
    /// through `stages`.
    fn rows_input(&self, stages: Vec<logical::ReadStage>) -> Input {
        Input::Rows {
            slots: self.entity_slots(),
            scalars: self.scalar_slot_ids(),
            stages,
        }
    }
}

struct CompiledSegment {
    source: sqlite::RowSource,
    mutations: Vec<Mutation>,
}

pub(crate) struct ResolvedSegment {
    pub(crate) source: ResolvedRowSource,
    pub(crate) mutations: Vec<ResolvedMutation>,
}

/// A stored value ready to execute: resolved once, or a prepared read over
/// the materialized rows that yields one value per row.
#[derive(Clone)]
pub(crate) enum ResolvedStoredValue {
    Static(serde_json::Value),
    Computed(PreparedReadPlan),
}

/// A property target resolved without executing its per-row selection.
#[derive(Clone)]
pub(crate) enum ResolvedPropertyTarget {
    Bound(SlotId),
    Computed(PreparedReadPlan),
}

#[derive(Clone)]
pub(crate) enum ResolvedMutation {
    CreateNode {
        slot: SlotId,
        labels: Vec<String>,
        properties: ResolvedStoredValue,
    },
    CreateRelationship {
        slot: SlotId,
        relationship_type: String,
        from: SlotId,
        to: SlotId,
        properties: ResolvedStoredValue,
    },
    SetProperty {
        target: ResolvedPropertyTarget,
        key: String,
        value: ResolvedStoredValue,
    },
    ReplaceProperties {
        slot: SlotId,
        properties: ResolvedStoredValue,
    },
    MergeProperties {
        slot: SlotId,
        properties: ResolvedStoredValue,
    },
    RemoveProperty {
        target: ResolvedPropertyTarget,
        key: String,
    },
    SetLabels {
        slot: SlotId,
        labels: Vec<String>,
    },
    RemoveLabels {
        slot: SlotId,
        labels: Vec<String>,
    },
    Delete {
        targets: Vec<(SlotId, EntityKind)>,
        computed_targets: Option<PreparedReadPlan>,
        detach: bool,
    },
    /// `matcher` reads the row index and the fresh slots' entities for every
    /// match of the pattern from each materialized row.
    Merge {
        matcher: PreparedReadPlan,
        fresh: Vec<(SlotId, EntityKind)>,
        creates: Vec<ResolvedMutation>,
        on_create: Vec<ResolvedMutation>,
        on_match: Vec<ResolvedMutation>,
    },
}

pub(crate) struct ResolvedMutationPlan {
    pub(crate) input: Option<ResolvedRowSource>,
    pub(crate) layout: RowLayout,
    pub(crate) mutations: Vec<ResolvedMutation>,
    pub(crate) segments: Vec<ResolvedSegment>,
    pub(crate) result: Option<PreparedReadPlan>,
}

impl MutationPlan {
    pub(crate) fn resolve(self, parameters: &Parameters) -> Result<ResolvedMutationPlan> {
        // Resolve and validate every parameter before the write transaction
        // starts. In particular, a late property parameter cannot leave an
        // earlier CREATE operation committed.
        let input = self
            .input
            .map(|input| input.resolve(parameters))
            .transpose()?;
        let layout = self.layout;
        let resolve_all = |mutations: Vec<Mutation>| {
            mutations
                .into_iter()
                .map(|mutation| resolve_mutation(mutation, parameters, &layout))
                .collect::<Result<Vec<_>>>()
        };
        let mutations = resolve_all(self.mutations)?;
        let segments = self
            .segments
            .into_iter()
            .map(|segment| {
                Ok(ResolvedSegment {
                    source: segment.source.resolve(parameters)?,
                    mutations: resolve_all(segment.mutations)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let result = self
            .result
            .map(|result| result.prepare(parameters))
            .transpose()?;

        Ok(ResolvedMutationPlan {
            input,
            layout,
            mutations,
            segments,
            result,
        })
    }
}

/// Compiles mutation expressions as a read over the materialized rows;
/// row order follows the rows table.
fn compile_row_expressions(layout: &RowLayout, expressions: Vec<Expr>) -> Result<ReadPlan> {
    let projection = Projection {
        columns: (0..expressions.len())
            .map(|index| format!("value{index}"))
            .collect(),
        expressions,
        distinct: false,
        order_by: Vec::new(),
        offset: None,
        limit: None,
    };
    sqlite::compile_read(&layout.rows_input(Vec::new()), &projection).map(ReadPlan::single)
}

fn resolve_mutation(
    mutation: Mutation,
    parameters: &Parameters,
    layout: &RowLayout,
) -> Result<ResolvedMutation> {
    let computed = |expression| -> Result<ResolvedStoredValue> {
        compile_row_expressions(layout, vec![expression])?
            .prepare(parameters)
            .map(ResolvedStoredValue::Computed)
    };
    let properties = |value, description: &str| -> Result<ResolvedStoredValue> {
        match value {
            StoredValue::Static(value) => value
                .resolve_properties(parameters, description)
                .map(ResolvedStoredValue::Static),
            StoredValue::Computed(expression) => computed(expression),
        }
    };
    let scalar = |value| -> Result<ResolvedStoredValue> {
        match value {
            StoredValue::Static(value) => {
                let value = value.resolve(parameters)?;
                validate_storable_value(&value)?;
                Ok(ResolvedStoredValue::Static(value))
            }
            StoredValue::Computed(expression) => computed(expression),
        }
    };
    let property_target = |target| -> Result<ResolvedPropertyTarget> {
        match target {
            EntityTarget::Bound(slot, _) => Ok(ResolvedPropertyTarget::Bound(slot)),
            EntityTarget::Computed(expression) => {
                compile_row_expressions(layout, vec![expression])?
                    .prepare(parameters)
                    .map(ResolvedPropertyTarget::Computed)
            }
        }
    };
    let resolve_all = |mutations: Vec<Mutation>| {
        mutations
            .into_iter()
            .map(|mutation| resolve_mutation(mutation, parameters, layout))
            .collect::<Result<Vec<_>>>()
    };
    Ok(match mutation {
        Mutation::CreateNode {
            slot,
            labels,
            properties: value,
        } => ResolvedMutation::CreateNode {
            slot,
            labels,
            properties: properties(value, "node properties")?,
        },
        Mutation::CreateRelationship {
            slot,
            relationship_type,
            from,
            to,
            properties: value,
        } => ResolvedMutation::CreateRelationship {
            slot,
            relationship_type,
            from,
            to,
            properties: properties(value, "relationship properties")?,
        },
        Mutation::SetProperty { target, key, value } => ResolvedMutation::SetProperty {
            target: property_target(target)?,
            key,
            value: scalar(value)?,
        },
        Mutation::ReplaceProperties {
            slot,
            properties: value,
        } => ResolvedMutation::ReplaceProperties {
            slot,
            properties: properties(value, "replacement properties")?,
        },
        Mutation::MergeProperties {
            slot,
            properties: value,
        } => ResolvedMutation::MergeProperties {
            slot,
            properties: properties(value, "merged properties")?,
        },
        Mutation::RemoveProperty { target, key } => ResolvedMutation::RemoveProperty {
            target: property_target(target)?,
            key,
        },
        Mutation::SetLabels { slot, labels } => ResolvedMutation::SetLabels { slot, labels },
        Mutation::RemoveLabels { slot, labels } => ResolvedMutation::RemoveLabels { slot, labels },
        Mutation::Delete { targets, detach } => {
            let mut bound = Vec::new();
            let mut expressions = Vec::new();
            for target in targets {
                match target {
                    EntityTarget::Bound(slot, kind) => bound.push((slot, kind)),
                    EntityTarget::Computed(expression) => expressions.push(expression),
                }
            }
            let computed_targets = if expressions.is_empty() {
                None
            } else {
                Some(compile_row_expressions(layout, expressions)?.prepare(parameters)?)
            };
            ResolvedMutation::Delete {
                targets: bound,
                computed_targets,
                detach,
            }
        }
        Mutation::Merge {
            pattern,
            fresh,
            creates,
            on_create,
            on_match,
        } => ResolvedMutation::Merge {
            matcher: compile_merge_matcher(layout, &fresh, pattern)?.prepare(parameters)?,
            fresh,
            creates: resolve_all(creates)?,
            on_create: resolve_all(on_create)?,
            on_match: resolve_all(on_match)?,
        },
    })
}

/// The read behind a `MERGE`: from each materialized row, with the slots the
/// pattern does not introduce already bound, every match of the pattern as
/// the row index followed by the fresh slots' entities.
fn compile_merge_matcher(
    layout: &RowLayout,
    fresh: &[(SlotId, EntityKind)],
    pattern: MatchPlan,
) -> Result<ReadPlan> {
    let bound = layout
        .entity_slots()
        .into_iter()
        .filter(|(slot, _)| !fresh.iter().any(|(fresh, _)| fresh == slot))
        .collect::<Vec<_>>();
    let mut columns = vec!["k".to_owned()];
    let mut expressions = vec![Expr::Scalar(ROW_INDEX_SLOT)];
    for (slot, _) in fresh {
        columns.push(format!("f{}", slot.0));
        expressions.push(Expr::Entity(*slot));
    }
    let projection = Projection {
        columns,
        expressions,
        distinct: false,
        order_by: Vec::new(),
        offset: None,
        limit: None,
    };
    sqlite::compile_merge_read(
        &Input::Rows {
            slots: bound,
            scalars: layout.scalar_slot_ids(),
            stages: vec![logical::ReadStage::Match(pattern)],
        },
        &projection,
    )
    .map(ReadPlan::single)
}
