//! A pinned, curated openCypher TCK run.
//!
//! Every scenario in the vendored `openCypher-2024.3` feature files executes
//! against an in-memory database. `tests/tck/allowlist.txt` names the
//! scenarios that must pass; `tests/tck/exclusions.txt` names every other
//! scenario with the observed reason it does not. The test fails when either
//! file disagrees with what actually happens, in either direction, so a
//! regression, a newly passing scenario, and a changed failure reason are all
//! visible. Set `CRABNET_TCK_UPDATE=1` to rewrite both files from a run, then
//! review the diff.

#[path = "tck/gherkin.rs"]
mod gherkin;
#[path = "tck/notation.rs"]
mod notation;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use crabnet::{CypherValue, Database, Error, Parameters, QueryResult, Result};
use gherkin::{Scenario, Step};
use notation::Expected;

const VENDOR: &str = "tests/tck/vendor/openCypher-2024.3/tck";
const ALLOWLIST: &str = "tests/tck/allowlist.txt";
const EXCLUSIONS: &str = "tests/tck/exclusions.txt";

#[derive(Debug, PartialEq, Eq, Clone)]
enum Outcome {
    Passed,
    Excluded(String),
}

#[test]
fn pinned_tck_allowlist_and_exclusions_match_observed_behavior() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scenarios = load_scenarios(&root.join(VENDOR).join("features"));
    assert!(
        scenarios.len() > 1000,
        "expected the full vendored TCK, found {} scenarios",
        scenarios.len()
    );

    let trace = std::env::var_os("CRABNET_TCK_TRACE").is_some();
    let observed = scenarios
        .iter()
        .map(|scenario| {
            if trace {
                eprintln!("tck: {}", scenario.id);
            }
            (scenario.id.clone(), run_scenario(root, scenario))
        })
        .collect::<BTreeMap<_, _>>();
    let passed = observed
        .values()
        .filter(|outcome| **outcome == Outcome::Passed)
        .count();
    eprintln!(
        "openCypher TCK: {passed} passed, {} excluded, {} total",
        observed.len() - passed,
        observed.len()
    );

    if std::env::var_os("CRABNET_TCK_UPDATE").is_some() {
        write_pins(root, &observed);
        return;
    }

    let pinned = read_pins(root);
    let mut problems = Vec::new();
    for (id, outcome) in &observed {
        match (pinned.get(id), outcome) {
            (Some(Outcome::Passed), Outcome::Passed) => {}
            (Some(Outcome::Excluded(pinned_reason)), Outcome::Excluded(reason))
                if pinned_reason == reason => {}
            (None, _) => problems.push(format!("not pinned: {id} -> {outcome:?}")),
            (Some(Outcome::Passed), Outcome::Excluded(reason)) => {
                problems.push(format!("regression: {id} -> {reason}"));
            }
            (Some(Outcome::Excluded(_)), Outcome::Passed) => {
                problems.push(format!("newly passing, promote to the allowlist: {id}"));
            }
            (Some(Outcome::Excluded(pinned_reason)), Outcome::Excluded(reason)) => {
                problems.push(format!(
                    "reason changed: {id}\n    pinned:   {pinned_reason}\n    observed: {reason}"
                ));
            }
        }
    }
    for id in pinned.keys() {
        if !observed.contains_key(id) {
            problems.push(format!("pinned scenario no longer exists: {id}"));
        }
    }
    assert!(
        problems.is_empty(),
        "{} TCK pin mismatches (rerun with CRABNET_TCK_UPDATE=1 and review the diff):\n{}",
        problems.len(),
        problems.join("\n")
    );
    assert!(passed > 0, "the allowlist must not be empty");
}

fn load_scenarios(features: &Path) -> Vec<Scenario> {
    let mut files = Vec::new();
    collect_feature_files(features, &mut files);
    files.sort();
    files
        .iter()
        .flat_map(|file| {
            let relative = file
                .strip_prefix(features)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(file).unwrap();
            gherkin::parse_feature(&relative, &source).scenarios
        })
        .collect()
}

fn collect_feature_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_feature_files(&path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "feature")
        {
            files.push(path);
        }
    }
}

fn read_pins(root: &Path) -> BTreeMap<String, Outcome> {
    let mut pins = BTreeMap::new();
    for line in fs::read_to_string(root.join(ALLOWLIST))
        .unwrap_or_default()
        .lines()
    {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            pins.insert(line.to_owned(), Outcome::Passed);
        }
    }
    for line in fs::read_to_string(root.join(EXCLUSIONS))
        .unwrap_or_default()
        .lines()
    {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (id, reason) = line
            .split_once('\t')
            .unwrap_or_else(|| panic!("exclusion line without a reason: {line}"));
        pins.insert(id.to_owned(), Outcome::Excluded(reason.to_owned()));
    }
    pins
}

fn write_pins(root: &Path, observed: &BTreeMap<String, Outcome>) {
    let mut allowlist = String::from(
        "# openCypher 2024.3 TCK scenarios that must pass. Generated by\n\
         # `CRABNET_TCK_UPDATE=1 cargo test --test tck`; review every change.\n",
    );
    let mut exclusions = String::from(
        "# openCypher 2024.3 TCK scenarios that do not pass yet, each with the\n\
         # observed reason. Generated by `CRABNET_TCK_UPDATE=1 cargo test --test tck`.\n",
    );
    for (id, outcome) in observed {
        match outcome {
            Outcome::Passed => {
                allowlist.push_str(id);
                allowlist.push('\n');
            }
            Outcome::Excluded(reason) => {
                exclusions.push_str(&format!("{id}\t{reason}\n"));
            }
        }
    }
    fs::write(root.join(ALLOWLIST), allowlist).unwrap();
    fs::write(root.join(EXCLUSIONS), exclusions).unwrap();
}

// ---------------------------------------------------------------------------
// Scenario execution

struct Runner<'a> {
    root: &'a Path,
    database: Database,
    parameters: Parameters,
    last: Option<Result<QueryResult>>,
    before: Option<Snapshot>,
    after: Option<Snapshot>,
}

fn run_scenario(root: &Path, scenario: &Scenario) -> Outcome {
    let mut runner = Runner {
        root,
        database: Database::in_memory().unwrap(),
        parameters: Parameters::new(),
        last: None,
        before: None,
        after: None,
    };
    for step in &scenario.steps {
        if let Err(reason) = runner.step(step) {
            return Outcome::Excluded(reason);
        }
    }
    Outcome::Passed
}

impl Runner<'_> {
    fn step(&mut self, step: &Step) -> std::result::Result<(), String> {
        let text = step.text.as_str();
        if text == "an empty graph" || text == "any graph" {
            return Ok(());
        }
        if let Some(graph) = text
            .strip_prefix("the ")
            .and_then(|rest| rest.strip_suffix(" graph"))
        {
            let path = self
                .root
                .join(VENDOR)
                .join("graphs")
                .join(graph)
                .join(format!("{graph}.cypher"));
            let source = fs::read_to_string(&path)
                .map_err(|error| format!("unsupported-step: named graph {graph}: {error}"))?;
            return self.setup(&source);
        }
        if text == "having executed:" {
            return self.setup(step.docstring.as_deref().unwrap_or_default());
        }
        if text == "parameters are:" {
            for row in &step.table {
                let [name, value] = row.as_slice() else {
                    return Err("unsupported-step: malformed parameter row".into());
                };
                let value = notation::parse(value)
                    .map_err(|error| format!("unsupported-step: parameter notation: {error}"))?
                    .to_json()
                    .ok_or_else(|| "unsupported-step: graph-valued parameter".to_owned())?;
                self.parameters.insert(name.clone(), value);
            }
            return Ok(());
        }
        if text == "executing query:" {
            let query = step.docstring.as_deref().unwrap_or_default();
            self.before = Some(self.snapshot()?);
            self.last = Some(self.database.execute_with(query, &self.parameters));
            self.after = Some(self.snapshot()?);
            return Ok(());
        }
        if text == "executing control query:" {
            let query = step.docstring.as_deref().unwrap_or_default();
            self.last = Some(self.database.execute_with(query, &self.parameters));
            return Ok(());
        }
        if let Some(rest) = text.strip_prefix("the result should be") {
            let ignore_list_order = rest.contains("ignoring element order for lists");
            let ordered = rest.contains("in order");
            let expected = if rest.trim().starts_with("empty") {
                None
            } else {
                Some(&step.table)
            };
            return self.check_result(expected, ordered, ignore_list_order);
        }
        if text == "no side effects" {
            return self.check_side_effects(&SideEffects::default());
        }
        if text == "the side effects should be:" {
            let mut expected = SideEffects::default();
            for row in &step.table {
                let [name, count] = row.as_slice() else {
                    return Err("unsupported-step: malformed side-effect row".into());
                };
                let count = count
                    .parse()
                    .map_err(|_| format!("unsupported-step: side-effect count {count}"))?;
                expected.set(name, count)?;
            }
            return self.check_side_effects(&expected);
        }
        if text.starts_with("a ") && text.contains(" should be raised at ") {
            let compile_time = text.contains("at compile time");
            return self.check_error(compile_time);
        }
        Err(format!("unsupported-step: {text}"))
    }

    fn setup(&mut self, source: &str) -> std::result::Result<(), String> {
        let source = source.trim().trim_end_matches(';');
        self.database
            .execute(source)
            .map(drop)
            .map_err(|error| format!("setup-failed: {}", describe(&error)))
    }

    fn check_result(
        &mut self,
        expected: Option<&Vec<Vec<String>>>,
        ordered: bool,
        ignore_list_order: bool,
    ) -> std::result::Result<(), String> {
        let actual = match self.last.as_ref() {
            Some(Ok(result)) => result,
            Some(Err(error)) => return Err(describe(error)),
            None => return Err("unsupported-step: result check before any query".into()),
        };
        let Some(table) = expected else {
            return if actual.rows.is_empty() {
                Ok(())
            } else {
                Err("result-mismatch: expected no rows".into())
            };
        };
        let (header, rows) = table
            .split_first()
            .ok_or_else(|| "unsupported-step: result table without a header".to_owned())?;
        if *header != actual.columns {
            return Err(format!(
                "result-mismatch: columns {:?} differ from {:?}",
                actual.columns, header
            ));
        }
        let expected_rows = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| notation::parse(cell))
                    .collect::<std::result::Result<Vec<_>, _>>()
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| format!("unsupported-step: result notation: {error}"))?;
        if expected_rows.len() != actual.rows.len() {
            return Err(format!(
                "result-mismatch: {} rows instead of {}",
                actual.rows.len(),
                expected_rows.len()
            ));
        }
        let matched = if ordered {
            expected_rows
                .iter()
                .zip(&actual.rows)
                .all(|(expected, actual)| row_matches(expected, actual, ignore_list_order))
        } else {
            let mut remaining = actual.rows.iter().collect::<Vec<_>>();
            expected_rows.iter().all(|expected| {
                remaining
                    .iter()
                    .position(|actual| row_matches(expected, actual, ignore_list_order))
                    .map(|position| {
                        remaining.swap_remove(position);
                    })
                    .is_some()
            })
        };
        if matched {
            Ok(())
        } else {
            Err("result-mismatch: row values differ".into())
        }
    }

    fn check_side_effects(&self, expected: &SideEffects) -> std::result::Result<(), String> {
        let (Some(before), Some(after)) = (&self.before, &self.after) else {
            return Err("unsupported-step: side-effect check before any query".into());
        };
        if let Some(Err(error)) = &self.last {
            return Err(describe(error));
        }
        let actual = before.diff(after);
        if actual == *expected {
            Ok(())
        } else {
            Err(format!(
                "side-effects-mismatch: observed {actual:?}, expected {expected:?}"
            ))
        }
    }

    fn check_error(&self, compile_time: bool) -> std::result::Result<(), String> {
        match &self.last {
            None => Err("unsupported-step: error check before any query".into()),
            Some(Ok(_)) => Err("expected-error-not-raised".into()),
            Some(Err(Error::Parse(_) | Error::Semantic(_) | Error::MissingParameter(_))) => Ok(()),
            Some(Err(Error::Data(_))) if !compile_time => Ok(()),
            Some(Err(error)) => Err(format!("wrong-error-kind: {}", describe(error))),
        }
    }

    fn snapshot(&mut self) -> std::result::Result<Snapshot, String> {
        let nodes = self
            .database
            .execute("MATCH (node) RETURN node")
            .map_err(|error| format!("snapshot-failed: {}", describe(&error)))?;
        let relationships = self
            .database
            .execute("MATCH ()-[relationship]->() RETURN relationship")
            .map_err(|error| format!("snapshot-failed: {}", describe(&error)))?;
        let mut snapshot = Snapshot::default();
        for row in nodes.rows {
            if let Some(CypherValue::Node(node)) = row.into_iter().next() {
                snapshot.nodes.insert(
                    node.element_id,
                    (node.labels.into_iter().collect(), node.properties),
                );
            }
        }
        for row in relationships.rows {
            if let Some(CypherValue::Relationship(relationship)) = row.into_iter().next() {
                snapshot
                    .relationships
                    .insert(relationship.element_id, relationship.properties);
            }
        }
        Ok(snapshot)
    }
}

fn row_matches(expected: &[Expected], actual: &[CypherValue], ignore_list_order: bool) -> bool {
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual)
            .all(|(expected, actual)| expected.matches(actual, ignore_list_order))
}

/// A stable one-line classification of an engine error for the exclusions
/// file: the variant, then the first line of its message.
fn describe(error: &Error) -> String {
    let (kind, message) = match error {
        Error::EmptyQuery => ("empty-query", String::new()),
        Error::Parse(message) => ("parse-error", message.clone()),
        Error::Unsupported(message) => ("unsupported", message.clone()),
        Error::Semantic(message) => ("semantic-error", message.clone()),
        Error::MissingParameter(name) => ("missing-parameter", name.clone()),
        Error::ResourceLimit(message) => ("resource-limit", message.clone()),
        Error::Sqlite(error) => ("sqlite-error", error.to_string()),
        Error::Data(message) => ("data-error", message.clone()),
        other => ("other-error", other.to_string()),
    };
    // Byte spans depend on the query text, so drop them to keep reasons
    // comparable across scenarios.
    let first_line = message.lines().next().unwrap_or_default();
    let without_span = match first_line.find(" at bytes ") {
        Some(position) => &first_line[..position],
        None => first_line,
    };
    let truncated = without_span.chars().take(140).collect::<String>();
    if truncated.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind}: {truncated}")
    }
}

// ---------------------------------------------------------------------------
// Side effects

type Properties = BTreeMap<String, CypherValue>;

#[derive(Default)]
struct Snapshot {
    nodes: BTreeMap<String, (BTreeSet<String>, Properties)>,
    relationships: BTreeMap<String, Properties>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SideEffects {
    nodes_added: usize,
    nodes_removed: usize,
    relationships_added: usize,
    relationships_removed: usize,
    labels_added: usize,
    labels_removed: usize,
    properties_added: usize,
    properties_removed: usize,
}

impl SideEffects {
    fn set(&mut self, name: &str, count: usize) -> std::result::Result<(), String> {
        let slot = match name {
            "+nodes" => &mut self.nodes_added,
            "-nodes" => &mut self.nodes_removed,
            "+relationships" => &mut self.relationships_added,
            "-relationships" => &mut self.relationships_removed,
            "+labels" => &mut self.labels_added,
            "-labels" => &mut self.labels_removed,
            "+properties" => &mut self.properties_added,
            "-properties" => &mut self.properties_removed,
            other => return Err(format!("unsupported-step: side effect {other}")),
        };
        *slot = count;
        Ok(())
    }
}

impl Snapshot {
    // The TCK counts a deleted entity's properties as removed, a created
    // entity's as added, and a changed property value as one added and one
    // removed. Labels are counted by distinct name: `CREATE (:L), (:L)` adds
    // one label while `SET n:A:B` adds two.
    fn diff(&self, after: &Self) -> SideEffects {
        let mut effects = SideEffects::default();
        let mut labels_added = BTreeSet::new();
        let mut labels_removed = BTreeSet::new();
        for (id, (labels, properties)) in &self.nodes {
            match after.nodes.get(id) {
                None => {
                    effects.nodes_removed += 1;
                    labels_removed.extend(labels.iter().cloned());
                    effects.properties_removed += properties.len();
                }
                Some((after_labels, after_properties)) => {
                    labels_added.extend(after_labels.difference(labels).cloned());
                    labels_removed.extend(labels.difference(after_labels).cloned());
                    diff_properties(properties, after_properties, &mut effects);
                }
            }
        }
        for (id, (labels, properties)) in &after.nodes {
            if !self.nodes.contains_key(id) {
                effects.nodes_added += 1;
                labels_added.extend(labels.iter().cloned());
                effects.properties_added += properties.len();
            }
        }
        effects.labels_added = labels_added.len();
        effects.labels_removed = labels_removed.len();
        for (id, properties) in &self.relationships {
            match after.relationships.get(id) {
                None => {
                    effects.relationships_removed += 1;
                    effects.properties_removed += properties.len();
                }
                Some(after_properties) => {
                    diff_properties(properties, after_properties, &mut effects);
                }
            }
        }
        for (id, properties) in &after.relationships {
            if !self.relationships.contains_key(id) {
                effects.relationships_added += 1;
                effects.properties_added += properties.len();
            }
        }
        effects
    }
}

fn diff_properties(before: &Properties, after: &Properties, effects: &mut SideEffects) {
    for (key, value) in before {
        match after.get(key) {
            None => effects.properties_removed += 1,
            Some(after_value) if after_value != value => {
                effects.properties_added += 1;
                effects.properties_removed += 1;
            }
            Some(_) => {}
        }
    }
    for key in after.keys() {
        if !before.contains_key(key) {
            effects.properties_added += 1;
        }
    }
}
