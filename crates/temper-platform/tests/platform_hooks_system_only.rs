//! Platform hooks run only for the platform's own system-tenant entities.
//!
//! The platform installs GovernanceDecision, whose hooks act on the tenant and
//! entity named in the record, only in `temper-system`. A tenant's own spec may
//! declare a trigger with the same hook name; for that tenant it is an ordinary
//! custom effect and never reaches the platform handler.

use std::collections::BTreeMap;
use std::time::Duration;

use temper_platform::bootstrap::{SYSTEM_TENANT, bootstrap_system_tenant};
use temper_platform::state::PlatformState;
use temper_runtime::TenantId;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const APP_TENANT: &str = "tenant-a";
const TARGET_TENANT: &str = "tenant-b";

fn csdl(entity_type: &str, entity_set: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
        <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
          <edmx:DataServices>
            <Schema Namespace="Hooks.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
              <EntityType Name="{entity_type}">
                <Key><PropertyRef Name="Id"/></Key>
                <Property Name="Id" Type="Edm.String" Nullable="false"/>
              </EntityType>
              <EntityContainer Name="HooksTestService">
                <EntitySet Name="{entity_set}" EntityType="Hooks.Test.{entity_type}"/>
              </EntityContainer>
            </Schema>
          </edmx:DataServices>
        </edmx:Edmx>"#
    )
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

/// An app tenant's own spec whose action names the platform's callback hook.
const RELAY_IOA: &str = r#"
[automaton]
name = "Relay"
initial = "Draft"
states = ["Draft", "Approved"]

[[state]]
name = "callback_tenant"
type = "string"
initial = ""

[[state]]
name = "callback_entity_set"
type = "string"
initial = ""

[[state]]
name = "callback_entity_id"
type = "string"
initial = ""

[[state]]
name = "callback_on_approve"
type = "string"
initial = ""

[[action]]
name = "Approve"
from = ["Draft"]
to = "Approved"
kind = "input"
params = ["callback_tenant", "callback_entity_set", "callback_entity_id", "callback_on_approve"]

[[action.triggers]]
name = "DispatchCallback"
kind = "hook"
hook = "DispatchCallback"
"#;

fn register(state: &PlatformState, tenant: &str, entity_type: &str, set: &str, ioa: &str) {
    let xml = csdl(entity_type, set);
    let doc = parse_csdl(&xml).expect("test CSDL parses");
    state
        .registry
        .write()
        .expect("registry lock")
        .register_tenant(tenant, doc, xml, &[(entity_type, ioa)]);
}

async fn session_status(state: &PlatformState, session_id: &str) -> String {
    state
        .server
        .get_tenant_entity_state(&TenantId::new(TARGET_TENANT), "Session", session_id)
        .await
        .expect("read target session")
        .state
        .status
}

/// Approve a system-tenant GovernanceDecision whose callback names a session.
async fn approve_governance_decision(state: &PlatformState, decision_id: &str, session_id: &str) {
    let system = TenantId::new(SYSTEM_TENANT);
    let service = AgentContext::for_service("governance-service");
    let steps = [
        (
            "CreateGovernanceDecision",
            serde_json::json!({
                "tenant": TARGET_TENANT,
                "agent_id": "bot-1",
                "action_name": "ResumeAfterApproval",
                "resource_type": "Session",
                "resource_id": session_id,
                "denial_reason": "",
                "scope": "narrow",
                "pending_decision_id": decision_id,
            }),
        ),
        (
            "RegisterCallback",
            serde_json::json!({
                "callback_tenant": TARGET_TENANT,
                "callback_entity_set": "Sessions",
                "callback_entity_id": session_id,
                "callback_on_approve": "ResumeAfterApproval",
                "callback_on_deny": "Fail",
            }),
        ),
        (
            "Approve",
            serde_json::json!({
                "decided_by": "human-reviewer",
                "scope": "narrow",
                "generated_policy": "",
            }),
        ),
    ];
    for (action, params) in steps {
        state
            .server
            .dispatch_tenant_action(
                &system,
                "GovernanceDecision",
                decision_id,
                action,
                params,
                &service,
            )
            .await
            .unwrap_or_else(|e| panic!("GovernanceDecision {action} should succeed: {e}"));
    }
}

#[tokio::test]
async fn a_tenant_spec_naming_a_platform_hook_does_not_act_on_another_tenant() {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    register(&state, APP_TENANT, "Relay", "Relays", RELAY_IOA);
    register(&state, TARGET_TENANT, "Session", "Sessions", SESSION_IOA);

    let untouched = "ss-tenant-relay";
    let resumed = "ss-governance";
    assert_eq!(
        session_status(&state, untouched).await,
        "WaitingForApproval"
    );

    // The app tenant's own action names the callback hook and the other
    // tenant's session in its params.
    let relay = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(APP_TENANT),
            "Relay",
            "relay-1",
            "Approve",
            serde_json::json!({
                "callback_tenant": TARGET_TENANT,
                "callback_entity_set": "Sessions",
                "callback_entity_id": untouched,
                "callback_on_approve": "ResumeAfterApproval",
            }),
            &AgentContext::for_service("app-agent"),
        )
        .await
        .expect("the app tenant's own action runs");
    assert_eq!(relay.state.status, "Approved");

    // The platform's own decision record still delivers its callback.
    approve_governance_decision(&state, "gd-system-callback", resumed).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while session_status(&state, resumed).await != "Executing" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the system decision's callback resumes its session");

    // Both callbacks would have been spawned by now; only the platform's ran.
    assert_eq!(
        session_status(&state, untouched).await,
        "WaitingForApproval",
        "the app tenant's action leaves the other tenant's session as it was"
    );
}
