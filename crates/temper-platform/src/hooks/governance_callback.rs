use sha2::{Digest, Sha256};
use temper_runtime::TenantId;
use temper_server::ServerState;
use temper_server::request_context::AgentContext;

/// Dispatch callback to a registered target entity.
///
/// Reads callback fields from GovernanceDecision entity state. If a callback
/// is registered (callback_tenant is non-empty), dispatches the appropriate
/// action on the target entity via cross-tenant dispatch.
///
/// This hook is also used for late callback registration: `RegisterCallback`
/// now triggers `DispatchCallback`, so replaying callback wiring after a
/// decision has already been approved or denied will immediately redeliver
/// the resolution to the waiting target entity.
pub(super) async fn handle_dispatch_callback(
    source_entity_id: &str,
    entity_fields: &serde_json::Value,
    server: &ServerState,
) -> Result<(), String> {
    let callback_tenant = entity_fields
        .get("callback_tenant")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let callback_entity_set = entity_fields
        .get("callback_entity_set")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let callback_entity_id = entity_fields
        .get("callback_entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if callback_tenant.is_empty() || callback_entity_set.is_empty() || callback_entity_id.is_empty()
    {
        tracing::debug!("DispatchCallback: no callback registered — skipping");
        return Ok(());
    }

    let status = entity_fields
        .get("Status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let callback_action = match status {
        "Approved" => entity_fields
            .get("callback_on_approve")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Denied" => entity_fields
            .get("callback_on_deny")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        "Pending" => {
            tracing::debug!("DispatchCallback: decision still pending — skipping callback");
            return Ok(());
        }
        _ => {
            tracing::debug!(
                status,
                "DispatchCallback: decision not in a callback-emitting terminal state — skipping"
            );
            return Ok(());
        }
    };

    if callback_action.is_empty() {
        tracing::debug!(
            status,
            "DispatchCallback: no callback action for status — skipping"
        );
        return Ok(());
    }

    let params = if status == "Denied" {
        serde_json::json!({"error_message": "Action denied by human reviewer"})
    } else {
        serde_json::json!({})
    };

    tracing::info!(
        callback_tenant,
        callback_entity_set,
        callback_entity_id,
        callback_action,
        "DispatchCallback: dispatching callback"
    );

    let tid = TenantId::new(callback_tenant);
    let entity_type = resolve_callback_entity_type(server, &tid, callback_entity_set);
    let mut agent_context = AgentContext::for_service("governance-service");
    agent_context.idempotency_key = Some(callback_idempotency_key(
        source_entity_id,
        status,
        callback_tenant,
        &entity_type,
        callback_entity_id,
        callback_action,
    ));
    let response = server
        .dispatch_tenant_action(
            &tid,
            &entity_type,
            callback_entity_id,
            callback_action,
            params,
            &agent_context,
        )
        .await
        .map_err(|error| {
            tracing::error!(
                error = %error,
                tenant = callback_tenant,
                entity_set = callback_entity_set,
                entity_type,
                entity_id = callback_entity_id,
                action = callback_action,
                "DispatchCallback: failed to dispatch callback action"
            );
            format!("DispatchCallback: failed to dispatch callback action: {error}")
        })?;
    if !response.success {
        let error = response
            .error
            .as_deref()
            .unwrap_or("callback action returned success=false without an error");
        tracing::error!(
            error,
            tenant = callback_tenant,
            entity_set = callback_entity_set,
            entity_type,
            entity_id = callback_entity_id,
            action = callback_action,
            "DispatchCallback: callback action was rejected"
        );
        return Err(format!(
            "DispatchCallback: callback action was rejected: {error}"
        ));
    }

    tracing::info!(
        tenant = callback_tenant,
        entity_set = callback_entity_set,
        entity_type,
        entity_id = callback_entity_id,
        action = callback_action,
        "DispatchCallback: callback action dispatched successfully"
    );

    Ok(())
}

fn callback_idempotency_key(
    source_entity_id: &str,
    status: &str,
    callback_tenant: &str,
    callback_entity_type: &str,
    callback_entity_id: &str,
    callback_action: &str,
) -> String {
    let mut hasher = Sha256::new();
    for part in [
        "DispatchCallback",
        source_entity_id,
        status,
        callback_tenant,
        callback_entity_type,
        callback_entity_id,
        callback_action,
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("governance-callback:{:x}", hasher.finalize())
}

/// Resolve a callback target from an OData entity-set name to a governed entity type.
///
/// Callbacks are registered over HTTP and therefore store OData-facing entity set names
/// like `Sessions`. The dispatch layer, however, expects governed entity type names like
/// `Session`. If the callback already contains a type name, we preserve it.
fn resolve_callback_entity_type(
    server: &ServerState,
    tenant: &TenantId,
    entity_set_or_type: &str,
) -> String {
    {
        let registry = match server.registry.read() {
            Ok(registry) => registry,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entity_type) = registry.resolve_entity_type(tenant, entity_set_or_type) {
            return entity_type;
        }
        if registry.get_spec(tenant, entity_set_or_type).is_some() {
            return entity_set_or_type.to_string();
        }
    }
    server
        .entity_set_map
        .get(entity_set_or_type)
        .cloned()
        .or_else(|| {
            server
                .transition_tables
                .contains_key(entity_set_or_type)
                .then(|| entity_set_or_type.to_string())
        })
        .unwrap_or_else(|| entity_set_or_type.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::*;
    use crate::bootstrap::{SYSTEM_TENANT, bootstrap_system_tenant};
    use crate::state::PlatformState;
    use temper_spec::csdl::{CsdlDocument, parse_csdl};

    fn session_callback_csdl() -> (CsdlDocument, String) {
        let xml = r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="OpenPaw.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="Session">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
              </EntityType>
              <EntityContainer Name="OpenPawTestService">
                <EntitySet Name="Sessions" EntityType="OpenPaw.Test.Session"/>
              </EntityContainer>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#;
        (parse_csdl(xml).unwrap(), xml.to_string())
    }

    const SESSION_IOA: &str = r#"
[automaton]
name = "Session"
initial = "WaitingForApproval"
states = ["WaitingForApproval", "Executing", "Failed"]

[[action]]
name = "ResumeAfterApproval"
from = ["WaitingForApproval"]
to = "Executing"
kind = "input"

[[action]]
name = "Fail"
from = ["WaitingForApproval"]
to = "Failed"
kind = "input"
params = ["error_message"]
"#;

    const RETRY_CALLBACK_IOA: &str = r#"
[automaton]
name = "Session"
initial = "Executing"
states = ["Executing"]

[[state]]
name = "callback_count"
type = "counter"
initial = "0"

[[action]]
name = "ResumeAfterApproval"
from = ["Executing"]
to = "Executing"
kind = "input"
effect = [{ type = "increment", var = "callback_count" }]
"#;

    #[test]
    fn callback_entity_set_resolution_uses_registry_mapping() {
        let state = PlatformState::new(None);
        let (csdl, xml) = session_callback_csdl();
        state.registry.write().unwrap().register_tenant(
            "default",
            csdl,
            xml,
            &[("Session", SESSION_IOA)],
        );

        let tenant = TenantId::new("default");
        assert_eq!(
            resolve_callback_entity_type(&state.server, &tenant, "Sessions"),
            "Session"
        );
    }

    #[test]
    fn callback_entity_set_resolution_preserves_entity_type_inputs() {
        let state = PlatformState::new(None);
        let (csdl, xml) = session_callback_csdl();
        state.registry.write().unwrap().register_tenant(
            "default",
            csdl,
            xml,
            &[("Session", SESSION_IOA)],
        );

        let tenant = TenantId::new("default");
        assert_eq!(
            resolve_callback_entity_type(&state.server, &tenant, "Session"),
            "Session"
        );
    }

    #[tokio::test]
    async fn callback_dispatch_failure_is_returned_before_effect_acknowledgement() {
        let state = PlatformState::new(None);
        let result = handle_dispatch_callback(
            "gd-missing-callback-test",
            &serde_json::json!({
                "Status": "Approved",
                "callback_tenant": "default",
                "callback_entity_set": "MissingEntities",
                "callback_entity_id": "missing-callback-target",
                "callback_on_approve": "ResumeAfterApproval",
            }),
            &state.server,
        )
        .await;

        assert!(
            result.is_err(),
            "a failed callback must keep the durable custom-effect receipt retryable"
        );
    }

    #[tokio::test]
    async fn rejected_callback_response_is_returned_before_effect_acknowledgement() {
        let state = PlatformState::new(None);
        let (csdl, xml) = session_callback_csdl();
        state.registry.write().unwrap().register_tenant(
            "default",
            csdl,
            xml,
            &[("Session", SESSION_IOA)],
        );

        let result = handle_dispatch_callback(
            "gd-rejected-callback-test",
            &serde_json::json!({
                "Status": "Approved",
                "callback_tenant": "default",
                "callback_entity_set": "Sessions",
                "callback_entity_id": "rejected-callback-target",
                "callback_on_approve": "MissingAction",
            }),
            &state.server,
        )
        .await;

        assert!(
            result.is_err(),
            "a rejected governed callback must keep the durable custom-effect receipt retryable"
        );
    }

    #[tokio::test]
    async fn retried_callback_uses_one_stable_target_idempotency_key() {
        let state = PlatformState::new(None);
        let (csdl, xml) = session_callback_csdl();
        state.registry.write().unwrap().register_tenant(
            "default",
            csdl,
            xml,
            &[("Session", RETRY_CALLBACK_IOA)],
        );
        let fields = serde_json::json!({
            "Status": "Approved",
            "callback_tenant": "default",
            "callback_entity_set": "Sessions",
            "callback_entity_id": "retried-callback-target",
            "callback_on_approve": "ResumeAfterApproval",
        });

        handle_dispatch_callback("gd-retried-callback-test", &fields, &state.server)
            .await
            .expect("first callback dispatch should succeed");
        handle_dispatch_callback("gd-retried-callback-test", &fields, &state.server)
            .await
            .expect("replayed callback dispatch should be idempotent");

        let entity = state
            .server
            .get_tenant_entity_state(
                &TenantId::new("default"),
                "Session",
                "retried-callback-target",
            )
            .await
            .expect("callback target should exist");
        assert_eq!(
            entity.state.counters.get("callback_count"),
            Some(&1),
            "a source-effect retry must not apply the callback twice"
        );
    }

    #[tokio::test]
    async fn approved_governance_decision_dispatches_callback_registered_with_entity_set_name() {
        let state = PlatformState::new(None);
        bootstrap_system_tenant(&state, &BTreeMap::new());

        let (csdl, xml) = session_callback_csdl();
        state.registry.write().unwrap().register_tenant(
            "default",
            csdl,
            xml,
            &[("Session", SESSION_IOA)],
        );

        let system_tenant = TenantId::new(SYSTEM_TENANT);
        let app_tenant = TenantId::new("default");
        let decision_id = "gd-callback-test";
        let session_id = "ss-callback-test";

        state
            .server
            .dispatch_tenant_action(
                &system_tenant,
                "GovernanceDecision",
                decision_id,
                "CreateGovernanceDecision",
                serde_json::json!({
                    "tenant": "default",
                    "agent_id": "bot-openpaw",
                    "action_name": "temper.submit_specs",
                    "resource_type": "Session",
                    "resource_id": session_id,
                    "denial_reason": "",
                    "scope": "narrow",
                    "pending_decision_id": decision_id,
                }),
                &AgentContext::for_service("governance-service"),
            )
            .await
            .expect("governance decision should be created");

        state
            .server
            .dispatch_tenant_action(
                &system_tenant,
                "GovernanceDecision",
                decision_id,
                "RegisterCallback",
                serde_json::json!({
                    "callback_tenant": "default",
                    "callback_entity_set": "Sessions",
                    "callback_entity_id": session_id,
                    "callback_on_approve": "ResumeAfterApproval",
                    "callback_on_deny": "Fail",
                }),
                &AgentContext::for_service("governance-service"),
            )
            .await
            .expect("callback should register");

        let approval = state
            .server
            .dispatch_tenant_action(
                &system_tenant,
                "GovernanceDecision",
                decision_id,
                "Approve",
                serde_json::json!({
                    "decided_by": "human-reviewer",
                    "scope": "narrow",
                    "generated_policy": "",
                    "policy_already_published": true,
                }),
                &AgentContext::for_service("governance-service"),
            )
            .await
            .expect("approval should succeed");
        assert!(
            approval.success,
            "approval post-effects should succeed: {:?}",
            approval.error
        );

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(entity) = state
                    .server
                    .get_tenant_entity_state(&app_tenant, "Session", session_id)
                    .await
                    && entity.state.status == "Executing"
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("callback should resume the target session");
    }

    #[tokio::test]
    async fn late_callback_registration_replays_approved_governance_decision() {
        let state = PlatformState::new(None);
        bootstrap_system_tenant(&state, &BTreeMap::new());

        let (csdl, xml) = session_callback_csdl();
        state.registry.write().unwrap().register_tenant(
            "default",
            csdl,
            xml,
            &[("Session", SESSION_IOA)],
        );

        let system_tenant = TenantId::new(SYSTEM_TENANT);
        let app_tenant = TenantId::new("default");
        let decision_id = "gd-late-callback-test";
        let session_id = "ss-late-callback-test";

        state
            .server
            .dispatch_tenant_action(
                &system_tenant,
                "GovernanceDecision",
                decision_id,
                "CreateGovernanceDecision",
                serde_json::json!({
                    "tenant": "default",
                    "agent_id": "bot-openpaw",
                    "action_name": "temper.submit_specs",
                    "resource_type": "Session",
                    "resource_id": session_id,
                    "denial_reason": "",
                    "scope": "narrow",
                    "pending_decision_id": decision_id,
                }),
                &AgentContext::for_service("governance-service"),
            )
            .await
            .expect("governance decision should be created");

        state
            .server
            .dispatch_tenant_action(
                &system_tenant,
                "GovernanceDecision",
                decision_id,
                "Approve",
                serde_json::json!({
                    "decided_by": "human-reviewer",
                    "scope": "narrow",
                    "generated_policy": "",
                }),
                &AgentContext::for_service("governance-service"),
            )
            .await
            .expect("approval should succeed before callback registration");

        state
            .server
            .dispatch_tenant_action(
                &system_tenant,
                "GovernanceDecision",
                decision_id,
                "RegisterCallback",
                serde_json::json!({
                    "callback_tenant": "default",
                    "callback_entity_set": "Sessions",
                    "callback_entity_id": session_id,
                    "callback_on_approve": "ResumeAfterApproval",
                    "callback_on_deny": "Fail",
                }),
                &AgentContext::for_service("governance-service"),
            )
            .await
            .expect("late callback registration should replay the approval callback");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(entity) = state
                    .server
                    .get_tenant_entity_state(&app_tenant, "Session", session_id)
                    .await
                    && entity.state.status == "Executing"
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("late callback registration should resume the target session");
    }
}
