mod explain;
mod logical;
mod lower;
mod sqlite;

use crate::{Error, Parameters, Result};

pub(crate) use logical::{EntityKind, ResolvedExpr, SlotId};
use logical::{Input, Mutation, Projection, Query, require_object, validate_storable_value};
pub(crate) use sqlite::{ResolvedRowSource, SqlQuery};

pub(crate) enum Plan {
    Read(SqlQuery),
    Mutate(MutationPlan),
}

pub(crate) fn compile(source: &str) -> Result<Plan> {
    let query = lower::lower(source)?;
    compile_logical(query)
}

/// Render the normalized logical plan without exposing backend SQL details.
///
/// The crate root exposes this through the experimental `crabnet::explain`
/// helper. Keeping the renderer here avoids making logical-plan types public.
pub(crate) fn explain_logical(source: &str) -> Result<String> {
    lower::lower(source).map(|query| explain::render(&query))
}

fn compile_logical(query: Query) -> Result<Plan> {
    if query.mutations.is_empty() {
        let projection = query.projection.as_ref().ok_or_else(|| {
            Error::Data("a read-only logical plan is missing its projection".into())
        })?;
        return sqlite::compile_read(&query.input, projection).map(Plan::Read);
    }

    let input = match &query.input {
        Input::Unit => None,
        Input::Match(plan) => Some(sqlite::compile_row_source(plan)?),
    };
    Ok(Plan::Mutate(MutationPlan {
        input,
        slots: query.slots,
        mutations: query.mutations,
        projection: query.projection,
    }))
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
