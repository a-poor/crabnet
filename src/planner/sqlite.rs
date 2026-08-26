use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
use serde_json::Value;

use crate::{Error, Parameters, Result};

use super::logical::{
    BinaryOp, Direction, EntityKind, Expr, Input, MatchOperator, MatchPlan, Projection, SlotId,
    SortDirection, UnaryOp, ValueExpr,
};

#[derive(Clone, Debug)]
enum SqlBinding {
    Text(String),
    Scalar(ValueExpr),
    Json(ValueExpr),
    Page(ValueExpr),
}

impl SqlBinding {
    fn resolve(&self, parameters: &Parameters) -> Result<SqlValue> {
        match self {
            Self::Text(value) => Ok(SqlValue::Text(value.clone())),
            Self::Scalar(value) => json_to_sql(value.resolve(parameters)?),
            Self::Json(value) => Ok(SqlValue::Text(value.resolve(parameters)?.to_string())),
            Self::Page(value) => {
                let value = value.resolve(parameters)?;
                let Value::Number(number) = value else {
                    return Err(Error::Semantic(
                        "SKIP and LIMIT require nonnegative integers".into(),
                    ));
                };
                if let Some(integer) = number.as_i64()
                    && integer >= 0
                {
                    return Ok(SqlValue::Integer(integer));
                }
                if let Some(unsigned) = number.as_u64()
                    && let Ok(integer) = i64::try_from(unsigned)
                {
                    return Ok(SqlValue::Integer(integer));
                }
                Err(Error::Semantic(
                    "SKIP and LIMIT require nonnegative integers".into(),
                ))
            }
        }
    }
}

fn json_to_sql(value: Value) -> Result<SqlValue> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(value) => Ok(SqlValue::Integer(i64::from(value))),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(SqlValue::Integer(value))
            } else if let Some(value) = number.as_u64() {
                i64::try_from(value).map(SqlValue::Integer).map_err(|_| {
                    Error::Semantic("integer parameter is outside the i64 range".into())
                })
            } else {
                number
                    .as_f64()
                    .map(SqlValue::Real)
                    .ok_or_else(|| Error::Semantic("non-finite floats are not supported".into()))
            }
        }
        Value::String(value) => Ok(SqlValue::Text(value)),
        Value::Array(_) | Value::Object(_) => Ok(SqlValue::Text(value.to_string())),
    }
}

pub(crate) struct SqlQuery {
    pub(crate) sql: String,
    pub(crate) columns: Vec<String>,
    pub(crate) output_shapes: Vec<OutputShape>,
    bindings: Vec<SqlBinding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputShape {
    Json,
    Entity(EntityKind),
}

impl SqlQuery {
    pub(crate) fn resolve_bindings(&self, parameters: &Parameters) -> Result<Vec<SqlValue>> {
        resolve_bindings(&self.bindings, parameters)
    }
}

pub(crate) struct RowSource {
    pub(crate) sql: String,
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    bindings: Vec<SqlBinding>,
}

pub(crate) struct ResolvedRowSource {
    pub(crate) sql: String,
    pub(crate) slots: Vec<(SlotId, EntityKind)>,
    pub(crate) bindings: Vec<SqlValue>,
}

impl RowSource {
    pub(crate) fn resolve(self, parameters: &Parameters) -> Result<ResolvedRowSource> {
        Ok(ResolvedRowSource {
            bindings: resolve_bindings(&self.bindings, parameters)?,
            sql: self.sql,
            slots: self.slots,
        })
    }
}

fn resolve_bindings(bindings: &[SqlBinding], parameters: &Parameters) -> Result<Vec<SqlValue>> {
    bindings
        .iter()
        .map(|binding| binding.resolve(parameters))
        .collect()
}

pub(crate) fn compile_read(input: &Input, projection: &Projection) -> Result<SqlQuery> {
    let mut compiler = Compiler::new();
    if let Input::Match(plan) = input {
        compiler.compile_match(plan)?;
    }

    let outputs = projection
        .expressions
        .iter()
        .enumerate()
        .map(|(index, expression)| {
            compiler
                .output_expression(expression)
                .map(|(sql, shape)| (format!("{sql} AS c{index}"), shape))
        })
        .collect::<Result<Vec<_>>>()?;
    let (select, output_shapes): (Vec<_>, Vec<_>) = outputs.into_iter().unzip();
    let mut sql = compiler.select_sql(&select);

    if !projection.order_by.is_empty() {
        let items = projection
            .order_by
            .iter()
            .map(|item| {
                let expression = compiler.raw_expression(&item.expression)?;
                let direction = match item.direction {
                    SortDirection::Ascending => " ASC",
                    SortDirection::Descending => " DESC",
                };
                Ok(format!("{expression}{direction}"))
            })
            .collect::<Result<Vec<_>>>()?;
        sql.push_str(" ORDER BY ");
        sql.push_str(&items.join(", "));
    }

    if projection.limit.is_some() || projection.offset.is_some() {
        sql.push_str(" LIMIT ");
        match &projection.limit {
            Some(limit) => sql.push_str(&compiler.push_binding(SqlBinding::Page(limit.clone()))),
            None => sql.push_str("-1"),
        }
    }
    if let Some(offset) = &projection.offset {
        sql.push_str(" OFFSET ");
        sql.push_str(&compiler.push_binding(SqlBinding::Page(offset.clone())));
    }

    debug_assert_eq!(projection.columns.len(), output_shapes.len());
    Ok(SqlQuery {
        sql,
        columns: projection.columns.clone(),
        output_shapes,
        bindings: compiler.bindings,
    })
}

pub(crate) fn compile_row_source(plan: &MatchPlan) -> Result<RowSource> {
    let mut compiler = Compiler::new();
    compiler.compile_match(plan)?;
    compiler.scanned.sort_by_key(|(slot, _)| *slot);

    let select = compiler
        .scanned
        .iter()
        .map(|(slot, kind)| compiler.encoded_entity(*slot, *kind))
        .collect::<Result<Vec<_>>>()?;
    let sql = compiler.select_sql(&select);

    Ok(RowSource {
        sql,
        slots: compiler.scanned,
        bindings: compiler.bindings,
    })
}

struct Compiler {
    aliases: HashMap<SlotId, String>,
    scanned: Vec<(SlotId, EntityKind)>,
    from: Vec<String>,
    filters: Vec<String>,
    bindings: Vec<SqlBinding>,
}

impl Compiler {
    fn new() -> Self {
        Self {
            aliases: HashMap::new(),
            scanned: Vec::new(),
            from: Vec::new(),
            filters: Vec::new(),
            bindings: Vec::new(),
        }
    }

    fn compile_match(&mut self, plan: &MatchPlan) -> Result<()> {
        for operator in &plan.operators {
            match operator {
                MatchOperator::ScanNode { slot } => {
                    let alias = format!("n{}", slot.0);
                    self.insert_scan(*slot, EntityKind::Node, alias.clone())?;
                    self.from.push(format!("nodes AS {alias}"));
                }
                MatchOperator::ScanRelationship { slot } => {
                    let alias = format!("r{}", slot.0);
                    self.insert_scan(*slot, EntityKind::Relationship, alias.clone())?;
                    self.from.push(format!("edges AS {alias}"));
                }
                MatchOperator::HasLabel { slot, label } => {
                    let alias = self.alias(*slot)?.to_owned();
                    let value = self.push_binding(SqlBinding::Text(label.clone()));
                    self.filters.push(format!("{alias}.label = {value}"));
                }
                MatchOperator::HasRelationshipType {
                    slot,
                    relationship_type,
                } => {
                    let alias = self.alias(*slot)?.to_owned();
                    let value = self.push_binding(SqlBinding::Text(relationship_type.clone()));
                    self.filters.push(format!("{alias}.edge_type = {value}"));
                }
                MatchOperator::PropertyEquals { slot, key, value } => {
                    let alias = self.alias(*slot)?.to_owned();
                    let (property, property_type) = self.property_parts(&alias, key);
                    let comparison = self.typed_value_comparison(
                        property,
                        property_type,
                        value.clone(),
                        BinaryOp::Equal,
                    );
                    self.filters.push(comparison);
                }
                MatchOperator::ExpandRelationship {
                    relationship,
                    left,
                    right,
                    direction,
                } => {
                    let relationship = self.alias(*relationship)?;
                    let left = self.alias(*left)?;
                    let right = self.alias(*right)?;
                    let forward = format!(
                        "({relationship}.from_node = {left}.id AND \
                         {relationship}.to_node = {right}.id)"
                    );
                    let reverse = format!(
                        "({relationship}.from_node = {right}.id AND \
                         {relationship}.to_node = {left}.id)"
                    );
                    self.filters.push(match direction {
                        Direction::Right => forward,
                        Direction::Left => reverse,
                        Direction::Undirected => format!("({forward} OR {reverse})"),
                    });
                }
                MatchOperator::DistinctRelationships(slots) => {
                    for (index, slot) in slots.iter().enumerate() {
                        for previous in &slots[..index] {
                            self.filters.push(format!(
                                "{}.id <> {}.id",
                                self.alias(*slot)?,
                                self.alias(*previous)?
                            ));
                        }
                    }
                }
                MatchOperator::Filter(expression) => {
                    let predicate = self.raw_expression(expression)?;
                    self.filters.push(format!("({predicate})"));
                }
            }
        }
        Ok(())
    }

    fn insert_scan(&mut self, slot: SlotId, kind: EntityKind, alias: String) -> Result<()> {
        if self.aliases.insert(slot, alias).is_some() {
            return Err(Error::Data("logical plan scans an entity twice".into()));
        }
        self.scanned.push((slot, kind));
        Ok(())
    }

    fn select_sql(&self, select: &[String]) -> String {
        let mut sql = format!("SELECT {}", select.join(", "));
        if !self.from.is_empty() {
            sql.push_str(" FROM ");
            sql.push_str(&self.from.join(", "));
        }
        if !self.filters.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&self.filters.join(" AND "));
        }
        sql
    }

    fn alias(&self, slot: SlotId) -> Result<&str> {
        self.aliases
            .get(&slot)
            .map(String::as_str)
            .ok_or_else(|| Error::Data(format!("logical slot {} has no SQL scan", slot.0)))
    }

    fn entity_kind(&self, slot: SlotId) -> Result<EntityKind> {
        self.scanned
            .iter()
            .find_map(|(candidate, kind)| (*candidate == slot).then_some(*kind))
            .ok_or_else(|| Error::Data(format!("logical slot {} has no entity kind", slot.0)))
    }

    fn encoded_entity(&self, slot: SlotId, kind: EntityKind) -> Result<String> {
        let alias = self.alias(slot)?;
        let record = match kind {
            EntityKind::Node => format!(
                "json_object('id', {alias}.id, 'label', {alias}.label, \
                 'props', json({alias}.props))"
            ),
            EntityKind::Relationship => format!(
                "json_object('id', {alias}.id, 'type', {alias}.edge_type, \
                 'from', {alias}.from_node, 'to', {alias}.to_node, \
                 'props', json({alias}.props))"
            ),
        };
        Ok(format!(
            "CASE WHEN {alias}.id IS NULL THEN 'null' ELSE {record} END"
        ))
    }

    fn output_expression(&mut self, expression: &Expr) -> Result<(String, OutputShape)> {
        match expression {
            Expr::Entity(slot) => {
                let kind = self.entity_kind(*slot)?;
                Ok((self.encoded_entity(*slot, kind)?, OutputShape::Entity(kind)))
            }
            Expr::Property(slot, key) => {
                let alias = self.alias(*slot)?.to_owned();
                let path = self.push_binding(SqlBinding::Text(key.clone()));
                Ok((
                    format!("COALESCE({alias}.props -> ('$.' || json_quote({path})), 'null')"),
                    OutputShape::Json,
                ))
            }
            Expr::ElementId(slot) => Ok((
                format!("json_quote({}.id)", self.alias(*slot)?),
                OutputShape::Json,
            )),
            Expr::RelationshipType(slot) => Ok((
                format!("json_quote({}.edge_type)", self.alias(*slot)?),
                OutputShape::Json,
            )),
            Expr::Labels(slot) => {
                let alias = self.alias(*slot)?;
                Ok((
                    format!(
                        "CASE WHEN {alias}.label IS NULL THEN '[]' \
                         ELSE json_array({alias}.label) END"
                    ),
                    OutputShape::Json,
                ))
            }
            Expr::Value(value) => Ok((
                self.push_binding(SqlBinding::Json(value.clone())),
                OutputShape::Json,
            )),
            _ => Err(Error::Data(
                "logical projection contains a non-output expression".into(),
            )),
        }
    }

    fn raw_expression(&mut self, expression: &Expr) -> Result<String> {
        match expression {
            Expr::Entity(_) => Err(Error::Unsupported("graph entities in ORDER BY".to_owned())),
            Expr::Value(value) => Ok(self.push_binding(SqlBinding::Scalar(value.clone()))),
            Expr::Property(slot, key) => {
                let alias = self.alias(*slot)?.to_owned();
                Ok(self.property_parts(&alias, key).0)
            }
            Expr::ElementId(slot) => Ok(format!("{}.id", self.alias(*slot)?)),
            Expr::RelationshipType(slot) => Ok(format!("{}.edge_type", self.alias(*slot)?)),
            Expr::Labels(slot) => {
                let alias = self.alias(*slot)?;
                Ok(format!(
                    "CASE WHEN {alias}.label IS NULL THEN '[]' \
                     ELSE json_array({alias}.label) END"
                ))
            }
            Expr::Binary {
                left,
                operator,
                right,
            } => {
                if matches!(operator, BinaryOp::Equal | BinaryOp::NotEqual) {
                    return self
                        .typed_comparison(left, right, *operator)?
                        .ok_or_else(|| Error::Unsupported("complex equality expression".into()));
                }
                let left = self.raw_expression(left)?;
                let right = self.raw_expression(right)?;
                let operator = match operator {
                    BinaryOp::Or => "OR",
                    BinaryOp::And => "AND",
                    BinaryOp::Less => "<",
                    BinaryOp::LessEqual => "<=",
                    BinaryOp::Greater => ">",
                    BinaryOp::GreaterEqual => ">=",
                    BinaryOp::Equal | BinaryOp::NotEqual => unreachable!(),
                };
                Ok(format!("({left} {operator} {right})"))
            }
            Expr::Unary { operator, operand } => {
                let operand = self.raw_expression(operand)?;
                let operator = match operator {
                    UnaryOp::Plus => "+",
                    UnaryOp::Minus => "-",
                    UnaryOp::Not => "NOT ",
                };
                Ok(format!("({operator}{operand})"))
            }
            Expr::IsNull {
                expression,
                negated,
            } => {
                let expression = self.raw_expression(expression)?;
                Ok(format!(
                    "({expression} IS {}NULL)",
                    if *negated { "NOT " } else { "" }
                ))
            }
        }
    }

    fn property_parts(&mut self, alias: &str, key: &str) -> (String, String) {
        let path = self.push_binding(SqlBinding::Text(key.to_owned()));
        let json_path = format!("'$.' || json_quote({path})");
        (
            format!("json_extract({alias}.props, {json_path})"),
            format!("json_type({alias}.props, {json_path})"),
        )
    }

    fn typed_comparison(
        &mut self,
        left: &Expr,
        right: &Expr,
        operator: BinaryOp,
    ) -> Result<Option<String>> {
        if let Expr::Property(slot, key) = left {
            let alias = self.alias(*slot)?.to_owned();
            let (property, property_type) = self.property_parts(&alias, key);
            if let Expr::Value(expected) = right {
                return Ok(Some(self.typed_value_comparison(
                    property,
                    property_type,
                    expected.clone(),
                    operator,
                )));
            }
            if let Expr::Property(right_slot, right_key) = right {
                let right_alias = self.alias(*right_slot)?.to_owned();
                let (right_value, right_type) = self.property_parts(&right_alias, right_key);
                return Ok(Some(typed_sql_comparison(
                    &property,
                    &property_type,
                    &right_value,
                    &right_type,
                    operator,
                )));
            }
        }

        if let (Expr::Value(expected), Expr::Property(slot, key)) = (left, right) {
            let alias = self.alias(*slot)?.to_owned();
            let (property, property_type) = self.property_parts(&alias, key);
            return Ok(Some(self.typed_value_comparison(
                property,
                property_type,
                expected.clone(),
                operator,
            )));
        }

        if let (Expr::Value(left), Expr::Value(right)) = (left, right) {
            let left_scalar = self.push_binding(SqlBinding::Scalar(left.clone()));
            let left_json = self.push_binding(SqlBinding::Json(left.clone()));
            let right_scalar = self.push_binding(SqlBinding::Scalar(right.clone()));
            let right_json = self.push_binding(SqlBinding::Json(right.clone()));
            return Ok(Some(typed_sql_comparison(
                &left_scalar,
                &format!("json_type(json({left_json}))"),
                &right_scalar,
                &format!("json_type(json({right_json}))"),
                operator,
            )));
        }

        Ok(None)
    }

    fn typed_value_comparison(
        &mut self,
        property: String,
        property_type: String,
        expected: ValueExpr,
        operator: BinaryOp,
    ) -> String {
        let expected_scalar = self.push_binding(SqlBinding::Scalar(expected.clone()));
        let expected_json = self.push_binding(SqlBinding::Json(expected));
        let expected_type = format!("json_type(json({expected_json}))");
        typed_sql_comparison(
            &property,
            &property_type,
            &expected_scalar,
            &expected_type,
            operator,
        )
    }

    fn push_binding(&mut self, binding: SqlBinding) -> String {
        self.bindings.push(binding);
        format!("?{}", self.bindings.len())
    }
}

fn typed_sql_comparison(
    left: &str,
    left_type: &str,
    right: &str,
    right_type: &str,
    operator: BinaryOp,
) -> String {
    let compatible_types = format!(
        "((({left_type}) IN ('integer', 'real') AND ({right_type}) IN ('integer', 'real')) \
         OR (({left_type}) IN ('true', 'false') AND ({right_type}) IN ('true', 'false')) \
         OR ({left_type}) = ({right_type}))"
    );
    let equal = format!("(({left}) = ({right}) AND {compatible_types})");
    let comparison = match operator {
        BinaryOp::Equal => equal,
        BinaryOp::NotEqual => format!("NOT {equal}"),
        _ => unreachable!("typed comparisons are only generated for equality operators"),
    };
    format!("(CASE WHEN ({left}) IS NULL OR ({right}) IS NULL THEN NULL ELSE {comparison} END)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::lower;

    #[test]
    fn generated_sql_uses_internal_aliases_and_bound_property_names() {
        let query =
            lower::lower("MATCH (`odd variable`:Person) RETURN `odd variable`.`odd.key` AS value")
                .unwrap();
        let projection = query.projection.as_ref().unwrap();
        let plan = compile_read(&query.input, projection).unwrap();

        assert!(plan.sql.contains("nodes AS n0"));
        assert!(!plan.sql.contains("odd variable"));
        assert!(!plan.sql.contains("odd.key"));
        assert_eq!(plan.columns, ["value"]);
    }

    #[test]
    fn relationship_joins_are_lowered_to_endpoint_predicates() {
        let query = lower::lower("MATCH (a)-[r:KNOWS]->(b) RETURN a.name, b.name").unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert!(plan.sql.contains("edges AS r1"));
        assert!(
            plan.sql
                .contains("r1.from_node = n0.id AND r1.to_node = n2.id")
        );
        assert!(!plan.sql.contains("KNOWS"));
    }

    #[test]
    fn whole_entities_have_explicit_output_shapes_and_null_guards() {
        let query = lower::lower("MATCH (a)-[r]->(b) RETURN a AS node, r AS relationship").unwrap();
        let plan = compile_read(&query.input, query.projection.as_ref().unwrap()).unwrap();

        assert_eq!(
            plan.output_shapes,
            [
                OutputShape::Entity(EntityKind::Node),
                OutputShape::Entity(EntityKind::Relationship),
            ]
        );
        assert!(
            plan.sql
                .contains("CASE WHEN n0.id IS NULL THEN 'null' ELSE json_object")
        );
        assert!(
            plan.sql
                .contains("CASE WHEN r1.id IS NULL THEN 'null' ELSE json_object")
        );
        assert!(plan.sql.contains("'from', r1.from_node"));
        assert!(plan.sql.contains("'to', r1.to_node"));
    }
}
