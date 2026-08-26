mod explain;
mod logical;
mod lower;
mod sqlite;

use crate::{Error, Parameters, Result};

pub(crate) use logical::{EntityKind, ResolvedExpr, SlotId};
use logical::{Input, Mutation, Projection, Query, require_object, validate_storable_value};
pub(crate) use sqlite::{OutputShape, ResolvedRowSource, SqlQuery};

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
    pub(crate) branches: Vec<SqlQuery>,
    pub(crate) unions: Vec<UnionKind>,
}

impl ReadPlan {
    fn single(query: SqlQuery) -> Self {
        Self {
            branches: vec![query],
            unions: Vec::new(),
        }
    }

    fn render_physical(&self) -> String {
        if let [query] = self.branches.as_slice() {
            return query.render_physical();
        }

        let mut output = String::from("physical-union\n");
        for (index, branch) in self.branches.iter().enumerate() {
            if index > 0 {
                let kind = match self.unions[index - 1] {
                    UnionKind::All => "all",
                    UnionKind::Distinct => "semantic-distinct",
                };
                output.push_str(&format!("  union: {kind}\n"));
            }
            output.push_str(&format!("  branch {index}:\n"));
            for line in branch.render_physical().lines() {
                output.push_str("    ");
                output.push_str(line);
                output.push('\n');
            }
        }
        output
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

    let input = match &query.input {
        Input::Unit => None,
        Input::Match(plan) => Some(sqlite::compile_row_source(plan)?),
        Input::Pipeline(_) => {
            return Err(Error::Data(
                "a mutation plan cannot contain a read-only WITH pipeline".into(),
            ));
        }
    };
    Ok(Plan::Mutate(Box::new(MutationPlan {
        input,
        slots: query.slots,
        mutations: query.mutations,
        projection: query.projection,
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

    Ok(Plan::Read(ReadPlan { branches, unions }))
}

fn compile_read_query(query: Query) -> Result<SqlQuery> {
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
    projection: Option<Projection>,
}

pub(crate) enum ResolvedMutation {
    CreateNode {
        slot: SlotId,
        label: Option<String>,
        properties: serde_json::Value,
    },
    CreateRelationship {
        slot: SlotId,
        relationship_type: String,
        from: SlotId,
        to: SlotId,
        properties: serde_json::Value,
    },
    SetProperty {
        slot: SlotId,
        key: String,
        value: serde_json::Value,
    },
    ReplaceProperties {
        slot: SlotId,
        properties: serde_json::Value,
    },
    MergeProperties {
        slot: SlotId,
        properties: serde_json::Value,
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

pub(crate) struct ResolvedProjection {
    pub(crate) columns: Vec<String>,
    pub(crate) expressions: Vec<ResolvedExpr>,
}

pub(crate) struct ResolvedMutationPlan {
    pub(crate) input: Option<ResolvedRowSource>,
    pub(crate) slots: Vec<EntityKind>,
    pub(crate) mutations: Vec<ResolvedMutation>,
    pub(crate) projection: Option<ResolvedProjection>,
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
        let mutations = self
            .mutations
            .into_iter()
            .map(|mutation| match mutation {
                Mutation::CreateNode {
                    slot,
                    label,
                    properties,
                } => {
                    let properties = properties.resolve(parameters)?;
                    require_object(&properties, "node properties")?;
                    validate_storable_value(&properties)?;
                    Ok(ResolvedMutation::CreateNode {
                        slot,
                        label,
                        properties,
                    })
                }
                Mutation::CreateRelationship {
                    slot,
                    relationship_type,
                    from,
                    to,
                    properties,
                } => {
                    let properties = properties.resolve(parameters)?;
                    require_object(&properties, "relationship properties")?;
                    validate_storable_value(&properties)?;
                    Ok(ResolvedMutation::CreateRelationship {
                        slot,
                        relationship_type,
                        from,
                        to,
                        properties,
                    })
                }
                Mutation::SetProperty { slot, key, value } => {
                    let value = value.resolve(parameters)?;
                    validate_storable_value(&value)?;
                    Ok(ResolvedMutation::SetProperty { slot, key, value })
                }
                Mutation::ReplaceProperties { slot, properties } => {
                    let properties = properties.resolve(parameters)?;
                    require_object(&properties, "replacement properties")?;
                    validate_storable_value(&properties)?;
                    Ok(ResolvedMutation::ReplaceProperties { slot, properties })
                }
                Mutation::MergeProperties { slot, properties } => {
                    let properties = properties.resolve(parameters)?;
                    require_object(&properties, "merged properties")?;
                    validate_storable_value(&properties)?;
                    Ok(ResolvedMutation::MergeProperties { slot, properties })
                }
                Mutation::RemoveProperty { slot, key } => {
                    Ok(ResolvedMutation::RemoveProperty { slot, key })
                }
                Mutation::Delete { targets, detach } => {
                    Ok(ResolvedMutation::Delete { targets, detach })
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let projection = self
            .projection
            .map(|projection| -> Result<ResolvedProjection> {
                let expressions = projection
                    .expressions
                    .iter()
                    .map(|expression| expression.resolve_output(parameters))
                    .collect::<Result<Vec<_>>>()?;
                Ok(ResolvedProjection {
                    columns: projection.columns,
                    expressions,
                })
            })
            .transpose()?;

        Ok(ResolvedMutationPlan {
            input,
            slots: self.slots,
            mutations,
            projection,
        })
    }
}
