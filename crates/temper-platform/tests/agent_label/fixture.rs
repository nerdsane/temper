//! Shared specs, token minting and platform setup.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_system_tenant};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const ISSUER: &str = "https://issuer.agent-label.local";
const AUD: &str = "temper-agent-label";
/// The agent's authenticated id.
pub(super) const AGENT: &str = "kc_hub";
/// The person the agent acts for.
pub(super) const PERSON: &str = "ann";
/// The type signed into the agent token.
pub(super) const KIND: &str = "claude-code";
/// The instance name signed into the agent token.
pub(super) const LABEL: &str = "hub-session";

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.AgentLabel" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="child" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="parent_type" Type="Edm.String"/>
        <Property Name="parent_id" Type="Edm.String"/>
        <Property Name="job_id" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Post">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="author" Type="Edm.String"/>
      </EntityType>
      <Action Name="Publish" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Post"/>
      </Action>
      <Action Name="Send" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Post"/>
      </Action>
      <Action Name="Edit" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Post"/>
      </Action>
      <Action Name="Touch" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Job"/>
      </Action>
      <Action Name="Go" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Job"/>
      </Action>
      <Action Name="Arm" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.AgentLabel.Job"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Jobs" EntityType="Temper.AgentLabel.Job"/>
        <EntitySet Name="Children" EntityType="Temper.AgentLabel.Child"/>
        <EntitySet Name="Posts" EntityType="Temper.AgentLabel.Post"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Go` sets off two reactions, the last spawning a Child. `Arm` enters a state
/// whose timer fires `Expire`.
const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Hopping", "Hopped", "Done", "Waiting", "Expired"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Hopping", "Hopped", "Done", "Expired"]
strict_action_params = true

[[state]]
name = "child"
type = "string"
initial = ""

[[action]]
name = "Touch"
kind = "input"
from = ["Idle"]
params = []

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Hopping"
params = []

[[action.triggers]]
name = "hop"
kind = "entity"
target_entity = "Job"
target_action = "Hop"
resolve_target = { kind = "same_id" }

[[action]]
name = "Hop"
kind = "input"
from = ["Hopping"]
to = "Hopped"
params = []

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Job"
target_action = "Finish"
resolve_target = { kind = "same_id" }

[[action]]
name = "Finish"
kind = "input"
from = ["Hopped"]
to = "Done"
params = []
effect = ["spawn('Child', 'Start', child)"]

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Waiting"
params = []

[[state_timeout]]
state = "Waiting"
after_seconds = AFTER
on_timeout = "Expire"

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting"]
to = "Expired"
params = []
"#;

const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["Idle", "Started"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Started"]
strict_action_params = true

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Started"
params = ["parent_type", "parent_id", "job_id"]
"#;

/// `Publish` keeps who made the post. `Send` gets there through a reaction,
/// and `Edit` is a later move that leaves it alone.
const WITHIN: Duration = Duration::from_secs(20);

fn signing_key() -> SigningKey {
    SigningKey::from_slice(&[13u8; 32]).expect("a P-256 key")
}

fn b64(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
}

fn mint(claims: Value) -> String {
    let header = json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" });
    let signing_input = format!("{}.{}", b64(&header), b64(&claims));
    let signature: Signature = signing_key().sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

fn jwks_json() -> String {
    let point = signing_key().verifying_key().to_encoded_point(false);
    json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
        "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
    }]})
    .to_string()
}

/// The agent acting for ann, with `label` as its `agent_label` claim.
pub(super) fn agent_labelled(label: &str) -> String {
    mint(json!({
        "iss": ISSUER, "aud": AUD, "sub": PERSON, "client_id": AGENT, "agent_type": KIND,
        "role": "contributor", "agent_label": label, "nbf": 0, "exp": 4_102_444_800i64,
    }))
}

/// A token for the named agent instance.
pub(super) fn agent() -> String {
    agent_labelled(LABEL)
}

/// Ann in person. A person's token that carries a label is still ann in person.
pub(super) fn person() -> String {
    mint(json!({
        "iss": ISSUER, "aud": AUD, "sub": PERSON, "agent_label": "not-an-agent",
        "nbf": 0, "exp": 4_102_444_800i64,
    }))
}

/// The tenant registered by this fixture.
pub(super) fn tenant() -> TenantId {
    TenantId::new("default")
}

/// Timer rows committed with the journal's events, as on Turso.
struct CommittedRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for CommittedRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(row);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

/// Permit the test calls independently of their labels.
pub(super) const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// A started server over `store`, under `policy`, with Job's timer at `after`
/// seconds.
pub(super) async fn server_under(store: &SimEventStore, after: u32, policy: &str) -> PlatformState {
    server_with(store, after, policy, &[]).await
}

/// [`server_under`], with `more` specs besides Job and Child.
pub(super) async fn server_with(
    store: &SimEventStore,
    after: u32,
    policy: &str,
    more: &[(&str, &str)],
) -> PlatformState {
    let job = JOB_IOA.replace("AFTER", &after.to_string());
    let mut specs = vec![("Job", job.as_str()), ("Child", CHILD_IOA)];
    specs.extend_from_slice(more);
    let mut state = PlatformState::new(None);
    state.server.set_storage_stack(
        StorageStack::from_sim(store.clone(), None).with_entity_rows_in_journal(),
    );
    state.server.state_timer_rows = Some(Arc::new(CommittedRows(store.clone())));
    let cache = BTreeMap::new();
    bootstrap_system_tenant(&state, &cache);
    bootstrap_agent_specs(&state, "default", false, &cache);
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .try_register_tenant_with_constraints(
                tenant(),
                parse_csdl(CSDL).expect("app CSDL parses"),
                CSDL.to_string(),
                &specs,
                None,
                true,
            )
            .expect("the app's specs merge into the tenant");
        for (entity_type, _) in &specs {
            registry.set_verification_status(
                &tenant(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "fixture".to_string(),
                        passed: true,
                        summary: "pre-verified".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-11-02T00:00:00Z".to_string(),
                }),
            );
        }
    }
    state
        .server
        .dispatch_tenant_action(
            &tenant(),
            "TrustedIssuer",
            ISSUER,
            "RegisterIssuer",
            json!({
                "issuer": ISSUER, "jwks_json": jwks_json(), "audience": AUD,
                "algorithms": "ES256", "description": "agent label", "created_by": "test",
            }),
            &AgentContext::system(),
        )
        .await
        .expect("register the issuer");
    state
        .server
        .authz
        .reload_tenant_policies("default", policy)
        .expect("the app's Cedar parses");
    state.server.rebuild_reaction_dispatcher();
    state.server.start_runtime().await;
    state
}

/// Start the test server with a policy permitting every call.
pub(super) async fn server(store: &SimEventStore, after: u32) -> PlatformState {
    server_under(store, after, PERMIT_ALL).await
}

/// Send a request through the platform router and decode its response.
pub(super) async fn call(state: &PlatformState, request: Request<Body>) -> (StatusCode, Value) {
    let response = build_platform_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// Read an endpoint as the bearer token's caller.
pub(super) async fn get(state: &PlatformState, uri: &str, token: &str) -> Value {
    let request = Request::get(uri)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(state, request).await;
    assert_eq!(status, StatusCode::OK, "GET {uri}: {body}");
    body
}

/// Make `action` on a Job, as `token`, creating the record first.
/// Create a Job if needed and move it as the bearer token's caller.
pub(super) async fn act(state: &PlatformState, id: &str, action: &str, token: &str) {
    state
        .server
        .get_or_create_tenant_entity(&tenant(), "Job", id, json!({}))
        .await
        .expect("the record is created");
    let request = Request::post(format!("/tdata/Jobs('{id}')/Temper.AgentLabel.{action}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = call(state, request).await;
    assert!(status.is_success(), "{action} on {id}: {status}: {body}");
}

/// Read a record as the named agent instance.
pub(super) async fn read(state: &PlatformState, set: &str, id: &str) -> Value {
    get(state, &format!("/tdata/{set}('{id}')"), &agent()).await
}

/// The record's history, oldest first.
pub(super) async fn history(state: &PlatformState, entity_type: &str, id: &str) -> Vec<Value> {
    let body = get(
        state,
        &format!("/observe/entities/{entity_type}/{id}/history"),
        &agent(),
    )
    .await;
    body["events"]
        .as_array()
        .expect("history has events")
        .clone()
}

/// Wait until the record reaches `status`, or fail saying what it read.
pub(super) async fn until_status(
    state: &PlatformState,
    set: &str,
    id: &str,
    status: &str,
) -> Value {
    let deadline = Instant::now() + WITHIN; // determinism-ok: real-time test
    loop {
        let record = read(state, set, id).await;
        if record["status"] == status {
            return record;
        }
        assert!(
            Instant::now() < deadline, // determinism-ok: real-time test
            "{set} {id} never reached {status}: {record}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

/// A record read names the agent: who, for whom, what kind and which label.
#[track_caller]
pub(super) fn record_names_the_agent(record: &Value) {
    assert_eq!(record["last_event_by"], AGENT, "{record}");
    assert_eq!(record["last_event_for"], PERSON, "{record}");
    assert_eq!(record["last_event_agent_type"], KIND, "{record}");
    assert_eq!(record["last_event_label"], LABEL, "{record}");
    assert!(record.get("last_event_role").is_none(), "{record}");
}

/// A history row says what a record read says.
#[track_caller]
pub(super) fn event_names_the_agent(event: &Value) {
    assert_eq!(event["by"], AGENT, "{event}");
    assert_eq!(event["for"], PERSON, "{event}");
    assert_eq!(event["agent_type"], KIND, "{event}");
    assert_eq!(event["label"], LABEL, "{event}");
    assert!(event.get("role").is_none(), "{event}");
}

/// A read returns the agent fields as null, never leaving a reader to guess.
#[track_caller]
pub(super) fn names_no_agent(value: &Value, fields: [&str; 3]) {
    for field in fields {
        assert_eq!(value.get(field), Some(&Value::Null), "{field}: {value}");
    }
}
