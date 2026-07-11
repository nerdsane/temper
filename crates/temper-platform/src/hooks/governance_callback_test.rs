use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::bootstrap::{SYSTEM_TENANT, bootstrap_system_tenant};
use crate::state::PlatformState;
use temper_authz::{ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope};
use temper_server::StorageStack;
use temper_server::authz::{DecisionPolicyInstall, DecisionPolicyReceipt, install_decision_policy};
use temper_server::state::{DecisionResolutionKind, DecisionResolutionPhase, PendingDecision};
use temper_spec::csdl::{CsdlDocument, parse_csdl};
use temper_store_turso::TursoEventStore;

async fn platform_state() -> (PlatformState, TursoEventStore) {
    let db_url = format!(
        "file:/tmp/temper-governance-hook-{}.db",
        uuid::Uuid::new_v4()
    );
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create governance hook store");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(StorageStack::from_turso(store.clone()));
    state.server.secrets_vault = Some(std::sync::Arc::new(
        temper_server::secrets::vault::SecretsVault::new(&[7; 32]),
    ));
    (state, store)
}

fn callback_params(
    state: &PlatformState,
    decision_id: &str,
    session_id: &str,
) -> serde_json::Value {
    let capability = state
        .server
        .mint_governance_callback_capability(
            decision_id,
            "default",
            "Session",
            session_id,
            "ResumeAfterApproval",
            "Fail",
        )
        .expect("mint callback capability");
    serde_json::json!({
        "callback_tenant": "default",
        "callback_entity_set": "Sessions",
        "callback_entity_id": session_id,
        "callback_on_approve": "ResumeAfterApproval",
        "callback_on_deny": "Fail",
        "callback_capability": capability,
    })
}

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

fn approval_fixture(
    governance_decision_id: &str,
    pending_decision_id: &str,
    session_id: &str,
) -> (serde_json::Value, String) {
    let matrix = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::ThisResource,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    let policy = temper_authz::generate_cedar_from_matrix(
        "bot-openpaw",
        "Agent",
        "temper.submit_specs",
        "Session",
        session_id,
        &matrix,
    )
    .expect("callback fixture policy should generate");
    let receipt = DecisionPolicyReceipt {
        pending_decision_id: pending_decision_id.to_string(),
        governance_decision_id: governance_decision_id.to_string(),
        principal_kind: "Agent".to_string(),
        scope_matrix: matrix,
    };
    (
        serde_json::json!({
            "decided_by": "human-reviewer",
            "scope": receipt.encode().expect("receipt should encode"),
            "generated_policy": policy,
        }),
        policy,
    )
}

async fn prepare_durable_approval(
    state: &PlatformState,
    store: &TursoEventStore,
    governance_decision_id: &str,
    pending_decision_id: &str,
    session_id: &str,
    policy: &str,
) {
    let policy_id = format!("decision:{pending_decision_id}");
    let publication_version = match install_decision_policy(
        &state.server,
        "default",
        &policy_id,
        policy,
        "human-reviewer",
    )
    .await
    .expect("install callback fixture policy")
    {
        DecisionPolicyInstall::Created {
            publication_version,
        }
        | DecisionPolicyInstall::AlreadyPresent {
            publication_version,
        } => publication_version,
    };
    let mut decision = PendingDecision::from_denial(
        "default",
        "bot-openpaw",
        "temper.submit_specs",
        "Session",
        session_id,
        serde_json::json!({}),
        "fixture denial",
        None,
    );
    decision.id = pending_decision_id.to_string();
    decision.principal_kind = Some("Agent".to_string());
    decision.governance_decision_id = Some(governance_decision_id.to_string());
    decision.resolution_owner = Some(format!("fixture-owner:{pending_decision_id}"));
    decision.resolution_kind = Some(DecisionResolutionKind::Approve);
    decision.resolution_phase = Some(DecisionResolutionPhase::PolicyPublished);
    decision.resolution_policy_version = Some(publication_version);
    let encoded = serde_json::to_string(&decision).expect("encode durable fixture decision");
    store
        .upsert_pending_decision(pending_decision_id, "default", "resolving", &encoded)
        .await
        .expect("persist durable fixture decision");
}

#[tokio::test]
async fn approved_governance_decision_dispatches_callback_registered_with_entity_set_name() {
    let (state, store) = platform_state().await;
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
            &AgentContext::for_service("platform-dispatch"),
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
            callback_params(&state, decision_id, session_id),
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
        .expect("callback should register");

    let (approval_params, policy) = approval_fixture(decision_id, decision_id, session_id);
    prepare_durable_approval(
        &state,
        &store,
        decision_id,
        decision_id,
        session_id,
        &policy,
    )
    .await;
    let approval = state
        .server
        .dispatch_tenant_action(
            &system_tenant,
            "GovernanceDecision",
            decision_id,
            "Approve",
            approval_params,
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
        .expect("approval should succeed");
    assert!(
        approval.success,
        "approval effects should succeed: {approval:?}"
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

    let sequence_after_first_callback = state
        .server
        .get_tenant_entity_state(&app_tenant, "Session", session_id)
        .await
        .expect("session should exist")
        .state
        .sequence_nr;
    state
        .server
        .dispatch_tenant_action(
            &system_tenant,
            "GovernanceDecision",
            decision_id,
            "RegisterCallback",
            callback_params(&state, decision_id, session_id),
            &AgentContext::for_service("governance-service"),
        )
        .await
        .expect("re-registering callback should be accepted");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sequence_after_retry = state
        .server
        .get_tenant_entity_state(&app_tenant, "Session", session_id)
        .await
        .expect("session should still exist")
        .state
        .sequence_nr;
    assert_eq!(
        sequence_after_retry, sequence_after_first_callback,
        "stable governance callback idempotency must not apply the callback twice"
    );
}

#[tokio::test]
async fn failed_policy_receipt_stops_callback_and_same_dispatch_key_retries_effects() {
    let (state, store) = platform_state().await;
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
    let decision_id = "gd-effect-retry";
    let pending_id = "pd-effect-retry";
    let session_id = "ss-effect-retry";
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
                "pending_decision_id": pending_id,
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
            callback_params(&state, decision_id, session_id),
            &AgentContext::for_service("governance-service"),
        )
        .await
        .expect("callback should register");

    let (approval_params, policy) = approval_fixture(decision_id, pending_id, session_id);
    let mut context = AgentContext::for_service("platform-dispatch");
    context.idempotency_key = Some("governance-approval:default:pd-effect-retry".to_string());
    let failed = state
        .server
        .dispatch_tenant_action(
            &system_tenant,
            "GovernanceDecision",
            decision_id,
            "Approve",
            approval_params.clone(),
            &context,
        )
        .await
        .expect("the transition result should be returned");
    assert!(
        !failed.success,
        "missing preinstalled policy must fail the effect"
    );
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("GenerateCedarPolicy"))
    );
    let waiting = state
        .server
        .get_tenant_entity_state(&app_tenant, "Session", session_id)
        .await
        .expect("callback target should resolve");
    assert_eq!(waiting.state.status, "WaitingForApproval");

    prepare_durable_approval(&state, &store, decision_id, pending_id, session_id, &policy).await;
    let retried = state
        .server
        .dispatch_tenant_action(
            &system_tenant,
            "GovernanceDecision",
            decision_id,
            "Approve",
            approval_params,
            &context,
        )
        .await
        .expect("same dispatch id should replay unapplied effects");
    assert!(
        retried.success,
        "receipt and callback retry should succeed: {retried:?}"
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
    .expect("effect retry should deliver callback after receipt activation");
}

#[tokio::test]
async fn late_callback_registration_replays_approved_governance_decision() {
    let (state, store) = platform_state().await;
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

    let (approval_params, policy) = approval_fixture(decision_id, decision_id, session_id);
    prepare_durable_approval(
        &state,
        &store,
        decision_id,
        decision_id,
        session_id,
        &policy,
    )
    .await;
    let approval = state
        .server
        .dispatch_tenant_action(
            &system_tenant,
            "GovernanceDecision",
            decision_id,
            "Approve",
            approval_params,
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
        .expect("approval should succeed before callback registration");
    assert!(
        approval.success,
        "approval effects should succeed: {approval:?}"
    );

    state
        .server
        .dispatch_tenant_action(
            &system_tenant,
            "GovernanceDecision",
            decision_id,
            "RegisterCallback",
            callback_params(&state, decision_id, session_id),
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
