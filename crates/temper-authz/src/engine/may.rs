//! Whether a kind of sender may send an action, decided from the policies
//! alone, without a request.
//!
//! The joint model of an app asks who can make each move: someone outside
//! the kernel, or one of the kernel's own services. A real request names one
//! principal, one record and its fields; this question names only the kind of
//! sender, the action and the entity type. Each policy is evaluated with what
//! that leaves unknown (an outside principal's id and attributes, the record's
//! id and fields, the context keys a record's fields may supply), and each
//! condition comes out as the set of results it can have: true, false or an
//! evaluation error, which Cedar takes as the policy not applying.
//!
//! The answer is [`May::Yes`] only when some permit applies whatever the
//! unknowns are and no forbid can, [`May::No`] when no permit can apply or a
//! forbid always does, and [`May::Maybe`] otherwise. A caller that knows some
//! of the record's attributes passes them, and those are unknown no longer.

mod json;
mod request;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use cedar_policy::{Effect, PolicySet};
use serde_json::Value as Json;

use super::platform_policy::merge_system_platform_policy;
use crate::error::AuthzError;
use request::{Request, TRUE};

/// A kind of sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    /// Anyone outside the kernel: a person, an agent, an operator's key, or a
    /// module calling the API as whoever invoked it. Never a `System`
    /// principal and never a `service:` id: those only the kernel builds.
    Outside,
    /// A kernel service as `AgentContext::for_service` builds it:
    /// `Agent::"service:<name>"` with `agent_type` the name, `role`
    /// "service" and `agentTypeVerified` false, and these context
    /// attributes besides the ones that builds.
    Service {
        /// The service's name, without `service:`.
        name: String,
        /// Context attributes the kernel adds, such as a declared trigger's.
        context: BTreeMap<String, Attr>,
    },
}

/// A context attribute the kernel sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attr {
    /// This value.
    Value(Json),
    /// The id of the record the request acts on, as `resource.id` holds it.
    ResourceId,
    /// A string known only by which others it equals: the same as each
    /// `Same` with this name and no other, never empty, and none of
    /// `unlike`. A model that names the values it makes up (rather than
    /// holding real ones) knows them only this way, so one is never taken
    /// to be a policy's literal, only kept apart from the literals the
    /// model holds beside it.
    Same {
        /// The name the model gave the value.
        name: String,
        /// Strings the value is certainly not.
        unlike: BTreeSet<String>,
    },
    /// Some value not known here.
    Unknown,
}

/// Whether a kind of sender may send an action to an entity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum May {
    /// Cedar refuses it whatever the request holds.
    No,
    /// It depends on what is not known here: which outside principal, or the
    /// record's fields.
    Maybe,
    /// Cedar allows it whatever the request holds.
    Yes,
}

/// A tenant's policies with the built-in platform policy, read for
/// [`SendAnalysis::may`].
#[derive(Debug, Clone)]
pub struct SendAnalysis {
    policies: Vec<(Effect, Json)>,
}

impl SendAnalysis {
    /// Read `policy_text` (a tenant's policies) with the platform's own.
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        let mut set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut set);
        let policies = set
            .policies()
            .map(|policy| {
                // A template-linked policy has no JSON form of its own; an
                // unknown condition makes it a policy that may apply.
                let json = policy.to_json().unwrap_or(Json::Null);
                (policy.effect(), json)
            })
            .collect();
        Ok(Self { policies })
    }

    /// Whether `asker` may send `action` to a record of `resource_type`, of
    /// which the caller knows the attributes in `record` (`id` its id).
    pub fn may(
        &self,
        asker: &Asker,
        action: &str,
        resource_type: &str,
        record: &BTreeMap<String, Attr>,
    ) -> May {
        let request = Request {
            asker,
            action,
            resource_type,
            record,
        };
        let (mut permit_can, mut permit_always) = (false, false);
        let (mut forbid_can, mut forbid_always) = (false, false);
        for (effect, policy) in &self.policies {
            let applies = request.applies(policy);
            let (can, always) = (applies.can(TRUE), applies == TRUE);
            match effect {
                Effect::Permit => {
                    permit_can |= can;
                    permit_always |= always;
                }
                Effect::Forbid => {
                    forbid_can |= can;
                    forbid_always |= always;
                }
            }
        }
        if !permit_can || forbid_always {
            May::No
        } else if permit_always && !forbid_can {
            May::Yes
        } else {
            May::Maybe
        }
    }

    /// The policies that may apply when `asker` sends `action` to a
    /// `resource_type` record whatever it holds, as text in a fixed order.
    /// [`Self::may`] asked again with some of the record known reads no
    /// other: knowing more only rules policies out.
    pub fn applicable(&self, asker: &Asker, action: &str, resource_type: &str) -> Vec<String> {
        let record = BTreeMap::new();
        let request = Request {
            asker,
            action,
            resource_type,
            record: &record,
        };
        let mut applicable: Vec<String> = self
            .policies
            .iter()
            .filter(|(_, policy)| request.applies(policy).can(TRUE))
            .map(|(effect, policy)| format!("{effect:?} {policy}"))
            .collect();
        applicable.sort();
        applicable
    }
}
