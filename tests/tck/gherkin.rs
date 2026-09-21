//! A deliberately small Gherkin reader covering the subset the openCypher TCK
//! uses: features, one optional background, scenarios, scenario outlines with
//! one `Examples` table, steps, `"""` doc strings, and `|` tables.

#[derive(Clone, Debug)]
pub struct Step {
    pub text: String,
    pub docstring: Option<String>,
    pub table: Vec<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub id: String,
    pub steps: Vec<Step>,
}

#[derive(Debug)]
pub struct Feature {
    pub scenarios: Vec<Scenario>,
}

#[derive(Default)]
struct Block {
    name: String,
    outline: bool,
    steps: Vec<Step>,
    examples: Vec<Vec<String>>,
}

enum Section {
    None,
    Background,
    Scenario,
    Examples,
}

pub fn parse_feature(relative_path: &str, source: &str) -> Feature {
    let lines = source.lines().collect::<Vec<_>>();
    let mut background = Vec::new();
    let mut blocks = Vec::<Block>::new();
    let mut section = Section::None;
    let mut index = 0;

    while index < lines.len() {
        let raw = lines[index];
        let line = raw.trim();
        index += 1;
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Feature:") {
            let _ = rest;
            section = Section::None;
            continue;
        }
        if line.starts_with("Background:") {
            section = Section::Background;
            continue;
        }
        if let Some(name) = line.strip_prefix("Scenario Outline:") {
            blocks.push(Block {
                name: name.trim().to_owned(),
                outline: true,
                ..Block::default()
            });
            section = Section::Scenario;
            continue;
        }
        if let Some(name) = line.strip_prefix("Scenario:") {
            blocks.push(Block {
                name: name.trim().to_owned(),
                ..Block::default()
            });
            section = Section::Scenario;
            continue;
        }
        if line.starts_with("Examples:") {
            section = Section::Examples;
            continue;
        }
        if line.starts_with('|') {
            // A table not attached to a step belongs to the Examples section.
            let table = read_table(&lines, &mut index, raw);
            if let (Section::Examples, Some(block)) = (&section, blocks.last_mut()) {
                block.examples = table;
            }
            continue;
        }
        let Some(text) = step_text(line) else {
            continue;
        };
        let mut step = Step {
            text: text.to_owned(),
            docstring: None,
            table: Vec::new(),
        };
        // Attach a doc string or table that immediately follows the step.
        while index < lines.len() {
            let next_raw = lines[index];
            let next = next_raw.trim();
            if next.is_empty() {
                index += 1;
                continue;
            }
            if next == "\"\"\"" {
                index += 1;
                step.docstring = Some(read_docstring(&lines, &mut index, next_raw));
                continue;
            }
            if next.starts_with('|') {
                index += 1;
                step.table = read_table(&lines, &mut index, next_raw);
                continue;
            }
            break;
        }
        match section {
            Section::Background => background.push(step),
            Section::Scenario => {
                if let Some(block) = blocks.last_mut() {
                    block.steps.push(step);
                }
            }
            Section::None | Section::Examples => {}
        }
    }

    let mut scenarios = Vec::new();
    for block in blocks {
        let mut steps = background.clone();
        steps.extend(block.steps.iter().cloned());
        let base_id = format!("{relative_path}::{}", block.name);
        if !block.outline {
            scenarios.push(Scenario { id: base_id, steps });
            continue;
        }
        let Some((header, rows)) = block.examples.split_first() else {
            continue;
        };
        for (example_index, row) in rows.iter().enumerate() {
            let substitutions = header
                .iter()
                .cloned()
                .zip(row.iter().cloned())
                .collect::<Vec<_>>();
            let steps = steps
                .iter()
                .map(|step| substitute_step(step, &substitutions))
                .collect();
            scenarios.push(Scenario {
                id: format!("{base_id}::example {}", example_index + 1),
                steps,
            });
        }
    }
    dedupe_ids(&mut scenarios);
    Feature { scenarios }
}

fn step_text(line: &str) -> Option<&str> {
    for keyword in ["Given ", "When ", "Then ", "And ", "But "] {
        if let Some(rest) = line.strip_prefix(keyword) {
            return Some(rest.trim());
        }
    }
    None
}

fn read_docstring(lines: &[&str], index: &mut usize, opening: &str) -> String {
    let indent = opening.len() - opening.trim_start().len();
    let mut body = Vec::new();
    while *index < lines.len() {
        let raw = lines[*index];
        *index += 1;
        if raw.trim() == "\"\"\"" {
            break;
        }
        let strip = raw
            .char_indices()
            .take_while(|(position, character)| *position < indent && *character == ' ')
            .count();
        body.push(raw[strip..].replace("\\\"\\\"\\\"", "\"\"\""));
    }
    body.join("\n")
}

fn read_table(lines: &[&str], index: &mut usize, first: &str) -> Vec<Vec<String>> {
    let mut rows = vec![split_row(first)];
    while *index < lines.len() && lines[*index].trim().starts_with('|') {
        rows.push(split_row(lines[*index]));
        *index += 1;
    }
    rows
}

fn split_row(line: &str) -> Vec<String> {
    // A bare `|` is a header-less zero-column table row, which some
    // procedure scenarios use for an empty signature.
    let inner = line
        .trim()
        .strip_prefix('|')
        .unwrap_or_default()
        .strip_suffix('|')
        .unwrap_or_default();
    if inner.trim().is_empty() {
        return Vec::new();
    }
    // Gherkin cells escape `|`, `\`, and newlines with a backslash.
    let mut cells = vec![String::new()];
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some('n') => cells.last_mut().unwrap().push('\n'),
                Some(escaped @ ('|' | '\\')) => cells.last_mut().unwrap().push(escaped),
                Some(other) => {
                    let cell = cells.last_mut().unwrap();
                    cell.push('\\');
                    cell.push(other);
                }
                None => cells.last_mut().unwrap().push('\\'),
            },
            '|' => cells.push(String::new()),
            other => cells.last_mut().unwrap().push(other),
        }
    }
    cells
        .into_iter()
        .map(|cell| cell.trim().to_owned())
        .collect()
}

fn substitute_step(step: &Step, substitutions: &[(String, String)]) -> Step {
    let apply = |text: &str| {
        substitutions
            .iter()
            .fold(text.to_owned(), |text, (name, value)| {
                text.replace(&format!("<{name}>"), value)
            })
    };
    Step {
        text: apply(&step.text),
        docstring: step.docstring.as_deref().map(apply),
        table: step
            .table
            .iter()
            .map(|row| row.iter().map(|cell| apply(cell)).collect())
            .collect(),
    }
}

fn dedupe_ids(scenarios: &mut [Scenario]) {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    for scenario in scenarios.iter_mut() {
        let count = seen.entry(scenario.id.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            scenario.id = format!("{} #{}", scenario.id, count);
        }
    }
}
