//! The platform's own Cedar: the built-in `system-platform` policy (ADR-0046)
//! and the one condition that says which principals act with platform
//! authority.

use std::collections::HashMap;

use cedar_policy::{PolicyId, PolicySet};

use super::{AuthzEngine, CompiledPolicies};
use crate::context::SecurityContext;
use crate::metrics::CedarCheck;

/// The single source of truth for "who is a verified platform operator" in
/// Cedar. Shared by the built-in [`system_platform_policy`] identity-entity
/// clauses here in temper-authz AND the seeded operator `manage_policies`
/// permit in temper-platform (which re-exports this const), so the two
/// authorization surfaces can never drift to different strength.
///
/// It requires BOTH `agent_type == "operator"` and `agentTypeVerified == true`:
/// a self-declared, credential-unverified context claiming the operator type
/// (e.g. one built via `SecurityContext::with_agent_context`) does NOT satisfy
/// it. The `has` guards make a principal lacking either attribute evaluate the
/// clause to `false` (a clean deny) rather than raising a Cedar evaluation
/// error.
///
/// "operator" is a privileged claim ultimately guarded by trusted-issuer
/// registration: a registered issuer could mint an operator-claim JWT, but
/// registering an issuer is itself gated to System/Admin/verified-operator by
/// the identity-entity clauses below.
pub const VERIFIED_OPERATOR_WHEN: &str = "principal has agent_type && principal.agent_type == \"operator\" && principal has agentTypeVerified && principal.agentTypeVerified == true";

/// The Cedar condition for platform authority: System, Admin, or a verified
/// operator. Every built-in platform clause grants with this condition, and
/// [`AuthzEngine::has_platform_authority`] evaluates the same text, so kernel
/// code that exempts platform callers cannot drift from what Cedar permits.
fn platform_authority_when() -> String {
    format!("principal is System || principal is Admin || ({VERIFIED_OPERATOR_WHEN})")
}

/// Built-in Cedar in every [`AuthzEngine`] (ADR-0046): System's broad
/// authority as an auditable policy instead of an unchecked allow, and
/// what only the platform may touch (issuers, generations, agent credentials
/// and types, HTTP routes).
fn system_platform_policy() -> String {
    format!(
        r#"
@id("system-platform:broad-permit")
permit(principal is System, action, resource);

@id("system-platform:identity-entities-permit-trusted-issuer")
permit(principal, action, resource is TrustedIssuer)
when {{ {authority} }};

@id("system-platform:identity-entities-permit-principal-generation")
permit(principal, action, resource is PrincipalGeneration)
when {{ {authority} }};

@id("system-platform:protect-trusted-issuer")
forbid(principal, action, resource is TrustedIssuer)
unless {{ {authority} }};

@id("system-platform:protect-principal-generation")
forbid(principal, action, resource is PrincipalGeneration)
unless {{ {authority} }};

// The bearer resolver takes an AgentCredential and its AgentType as a verified
// identity, so only the platform writes them. Reading them is the tenant's to decide.
// Each type is in the scope, not the condition, so other types' list reads stay one decision.
@id("system-platform:protect-agent-credential")
forbid(principal, action, resource is AgentCredential)
unless {{ {authority}
  || action in [Action::"read", Action::"list"] }};

@id("system-platform:protect-agent-type")
forbid(principal, action, resource is AgentType)
unless {{ {authority}
  || action in [Action::"read", Action::"list"] }};

// ADR-0069: a route reshapes the tenant's HTTP surface, so only the platform manages one.
@id("system-platform:http-endpoint-management-permit")
permit(principal, action in [Action::"create", Action::"Create", Action::"update", Action::"delete", Action::"Pause", Action::"Resume", Action::"Delete"], resource is HttpEndpoint)
when {{ {authority} }};

@id("system-platform:protect-http-endpoint-management")
forbid(principal, action in [Action::"create", Action::"Create", Action::"update", Action::"delete", Action::"Pause", Action::"Resume", Action::"Delete"], resource is HttpEndpoint)
unless {{ {authority} }};
"#,
        authority = platform_authority_when()
    )
}

/// The action and resource type the platform-authority question is asked
/// with. Only [`platform_authority_policy`] names them.
const PLATFORM_AUTHORITY_ACTION: &str = "ActWithPlatformAuthority";
const PLATFORM_AUTHORITY_RESOURCE: &str = "Platform";

/// A one-statement policy set that permits exactly the principals with
/// platform authority. If it fails to parse the set stays empty, so every
/// principal is denied.
pub(super) fn platform_authority_policy() -> CompiledPolicies {
    let text = format!(
        "permit(principal, action == Action::\"{PLATFORM_AUTHORITY_ACTION}\", resource) when {{ {} }};",
        platform_authority_when()
    );
    CompiledPolicies::new(text.parse().unwrap_or_default())
}

impl AuthzEngine {
    /// Whether the caller acts with platform authority (System, Admin, or a
    /// verified operator), decided by Cedar with the same condition the
    /// built-in platform policy grants with. Kernel rules that exempt the
    /// platform ask this instead of reading the principal's fields.
    pub fn has_platform_authority(&self, security_ctx: &SecurityContext) -> bool {
        self.evaluate_request(
            security_ctx,
            PLATFORM_AUTHORITY_ACTION,
            PLATFORM_AUTHORITY_RESOURCE,
            &HashMap::new(),
            &self.platform_authority_policy,
            CedarCheck::PlatformAuthority,
        )
        .is_allowed()
    }
}

/// PolicyId prefix used for the built-in system-platform policies
/// (ADR-0046). Used to exclude them from user-facing counts.
const SYSTEM_PLATFORM_POLICY_ID_PREFIX: &str = "system-platform:";

/// Merge the built-in system-platform policy into an existing [`PolicySet`].
///
/// Policies are added with explicit `PolicyId`s prefixed by
/// [`SYSTEM_PLATFORM_POLICY_ID_PREFIX`] so downstream code can filter them
/// out of user-facing reports (see [`count_user_policies`]). If the
/// hard-coded system policy fails to parse, the combined set is left
/// unchanged — preserving availability at the cost of System auth.
pub(super) fn merge_system_platform_policy(combined: &mut PolicySet) {
    let system_set: PolicySet = match system_platform_policy().parse() {
        Ok(ps) => ps,
        Err(_) => return,
    };
    for (idx, policy) in system_set.policies().enumerate() {
        let named = policy.clone().new_id(PolicyId::new(format!(
            "{SYSTEM_PLATFORM_POLICY_ID_PREFIX}broad-permit-{idx}"
        )));
        let _ = combined.add(named);
    }
}

pub(super) fn compiled_system_platform_policies() -> CompiledPolicies {
    let mut policy_set = PolicySet::new();
    merge_system_platform_policy(&mut policy_set);
    CompiledPolicies::new(policy_set)
}

/// Count user-authored policies in a [`PolicySet`], excluding the built-in
/// `system-platform` policies (ADR-0046). Tenants should reason about their
/// own policy surface without the platform's internals polluting the count.
pub(super) fn count_user_policies(ps: &PolicySet) -> usize {
    ps.policies()
        .filter(|p| {
            !p.id()
                .to_string()
                .starts_with(SYSTEM_PLATFORM_POLICY_ID_PREFIX)
        })
        .count()
}

#[cfg(test)]
#[path = "platform_policy_test.rs"]
mod tests;
