//! Authenticated action inputs are resolved from trusted identity before validation.
//! Exercises public OData, direct dispatch, fail-closed rejection and durable replay.
#[path = "authenticated_params/postgres.rs"]
mod postgres;

use axum::{
    Extension,
    body::{Bytes, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState,
    registry::{EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus},
};

const IOA: &str = r#"
[automaton]
name = "OwnedInstance"
states = ["Requested", "Running"]
initial = "Requested"
allow_indefinite_states = ["Requested", "Running"]
strict_action_params = true
[[state]]
name = "owner_id"
type = "string"
initial = ""
[[action]]
name = "Create"
from = ["Requested"]
to = "Running"
params = [{ name = "owner_id", type = "string", source = "authenticated_subject" }]
[[action.constraints]]
kind = "param_nonempty"
param = "owner_id"
[[action]]
name = "Touch"
from = ["Running"]
params = []
"#;
const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Probe" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="OwnedInstance"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="owner_id" Type="Edm.String"/>
</EntityType>
<Action Name="Create" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.OwnedInstance"/></Action>
<Action Name="Touch" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.OwnedInstance"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Instances" EntityType="Probe.OwnedInstance"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

fn fixture(extra_policy: &str) -> ServerState {
    // Run verification rather than disabling or fabricating the OData gate.
    let verified = temper_verify::cascade::VerificationCascade::from_ioa(IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(verified.all_passed, "probe IOA verification failed");
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            "cp",
            temper_spec::csdl::parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("OwnedInstance", IOA)],
        )
        .unwrap();
    registry.set_verification_status(
        &TenantId::new("cp"),
        "OwnedInstance",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: verified.all_passed,
            levels: verified
                .levels
                .into_iter()
                .map(|r| EntityLevelSummary {
                    level: r.level.to_string(),
                    passed: r.passed,
                    summary: r.summary,
                    details: None,
                })
                .collect(),
            verified_at: "2026-09-06T00:00:00Z".into(),
        }),
    );
    let state = ServerState::from_registry(ActorSystem::new("owner-binding-probe"), registry);
    // Any authenticated user must be able to request a new instance. Later
    // actions are granted only to the stored owner. No service bypass is used.
    let policy = format!(
        r#"
permit(principal is Agent, action in [Action::"create", Action::"Create"], resource is OwnedInstance)
when {{ context has actingFor && context.actingFor == "alice" }};
permit(principal is Agent, action == Action::"Touch", resource is OwnedInstance)
when {{ resource.owner_id == context.actingFor }};
{extra_policy}
"#
    );
    state.authz.reload_tenant_policies("cp", &policy).unwrap();
    state
}

async fn post(state: &ServerState, path: &str, params: Value) -> (StatusCode, Value) {
    let identity = SecurityContext::from_verified_jwt(
        "local-control-plane",
        PrincipalKind::Agent,
        Some("control-plane-gateway"),
        Some("alice"),
        None,
        None,
    );
    let response = temper_server::odata::handle_odata_post(
        State(state.clone()),
        Some(Extension(AuthenticatedRequestContext::new(
            TenantId::new("cp"),
            identity,
        ))),
        None,
        HeaderMap::new(),
        Path(path.into()),
        Query(Default::default()),
        Bytes::from(serde_json::to_vec(&params).unwrap()),
    )
    .await
    .into_response();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn owner_is_bound_to_authenticated_subject() {
    let state = fixture("");
    let (status, body) = post(&state, "Instances", json!({"Id":"probe"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = post(&state, "Instances('probe')/Probe.Create", json!({})).await;
    assert!(status.is_success(), "{status}: {body}");
    let stored = state
        .get_tenant_entity_state(&TenantId::new("cp"), "OwnedInstance", "probe")
        .await
        .unwrap();
    assert_eq!(stored.state.fields["owner_id"], "alice");
}

#[tokio::test]
async fn caller_cannot_supply_bound_parameter() {
    let state = fixture("");
    let (status, body) = post(&state, "Instances", json!({"Id":"probe"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for owner in ["bob", "alice"] {
        let (status, body) = post(
            &state,
            "Instances('probe')/Probe.Create",
            json!({"owner_id": owner}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "AuthenticatedParameter");
    }
    let unchanged = state
        .get_tenant_entity_state(&TenantId::new("cp"), "OwnedInstance", "probe")
        .await
        .unwrap();
    assert_eq!(unchanged.state.status, "Requested");
    assert_eq!(unchanged.state.fields["owner_id"], "");
}

#[tokio::test]
async fn direct_dispatch_requires_authenticated_context_and_ignores_attribution_fields() {
    use temper_server::request_context::AgentContext;
    let state = fixture("");
    let tenant = TenantId::new("cp");
    for ctx in [
        AgentContext::default(),
        AgentContext::system(),
        AgentContext::for_service("worker"),
    ] {
        assert!(
            state
                .dispatch_tenant_action(
                    &tenant,
                    "OwnedInstance",
                    "absent",
                    "Create",
                    json!({}),
                    &ctx
                )
                .await
                .is_err()
        );
        assert!(!state.entity_exists(&tenant, "OwnedInstance", "absent"));
    }
    let ctx = AgentContext {
        agent_id: Some("forged-attribution".into()),
        security_ctx: Some(SecurityContext::from_verified_jwt(
            "real-principal",
            PrincipalKind::Agent,
            Some("test"),
            Some("alice"),
            None,
            None,
        )),
        ..Default::default()
    };
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "OwnedInstance",
            "native",
            "Create",
            json!({}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(response.success, "{:?}", response.error);
    assert_eq!(response.state.fields["owner_id"], "alice");
    let override_result = state
        .dispatch_tenant_action(
            &tenant,
            "OwnedInstance",
            "native",
            "Touch",
            json!({"owner_id":"bob"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!override_result.success);
    let stored = state
        .get_tenant_entity_state(&tenant, "OwnedInstance", "native")
        .await
        .unwrap();
    assert_eq!(stored.state.fields["owner_id"], "alice");
}

#[tokio::test]
async fn authenticated_owner_survives_libsql_replay() {
    use temper_server::StorageStack;
    use temper_store_turso::TursoEventStore;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file:{}", dir.path().join("events.db").display());
    let mut state = fixture("");
    state.set_storage_stack(StorageStack::from_turso(
        TursoEventStore::new(&url, None).await.unwrap(),
    ));
    let (status, body) = post(&state, "Instances", json!({"Id":"durable"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = post(&state, "Instances('durable')/Probe.Create", json!({})).await;
    assert!(status.is_success(), "{body}");
    let mut restored = fixture("");
    restored.set_storage_stack(StorageStack::from_turso(
        TursoEventStore::new(&url, None).await.unwrap(),
    ));
    let tenant = TenantId::new("cp");
    assert!(
        restored
            .ensure_entity_loaded(&tenant, "OwnedInstance", "durable")
            .await
    );
    let response = restored
        .get_tenant_entity_state(&tenant, "OwnedInstance", "durable")
        .await
        .unwrap();
    assert_eq!(response.state.fields["owner_id"], "alice");
    assert_eq!(response.state.status, "Running");
}

#[test]
fn invalid_bound_parameter_declarations_are_rejected() {
    for source in [
        IOA.replace("authenticated_subject", "untrusted_header"),
        IOA.replace(
            "strict_action_params = true",
            "strict_action_params = false",
        ),
        IOA.replace("type = \"string\", source", "type = \"bool\", source"),
    ] {
        assert!(temper_spec::automaton::parse_automaton(&source).is_err());
    }
}

#[tokio::test]
async fn subject_fallback_and_tenant_contracts_are_isolated() {
    use temper_server::request_context::AgentContext;
    let state = fixture("");
    // The same entity type in another tenant has an ordinary caller parameter.
    let ordinary = IOA.replace(r#", source = "authenticated_subject""#, "");
    state
        .registry
        .write()
        .unwrap()
        .try_register_tenant(
            "other",
            temper_spec::csdl::parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("OwnedInstance", &ordinary)],
        )
        .unwrap();
    let ctx = AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            "alice",
            PrincipalKind::Agent,
            Some("test"),
            None,
            None,
            None,
        )),
        ..Default::default()
    };
    for index in 0..32 {
        let id = format!("subject-{index}");
        let owned = state
            .dispatch_tenant_action(
                &TenantId::new("cp"),
                "OwnedInstance",
                &id,
                "Create",
                json!({}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(owned.success, "{:?}", owned.error);
        assert_eq!(owned.state.fields["owner_id"], "alice");
        let ordinary = state
            .dispatch_tenant_action(
                &TenantId::new("other"),
                "OwnedInstance",
                &id,
                "Create",
                json!({"owner_id":"declared-input"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(ordinary.success, "{:?}", ordinary.error);
        assert_eq!(ordinary.state.fields["owner_id"], "declared-input");
    }
}

#[test]
fn binding_survives_transition_table_serialization() {
    let table = temper_jit::table::TransitionTable::from_ioa_source(IOA);
    let restored: temper_jit::table::TransitionTable =
        serde_json::from_value(serde_json::to_value(table).unwrap()).unwrap();
    assert_eq!(
        restored.action_contracts["Create"].param_sources["owner_id"],
        temper_spec::automaton::ParameterSource::AuthenticatedSubject,
    );
}

#[tokio::test]
async fn bound_create_on_an_absent_instance_is_not_collection_creation() {
    let state = fixture("");
    let (status, body) = post(&state, "Instances('absent')/Probe.Create", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "EntityNotFound");
    assert_eq!(state.active_actor_count(), 0);
    assert!(!state.entity_exists(&TenantId::new("cp"), "OwnedInstance", "absent"));
}
