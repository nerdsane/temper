//! Install an approved `Policy` into the authorization engine.
//!
//! `Policy.Activate`'s hint is "Install the Cedar policy into the authorization
//! engine" and `Policy.Revoke`'s is "Removes it from the authorization engine".
//! Nothing did either: the actions emitted events nobody consumed, so a human
//! approved a policy, the row reached `Active`, and the live tenant policy was
//! unchanged — every newly installed app answered 403 until someone made a
//! manual policy API call (ARN-164, ARN-494).
//!
//! This is the consumer, and it is deliberately small. The kernel already has
//! the primitive it needs: the durable `policies` table is the source of truth
//! for a tenant's Cedar set, and [`load_and_activate_tenant_policies`] recomposes
//! the engine from that table, one named policy per row. So installing a policy
//! is *write its row, recompose*; revoking it is *disable its row, recompose*.
//! Ownership is the row. Two rows with identical text are two policies. The
//! bootstrap permits are rows of their own. Nothing here edits policy text.
//!
//! It runs as a bound-action hook — inside the `Activate` / `Revoke` dispatch,
//! not as a subscriber catching up afterwards — so there is no event to miss,
//! no lag to sweep for, and the caller learns synchronously if the install
//! failed. A policy that does not parse is refused before its row is written.

use temper_server::authz::load_and_activate_tenant_policies;
use temper_server::state::{BoundActionHook, BoundActionHookContext};

/// Who the durable row says wrote it.
const WRITER: &str = "policy-activation";

/// Keeps the authorization engine agreeing with `Policy` rows, at the moment
/// they change.
pub struct PolicyActivationHook;

#[async_trait::async_trait]
impl BoundActionHook for PolicyActivationHook {
    async fn after_bound_action(
        &self,
        ctx: BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        if ctx.entity_type != "Policy" {
            return Ok(None);
        }
        let tenant = ctx.tenant.as_str();
        let action = ctx.action.rsplit('.').next().unwrap_or(ctx.action);

        // Only the two terminal transitions touch the engine. Decide that
        // BEFORE requiring a store, so Propose/Approve/Reject stay no-ops even
        // where no store is configured.
        if action != "Activate" && action != "Revoke" {
            return Ok(None);
        }
        let Some(store) = ctx.state.policy_store() else {
            // Without a durable store there is nothing to recompose from, and
            // a policy installed only in memory would vanish on restart while
            // its row still said Active. Refuse rather than half-do it.
            return Err(format!(
                "Policy.{action} requires a durable policy store; none is configured"
            ));
        };

        let row_id = policy_row_id(ctx.entity_id);
        let marker = policy_row_marker(ctx.entity_id);
        match action {
            "Activate" => {
                let fields = ctx.state_json.get("fields").unwrap_or(ctx.state_json);
                let statement = fields
                    .get("cedar_statement")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|statement| !statement.is_empty())
                    .ok_or_else(|| "Policy.Activate: cedar_statement is empty".to_string())?;
                let row_text = policy_row_text(ctx.entity_id, statement);

                // Refuse a statement that does not parse BEFORE it becomes a row,
                // or every later recompose — including the next boot — would
                // fail on it.
                ctx.state
                    .authz
                    .validate_tenant_policies(&row_text)
                    .map_err(|error| {
                        format!("Policy.Activate: statement does not parse: {error}")
                    })?;

                // `save_policy` keys on (tenant, policy_id) and skips the insert
                // when identical text is already enabled under ANOTHER id. The
                // marker comment makes this row's text unique to this Policy,
                // so the only way it returns Ok(false) is the idempotent one:
                // this row, this text, already there.
                store
                    .save_policy(tenant, &row_id, &row_text, WRITER)
                    .await
                    .map_err(|error| format!("Policy.Activate: could not persist: {error}"))?;
            }
            "Revoke" => {
                let changed = store
                    .toggle_policy_enabled(tenant, &row_id, false)
                    .await
                    .map_err(|error| format!("Policy.Revoke: could not disable: {error}"))?;
                if !changed {
                    tracing::warn!(
                        tenant,
                        entity_id = ctx.entity_id,
                        "Policy.Revoke: no enabled row to disable (never activated on this kernel, or already revoked)"
                    );
                }
            }
            _ => unreachable!("filtered above"),
        }

        // Recompose, then CHECK. `load_and_activate_tenant_policies` reports
        // failure by logging, so the only way to know the engine changed is to
        // read it back: the marker must be present after Activate and absent
        // after Revoke.
        load_and_activate_tenant_policies(ctx.state, tenant).await;
        let live = ctx
            .state
            .authz
            .get_tenant_policy_text(tenant)
            .unwrap_or_default();
        let in_force = live.contains(&marker);
        let expected = action == "Activate";
        if in_force != expected {
            return Err(format!(
                "Policy.{action}: the authorization engine did not take the change \
                 (statement {} after recompose); see the kernel log for the reload error",
                if in_force { "still present" } else { "absent" }
            ));
        }
        tracing::info!(
            tenant,
            entity_id = ctx.entity_id,
            action,
            "Policy change applied to the authorization engine"
        );
        Ok(None)
    }
}

/// The durable row for a Policy entity, namespaced so it can never collide with
/// a bootstrap or app policy id.
fn policy_row_id(entity_id: &str) -> String {
    format!("policy-entity:{entity_id}")
}

/// A Cedar comment naming the entity, placed at the top of its row text. It
/// makes the row self-describing and unique per Policy — two Policies with
/// identical statements are two rows — and it is what the post-recompose check
/// looks for in the live engine text.
fn policy_row_marker(entity_id: &str) -> String {
    format!("// policy-entity:{entity_id}")
}

fn policy_row_text(entity_id: &str, statement: &str) -> String {
    format!("{}\n{}", policy_row_marker(entity_id), statement.trim())
}

/// The platform's single bound-action hook slot, shared by the concerns that
/// need one. Each hook answers only for its own entity type.
pub struct PlatformActionHooks {
    genesis_install: crate::genesis_install::GenesisInstallHook,
    policy_activation: PolicyActivationHook,
}

impl PlatformActionHooks {
    pub fn new(state: crate::state::PlatformState) -> Self {
        Self {
            genesis_install: crate::genesis_install::GenesisInstallHook::new(state),
            policy_activation: PolicyActivationHook,
        }
    }
}

#[async_trait::async_trait]
impl BoundActionHook for PlatformActionHooks {
    async fn after_bound_action(
        &self,
        ctx: BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String> {
        match ctx.entity_type {
            "Policy" => self.policy_activation.after_bound_action(ctx).await,
            _ => self.genesis_install.after_bound_action(ctx).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::tenant::TenantId;

    #[tokio::test]
    async fn without_a_durable_store_activate_refuses_rather_than_half_applying() {
        // A policy installed only in memory would vanish on restart while its
        // row still said Active — the ARN-494 shape. Refuse loudly instead.
        let state = crate::state::PlatformState::new(None);
        let tenant = TenantId::default();
        let state_json = serde_json::json!({"status":"Active","fields":{"cedar_statement":"permit(principal, action, resource);"}});
        let result = PolicyActivationHook
            .after_bound_action(BoundActionHookContext {
                state: &state.server,
                tenant: &tenant,
                entity_type: "Policy",
                entity_id: "p1",
                action: "Temper.Activate",
                params: &serde_json::json!({}),
                state_json: &state_json,
            })
            .await;
        let error = result.expect_err("no store must be an error the caller sees");
        assert!(error.contains("durable policy store"), "{error}");
    }

    #[tokio::test]
    async fn other_entities_and_other_policy_actions_are_ignored() {
        let state = crate::state::PlatformState::new(None);
        let tenant = TenantId::default();
        let empty = serde_json::json!({});
        for (entity_type, action) in [("App", "Temper.Activate"), ("Policy", "Temper.Propose")] {
            let result = PolicyActivationHook
                .after_bound_action(BoundActionHookContext {
                    state: &state.server,
                    tenant: &tenant,
                    entity_type,
                    entity_id: "x",
                    action,
                    params: &empty,
                    state_json: &empty,
                })
                .await;
            assert!(
                matches!(result, Ok(None)),
                "{entity_type}.{action} must be a no-op"
            );
        }
    }

    #[test]
    fn a_policy_row_is_namespaced_and_self_describing() {
        assert_eq!(policy_row_id("p1"), "policy-entity:p1");
        let text = policy_row_text("p1", "  permit(principal, action, resource);  ");
        assert!(text.starts_with("// policy-entity:p1\n"), "{text}");
        assert!(
            text.ends_with("permit(principal, action, resource);"),
            "{text}"
        );
        // two Policies with the same statement are two distinct rows
        assert_ne!(
            policy_row_text("p1", "permit(principal, action, resource);"),
            policy_row_text("p2", "permit(principal, action, resource);")
        );
    }
}
