//! What decides a kind of sender from here on, as a small number.
//!
//! Two kinds whose answers are the same, whose policies leave the same parts
//! open, and which hold the same facts where those parts read, answer alike
//! however the rest comes to be known. Reading that form for each kind is the
//! partition's main work and memory, and almost all of it repeats, so each
//! distinct text is kept once and a form is a list of numbers.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use cedar_policy::Effect;
use serde_json::Value as Json;

use super::super::json::single;
use super::super::profile::UNKNOWN;
use super::super::request::{Results, TRUE};
use super::{Asker, May, Place, Profile, Question, Test, ask, facts, place};

/// What one policy leaves of a question for one kind.
#[derive(Clone)]
pub(super) struct Part {
    pub(super) applies: Results,
    /// The number of what it leaves open, once a Maybe answer asks.
    pub(super) text: Option<u32>,
}

impl Part {
    /// What a kind with one more fact keeps of `part`: an answer no fact can
    /// change.
    pub(super) fn stays(part: &Option<Part>) -> Option<Part> {
        part.clone()
            .filter(|part| part.applies == TRUE || !part.applies.can(TRUE))
    }
}

/// The policies that read alike whatever the sender: for each question, the
/// class of each candidate. A question's asker, record and cause are all a
/// policy reads besides the sender, its action and its resource type, so the
/// same condition on many actions is read once for each kind. A policy is
/// read as the question leaves it to someone unknown: a part the question
/// decides, such as `action in [...]`, is its result, and the action or
/// resource type is part of the class only while a part left open reads it.
pub(super) struct Classes {
    pub(super) of: Vec<Vec<usize>>,
    pub(super) count: usize,
}

impl Classes {
    pub(super) fn read(
        policies: &[(Effect, Json)],
        questions: &[Question<'_>],
        candidates: &[Vec<usize>],
    ) -> Self {
        let none = BTreeMap::new();
        let anyone = Asker::Outside;
        let mut numbers: HashMap<String, usize> = HashMap::new();
        let of = questions
            .iter()
            .zip(candidates)
            .map(|(question, candidates)| {
                let request = ask(&anyone, &UNKNOWN, question, &none);
                candidates
                    .iter()
                    .map(|&at| {
                        let (effect, policy) = &policies[at];
                        // Knowing more of the sender keeps what is decided
                        // and reads what is open alike for every question
                        // that leaves the same: no part the question
                        // decides is read again.
                        let conditions = request.residual(policy, &|_| false).map_or_else(
                            || policy["conditions"].to_string(),
                            |left| left.to_string(),
                        );
                        let action = if conditions.contains(r#"{"Var":"action"}"#) {
                            question.action
                        } else {
                            ""
                        };
                        let resource = if conditions.contains(r#"{"Var":"resource"}"#) {
                            question.resource_type
                        } else {
                            ""
                        };
                        let class = format!(
                            "{effect:?}|{}|{conditions}|{:?}|{:?}|{action}|{resource}",
                            policy["principal"],
                            question.cause,
                            request.fixed_scope(policy),
                        );
                        let next = numbers.len();
                        *numbers.entry(class).or_insert(next)
                    })
                    .collect()
            })
            .collect();
        Self {
            of,
            count: numbers.len(),
        }
    }
}

#[derive(Default)]
pub(super) struct Forms {
    texts: RefCell<HashMap<String, u32>>,
    lists: RefCell<HashMap<Vec<u32>, u32>>,
    bytes: Cell<usize>,
}

impl Forms {
    /// The number of a definite answer: no text or list has it.
    pub(super) fn answer(&self, answer: May) -> u32 {
        match answer {
            May::Yes => u32::MAX,
            May::No => u32::MAX - 1,
            May::Maybe => u32::MAX - 2,
        }
    }

    /// The number of this text.
    pub(super) fn text(&self, text: &str) -> u32 {
        let mut texts = self.texts.borrow_mut();
        if let Some(&known) = texts.get(text) {
            return known;
        }
        let number = texts.len() as u32;
        // A map entry and the text, with the map's spare room.
        self.bytes.set(
            self.bytes.get() + 2 * (size_of::<(String, u32)>() + size_of::<u64>()) + text.len(),
        );
        texts.insert(text.to_string(), number);
        number
    }

    /// The number of this list of numbers.
    pub(super) fn list(&self, list: Vec<u32>) -> u32 {
        let mut lists = self.lists.borrow_mut();
        if let Some(&known) = lists.get(&list) {
            return known;
        }
        let number = lists.len() as u32;
        self.bytes.set(
            self.bytes.get()
                + 2 * (size_of::<(Vec<u32>, u32)>() + size_of::<u64>())
                + list.len() * size_of::<u32>(),
        );
        lists.insert(list, number);
        number
    }

    /// The bytes the forms hold.
    pub(super) fn bytes(&self) -> usize {
        self.bytes.get()
    }
}

/// Whether `expr` reads the principal or context other than by name, so any
/// fact held of the sender may matter to it.
pub(super) fn reads_whole_sender(expr: &Json) -> bool {
    match single(expr) {
        Some(("Value", _)) => false,
        Some(("Var", Json::String(var))) => var == "principal" || var == "context",
        Some(("." | "has", arg)) if place(&arg["left"]).is_some() => false,
        Some((_, arg)) => reads_whole_sender(arg),
        None => match expr {
            Json::Object(object) => object.values().any(reads_whole_sender),
            Json::Array(items) => items.iter().any(reads_whole_sender),
            _ => false,
        },
    }
}

/// The facts of `profile` the parts still open read, as text.
pub(super) fn reads_of(profile: &Profile, reads: &BTreeSet<Test>, whole: bool) -> String {
    if whole {
        return format!("{:?}{:?}{:?}", profile.kind, profile.attrs, profile.context);
    }
    let mut text = String::new();
    let mut named = BTreeSet::new();
    for test in reads {
        match test {
            Test::Kind => text.push_str(&format!("kind {:?};", profile.kind)),
            Test::Named(at, name) | Test::Like(at, name, _) => {
                named.insert((*at, name.as_str()));
            }
        }
    }
    for (at, name) in named {
        let at_name = if at == Place::Principal {
            ""
        } else {
            "context."
        };
        text.push_str(&format!(
            "{at_name}{name} {:?};",
            facts(profile, at).get(name)
        ));
    }
    text
}
