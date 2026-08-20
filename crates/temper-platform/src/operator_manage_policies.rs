//! Narrow operator `manage_policies` permit seeded at credential bootstrap.
//!
//! See ADR-0172. This is ordinary Cedar — merged into live tenant policy,
//! persisted as a granular row, not a code bypass and not permit-all.

use temper_server::authz::persist_and_activate_policy;

use crate::state::PlatformState;

/// Stable granular policy id for the operator bootstrap permit.
pub const OPERATOR_MANAGE_POLICIES_POLICY_ID: &str = "operator-bootstrap-manage-policies";

/// Cedar statement granting a verified operator `manage_policies` on this tenant.
pub fn operator_manage_policies_cedar(tenant: &str) -> String {
    debug_assert!(
        !tenant.is_empty() && !tenant.contains('"'),
        "tenant id must be a Cedar-safe identifier"
    );
    format!(
        r#"permit(
  principal is Agent,
  action == Action::"manage_policies",
  resource == PolicySet::"{tenant}"
) when {{
  principal.agent_type == "operator" &&
  principal.agentTypeVerified == true
}};"#
    )
}

/// Append `statement` to `existing` when it is not already present.
pub fn merge_cedar_statement(existing: &str, statement: &str) -> String {
    let statement = statement.trim();
    let existing = existing.trim_end();
    if statement.is_empty() || existing.contains(statement) {
        return existing.to_string();
    }
    if existing.is_empty() {
        statement.to_string()
    } else {
        format!("{existing}\n{statement}")
    }
}

fn live_tenant_policy_text(state: &PlatformState, tenant: &str) -> String {
    if let Some(active_text) = state
        .server
        .authz
        .get_tenant_policy_text(tenant)
        .filter(|policy_text| !policy_text.trim().is_empty())
    {
        return active_text;
    }

    state
        .server
        .tenant_policies
        .read()
        .ok()
        .and_then(|policies| policies.get(tenant).cloned())
        .unwrap_or_default()
}

/// Merge, activate, and persist the operator `manage_policies` permit for `tenant`.
///
/// Idempotent: re-bootstrap does not duplicate the live statement or the
/// granular row. Does not replace existing app Cedar.
pub async fn seed_operator_manage_policies(state: &PlatformState, tenant: &str) {
    assert!(
        !tenant.is_empty() && !tenant.contains('"'),
        "tenant id must be a Cedar-safe identifier"
    );

    let statement = operator_manage_policies_cedar(tenant);
    let existing = live_tenant_policy_text(state, tenant);
    let merged = merge_cedar_statement(&existing, &statement);

    if let Err(error) = state.server.authz.reload_tenant_policies(tenant, &merged) {
        tracing::warn!(
            tenant,
            error = %error,
            "failed to activate operator manage_policies Cedar permit"
        );
        return;
    }

    {
        let mut policies = state
            .server
            .tenant_policies
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        policies.insert(tenant.to_string(), merged);
    }

    persist_and_activate_policy(
        &state.server,
        tenant,
        OPERATOR_MANAGE_POLICIES_POLICY_ID,
        &statement,
        "bootstrap",
    )
    .await;

    tracing::info!(
        tenant,
        policy_id = OPERATOR_MANAGE_POLICIES_POLICY_ID,
        "operator manage_policies Cedar permit seeded"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_cedar_statement_is_idempotent_and_preserves_existing() {
        let statement = operator_manage_policies_cedar("acme");
        let app = r#"permit(principal, action == Action::"read", resource is Issue);"#;

        let once = merge_cedar_statement(app, &statement);
        assert!(once.contains("resource is Issue"));
        assert!(once.contains(r#"Action::"manage_policies""#));
        assert_eq!(merge_cedar_statement(&once, &statement), once);

        assert_eq!(merge_cedar_statement("", &statement), statement.trim());
    }

    #[test]
    fn operator_manage_policies_cedar_is_tenant_scoped() {
        let acme = operator_manage_policies_cedar("acme");
        let other = operator_manage_policies_cedar("other");
        assert!(acme.contains(r#"PolicySet::"acme""#));
        assert!(!acme.contains(r#"PolicySet::"other""#));
        assert!(other.contains(r#"PolicySet::"other""#));
        assert!(acme.contains(r#"principal.agent_type == "operator""#));
        assert!(acme.contains("principal.agentTypeVerified == true"));
    }
}
