//! Approving or denying a decision hands the GovernanceDecision move off to
//! work of its own.
//!
//! The approve and deny requests answer without waiting for the move, so the
//! move must be a trace of its own whose root span is exported and links to the
//! request that handed it off, not a late child of the request's trace.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use opentelemetry::trace::SpanId;
use opentelemetry_sdk::trace::SpanData;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use crate::registry::SpecRegistry;
use crate::state::{PendingDecision, ServerState};
use crate::storage::StorageStack;
use crate::test_spans;

const SYSTEM_TENANT: &str = "temper-system";

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Governance" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="GovernanceDecision">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="GovernanceDecisions" EntityType="Temper.Governance.GovernanceDecision"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// The two moves the approve and deny endpoints make on a decision's record.
const GOVERNANCE_DECISION_IOA: &str = r#"
[automaton]
name = "GovernanceDecision"
states = ["Pending", "Approved", "Denied"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Approved", "Denied"]

[[action]]
name = "Approve"
kind = "input"
from = ["Pending"]
to = "Approved"
params = ["decided_by", "scope", "generated_policy"]

[[action]]
name = "Deny"
kind = "input"
from = ["Pending"]
to = "Denied"
params = ["decided_by", "denial_reason"]
"#;

const ADMIN_MANAGE_POLICIES_POLICY: &str = r#"
permit(
  principal is Admin,
  action == Action::"manage_policies",
  resource is PolicySet
);
"#;

async fn build_state() -> ServerState {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let base = format!("/tmp/temper-decisions-trace-{}-{id}", std::process::id());
    let db_path = format!("{base}.db");
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_dir_all(format!("{base}-data"));
    let turso = TursoEventStore::new(&format!("file:{db_path}"), None)
        .await
        .expect("create local turso db");

    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        SYSTEM_TENANT,
        parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.to_string(),
        &[("GovernanceDecision", GOVERNANCE_DECISION_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("decisions-trace"), registry);
    state.data_dir = std::path::PathBuf::from(format!("{base}-data"));
    state.set_storage_stack(StorageStack::from_turso(turso));
    state
        .authz
        .reload_tenant_policies("default", ADMIN_MANAGE_POLICIES_POLICY)
        .expect("admin policy loads");
    state
        .authz
        .reload_tenant_policies(SYSTEM_TENANT, "permit(principal, action, resource);")
        .expect("system policy loads");
    state
}

/// A pending decision in `default` whose resolution moves the
/// GovernanceDecision record `gd_id`.
async fn pending_decision(state: &ServerState, gd_id: &str) -> String {
    let mut pending = PendingDecision::from_denial(
        "default",
        "agent-1",
        "manage_wasm",
        "WasmModule",
        gd_id,
        serde_json::json!({ "id": gd_id }),
        "test denial",
        Some(gd_id.to_string()),
    );
    pending.governance_decision_id = Some(gd_id.to_string());
    state
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision");
    pending.id
}

fn admin_post(uri: String, body: &'static str) -> Request<Body> {
    let mut request = Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request builds");
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext {
                principal: temper_authz::Principal {
                    id: "admin-1".to_string(),
                    kind: temper_authz::PrincipalKind::Admin,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    label: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "decisions-trace-test".to_string(),
                cause: None,
                credential: None,
            },
        ));
    request
}

async fn status(state: &ServerState, gd_id: &str) -> Option<String> {
    state
        .get_tenant_entity_state(&TenantId::new(SYSTEM_TENANT), "GovernanceDecision", gd_id)
        .await
        .ok()
        .map(|entity| entity.state.status)
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

#[tokio::test]
async fn decision_resolution_is_its_own_trace_linked_to_the_approving_request() {
    let spans = test_spans::capture();
    let state = build_state().await;
    let approved = pending_decision(&state, "gd-approved").await;
    let denied = pending_decision(&state, "gd-denied").await;
    let app = Router::new()
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());

    // Deny first: the admin policy is loaded straight into the engine, and
    // approving reloads the tenant from its stored policies plus the approved
    // one, which drops it.
    let response = app
        .clone()
        .oneshot(admin_post(
            format!("/api/tenants/default/decisions/{denied}/deny"),
            "{}",
        ))
        .await
        .expect("deny responds");
    assert_eq!(response.status(), StatusCode::OK, "deny succeeds");
    let response = app
        .oneshot(admin_post(
            format!("/api/tenants/default/decisions/{approved}/approve"),
            r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"},"decided_by":"admin-1"}"#,
        ))
        .await
        .expect("approve responds");
    assert_eq!(response.status(), StatusCode::OK, "approve succeeds");

    for _ in 0..100 {
        if status(&state, "gd-approved").await.as_deref() == Some("Approved")
            && status(&state, "gd-denied").await.as_deref() == Some("Denied")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        status(&state, "gd-approved").await.as_deref(),
        Some("Approved")
    );
    assert_eq!(status(&state, "gd-denied").await.as_deref(), Some("Denied"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let finished = spans.finished();

    // The approve and deny requests: each is a root of its own trace.
    let requests: BTreeSet<String> = finished
        .iter()
        .filter(|s| s.name.starts_with("POST /api/tenants/"))
        .map(|s| s.span_context.trace_id().to_string())
        .collect();
    assert_eq!(requests.len(), 2, "two request traces");

    // 1. Every trace's root is exported.
    let exported: BTreeSet<String> = finished
        .iter()
        .map(|s| s.span_context.span_id().to_string())
        .collect();
    let orphans: Vec<String> = finished
        .iter()
        .filter(|s| {
            s.parent_span_id != SpanId::INVALID && !exported.contains(&s.parent_span_id.to_string())
        })
        .map(|s| s.name.to_string())
        .collect();
    assert!(
        orphans.is_empty(),
        "spans whose parent was never exported: {orphans:?}"
    );

    // 2. Each resolution is a root of its own trace linked to its request.
    let resolutions: Vec<&SpanData> = finished
        .iter()
        .filter(|s| s.name == "governance.decision_resolution")
        .collect();
    assert_eq!(resolutions.len(), 2, "one resolution per request");
    let mut linked = BTreeSet::new();
    for root in &resolutions {
        assert_eq!(root.parent_span_id, SpanId::INVALID, "a trace of its own");
        assert_eq!(
            attribute(root, "runtime_work").as_deref(),
            Some("decision_resolution"),
            "marked as runtime work of its kind"
        );
        assert!(
            !requests.contains(&root.span_context.trace_id().to_string()),
            "not part of the request's trace"
        );
        assert_eq!(root.links.len(), 1, "links to the request");
        let link = root.links[0].span_context.trace_id().to_string();
        assert!(requests.contains(&link), "links to a request trace");
        linked.insert(link);
    }
    assert_eq!(linked.len(), 2, "each resolution links to its own request");

    // 3. The Approve and Deny moves run in the resolution traces, never in a
    // request's trace.
    let resolution_traces: BTreeMap<String, String> = resolutions
        .iter()
        .map(|s| {
            (
                s.span_context.trace_id().to_string(),
                attribute(s, "action").unwrap_or_default(),
            )
        })
        .collect();
    let mut moves = Vec::new();
    for span in finished
        .iter()
        .filter(|s| s.name == "dispatch.dispatch_tenant_action_core")
    {
        let action = attribute(span, "action_name").unwrap_or_default();
        let trace = span.span_context.trace_id().to_string();
        assert!(
            !requests.contains(&trace),
            "{action} ran in a request's trace"
        );
        assert_eq!(
            resolution_traces.get(&trace),
            Some(&action),
            "{action} runs in its own resolution's trace"
        );
        moves.push(action);
    }
    moves.sort();
    assert_eq!(moves, vec!["Approve".to_string(), "Deny".to_string()]);
}
