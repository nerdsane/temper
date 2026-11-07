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
use std::sync::Arc;

use serde_json::Value as Json;

use super::{Cause, May};

/// A kind of outside sender: the principals any of its profiles
/// describes. A question about it is answered for each, and where they
/// differ it may go either way.
///
/// Its examples answer each question its split asked as all its senders
/// do. Others it holds differ from them only where no asked question
/// looks, so a question its split did not ask reads those too ([`Self::profiles`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kind {
    pub(super) examples: Vec<Profile>,
    /// Profiles wide enough for the senders the examples stand for.
    pub(super) others: Vec<Profile>,
    /// The questions its split asked: action, then resource type, then
    /// each cause it was asked with.
    pub(super) asked: Arc<Asked>,
}

/// The questions a split asked, by action and resource type. Ordered, as
/// a kind prints into the digest a kept verdict is found by.
pub(super) type Asked = BTreeMap<String, BTreeMap<String, Vec<Cause>>>;

impl Kind {
    /// The kind of the one sender `profile` describes.
    pub(super) fn of(profile: Profile) -> Kind {
        Kind {
            examples: vec![profile],
            others: Vec::new(),
            asked: Arc::default(),
        }
    }

    /// The senders of either kind.
    #[must_use]
    pub fn join(mut self, other: Kind) -> Kind {
        self.examples.extend(other.examples);
        self.others.extend(other.others);
        if self.asked != other.asked {
            // Only what both splits asked is asked of the senders of both.
            let asked = Arc::make_mut(&mut self.asked);
            for (action, types) in asked.iter_mut() {
                for (resource_type, causes) in types.iter_mut() {
                    let theirs = other.asked.get(action).and_then(|t| t.get(resource_type));
                    causes.retain(|cause| theirs.is_some_and(|theirs| theirs.contains(cause)));
                }
            }
        }
        self
    }

    /// The profiles that answer `action` on a `resource_type` record for a
    /// move with `cause` as all its senders would: the examples when its
    /// split asked that, or that with more of the cause known (senders with
    /// one form have one residual, whatever fills it in), else the others
    /// too. The others come right after the first example: they differ from
    /// the examples where the split did not look, so an answer that is not
    /// one for all its senders shows soonest.
    pub(super) fn profiles(
        &self,
        action: &str,
        resource_type: &str,
        cause: Option<&Cause>,
    ) -> impl Iterator<Item = &Profile> {
        let asked = cause.is_some_and(|cause| {
            self.asked
                .get(action)
                .and_then(|types| types.get(resource_type))
                .is_some_and(|causes| causes.iter().any(|asked| cause.within(asked)))
        });
        let others: &[Profile] = if asked { &[] } else { &self.others };
        let (first, rest) = self.examples.split_at(self.examples.len().min(1));
        first.iter().chain(others).chain(rest)
    }

    /// Every profile it holds.
    pub(super) fn every(&self) -> impl Iterator<Item = &Profile> {
        self.examples.iter().chain(&self.others)
    }

    /// Whether its senders name a caller, as the kernel reads one
    /// (`names_a_caller`): `Yes` when each does, `No` when none does, `Maybe`
    /// when some may and some may not. A sender outside resolves to a
    /// principal with an id that is never `System`, and no credential may
    /// claim role "service" (the identity resolver refuses it), so one names
    /// no caller only as the anonymous Customer a public endpoint runs as.
    #[must_use]
    pub fn names_a_caller(&self) -> May {
        let (mut may, mut may_not) = (false, false);
        for profile in self.every() {
            let anonymous = profile.anonymous();
            may |= anonymous != Some(true);
            may_not |= anonymous != Some(false);
        }
        match (may, may_not) {
            (true, false) => May::Yes,
            (false, true) => May::No,
            _ => May::Maybe,
        }
    }
}

impl Profile {
    /// Whether it is the anonymous Customer: `None` when that is left open.
    fn anonymous(&self) -> Option<bool> {
        let kind = self.fact("kind", self.kind.as_ref());
        let id = self.fact("id", self.attrs.get("id"));
        if !(may_hold(kind, "Customer") && may_hold(id, "anonymous")) {
            return Some(false);
        }
        let surely = |fact: Option<&Fact>, value: &str| matches!(fact, Some(Fact::Is(Json::String(held))) if held == value);
        (surely(kind, "Customer") && surely(id, "anonymous")).then_some(true)
    }

    /// What it holds under `name`, when that is a rule and not an example
    /// standing for others.
    fn fact<'a>(&self, name: &str, fact: Option<&'a Fact>) -> Option<&'a Fact> {
        fact.filter(|_| !self.stand_ins.contains(name))
    }
}

/// Whether a sender holding `fact` (`None`: unknown) may hold the string
/// `value` there.
fn may_hold(fact: Option<&Fact>, value: &str) -> bool {
    match fact {
        None => true,
        Some(Fact::Absent) => false,
        Some(Fact::Is(held)) => held.as_str() == Some(value),
        Some(Fact::Other {
            unlike,
            like,
            string,
        }) => {
            *string != Some(false)
                && !unlike.iter().any(|other| other.as_str() == Some(value))
                && like.iter().all(|(pattern, matches)| {
                    serde_json::from_str::<Vec<Json>>(pattern).map_or(true, |elements| {
                        super::json::like(value, &elements) == *matches
                    })
                })
        }
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
            .examples
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
