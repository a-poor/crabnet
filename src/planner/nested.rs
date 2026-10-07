//! Type information for collections that contain graph values. Their private
//! tagged transport is separate from property JSON, including user maps that
//! happen to look like entity records.
use std::collections::BTreeMap;

use super::logical::{EntityKind, Expr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Json,
    Null,
    Entity(EntityKind),
    Path,
    List(Box<Self>),
    Tuple(Vec<Self>),
    Map(BTreeMap<String, Self>),
    Mixed,
}

impl Shape {
    /// Concatenation flattens one list level. Unknown operands may themselves
    /// be lists, so their positions and member kinds cannot be fixed yet.
    pub(crate) fn concatenate(self, right: Self) -> Option<Self> {
        if self == Self::Null || right == Self::Null {
            return Some(Self::Null);
        }
        match (self, right) {
            (Self::Tuple(mut left), Self::Tuple(right)) => {
                left.extend(right);
                Some(Self::Tuple(left))
            }
            (Self::Tuple(mut left), right) if !matches!(right, Self::List(_) | Self::Mixed) => {
                left.push(right);
                Some(Self::Tuple(left))
            }
            (left, Self::Tuple(mut right)) if !matches!(left, Self::List(_) | Self::Mixed) => {
                right.insert(0, left);
                Some(Self::Tuple(right))
            }
            (left, right)
                if matches!(left, Self::List(_) | Self::Tuple(_))
                    || matches!(right, Self::List(_) | Self::Tuple(_)) =>
            {
                let members = |shape| match shape {
                    Self::List(element) => vec![*element],
                    Self::Tuple(elements) => elements,
                    other => vec![other],
                };
                let mut shapes = members(left);
                shapes.extend(members(right));
                Some(Self::List(Box::new(Self::merge(shapes.iter()))))
            }
            (Self::Mixed, _) | (_, Self::Mixed) => Some(Self::Mixed),
            _ => None,
        }
    }

    pub(crate) fn contains_graph(&self) -> bool {
        match self {
            Self::Json | Self::Null => false,
            Self::List(value) => value.contains_graph(),
            Self::Tuple(values) => values.iter().any(Self::contains_graph),
            Self::Map(values) => values.values().any(Self::contains_graph),
            _ => true,
        }
    }
    pub(crate) fn element(&self) -> Self {
        match self {
            Self::List(element) => *element.clone(),
            Self::Tuple(elements) => Self::merge(elements.iter()),
            _ => Self::Mixed,
        }
    }

    pub(crate) fn merge<'a>(shapes: impl IntoIterator<Item = &'a Self>) -> Self {
        let mut shapes = shapes.into_iter().filter(|shape| **shape != Self::Null);
        let Some(first) = shapes.next() else {
            return Self::Json;
        };
        if shapes.all(|shape| shape == first) {
            first.clone()
        } else {
            Self::Mixed
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Json,
    Node,
    Relationship,
    Nodes,
    Relationships,
    Path,
    List,
    Map,
    Payload,
    ToJson,
    Elements,
    EntityList,
    Get,
    Property,
    Size,
    Equal,
    In,
    Add,
    Reverse,
    ElementId,
    Labels,
    RelationshipType,
    Properties,
    Keys,
    HasLabel,
    Length,
    PathNodes,
    PathRelationships,
}

impl Op {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Node => "node",
            Self::Relationship => "relationship",
            Self::Nodes => "nodes",
            Self::Relationships => "relationships",
            Self::Path => "path",
            Self::List => "list",
            Self::Map => "map",
            Self::Payload => "payload",
            Self::ToJson => "to_json",
            Self::Elements => "elements",
            Self::EntityList => "entity_list",
            Self::Get => "get",
            Self::Property => "property",
            Self::Size => "size",
            Self::Equal => "equal",
            Self::In => "in",
            Self::Add => "add",
            Self::Reverse => "reverse",
            Self::ElementId => "element_id",
            Self::Labels => "labels",
            Self::RelationshipType => "relationship_type",
            Self::Properties => "properties",
            Self::Keys => "keys",
            Self::HasLabel => "has_label",
            Self::Length => "length",
            Self::PathNodes => "path_nodes",
            Self::PathRelationships => "path_relationships",
        }
    }
}

pub(crate) fn call(op: Op, arguments: Vec<Expr>) -> Expr {
    Expr::NestedCall { op, arguments }
}

/// Recover a statically known entity or property value after selecting a
/// tagged member. Containers and heterogeneous selections retain their tag.
pub(crate) fn restore(shape: Shape, expression: Expr) -> Expr {
    match shape {
        Shape::Json | Shape::Null => call(Op::ToJson, vec![expression]),
        Shape::Entity(kind) => Expr::EntityValue {
            kind,
            expression: Box::new(call(Op::Payload, vec![expression])),
        },
        shape => Expr::Nested {
            shape: Box::new(shape),
            expression: Box::new(expression),
        },
    }
}
