//! One policy evaluated for a kind of sender: each condition as the set of
//! results it can have, given what the question leaves unknown.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use super::json::{entity_parts, like, same, single};
use super::profile::{Fact, Profile, UNKNOWN};
use sender::fact_equal;

mod attribute;
mod residual;
mod sender;
use super::{Asker, Attr, Cause};

/// The results a condition can have, as a set: true, false, an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Results(u8);

pub(super) const TRUE: Results = Results(1);
const FALSE: Results = Results(2);
const ERROR: Results = Results(4);
const TRUE_OR_FALSE: Results = Results(3);
const ANY: Results = Results(7);

impl Results {
    pub(super) fn can(self, other: Results) -> bool {
        self.0 & other.0 != 0
    }

    fn or(self, other: Results) -> Results {
        Results(self.0 | other.0)
    }

    fn of(known: bool) -> Results {
        if known { TRUE } else { FALSE }
    }

    fn not(self) -> Results {
        let mut out = Results(self.0 & ERROR.0);
        if self.can(TRUE) {
            out = out.or(FALSE);
        }
        if self.can(FALSE) {
            out = out.or(TRUE);
        }
        out
    }

    /// `self && right`, as Cedar evaluates it: the right side only when the
    /// left is true.
    fn and_then(self, right: impl FnOnce() -> Results) -> Results {
        let mut out = Results(self.0 & (FALSE.0 | ERROR.0));
        if self.can(TRUE) {
            out = out.or(right());
        }
        out
    }

    /// `self || right`: the right side only when the left is false.
    fn or_else(self, right: impl FnOnce() -> Results) -> Results {
        let mut out = Results(self.0 & (TRUE.0 | ERROR.0));
        if self.can(FALSE) {
            out = out.or(right());
        }
        out
    }
}

/// What an expression evaluates to: most of it is read from a policy or from
/// what the caller knows, and is not copied.
#[derive(Debug, Clone)]
enum Val<'e> {
    /// A value known here, in Cedar's JSON form.
    Known(Cow<'e, Json>),
    /// The request's principal, action, resource or context.
    Principal,
    Action,
    Resource,
    Context,
    /// The id of the record the request acts on.
    ResourceId,
    /// A string known only by which others it equals ([`Attr::Same`]).
    Same {
        name: &'e str,
        unlike: &'e BTreeSet<String>,
    },
    /// Some value of an outside sender's that is none of `unlike`, matching
    /// the `like` patterns marked ([`Fact::Other`]).
    Other {
        unlike: &'e [Json],
        like: &'e BTreeMap<String, bool>,
        string: Option<bool>,
    },
    /// Any value, or an error.
    Unknown,
    /// Certainly an error.
    Error,
}

pub(super) struct Request<'a> {
    pub(super) asker: &'a Asker,
    /// What is known of the sender, when it is someone outside.
    pub(super) profile: &'a Profile,
    pub(super) action: &'a str,
    pub(super) resource_type: &'a str,
    /// The record's attributes the caller knows.
    pub(super) record: &'a BTreeMap<String, Attr>,
    /// How the move came about, when the caller knows.
    pub(super) cause: Option<&'a Cause>,
}

/// A value that is known, read from where it already is.
fn read(value: &Json) -> Val<'_> {
    Val::Known(Cow::Borrowed(value))
}

/// A value that is known, made here.
fn made<'e>(value: Json) -> Val<'e> {
    Val::Known(Cow::Owned(value))
}

/// The attributes of a record value; an entity holds none here.
fn record_of(value: &Json) -> Option<&serde_json::Map<String, Json>> {
    value
        .as_object()
        .filter(|record| !record.contains_key("__entity"))
}

impl<'a> Request<'a> {
    /// Whether `policy` applies: its scope and every condition.
    pub(super) fn applies(&self, policy: &Json) -> Results {
        let Some(object) = policy.as_object() else {
            return ANY;
        };
        let scope = self
            .principal_scope(&object["principal"])
            .and_then(|| self.action_scope(&object["action"]))
            .and_then(|| self.resource_scope(&object["resource"]));
        let conditions = object
            .get("conditions")
            .and_then(Json::as_array)
            .map_or(&[][..], Vec::as_slice);
        conditions.iter().fold(scope, |sofar, condition| {
            sofar.and_then(|| {
                let body = self.truth(&condition["body"]);
                match condition["kind"].as_str() {
                    Some("when") => body,
                    Some("unless") => body.not(),
                    _ => ANY,
                }
            })
        })
    }

    fn principal_scope(&self, scope: &Json) -> Results {
        match (scope["op"].as_str(), scope.get("in")) {
            (Some("All"), _) => TRUE,
            (Some("=="), _) => self.equal(&Val::Principal, &read(&scope["entity"])),
            (Some("is"), None) => self.principal_is(scope["entity_type"].as_str()),
            _ => ANY,
        }
    }

    fn action_scope(&self, scope: &Json) -> Results {
        match scope["op"].as_str() {
            Some("All") => TRUE,
            Some("==") => Results::of(self.is_action(&scope["entity"])),
            Some("in") => match (scope.get("entities"), scope.get("entity")) {
                (Some(Json::Array(list)), _) => {
                    Results::of(list.iter().any(|entity| self.is_action(entity)))
                }
                (_, Some(entity)) => Results::of(self.is_action(entity)),
                _ => ANY,
            },
            _ => ANY,
        }
    }

    fn resource_scope(&self, scope: &Json) -> Results {
        match scope["op"].as_str() {
            Some("All") => TRUE,
            Some("is") if scope.get("in").is_none() => {
                Results::of(scope["entity_type"].as_str() == Some(self.resource_type))
            }
            // A record has no parents, so `in` holds only of the record itself.
            Some("==" | "in") => match entity_parts(&scope["entity"]) {
                Some((kind, _)) if kind != self.resource_type => FALSE,
                _ => TRUE_OR_FALSE,
            },
            _ => ANY,
        }
    }

    fn is_action(&self, entity: &Json) -> bool {
        entity_parts(entity).is_some_and(|(kind, id)| kind == "Action" && id == self.action)
    }

    fn principal_is(&self, kind: Option<&str>) -> Results {
        let Some(profile) = self.outside() else {
            return Results::of(kind == Some("Agent"));
        };
        match (kind, &profile.kind) {
            (Some("System"), _) => FALSE,
            (Some(kind), Some(fact)) => fact_equal(fact, &Json::from(kind)),
            _ => TRUE_OR_FALSE,
        }
    }

    /// The results of a condition.
    fn truth(&self, expr: &Json) -> Results {
        let Some((op, arg)) = single(expr) else {
            return self.as_bool(&self.value(expr));
        };
        match op {
            "&&" => self
                .truth(&arg["left"])
                .and_then(|| self.truth(&arg["right"])),
            "||" => self
                .truth(&arg["left"])
                .or_else(|| self.truth(&arg["right"])),
            "!" => self.truth(&arg["arg"]).not(),
            "==" => self.equal(&self.value(&arg["left"]), &self.value(&arg["right"])),
            "!=" => self
                .equal(&self.value(&arg["left"]), &self.value(&arg["right"]))
                .not(),
            "has" => self.has(
                &self.value(&arg["left"]),
                arg["attr"].as_str().unwrap_or(""),
            ),
            "is" if arg.get("in").is_none() => match self.value(&arg["left"]) {
                Val::Principal => self.principal_is(arg["entity_type"].as_str()),
                Val::Resource => {
                    Results::of(arg["entity_type"].as_str() == Some(self.resource_type))
                }
                Val::Action => Results::of(arg["entity_type"].as_str() == Some("Action")),
                Val::Known(value) => match entity_parts(&value) {
                    Some((kind, _)) => Results::of(Some(kind) == arg["entity_type"].as_str()),
                    None => ERROR,
                },
                Val::Error => ERROR,
                _ => ANY,
            },
            "in" => {
                let left = self.value(&arg["left"]);
                match single(&arg["right"]) {
                    Some(("Set", list)) => match self.members(list) {
                        Ok(members) => {
                            self.within_members(&left, members.iter().map(AsRef::as_ref))
                        }
                        Err(val) => self.within(&left, &val),
                    },
                    _ => self.within(&left, &self.value(&arg["right"])),
                }
            }
            "like" => match self.value(&arg["left"]) {
                Val::Known(value) => match (value.as_str(), arg["pattern"].as_array()) {
                    (Some(text), Some(pattern)) => Results::of(like(text, pattern)),
                    (Some(_), None) => ANY,
                    (None, _) => ERROR,
                },
                Val::Other {
                    string: Some(false),
                    ..
                } => ERROR,
                Val::Other { like, .. } => match like.get(&arg["pattern"].to_string()) {
                    Some(matches) => Results::of(*matches),
                    None => ANY,
                },
                Val::Error => ERROR,
                _ => ANY,
            },
            "contains" => match (self.value(&arg["left"]), self.value(&arg["right"])) {
                (Val::Known(set), Val::Known(item)) if set.is_array() => Results::of(
                    set.as_array()
                        .into_iter()
                        .flatten()
                        .any(|member| same(member, &item)),
                ),
                (Val::Error, _) | (_, Val::Error) => ERROR,
                (set @ Val::Known(_), item) if matches!(&set, Val::Known(json) if json.is_array()) => {
                    self.within(&item, &set)
                }
                _ => ANY,
            },
            "if-then-else" => {
                let test = self.truth(&arg["if"]);
                let mut out = Results(test.0 & ERROR.0);
                if test.can(TRUE) {
                    out = out.or(self.truth(&arg["then"]));
                }
                if test.can(FALSE) {
                    out = out.or(self.truth(&arg["else"]));
                }
                out
            }
            _ => self.as_bool(&self.value(expr)),
        }
    }

    fn as_bool(&self, value: &Val) -> Results {
        match value {
            Val::Known(known) => known.as_bool().map_or(ERROR, Results::of),
            Val::Principal
            | Val::Action
            | Val::Resource
            | Val::Context
            | Val::ResourceId
            | Val::Same { .. } => ERROR,
            Val::Error => ERROR,
            Val::Unknown | Val::Other { .. } => ANY,
        }
    }

    /// The value of an expression.
    fn value<'e>(&self, expr: &'e Json) -> Val<'e>
    where
        'a: 'e,
    {
        let Some((op, arg)) = single(expr) else {
            return Val::Unknown;
        };
        match op {
            "Value" => read(arg),
            "Var" => match arg.as_str() {
                Some("principal") => Val::Principal,
                Some("action") => Val::Action,
                Some("resource") => Val::Resource,
                Some("context") => Val::Context,
                _ => Val::Unknown,
            },
            "Set" => match self.members(arg) {
                Ok(members) => made(Json::Array(
                    members.into_iter().map(Cow::into_owned).collect(),
                )),
                Err(val) => val,
            },
            "." => self.attribute(
                &self.value(&arg["left"]),
                arg["attr"].as_str().unwrap_or(""),
            ),
            "&&" | "||" | "!" | "==" | "!=" | "has" | "is" | "in" | "like" | "contains"
            | "if-then-else" => {
                let truth = self.truth(expr);
                if truth == TRUE {
                    made(Json::Bool(true))
                } else if truth == FALSE {
                    made(Json::Bool(false))
                } else if truth == ERROR {
                    Val::Error
                } else {
                    Val::Unknown
                }
            }
            _ => Val::Unknown,
        }
    }

    /// A set expression's members, or what it is when one is not known.
    fn members<'e>(&self, list: &'e Json) -> Result<Vec<Cow<'e, Json>>, Val<'e>>
    where
        'a: 'e,
    {
        let mut members = Vec::new();
        for member in list.as_array().into_iter().flatten() {
            match self.value(member) {
                Val::Known(value) => members.push(value),
                Val::Error => return Err(Val::Error),
                _ => return Err(Val::Unknown),
            }
        }
        Ok(members)
    }

    fn equal(&self, left: &Val<'_>, right: &Val<'_>) -> Results {
        match (left, right) {
            (Val::Error, _) | (_, Val::Error) => ERROR,
            (Val::Known(a), Val::Known(b)) => Results::of(same(a, b)),
            (Val::Other { unlike, .. }, Val::Known(value))
            | (Val::Known(value), Val::Other { unlike, .. }) => {
                if unlike.iter().any(|other| same(other, value)) {
                    FALSE
                } else {
                    TRUE_OR_FALSE
                }
            }
            (Val::Other { .. }, _) | (_, Val::Other { .. }) => ANY,
            (Val::ResourceId, Val::ResourceId) => TRUE,
            (Val::ResourceId, Val::Known(value)) | (Val::Known(value), Val::ResourceId) => {
                if value.is_string() {
                    TRUE_OR_FALSE
                } else {
                    FALSE
                }
            }
            (Val::Same { name: a, .. }, Val::Same { name: b, .. }) => Results::of(a == b),
            (Val::Same { unlike, .. }, Val::Known(value))
            | (Val::Known(value), Val::Same { unlike, .. }) => match value.as_str() {
                Some(text) if !text.is_empty() && !unlike.contains(text) => TRUE_OR_FALSE,
                _ => FALSE,
            },
            (Val::Principal, Val::Known(entity)) | (Val::Known(entity), Val::Principal) => {
                match (self.asker, entity_parts(entity)) {
                    (Asker::Service { name, .. }, Some((kind, id))) => {
                        Results::of(kind == "Agent" && id == format!("service:{name}"))
                    }
                    (_, Some((kind, id))) if kind == "System" || id.starts_with("service:") => {
                        FALSE
                    }
                    (_, None) => FALSE,
                    // The id in a principal's uid is its `id` attribute.
                    (_, Some((kind, id))) => {
                        let profile = self.outside().unwrap_or(&UNKNOWN);
                        let fact = |fact: Option<&Fact>, value: &str| {
                            fact.map_or(TRUE_OR_FALSE, |fact| fact_equal(fact, &Json::from(value)))
                        };
                        fact(profile.kind.as_ref(), kind)
                            .and_then(|| fact(profile.attrs.get("id"), id))
                    }
                }
            }
            (Val::Action, Val::Known(entity)) | (Val::Known(entity), Val::Action) => {
                Results::of(self.is_action(entity))
            }
            (Val::Resource, Val::Known(entity)) | (Val::Known(entity), Val::Resource) => {
                match entity_parts(entity) {
                    Some((kind, _)) if kind == self.resource_type => TRUE_OR_FALSE,
                    _ => FALSE,
                }
            }
            (Val::Unknown, _) | (_, Val::Unknown) => ANY,
            _ => TRUE_OR_FALSE,
        }
    }

    /// `left in right`: an action in a list of actions, or an entity in
    /// itself (records and the kernel's principals have no parents).
    fn within(&self, left: &Val<'_>, right: &Val<'_>) -> Results {
        match right {
            Val::Known(set) if set.is_array() => {
                self.within_members(left, set.as_array().into_iter().flatten())
            }
            Val::Known(entity) if entity_parts(entity).is_some() => match left {
                Val::Principal if self.outside().is_some() => TRUE_OR_FALSE,
                Val::Other { .. } => ANY,
                _ => self.equal(left, right),
            },
            Val::Error => ERROR,
            _ => ANY,
        }
    }

    /// `left in` a set of these `members`.
    fn within_members<'m>(
        &self,
        left: &Val<'_>,
        members: impl Iterator<Item = &'m Json>,
    ) -> Results {
        members
            .map(|member| self.equal(left, &read(member)))
            .fold(FALSE, |sofar, next| sofar.or_else(|| next))
    }
}
