//! One policy evaluated for a kind of sender: each condition as the set of
//! results it can have, given what the question leaves unknown.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use super::json::{entity_parts, like, same, single};
use super::{Asker, Attr};

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

/// What an expression evaluates to.
#[derive(Debug, Clone)]
enum Val {
    /// A value known here, in Cedar's JSON form.
    Known(Json),
    /// The request's principal, action, resource or context.
    Principal,
    Action,
    Resource,
    Context,
    /// The id of the record the request acts on.
    ResourceId,
    /// A string known only by which others it equals ([`Attr::Same`]).
    Same {
        name: String,
        unlike: BTreeSet<String>,
    },
    /// Any value, or an error.
    Unknown,
    /// Certainly an error.
    Error,
}

pub(super) struct Request<'a> {
    pub(super) asker: &'a Asker,
    pub(super) action: &'a str,
    pub(super) resource_type: &'a str,
    /// The record's attributes the caller knows.
    pub(super) record: &'a BTreeMap<String, Attr>,
}

impl Request<'_> {
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
            .cloned()
            .unwrap_or_default();
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
            (Some("=="), _) => self.equal(&Val::Principal, &Val::Known(scope["entity"].clone())),
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
        match self.asker {
            Asker::Service { .. } => Results::of(kind == Some("Agent")),
            Asker::Outside if kind == Some("System") => FALSE,
            Asker::Outside => TRUE_OR_FALSE,
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
            "in" => self.within(&self.value(&arg["left"]), &self.value(&arg["right"])),
            "like" => match self.value(&arg["left"]) {
                Val::Known(Json::String(text)) => match arg["pattern"].as_array() {
                    Some(pattern) => Results::of(like(&text, pattern)),
                    None => ANY,
                },
                Val::Known(_) | Val::Error => ERROR,
                _ => ANY,
            },
            "contains" => match (self.value(&arg["left"]), self.value(&arg["right"])) {
                (Val::Known(Json::Array(set)), Val::Known(item)) => {
                    Results::of(set.iter().any(|member| same(member, &item)))
                }
                (Val::Error, _) | (_, Val::Error) => ERROR,
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
            Val::Known(Json::Bool(known)) => Results::of(*known),
            Val::Known(_)
            | Val::Principal
            | Val::Action
            | Val::Resource
            | Val::Context
            | Val::ResourceId
            | Val::Same { .. } => ERROR,
            Val::Error => ERROR,
            Val::Unknown => ANY,
        }
    }

    /// The value of an expression.
    fn value(&self, expr: &Json) -> Val {
        let Some((op, arg)) = single(expr) else {
            return Val::Unknown;
        };
        match op {
            "Value" => Val::Known(arg.clone()),
            "Var" => match arg.as_str() {
                Some("principal") => Val::Principal,
                Some("action") => Val::Action,
                Some("resource") => Val::Resource,
                Some("context") => Val::Context,
                _ => Val::Unknown,
            },
            "Set" => {
                let mut members = Vec::new();
                for member in arg.as_array().into_iter().flatten() {
                    match self.value(member) {
                        Val::Known(value) => members.push(value),
                        Val::Error => return Val::Error,
                        _ => return Val::Unknown,
                    }
                }
                Val::Known(Json::Array(members))
            }
            "." => self.attribute(
                &self.value(&arg["left"]),
                arg["attr"].as_str().unwrap_or(""),
            ),
            "&&" | "||" | "!" | "==" | "!=" | "has" | "is" | "in" | "like" | "contains"
            | "if-then-else" => {
                let truth = self.truth(expr);
                if truth == TRUE {
                    Val::Known(Json::Bool(true))
                } else if truth == FALSE {
                    Val::Known(Json::Bool(false))
                } else if truth == ERROR {
                    Val::Error
                } else {
                    Val::Unknown
                }
            }
            _ => Val::Unknown,
        }
    }

    /// The service principal's attributes, as the engine builds them.
    fn service_attribute(name: &str, attr: &str) -> Option<Json> {
        let id = format!("service:{name}");
        match attr {
            "id" | "accountId" => Some(Json::String(id)),
            "agent_type" => Some(Json::String(name.to_string())),
            "role" => Some(Json::String("service".to_string())),
            "agentTypeVerified" => Some(Json::Bool(false)),
            _ => None,
        }
    }

    /// A service's context: the engine's own keys, then the kernel's. A key
    /// it lacks may still come from the record's fields.
    fn service_context(&self, key: &str) -> Option<Attr> {
        let Asker::Service { name, context } = self.asker else {
            return None;
        };
        let id = format!("service:{name}");
        match key {
            "role" => Some(Attr::Value(Json::String("service".to_string()))),
            "agentId" => Some(Attr::Value(Json::String(id))),
            "agentType" => Some(Attr::Value(Json::String(name.clone()))),
            "agentTypeVerified" => Some(Attr::Value(Json::Bool(false))),
            _ => context.get(key).cloned(),
        }
    }

    fn attribute(&self, of: &Val, attr: &str) -> Val {
        match (of, self.asker) {
            (Val::Principal, Asker::Service { name, .. }) => {
                Self::service_attribute(name, attr).map_or(Val::Error, Val::Known)
            }
            (Val::Context, Asker::Service { .. }) => self
                .service_context(attr)
                .map_or(Val::Unknown, |a| self.known(a)),
            // The kernel puts the record's id in its attributes under both
            // names (`build_authz_resource_attrs`).
            (Val::Resource, _) if matches!(attr, "id" | "Id") => self.known(Attr::ResourceId),
            (Val::Resource, _) => self
                .record
                .get(attr)
                .cloned()
                .map_or(Val::Unknown, |a| self.known(a)),
            (Val::Known(Json::Object(record)), _) if !record.contains_key("__entity") => {
                record.get(attr).cloned().map_or(Val::Error, Val::Known)
            }
            (Val::Known(_) | Val::Action | Val::Error | Val::Same { .. }, _) => Val::Error,
            _ => Val::Unknown,
        }
    }

    /// The value of an attribute the kernel sets or the caller knows; the
    /// record's id is the caller's when it knows it.
    fn known(&self, attr: Attr) -> Val {
        match attr {
            Attr::Value(value) => Val::Known(value),
            Attr::ResourceId => match self.record.get("id") {
                Some(Attr::Value(value)) => Val::Known(value.clone()),
                Some(Attr::Same { name, unlike }) => Val::Same {
                    name: name.clone(),
                    unlike: unlike.clone(),
                },
                _ => Val::ResourceId,
            },
            Attr::Same { name, unlike } => Val::Same { name, unlike },
            Attr::Unknown => Val::Unknown,
        }
    }

    fn has(&self, of: &Val, attr: &str) -> Results {
        match (of, self.asker) {
            (Val::Principal, Asker::Service { name, .. }) => {
                Results::of(Self::service_attribute(name, attr).is_some())
            }
            (Val::Context, Asker::Service { .. }) => match self.service_context(attr) {
                Some(_) => TRUE,
                None => TRUE_OR_FALSE,
            },
            (Val::Known(Json::Object(record)), _) if !record.contains_key("__entity") => {
                Results::of(record.contains_key(attr))
            }
            (Val::Action, _) => FALSE,
            (Val::Resource, _) if matches!(attr, "id" | "Id") => TRUE,
            (Val::Resource, _) if self.record.contains_key(attr) => TRUE,
            (Val::Known(_) | Val::Error | Val::ResourceId | Val::Same { .. }, _) => ERROR,
            (Val::Unknown, _) => ANY,
            _ => TRUE_OR_FALSE,
        }
    }

    fn equal(&self, left: &Val, right: &Val) -> Results {
        match (left, right) {
            (Val::Error, _) | (_, Val::Error) => ERROR,
            (Val::Known(a), Val::Known(b)) => Results::of(same(a, b)),
            (Val::ResourceId, Val::ResourceId) => TRUE,
            (Val::ResourceId, Val::Known(Json::String(_)))
            | (Val::Known(Json::String(_)), Val::ResourceId) => TRUE_OR_FALSE,
            (Val::ResourceId, Val::Known(_)) | (Val::Known(_), Val::ResourceId) => FALSE,
            (Val::Same { name: a, .. }, Val::Same { name: b, .. }) => Results::of(a == b),
            (Val::Same { unlike, .. }, Val::Known(Json::String(text)))
            | (Val::Known(Json::String(text)), Val::Same { unlike, .. }) => {
                if text.is_empty() || unlike.contains(text) {
                    FALSE
                } else {
                    TRUE_OR_FALSE
                }
            }
            (Val::Same { .. }, Val::Known(_)) | (Val::Known(_), Val::Same { .. }) => FALSE,
            (Val::Principal, Val::Known(entity)) | (Val::Known(entity), Val::Principal) => {
                match (self.asker, entity_parts(entity)) {
                    (Asker::Service { name, .. }, Some((kind, id))) => {
                        Results::of(kind == "Agent" && id == format!("service:{name}"))
                    }
                    (Asker::Outside, Some((kind, id)))
                        if kind == "System" || id.starts_with("service:") =>
                    {
                        FALSE
                    }
                    (_, None) => FALSE,
                    (Asker::Outside, Some(_)) => TRUE_OR_FALSE,
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
    fn within(&self, left: &Val, right: &Val) -> Results {
        match right {
            Val::Known(Json::Array(set)) => set
                .iter()
                .map(|member| self.equal(left, &Val::Known(member.clone())))
                .fold(FALSE, |sofar, next| sofar.or_else(|| next)),
            Val::Known(entity) if entity_parts(entity).is_some() => match (left, self.asker) {
                (Val::Principal, Asker::Outside) => TRUE_OR_FALSE,
                _ => self.equal(left, right),
            },
            Val::Error => ERROR,
            _ => ANY,
        }
    }
}
