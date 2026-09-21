//! The openCypher TCK's expected-value notation: Cypher-like literals plus
//! structural node `(:L {k: v})`, relationship `[:T {k: v}]`, and path
//! `<(...)-[...]->(...)>` forms. Graph values compare structurally, never by
//! element ID, exactly as the TCK specifies.

use std::collections::{BTreeMap, BTreeSet};

use crabnet::{CypherValue, NodeValue, PathValue, RelationshipValue};

#[derive(Clone, Debug)]
pub enum Expected {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    List(Vec<Expected>),
    Map(BTreeMap<String, Expected>),
    Node(ExpectedNode),
    Relationship(ExpectedRelationship),
    Path(ExpectedPath),
}

#[derive(Clone, Debug)]
pub struct ExpectedNode {
    labels: BTreeSet<String>,
    properties: BTreeMap<String, Expected>,
}

#[derive(Clone, Debug)]
pub struct ExpectedRelationship {
    relationship_type: String,
    properties: BTreeMap<String, Expected>,
}

#[derive(Clone, Debug)]
pub struct ExpectedPath {
    nodes: Vec<ExpectedNode>,
    /// Each relationship with `true` when it points from the node before it
    /// to the node after it in path order.
    relationships: Vec<(ExpectedRelationship, bool)>,
}

pub fn parse(text: &str) -> Result<Expected, String> {
    let mut parser = Parser {
        chars: text.chars().collect(),
        position: 0,
    };
    let value = parser.value()?;
    parser.skip_space();
    if parser.position != parser.chars.len() {
        return Err(format!("trailing input in `{text}`"));
    }
    Ok(value)
}

struct Parser {
    chars: Vec<char>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.position + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += 1;
        Some(character)
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.position += 1;
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), String> {
        self.skip_space();
        match self.bump() {
            Some(character) if character == expected => Ok(()),
            other => Err(format!(
                "expected `{expected}` at {} but found {other:?}",
                self.position
            )),
        }
    }

    fn eat(&mut self, expected: char) -> bool {
        self.skip_space();
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn starts_with(&self, word: &str) -> bool {
        word.chars()
            .enumerate()
            .all(|(offset, character)| self.peek_at(offset) == Some(character))
    }

    fn value(&mut self) -> Result<Expected, String> {
        self.skip_space();
        match self.peek() {
            None => Err("unexpected end of value".into()),
            Some('\'') => self.string().map(Expected::Text),
            Some('[') => self.list(),
            Some('{') => self.map().map(Expected::Map),
            Some('(') => self.node().map(Expected::Node),
            Some('<') => self.path().map(Expected::Path),
            Some(character) if character == '-' || character.is_ascii_digit() => self.number(),
            Some(_) => self.word(),
        }
    }

    fn word(&mut self) -> Result<Expected, String> {
        for (word, value) in [
            ("null", Expected::Null),
            ("true", Expected::Boolean(true)),
            ("false", Expected::Boolean(false)),
            ("NaN", Expected::Float(f64::NAN)),
            ("Inf", Expected::Float(f64::INFINITY)),
        ] {
            if self.starts_with(word) {
                self.position += word.len();
                return Ok(value);
            }
        }
        Err(format!("unrecognised value at {}", self.position))
    }

    fn number(&mut self) -> Result<Expected, String> {
        let start = self.position;
        if self.peek() == Some('-') {
            self.position += 1;
            if self.starts_with("Inf") {
                self.position += 3;
                return Ok(Expected::Float(f64::NEG_INFINITY));
            }
        }
        let mut is_float = false;
        while let Some(character) = self.peek() {
            match character {
                '0'..='9' => {}
                '.' if self.peek_at(1).is_some_and(|next| next.is_ascii_digit()) => {
                    is_float = true;
                }
                'e' | 'E' => {
                    is_float = true;
                    if matches!(self.peek_at(1), Some('+' | '-')) {
                        self.position += 1;
                    }
                }
                _ => break,
            }
            self.position += 1;
        }
        let text = self.chars[start..self.position].iter().collect::<String>();
        if is_float {
            text.parse()
                .map(Expected::Float)
                .map_err(|error| format!("bad float `{text}`: {error}"))
        } else {
            text.parse()
                .map(Expected::Integer)
                .map_err(|error| format!("bad integer `{text}`: {error}"))
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('\'')?;
        let mut text = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".into()),
                Some('\'') => return Ok(text),
                Some('\\') => match self.bump() {
                    Some('n') => text.push('\n'),
                    Some('t') => text.push('\t'),
                    Some('r') => text.push('\r'),
                    Some('b') => text.push('\u{0008}'),
                    Some('f') => text.push('\u{000c}'),
                    Some(other) => text.push(other),
                    None => return Err("unterminated escape".into()),
                },
                Some(other) => text.push(other),
            }
        }
    }

    fn identifier(&mut self) -> Result<String, String> {
        self.skip_space();
        if self.peek() == Some('`') {
            self.position += 1;
            let mut name = String::new();
            loop {
                match self.bump() {
                    None => return Err("unterminated backtick name".into()),
                    Some('`') if self.peek() == Some('`') => {
                        self.position += 1;
                        name.push('`');
                    }
                    Some('`') => return Ok(name),
                    Some(other) => name.push(other),
                }
            }
        }
        let start = self.position;
        while self
            .peek()
            .is_some_and(|character| character.is_alphanumeric() || character == '_')
        {
            self.position += 1;
        }
        if start == self.position {
            return Err(format!("expected a name at {start}"));
        }
        Ok(self.chars[start..self.position].iter().collect())
    }

    fn list(&mut self) -> Result<Expected, String> {
        self.expect('[')?;
        // A relationship is `[:TYPE {...}]`; a list is anything else.
        self.skip_space();
        if self.peek() == Some(':') {
            let relationship = self.relationship_body()?;
            self.expect(']')?;
            return Ok(Expected::Relationship(relationship));
        }
        let mut items = Vec::new();
        if self.eat(']') {
            return Ok(Expected::List(items));
        }
        loop {
            items.push(self.value()?);
            if self.eat(',') {
                continue;
            }
            self.expect(']')?;
            return Ok(Expected::List(items));
        }
    }

    fn map(&mut self) -> Result<BTreeMap<String, Expected>, String> {
        self.expect('{')?;
        let mut entries = BTreeMap::new();
        if self.eat('}') {
            return Ok(entries);
        }
        loop {
            let key = self.identifier()?;
            self.expect(':')?;
            entries.insert(key, self.value()?);
            if self.eat(',') {
                continue;
            }
            self.expect('}')?;
            return Ok(entries);
        }
    }

    fn node(&mut self) -> Result<ExpectedNode, String> {
        self.expect('(')?;
        let mut labels = BTreeSet::new();
        while self.eat(':') {
            labels.insert(self.identifier()?);
        }
        self.skip_space();
        let properties = if self.peek() == Some('{') {
            self.map()?
        } else {
            BTreeMap::new()
        };
        self.expect(')')?;
        Ok(ExpectedNode { labels, properties })
    }

    fn relationship_body(&mut self) -> Result<ExpectedRelationship, String> {
        self.expect(':')?;
        let relationship_type = self.identifier()?;
        self.skip_space();
        let properties = if self.peek() == Some('{') {
            self.map()?
        } else {
            BTreeMap::new()
        };
        Ok(ExpectedRelationship {
            relationship_type,
            properties,
        })
    }

    fn path(&mut self) -> Result<ExpectedPath, String> {
        self.expect('<')?;
        let mut nodes = vec![self.node()?];
        let mut relationships = Vec::new();
        loop {
            self.skip_space();
            let incoming = if self.starts_with("<-") {
                self.position += 2;
                true
            } else if self.eat('-') {
                false
            } else {
                break;
            };
            self.expect('[')?;
            let relationship = self.relationship_body()?;
            self.expect(']')?;
            self.expect('-')?;
            let outgoing = self.eat('>');
            if incoming == outgoing {
                return Err("a path relationship needs exactly one arrow head".into());
            }
            relationships.push((relationship, outgoing));
            nodes.push(self.node()?);
        }
        self.expect('>')?;
        Ok(ExpectedPath {
            nodes,
            relationships,
        })
    }
}

impl Expected {
    /// The JSON form used for query parameters; graph values have none.
    pub fn to_json(&self) -> Option<serde_json::Value> {
        Some(match self {
            Expected::Null => serde_json::Value::Null,
            Expected::Boolean(value) => serde_json::Value::Bool(*value),
            Expected::Integer(value) => serde_json::Value::from(*value),
            Expected::Float(value) => serde_json::Value::from(*value),
            Expected::Text(value) => serde_json::Value::String(value.clone()),
            Expected::List(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(Expected::to_json)
                    .collect::<Option<Vec<_>>>()?,
            ),
            Expected::Map(entries) => serde_json::Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| value.to_json().map(|value| (key.clone(), value)))
                    .collect::<Option<_>>()?,
            ),
            Expected::Node(_) | Expected::Relationship(_) | Expected::Path(_) => return None,
        })
    }

    pub fn matches(&self, actual: &CypherValue, ignore_list_order: bool) -> bool {
        match (self, actual) {
            (Expected::Null, CypherValue::Null) => true,
            (Expected::Boolean(expected), CypherValue::Boolean(actual)) => expected == actual,
            (Expected::Integer(expected), CypherValue::Integer(actual)) => expected == actual,
            (Expected::Float(expected), CypherValue::Float(actual)) => {
                expected == actual || (expected.is_nan() && actual.is_nan())
            }
            (Expected::Text(expected), CypherValue::String(actual)) => expected == actual,
            (Expected::List(expected), CypherValue::List(actual)) => {
                if ignore_list_order {
                    multiset_matches(expected, actual, ignore_list_order)
                } else {
                    expected.len() == actual.len()
                        && expected
                            .iter()
                            .zip(actual)
                            .all(|(expected, actual)| expected.matches(actual, ignore_list_order))
                }
            }
            (Expected::Map(expected), CypherValue::Map(actual)) => {
                map_matches(expected, actual, ignore_list_order)
            }
            (Expected::Node(expected), CypherValue::Node(actual)) => {
                expected.matches(actual, ignore_list_order)
            }
            (Expected::Relationship(expected), CypherValue::Relationship(actual)) => {
                expected.matches(actual, ignore_list_order)
            }
            (Expected::Path(expected), CypherValue::Path(actual)) => {
                expected.matches(actual, ignore_list_order)
            }
            _ => false,
        }
    }
}

impl ExpectedNode {
    fn matches(&self, actual: &NodeValue, ignore_list_order: bool) -> bool {
        let actual_labels = actual.labels.iter().cloned().collect::<BTreeSet<_>>();
        self.labels == actual_labels
            && map_matches(&self.properties, &actual.properties, ignore_list_order)
    }
}

impl ExpectedRelationship {
    fn matches(&self, actual: &RelationshipValue, ignore_list_order: bool) -> bool {
        self.relationship_type == actual.relationship_type
            && map_matches(&self.properties, &actual.properties, ignore_list_order)
    }
}

impl ExpectedPath {
    fn matches(&self, actual: &PathValue, ignore_list_order: bool) -> bool {
        if self.nodes.len() != actual.nodes.len()
            || self.relationships.len() != actual.relationships.len()
        {
            return false;
        }
        let nodes_match = self
            .nodes
            .iter()
            .zip(&actual.nodes)
            .all(|(expected, actual)| expected.matches(actual, ignore_list_order));
        let relationships_match =
            self.relationships
                .iter()
                .enumerate()
                .all(|(index, (expected, outgoing))| {
                    let actual_relationship = &actual.relationships[index];
                    let left = &actual.nodes[index].element_id;
                    let right = &actual.nodes[index + 1].element_id;
                    let (start, end) = if *outgoing {
                        (left, right)
                    } else {
                        (right, left)
                    };
                    expected.matches(actual_relationship, ignore_list_order)
                        && actual_relationship.start_node_element_id == *start
                        && actual_relationship.end_node_element_id == *end
                });
        nodes_match && relationships_match
    }
}

fn map_matches(
    expected: &BTreeMap<String, Expected>,
    actual: &BTreeMap<String, CypherValue>,
    ignore_list_order: bool,
) -> bool {
    expected.len() == actual.len()
        && expected.iter().all(|(key, expected)| {
            actual
                .get(key)
                .is_some_and(|actual| expected.matches(actual, ignore_list_order))
        })
}

/// Match `expected` against `actual` as bags, consuming each actual at most
/// once. Rows and unordered lists both use this.
pub fn multiset_matches(
    expected: &[Expected],
    actual: &[CypherValue],
    ignore_list_order: bool,
) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let mut remaining = actual.iter().collect::<Vec<_>>();
    for expected in expected {
        let Some(position) = remaining
            .iter()
            .position(|actual| expected.matches(actual, ignore_list_order))
        else {
            return false;
        };
        remaining.swap_remove(position);
    }
    true
}
