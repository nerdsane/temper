//! Who sends: a kernel service as the engine builds it, or someone
//! outside as its profile knows it.

use serde_json::Value as Json;

use super::super::Attr;
use super::super::json::same;
use super::super::profile::{Fact, Profile};
use super::{Asker, FALSE, Request, Results, TRUE, TRUE_OR_FALSE, Val, made, read};

impl<'a> Request<'a> {
    /// The service principal's attributes, as the engine builds them.
    pub(super) fn service_attribute(name: &str, attr: &str) -> Option<Json> {
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
    pub(super) fn service_context<'e>(&self, key: &str) -> Option<Val<'e>>
    where
        'a: 'e,
    {
        let Asker::Service { name, context } = self.asker else {
            return None;
        };
        let id = format!("service:{name}");
        match key {
            "role" => Some(made(Json::String("service".to_string()))),
            "agentId" => Some(made(Json::String(id))),
            "agentType" => Some(made(Json::String(name.clone()))),
            "agentTypeVerified" => Some(made(Json::Bool(false))),
            _ => context.get(key).map(|attr: &'a Attr| self.known(attr)),
        }
    }

    /// What is known of the sender when it is someone outside.
    pub(super) fn outside(&self) -> Option<&'a Profile> {
        match self.asker {
            Asker::Outside | Asker::OutsideAs(_) => Some(self.profile),
            Asker::Service { .. } => None,
        }
    }

    /// The value of what an outside sender holds, as its profile knows it
    /// (`fact`), or unknown.
    pub(super) fn fact_value<'e>(fact: Option<&'e Fact>) -> Val<'e> {
        match fact {
            Some(Fact::Absent) => Val::Error,
            Some(Fact::Is(value)) => read(value),
            Some(Fact::Other {
                unlike,
                like,
                string,
            }) => Val::Other {
                unlike,
                like,
                string: *string,
            },
            None => Val::Unknown,
        }
    }

    /// What an outside sender's security context holds under `key`, when
    /// its profile knows: only a key the kernel alone sets, which no record
    /// field fills.
    pub(super) fn sender_key(&self, key: &str) -> Option<&'a Fact> {
        self.outside()?.context.get(key)
    }
}

/// Whether a sender's `fact` equals `value`.
pub(super) fn fact_equal(fact: &Fact, value: &Json) -> Results {
    match fact {
        Fact::Absent => FALSE,
        Fact::Is(known) => Results::of(same(known, value)),
        Fact::Other { unlike, .. } if unlike.iter().any(|other| same(other, value)) => FALSE,
        Fact::Other { .. } => TRUE_OR_FALSE,
    }
}

impl Request<'_> {
    /// Whether `policy`'s action and resource scopes fit this request; they
    /// read neither the sender nor the cause.
    pub(in crate::engine::may) fn scope_fits(&self, policy: &Json) -> bool {
        let Some(object) = policy.as_object() else {
            return true;
        };
        self.action_scope(&object["action"])
            .and_then(|| self.resource_scope(&object["resource"]))
            .can(TRUE)
    }
}
