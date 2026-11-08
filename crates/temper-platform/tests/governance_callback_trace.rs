//! The governance callback is work of its own.
//!
//! Resolving a GovernanceDecision hands the callback that resumes the waiting
//! record to the runtime. That callback must be a trace of its own whose root
//! span is exported and links to the decision's move, not an unlinked trace and
//! not part of the move that handed it off.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use opentelemetry::trace::{SpanId, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use temper_platform::bootstrap::{SYSTEM_TENANT, bootstrap_system_tenant};
use temper_platform::state::PlatformState;
use temper_runtime::TenantId;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;
use tracing::Instrument;
use tracing_subscriber::prelude::*;

const APP_TENANT: &str = "default";

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Callback.Trace" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Session">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="CallbackTraceService">
        <EntitySet Name="Sessions" EntityType="Callback.Trace.Session"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

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

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

async fn session_status(state: &PlatformState, session_id: &str) -> Option<String> {
    state
        .server
        .get_tenant_entity_state(&TenantId::new(APP_TENANT), "Session", session_id)
        .await
        .ok()
        .map(|entity| entity.state.status)
}

#[tokio::test]
async fn governance_callback_is_its_own_trace_linked_to_the_decision_move() {
    // Export every span made on this thread (the whole current-thread runtime).
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("platform-test"))),
    );
    let finished = || {
        let _ = provider.force_flush();
        exporter.get_finished_spans().expect("finished spans")
    };

    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    state
        .registry
        .write()
        .expect("registry lock")
        .register_tenant(
            APP_TENANT,
            parse_csdl(CSDL).expect("CSDL parses"),
            CSDL.to_string(),
            &[("Session", SESSION_IOA)],
        );

    let system = TenantId::new(SYSTEM_TENANT);
    let service = AgentContext::for_service("governance-service");
    let decision_id = "gd-trace";
    let session_id = "ss-trace";
    for (action, params) in [
        (
            "CreateGovernanceDecision",
            serde_json::json!({
                "tenant": APP_TENANT,
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
                "callback_tenant": APP_TENANT,
                "callback_entity_set": "Sessions",
                "callback_entity_id": session_id,
                "callback_on_approve": "ResumeAfterApproval",
                "callback_on_deny": "Fail",
            }),
        ),
    ] {
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
            .expect("decision setup dispatches");
    }

    // The approval is one request; the callback it hands off is runtime work.
    let request = tracing::info_span!(parent: None, "test.request");
    let response = state
        .server
        .dispatch_tenant_action(
            &system,
            "GovernanceDecision",
            decision_id,
            "Approve",
            serde_json::json!({
                "decided_by": "human-reviewer",
                "scope": "narrow",
                "generated_policy": "",
            }),
            &service,
        )
        .instrument(request)
        .await
        .expect("approval dispatches");
    assert!(response.success, "Approve: {:?}", response.error);

    for _ in 0..100 {
        if session_status(&state, session_id).await.as_deref() == Some("Executing") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        session_status(&state, session_id).await.as_deref(),
        Some("Executing"),
        "the callback resumed the session"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let finished = finished();

    let request_trace = finished
        .iter()
        .find(|s| s.name == "test.request")
        .map(|s| s.span_context.trace_id().to_string())
        .expect("the request span is exported");

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

    // 2. The callback is a root of its own trace, linked to the decision's move
    // in the request's trace.
    let callbacks: Vec<&SpanData> = finished
        .iter()
        .filter(|s| s.name == "governance.dispatch_callback")
        .collect();
    assert_eq!(callbacks.len(), 1, "one callback for one approval");
    let callback = callbacks[0];
    assert_eq!(
        callback.parent_span_id,
        SpanId::INVALID,
        "a trace of its own"
    );
    assert_eq!(
        attribute(callback, "runtime_work").as_deref(),
        Some("governance_callback"),
        "marked as runtime work of its kind"
    );
    let callback_trace = callback.span_context.trace_id().to_string();
    assert_ne!(
        callback_trace, request_trace,
        "not part of the request's trace"
    );
    assert_eq!(
        callback.links.len(),
        1,
        "links to the move that handed it off"
    );
    let link = &callback.links[0].span_context;
    assert_eq!(
        link.trace_id().to_string(),
        request_trace,
        "links into the approving request"
    );
    assert!(
        exported.contains(&link.span_id().to_string()),
        "links to an exported span of the decision's move"
    );

    // 3. The session's move runs in the callback's trace; the request's trace
    // holds only the decision's move.
    let mut moves: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for span in finished
        .iter()
        .filter(|s| s.name == "dispatch.dispatch_tenant_action_core")
    {
        moves
            .entry(span.span_context.trace_id().to_string())
            .or_default()
            .push(attribute(span, "action_name").unwrap_or_default());
    }
    assert_eq!(
        moves.get(&callback_trace).cloned().unwrap_or_default(),
        vec!["ResumeAfterApproval".to_string()],
        "the callback's trace holds the session's move"
    );
    assert_eq!(
        moves.get(&request_trace).cloned().unwrap_or_default(),
        vec!["Approve".to_string()],
        "the request's trace holds only the decision's move"
    );
}
