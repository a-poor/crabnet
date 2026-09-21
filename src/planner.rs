mod explain;
mod logical;
mod lower;
mod physical;
mod sqlite;

use crate::{Error, Parameters, Result};

pub(crate) use logical::{
    EntityKind, SlotId, require_object, validate_storable_properties, validate_storable_value,
};
use logical::{Input, Mutation, Projection, Query, StoredValue};
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
    if query.mutations.is_empty() {
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
        Input::Match(plan) => Some(sqlite::compile_row_source(plan)?),
        Input::Pipeline(_) | Input::Rows(_) => {
            return Err(Error::Data(
                "a mutation plan cannot contain a read-only WITH pipeline".into(),
            ));
        }
    };
    // The post-mutation RETURN is an ordinary read over the rows the
    // mutations leave behind, so it supports everything a read projection
    // does: computed expressions, aggregates, DISTINCT, ordering, and paging.
    let result = query
        .projection
        .as_ref()
        .map(|projection| {
            let slots = query
                .slots
                .iter()
                .enumerate()
                .map(|(index, kind)| (SlotId(index), *kind))
                .collect();
            sqlite::compile_read(&Input::Rows(slots), projection).map(ReadPlan::single)
        })
        .transpose()?;
    Ok(Plan::Mutate(Box::new(MutationPlan {
        input,
        slots: query.slots,
        mutations: query.mutations,
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
    slots: Vec<EntityKind>,
    mutations: Vec<Mutation>,
    result: Option<ReadPlan>,
}

/// A stored value ready to execute: resolved once, or a prepared read over
/// the materialized rows that yields one value per row.
pub(crate) enum ResolvedStoredValue {
    Static(serde_json::Value),
    Computed(PreparedReadPlan),
}

pub(crate) enum ResolvedMutation {
    CreateNode {
        slot: SlotId,
        label: Option<String>,
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
        slot: SlotId,
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
        slot: SlotId,
        key: String,
    },
    Delete {
        targets: Vec<(SlotId, EntityKind)>,
        detach: bool,
    },
}

pub(crate) struct ResolvedMutationPlan {
    pub(crate) input: Option<ResolvedRowSource>,
    pub(crate) slots: Vec<EntityKind>,
    pub(crate) mutations: Vec<ResolvedMutation>,
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
        let slots = self
            .slots
            .iter()
            .enumerate()
            .map(|(index, kind)| (SlotId(index), *kind))
            .collect::<Vec<_>>();
        let computed = |expression| -> Result<ResolvedStoredValue> {
            compile_stored_expression(&slots, expression)?
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
        let mutations = self
            .mutations
            .into_iter()
            .map(|mutation| match mutation {
                Mutation::CreateNode {
                    slot,
                    label,
                    properties: value,
                } => Ok(ResolvedMutation::CreateNode {
                    slot,
                    label,
                    properties: properties(value, "node properties")?,
                }),
                Mutation::CreateRelationship {
                    slot,
                    relationship_type,
                    from,
                    to,
                    properties: value,
                } => Ok(ResolvedMutation::CreateRelationship {
                    slot,
                    relationship_type,
                    from,
                    to,
                    properties: properties(value, "relationship properties")?,
                }),
                Mutation::SetProperty { slot, key, value } => Ok(ResolvedMutation::SetProperty {
                    slot,
                    key,
                    value: scalar(value)?,
                }),
                Mutation::ReplaceProperties {
                    slot,
                    properties: value,
                } => Ok(ResolvedMutation::ReplaceProperties {
                    slot,
                    properties: properties(value, "replacement properties")?,
                }),
                Mutation::MergeProperties {
                    slot,
                    properties: value,
                } => Ok(ResolvedMutation::MergeProperties {
                    slot,
                    properties: properties(value, "merged properties")?,
                }),
                Mutation::RemoveProperty { slot, key } => {
                    Ok(ResolvedMutation::RemoveProperty { slot, key })
                }
                Mutation::Delete { targets, detach } => {
                    Ok(ResolvedMutation::Delete { targets, detach })
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let result = self
            .result
            .map(|result| result.prepare(parameters))
            .transpose()?;

        Ok(ResolvedMutationPlan {
            input,
            slots: self.slots,
            mutations,
            result,
        })
    }
}

/// Compiles a computed stored value as a one-column read over the
/// materialized rows; row order follows the rows table.
fn compile_stored_expression(
    slots: &[(SlotId, EntityKind)],
    expression: logical::Expr,
) -> Result<ReadPlan> {
    let projection = Projection {
        columns: vec!["value".to_owned()],
        expressions: vec![expression],
        distinct: false,
        order_by: Vec::new(),
        offset: None,
        limit: None,
    };
    sqlite::compile_read(&Input::Rows(slots.to_vec()), &projection).map(ReadPlan::single)
}
