//! Work the runtime starts on its own is a trace of its own.
//!
//! A record moved by a request arms a state timer, schedules an action and
//! spawns a child; the timer's move triggers a reaction. Each of those pieces of
//! work must start a trace whose root span is exported, link back to the move
//! that caused it, and end with the work: later moves of the same record never
//! join an earlier piece of work's trace.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use opentelemetry::trace::SpanId;
use opentelemetry_sdk::trace::SpanData;
use tracing::Instrument;

use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::test_spans;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.RuntimeWork" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Note">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Jobs" EntityType="Temper.RuntimeWork.Job"/>
        <EntitySet Name="Notes" EntityType="Temper.RuntimeWork.Note"/>
        <EntitySet Name="Children" EntityType="Temper.RuntimeWork.Child"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Start` schedules a `Ping` and spawns a `Child`; a second in `Waiting`
/// fires `Expire`, whose reaction records the job's `Note`. `Expired` can start
/// again, so one record moves through many rounds.
const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Expired"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Expired"]

[[state]]
name = "fires"
type = "counter"
initial = 0

[[action]]
name = "Start"
kind = "input"
from = ["Idle", "Expired"]
to = "Waiting"
effect = ["schedule('Ping', 1)", "spawn('Child', 'Init')"]

[[action]]
name = "Ping"
kind = "internal"
from = ["Idle", "Waiting", "Expired"]

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting"]
to = "Expired"
effect = ["fires += 1"]

[[action.triggers]]
name = "expire_records_note"
kind = "entity"
target_entity = "Note"
target_action = "Record"
resolve_target = { kind = "same_id" }

[[state_timeout]]
state = "Waiting"
after_seconds = 1
on_timeout = "Expire"
"#;

const NOTE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Blank", "Recorded"]
initial = "Blank"
allow_indefinite_states = ["Blank", "Recorded"]

[[action]]
name = "Record"
kind = "internal"
from = ["Blank", "Recorded"]
to = "Recorded"
"#;

const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["New", "Ready"]
initial = "New"
allow_indefinite_states = ["New", "Ready"]

[[action]]
name = "Init"
kind = "internal"
from = ["New"]
to = "Ready"
"#;

/// The root span names of the runtime-started work this scenario makes, one
/// per round each, with the `runtime_work` kind each root carries.
const RUNTIME_WORK: &[(&str, &str)] = &[
    ("dispatch.state_timeout.fire", "state_timeout"),
    ("dispatch.scheduled_actions", "scheduled_action"),
    ("dispatch.background_spawn_entity", "background_spawn"),
];
const ROUNDS: usize = 2;

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.to_string(),
        &[("Job", JOB_IOA), ("Note", NOTE_IOA), ("Child", CHILD_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("runtime-work-trace"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy loads");
    state.rebuild_reaction_dispatcher();
    state
}

async fn fires(state: &ServerState, tenant: &TenantId) -> usize {
    state
        .get_tenant_entity_state(tenant, "Job", "j1")
        .await
        .expect("job state")
        .state
        .counters
        .get("fires")
        .copied()
        .unwrap_or(0)
}

fn named<'a>(finished: &'a [SpanData], name: &str) -> Vec<&'a SpanData> {
    finished.iter().filter(|s| s.name == name).collect()
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// The moves (the action of each core dispatch span) in each trace. Every
/// action name in these specs is unique, so the action names the move.
fn moves_by_trace(finished: &[SpanData]) -> BTreeMap<String, Vec<String>> {
    let mut moves: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for span in named(finished, "dispatch.dispatch_tenant_action_core") {
        let Some(action) = attribute(span, "action_name") else {
            continue;
        };
        moves
            .entry(span.span_context.trace_id().to_string())
            .or_default()
            .push(action);
    }
    moves
}

#[tokio::test]
async fn runtime_started_work_is_its_own_exported_trace_linked_to_its_cause() {
    let spans = test_spans::capture();
    let state = build_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("runtime-work-test");

    // Each round is one request that moves the job; the runtime does the rest.
    let mut requests = Vec::new();
    for round in 1..=ROUNDS {
        let request = tracing::info_span!(parent: None, "test.request");
        requests.push(
            crate::trace_context::span_context(&request)
                .trace_id()
                .to_string(),
        );
        let response = state
            .dispatch_tenant_action(&tenant, "Job", "j1", "Start", serde_json::json!({}), &agent)
            .instrument(request)
            .await
            .expect("Start dispatches");
        assert!(response.success, "Start: {:?}", response.error);
        for _ in 0..100 {
            if fires(&state, &tenant).await >= round {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(fires(&state, &tenant).await, round, "the timer fired");
    }

    // Wait until every piece of runtime work has finished and been exported.
    let mut finished = spans.finished();
    for _ in 0..100 {
        if RUNTIME_WORK
            .iter()
            .all(|(name, _)| named(&finished, name).len() >= ROUNDS)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        finished = spans.finished();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let finished = spans.finished();

    // 1. Every trace's root is exported: no span names a parent that never ended.
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
        "spans whose parent (a trace root) was never exported: {orphans:?}"
    );

    // 2. Each piece of runtime work is a root of its own trace, linked to the
    // request whose move started it: one per round, in that round's request.
    for (name, kind) in RUNTIME_WORK {
        let roots = named(&finished, name);
        assert_eq!(roots.len(), ROUNDS, "{name}: one per round");
        let mut linked_requests = BTreeSet::new();
        for root in &roots {
            assert_eq!(
                root.parent_span_id,
                SpanId::INVALID,
                "{name} starts its own trace"
            );
            assert_eq!(
                attribute(root, "runtime_work").as_deref(),
                Some(*kind),
                "{name} is marked as runtime work of its kind"
            );
            assert!(
                !requests.contains(&root.span_context.trace_id().to_string()),
                "{name} is not part of a request's trace"
            );
            let links: Vec<String> = root
                .links
                .iter()
                .map(|link| link.span_context.trace_id().to_string())
                .collect();
            assert_eq!(links.len(), 1, "{name} links to the move that caused it");
            assert!(
                requests.contains(&links[0]),
                "{name} links to the request that started it"
            );
            linked_requests.insert(links[0].clone());
        }
        assert_eq!(
            linked_requests.len(),
            ROUNDS,
            "{name}: each round's work links to its own request"
        );
    }

    // 3. No trace grows across moves: each request trace holds only its own
    // Start, and each timer's trace holds only its Expire and the reaction it ran.
    let moves = moves_by_trace(&finished);
    for request in &requests {
        assert_eq!(
            moves.get(request).cloned().unwrap_or_default(),
            vec!["Start".to_string()],
            "a request's trace holds only its own move"
        );
    }
    for fire in named(&finished, "dispatch.state_timeout.fire") {
        let mut in_trace = moves
            .get(&fire.span_context.trace_id().to_string())
            .cloned()
            .unwrap_or_default();
        in_trace.sort();
        assert_eq!(
            in_trace,
            vec!["Expire".to_string(), "Record".to_string()],
            "a timer's trace holds its move and the reaction it ran, nothing later"
        );
    }
    let expire_traces: BTreeSet<&String> = moves
        .iter()
        .filter(|(_, m)| m.iter().any(|m| m == "Expire"))
        .map(|(trace, _)| trace)
        .collect();
    assert_eq!(
        expire_traces.len(),
        ROUNDS,
        "each timer move is in a trace of its own"
    );
}
