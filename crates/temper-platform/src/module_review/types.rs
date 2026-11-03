//! The judge a review asks, what it asks, and what a review finds.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One question about a state, in Jev's wire form (`{"type": "noul", ...}`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// How true a statement is of the state, 0..1.
    Noul { instructions: String },
    /// Which of the named options fits the instructions best.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
}

/// One question's answer: a rating for a noul, a key for a choice.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Answer {
    #[serde(default)]
    pub noul: Option<f64>,
    #[serde(default)]
    pub choice: Option<String>,
}

/// Every question's answer about one state, and what reading the state cost.
#[derive(Debug, Clone, Default)]
pub struct Answers {
    pub answers: BTreeMap<String, Answer>,
    pub input_tokens: u64,
}

impl Answers {
    /// A finite rating for `key`, if Jev gave one.
    pub fn rating(&self, key: &str) -> Option<f64> {
        self.answers
            .get(key)
            .and_then(|answer| answer.noul)
            .filter(|rating| rating.is_finite())
    }

    /// The option Jev chose for `key`, if it chose one.
    pub fn choice(&self, key: &str) -> Option<&str> {
        self.answers
            .get(key)
            .and_then(|answer| answer.choice.as_deref())
    }
}

/// Why the judge gave no answer. Each is said once in the install result and never retried in a loop: a review is
/// advice, and an install never waits on it twice.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum JudgeUnavailable {
    #[error("no Jev key is set on this server")]
    NoKey,
    #[error("Jev is out of credit (HTTP 402)")]
    OutOfCredit,
    /// The kinds of the failed requests ("HTTP 503", "timeout"), never a response body.
    #[error("Jev did not answer ({0})")]
    Down(String),
}

impl JudgeUnavailable {
    /// The words an install result gives for it.
    pub fn words(&self) -> &'static str {
        match self {
            Self::NoKey => "Jev is not set up on this server, so the modules were not reviewed.",
            Self::OutOfCredit => {
                "Jev is out of credit, so the modules were not reviewed. The install went on; nothing to do about it here."
            }
            Self::Down(_) => {
                "Jev did not answer, so the modules were not reviewed this time. The install went on."
            }
        }
    }
}

/// The reader of a review's questions: Jev in production, a stub in tests and simulation.
#[async_trait::async_trait]
pub trait Judge: Send + Sync {
    /// Ask every question about one state of `module` (its kernel name) in one call.
    ///
    /// # Errors
    ///
    /// [`JudgeUnavailable`] when the judge gave no answer.
    async fn ask(
        &self,
        module: &str,
        state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, JudgeUnavailable>;
}

/// A module to review: its kernel name, where its Rust source sits in the app, and that source.
#[derive(Debug, Clone)]
pub struct ModuleSource {
    pub module: String,
    pub path: String,
    pub source: String,
}

/// One record type's spec, as installed, and which of its actions run which module.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecSource {
    pub entity_type: String,
    pub source: String,
    /// `(action, module)` for each trigger that runs a module, in spec order.
    pub module_triggers: Vec<(String, String)>,
}

impl SpecSource {
    /// A spec's text with the module each action's `[[action.triggers]]` runs. Read as plain TOML: the install parses
    /// and checks the spec itself, and a spec that does not read names no module here.
    pub fn new(entity_type: &str, source: &str) -> Self {
        let parsed: toml::Value = toml::from_str(source).unwrap_or(toml::Value::Boolean(false));
        let actions = parsed.get("action").and_then(toml::Value::as_array);
        let module_triggers = actions
            .into_iter()
            .flatten()
            .filter_map(|action| {
                let name = action.get("name")?.as_str()?;
                let triggers = action.get("triggers")?.as_array()?;
                Some(triggers.iter().filter_map(move |trigger| {
                    let module = trigger.get("module")?.as_str()?;
                    Some((name.to_string(), module.to_string()))
                }))
            })
            .flatten()
            .collect();
        Self {
            entity_type: entity_type.to_string(),
            source: source.to_string(),
            module_triggers,
        }
    }
}

/// One thing a module does that the spec should do, where it shows, and what should do it instead.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// The item of [`super::items::ITEMS`], 1 to 7.
    pub item: u8,
    /// The item's kind, or `large_module` for a module flagged by its size.
    pub kind: String,
    pub title: String,
    /// Jev's rating of the item's statement, to two places; 1 for a size flag.
    pub rating: f64,
    /// `path:line`, or the path alone when Jev did not say where.
    #[serde(rename = "where")]
    pub location: String,
    /// The line's text, without its comments.
    pub line: Option<String>,
    pub why: String,
    pub instead: String,
}

/// What a review of one module found. No findings is a valid answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModuleReview {
    pub module: String,
    pub findings: Vec<Finding>,
    /// Input tokens Jev read for it; output is free.
    pub input_tokens: u64,
    /// The parts of the specs it needed and could not read ([`super::excerpt::SPEC_BYTES`]): none, or its review is
    /// of part of what runs it, and finding nothing does not clear it.
    pub left_out: Vec<String>,
}
