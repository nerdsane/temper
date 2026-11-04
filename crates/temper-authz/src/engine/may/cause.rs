//! How the move a question is about came about, as the kernel tells Cedar.
//!
//! The kernel sets the keys of [`platform_keys::CAUSE`] on every check from
//! the move's cause (`cause_context`), and only the kernel sets them: a
//! request is `via == "request"` at `depth` 0, a reaction is `"reaction"`, or
//! `"timer"` anywhere in a timer's chain, with its rule and the move that set
//! it off, and a timer's chain names who armed the timer. A question that
//! names its cause knows which keys are there; a key it leaves out the check
//! does not have.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use super::Attr;
use super::json::single;
use crate::platform_keys;

/// The cause keys a check has, each known or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cause(BTreeMap<&'static str, Attr>);

/// The reaction hop a reaction's cause names, each part known or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    /// The rule, `<Type>:<Action>:<trigger>`.
    pub rule: Attr,
    /// The move that set it off.
    pub action: Attr,
    /// That move's entity type.
    pub entity_type: Attr,
    /// That move's entity id.
    pub entity_id: Attr,
}

impl Hop {
    /// A hop known only to be one.
    pub fn unknown() -> Self {
        Self {
            rule: Attr::Unknown,
            action: Attr::Unknown,
            entity_type: Attr::Unknown,
            entity_id: Attr::Unknown,
        }
    }
}

impl Cause {
    /// A request from outside: `via` "request", `depth` 0, nothing else.
    pub fn request() -> Self {
        Self(BTreeMap::from([
            (platform_keys::VIA, text("request")),
            (platform_keys::DEPTH, Attr::Value(Json::from(0))),
        ]))
    }

    /// A reaction making `hop`: `via` "reaction", or "timer" when its chain
    /// started at a timer's fire, which also names who armed the timer. Its
    /// depth is not known here.
    pub fn reaction(in_timer_chain: bool, hop: Hop) -> Self {
        let via = if in_timer_chain { "timer" } else { "reaction" };
        let mut keys = BTreeMap::from([
            (platform_keys::VIA, text(via)),
            (platform_keys::DEPTH, Attr::Unknown),
            (platform_keys::RULE, hop.rule),
            (platform_keys::CAUSE_ACTION, hop.action),
            (platform_keys::CAUSE_ENTITY_TYPE, hop.entity_type),
            (platform_keys::CAUSE_ENTITY_ID, hop.entity_id),
        ]);
        if in_timer_chain {
            keys.insert(platform_keys::ARMED_BY, Attr::Unknown);
        }
        Self(keys)
    }

    /// What the check holds under `key`: None for a key the kernel does not
    /// set from the cause, `Some(None)` for a cause key this check lacks.
    pub(super) fn get(&self, key: &str) -> Option<Option<&Attr>> {
        platform_keys::CAUSE.contains(&key).then(|| self.0.get(key))
    }
}

fn text(value: &str) -> Attr {
    Attr::Value(Json::String(value.to_string()))
}

/// Whether `policy` can tell a move in a timer's chain from the same move in
/// a request's: it reads `context.armedBy`, or reads `context.via` other than
/// to compare it with "request", which neither chain's reactions are.
pub(super) fn tells_timer_chains(policy: &Json) -> bool {
    let Some((op, arg)) = single(policy) else {
        return match policy {
            Json::Object(object) => object.values().any(tells_timer_chains),
            Json::Array(items) => items.iter().any(tells_timer_chains),
            _ => false,
        };
    };
    match op {
        "==" | "!=" => {
            let (left, right) = (&arg["left"], &arg["right"]);
            let against_request = |via: &Json, other: &Json| {
                reads_context(via, platform_keys::VIA)
                    && single(other) == Some(("Value", &Json::from("request")))
            };
            if against_request(left, right) || against_request(right, left) {
                return false;
            }
            tells_timer_chains(left) || tells_timer_chains(right)
        }
        // Both chains' reactions have `via`; only a timer's has `armedBy`.
        "." if is_context(&arg["left"]) => matches!(
            arg["attr"].as_str(),
            Some(platform_keys::VIA | platform_keys::ARMED_BY)
        ),
        "has" if is_context(&arg["left"]) => arg["attr"].as_str() == Some(platform_keys::ARMED_BY),
        _ => tells_timer_chains(arg),
    }
}

/// Whether `policy` reads the rule or the move that set a reaction off, or
/// the whole context, which holds them.
pub(super) fn reads_source_move(policy: &Json) -> bool {
    let Some((op, arg)) = single(policy) else {
        return match policy {
            Json::Object(object) => object.values().any(reads_source_move),
            Json::Array(items) => items.iter().any(reads_source_move),
            _ => false,
        };
    };
    match op {
        "." | "has" if is_context(&arg["left"]) => matches!(
            arg["attr"].as_str(),
            Some(platform_keys::RULE | platform_keys::CAUSE_ACTION)
        ),
        "Var" => is_context(policy),
        _ => reads_source_move(arg),
    }
}

/// Whether `expr` is `context.<key>`.
fn reads_context(expr: &Json, key: &str) -> bool {
    single(expr).is_some_and(|(op, arg)| {
        op == "." && is_context(&arg["left"]) && arg["attr"].as_str() == Some(key)
    })
}

fn is_context(expr: &Json) -> bool {
    single(expr) == Some(("Var", &Json::from("context")))
}
