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
