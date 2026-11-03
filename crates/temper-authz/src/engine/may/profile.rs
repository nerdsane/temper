//! What is known of one outside sender: the answers the policies' tests of
//! the principal find, and the keys of its security context.
//!
//! A real request names one principal, and every check along its chain (its
//! reactions, the module answers and timer fires that run as it) names the
//! same one, with the same security context (`resolve_trigger_principal`
//! clones the invoking context; a timer keeps the arming one). A question
//! about "someone outside" leaves all of that unknown, so two questions
//! about one sender can be answered for two different principals. A
//! [`Kind`] is the outside principals some profiles describe, which the
//! policies answer alike
//! ([`SendAnalysis::outside_kinds`](super::SendAnalysis::outside_kinds)),
//! so each question about one sender is answered for one kind.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value as Json;

/// A kind of outside sender: the principals any of its profiles
/// describes. A question about it is answered for each, and where they
/// differ it may go either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind(pub(super) Vec<Profile>);

impl Kind {
    /// The senders of either kind.
    #[must_use]
    pub fn join(mut self, other: Kind) -> Kind {
        self.0.extend(other.0);
        self
    }
}

/// What is known of one outside sender. What it leaves out is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Profile {
    /// The principal's entity type, as `principal is` reads it.
    pub(super) kind: Option<Fact>,
    /// The principal's attributes, `id` among them (the id in its uid too).
    pub(super) attrs: BTreeMap<String, Fact>,
    /// The keys of its security context the kernel alone sets
    /// (`platform_keys`), which no record field stands in for.
    pub(super) context: BTreeMap<String, Fact>,
    /// The names (`kind`, an attribute, `context.<key>`) whose fact stands
    /// for others the policies answer alike: an example, not a rule.
    pub(super) stand_ins: BTreeSet<String>,
}

/// What a sender holds under one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Fact {
    /// Nothing: `has` is false, and reading it is an error.
    Absent,
    /// This value.
    Is(Json),
    /// Some value that is none of `unlike` (the literals the policies compare
    /// it with), matching each `like` pattern (by its JSON text) as marked;
    /// a pattern not marked stays open unless the value is a non-string.
    Other {
        unlike: Vec<Json>,
        like: BTreeMap<String, bool>,
        /// Whether this is a string, when a `like` test has split it.
        string: Option<bool>,
    },
}

/// The profile of an outside sender of whom nothing is known.
pub(super) static UNKNOWN: Profile = Profile {
    kind: None,
    attrs: BTreeMap::new(),
    context: BTreeMap::new(),
    stand_ins: BTreeSet::new(),
};

/// One of its senders, in words, as a walk names it: "such as a Customer,
/// role "guest:link"".
impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The plainest example.
        let example = self
            .0
            .iter()
            .map(ToString::to_string)
            .min_by_key(String::len);
        match example {
            Some(example) if !example.is_empty() => write!(f, "such as {example}"),
            _ => write!(f, "of any kind"),
        }
    }
}

/// The known facts, in words: "a Customer, role "guest:link"".
impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        let kind = self
            .kind
            .as_ref()
            .filter(|_| !self.stand_ins.contains("kind"));
        match kind {
            Some(Fact::Is(Json::String(kind))) => parts.push(format!("a {kind}")),
            Some(Fact::Other { unlike, .. }) => parts.push(format!("not {}", list(unlike, " or "))),
            _ => {}
        }
        for (prefix, facts) in [("", &self.attrs), ("context.", &self.context)] {
            for (name, fact) in facts {
                if self.stand_ins.contains(&format!("{prefix}{name}")) {
                    continue;
                }
                parts.push(match fact {
                    Fact::Absent => format!("no {prefix}{name}"),
                    Fact::Is(value) => format!("{prefix}{name} {value}"),
                    Fact::Other { like, .. } if like.values().any(|matches| *matches) => {
                        let patterns: Vec<String> = like
                            .iter()
                            .filter(|(_, matches)| **matches)
                            .map(|(pattern, _)| pattern_text(pattern))
                            .collect();
                        format!("{prefix}{name} like {}", patterns.join(" and "))
                    }
                    Fact::Other { unlike, .. } if unlike.is_empty() => {
                        format!("some {prefix}{name}")
                    }
                    Fact::Other { unlike, .. } => {
                        format!("{prefix}{name} not {}", list(unlike, " or "))
                    }
                });
            }
        }
        write!(f, "{}", parts.join(", "))
    }
}

fn list(values: &[Json], by: &str) -> String {
    values
        .iter()
        .map(|value| match value {
            Json::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(by)
}

/// A `like` pattern, from its JSON, as Cedar writes it: `"guest*"`.
fn pattern_text(pattern: &str) -> String {
    let elements: Vec<Json> = serde_json::from_str(pattern).unwrap_or_default();
    let text: String = elements
        .iter()
        .map(
            |element| match element.get("Literal").and_then(Json::as_str) {
                Some(literal) => literal.to_string(),
                None => "*".to_string(),
            },
        )
        .collect();
    format!("{text:?}")
}
