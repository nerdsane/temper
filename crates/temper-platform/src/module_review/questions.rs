//! What Jev reads about a module and what it is asked: the app's specs that run the module, the module's numbered
//! lines, one statement per item, then where each found item shows.

use std::collections::BTreeMap;

use super::excerpt::Excerpt;
use super::items::{ITEMS, Item, LEAF};
use super::source::{Line, Part};
use super::types::{Question, SpecSource};

const INTRO: &str = "An app on Temper. Its records move only through the states and actions written in its spec: guards, timers and Cedar policies decide what happens next, and an action's trigger may run a module (Rust code) between two moves. What the module returns becomes the parameters of the trigger's on_success action.";

/// The record types whose triggers run `module`, and the actions on each that run it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Runners {
    pub specs: Vec<SpecSource>,
    /// `(entity type, action)` pairs, in spec order.
    pub actions: Vec<(String, String)>,
}

/// The specs that run `module` from a trigger, else every spec (an app with one type usually has one).
pub fn runners(module: &str, specs: &[SpecSource]) -> Runners {
    let mut runners = Runners::default();
    for spec in specs {
        let mut actions: Vec<String> = spec
            .module_triggers
            .iter()
            .filter(|(_, runs)| runs == module)
            .map(|(action, _)| action.clone())
            .collect();
        actions.dedup();
        if actions.is_empty() {
            continue;
        }
        runners.specs.push(spec.clone());
        runners.actions.extend(
            actions
                .into_iter()
                .map(|action| (spec.entity_type.clone(), action)),
        );
    }
    if runners.specs.is_empty() {
        runners.specs = specs.to_vec();
    }
    runners
}

fn numbered(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| format!("{:>4} | {}", line.number, line.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The state Jev reads for one window of a module: the intro, the parts of the specs its review reads, and the
/// window's numbered lines.
pub fn review_state(
    module: &str,
    window: &[Line],
    excerpt: &Excerpt,
    total_lines: usize,
) -> String {
    let first = window.first().map_or(1, |line| line.number);
    let last = window.last().map_or(1, |line| line.number);
    let part = if first > 1 || last < total_lines {
        format!(" Lines {first} to {last} of its {total_lines}; the rest is read separately.")
    } else {
        String::new()
    };
    [
        INTRO.to_string(),
        String::new(),
        excerpt.heading.clone(),
        excerpt.text.clone(),
        String::new(),
        format!(
            "The module \"{module}\" (Rust; line numbers on the left; comments removed).{part}"
        ),
        numbered(window),
    ]
    .join("\n")
}

/// One call: every item's statement and the leaf statement, rated.
pub fn item_questions() -> BTreeMap<String, Question> {
    let mut questions = BTreeMap::new();
    questions.insert(
        "leaf".to_string(),
        Question::Noul {
            instructions: LEAF.to_string(),
        },
    );
    for item in &ITEMS {
        questions.insert(
            item.kind.to_string(),
            Question::Noul {
                instructions: item.shows.to_string(),
            },
        );
    }
    questions
}

/// The key of the question asking where `item` shows.
pub fn where_key(item: &Item) -> String {
    format!("where_{}", item.kind)
}

/// The key of the question asking which line shows `item`.
pub fn line_key(item: &Item) -> String {
    format!("line_{}", item.kind)
}

/// Where each found item shows: one choice among the window's parts, per item.
pub fn where_questions(found: &[&Item], parts: &[Part]) -> BTreeMap<String, Question> {
    let criteria: BTreeMap<String, String> = parts
        .iter()
        .map(|part| {
            let text = part
                .lines
                .iter()
                .map(|line| format!("{} | {}", line.number, line.text.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            (part.key.clone(), text)
        })
        .collect();
    found
        .iter()
        .map(|item| {
            let instructions = format!(
                "Which part of the module shows this most clearly: {}",
                item.shows
            );
            (
                where_key(item),
                Question::Choice {
                    instructions,
                    criteria: criteria.clone(),
                },
            )
        })
        .collect()
}

/// Which line of its part shows each placed item.
pub fn line_questions(placed: &[(&Item, &Part)]) -> BTreeMap<String, Question> {
    placed
        .iter()
        .map(|(item, part)| {
            let criteria = part
                .lines
                .iter()
                .map(|line| (format!("L{}", line.number), line.text.trim().to_string()))
                .collect();
            let instructions = format!("Which line shows this most clearly: {}", item.shows);
            (
                line_key(item),
                Question::Choice {
                    instructions,
                    criteria,
                },
            )
        })
        .collect()
}

/// How a finding names what runs the module: `Order's Approve, Close`, else the app's one record type, else "the
/// record".
pub fn runs_from(runners: &Runners) -> String {
    let Some((first_type, _)) = runners.actions.first() else {
        return match runners.specs.as_slice() {
            [only] => only.entity_type.clone(),
            _ => "the record".to_string(),
        };
    };
    let actions: Vec<&str> = runners
        .actions
        .iter()
        .filter(|(entity_type, _)| entity_type == first_type)
        .map(|(_, action)| action.as_str())
        .collect();
    format!("{first_type}'s {}", actions.join(", "))
}
