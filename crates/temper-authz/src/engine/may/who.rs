//! Whether an open answer can come out differently for different callers.
//!
//! [`SendAnalysis::may`] says [`May::Maybe`] when it doesn't know enough to
//! answer. That can be who asks, which real callers settle both ways, or only a
//! field of the record it wasn't told, where the answer is one for every caller
//! and may never be the other.

use std::collections::BTreeMap;

use serde_json::Value as Json;

use super::json::single;
use super::profile::UNKNOWN;
use super::{Asker, Request, SendAnalysis, TRUE};
use crate::platform_keys;

impl SendAnalysis {
    /// Whether some policy `asker`'s answer for `action` on a `resource_type`
    /// record is open on reads who asks, or a context key that says so. The
    /// keys of a move's cause come from the move, and a service's own id is
    /// known, so neither counts.
    pub fn turns_on_who_asks(&self, asker: &Asker, action: &str, resource_type: &str) -> bool {
        let record = BTreeMap::new();
        let request = Request {
            asker,
            profile: &UNKNOWN,
            action,
            resource_type,
            record: &record,
            cause: None,
        };
        self.scoped(action, resource_type).iter().any(|&at| {
            let (_, policy) = &self.policies[at];
            let applies = request.applies(policy);
            applies.can(TRUE) && applies != TRUE && reads_who(policy, asker)
        })
    }
}

fn reads_who(policy: &Json, asker: &Asker) -> bool {
    let outside = !matches!(asker, Asker::Service { .. });
    if outside && policy["principal"]["op"].as_str() != Some("All") {
        return true;
    }
    policy["conditions"]
        .as_array()
        .is_some_and(|conditions| conditions.iter().any(|c| reads(&c["body"], asker, outside)))
}

fn reads(expr: &Json, asker: &Asker, outside: bool) -> bool {
    if let Some(("." | "has", of)) = single(expr)
        && single(&of["left"]) == Some(("Var", &Json::from("context")))
    {
        return of["attr"]
            .as_str()
            .is_some_and(|key| !known_key(key, asker));
    }
    if single(expr) == Some(("Var", &Json::from("principal"))) {
        return outside;
    }
    match expr {
        Json::Object(fields) => fields.values().any(|value| reads(value, asker, outside)),
        Json::Array(items) => items.iter().any(|item| reads(item, asker, outside)),
        _ => false,
    }
}

/// Whether the context key `key` is one the move or the kernel's service fixes.
fn known_key(key: &str, asker: &Asker) -> bool {
    platform_keys::CAUSE.contains(&key)
        || matches!(asker, Asker::Service { context, .. } if context.contains_key(key))
}
