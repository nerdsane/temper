//! The latest committed event owns record metadata across timers and restarts.
#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::{ActorSystem, persistence::EventStore, tenant::TenantId};
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const SPEC: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Expired", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Expired", "Done"]
[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"
[[action]]
name = "Poke"
from = ["Waiting"]
[[action]]
name = "Expire"
from = ["Waiting"]
to = "Expired"
[[action]]
name = "Finish"
from = ["Waiting", "Expired"]
to = "Done"
[[action]]
name = "React"
from = ["Idle"]
to = "Waiting"
[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Job"
resolve_target = { kind = "same_id" }
target_action = "Finish"
[[state_timeout]]
state = "Waiting"
after_seconds = 180
on_timeout = "Expire"
"#;
const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Poke" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="React" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

fn server(store: &SimEventStore) -> ServerState {
    server_with_spec(store, SPEC)
}
fn server_with_spec(store: &SimEventStore, spec: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", spec)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Job",
        temper_server::registry::VerificationStatus::Completed(
            temper_server::registry::EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-01-01T00:00:00Z".into(),
            },
        ),
    );
    let mut state = ServerState::from_registry(ActorSystem::new("last-event"), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    state
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}
async fn request(state: &ServerState, method: &str, uri: &str, body: Value) -> Value {
    let ann = SecurityContext::from_resolved_identity("ann", "person", None);
    request_as(state, ann, method, uri, body).await
}
async fn request_as(
    state: &ServerState,
    caller: SecurityContext,
    method: &str,
    uri: &str,
    body: Value,
) -> Value {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            caller,
        ));
    let app = if uri.starts_with("/observe") {
        axum::Router::new()
            .nest("/observe", temper_server::observe::build_observe_router())
            .with_state(state.clone())
    } else {
        build_router(state.clone())
    };
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert!(status.is_success(), "{method} {uri}: {status}: {value}");
    value
}
fn metadata(row: &Value) -> Value {
    json!([
        row["last_event_at"],
        row["last_event_via"],
        row["last_event_by"]
    ])
}

#[test]
fn dst_latest_event_survives_timers_reactions_and_snapshot_restore() {
    for seed in 0..16 {
        let clock = Arc::new(process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let store = SimEventStore::no_faults(seed);
        let mut rng = DeterministicRng::new(seed);
        let mut expected = Vec::new();
        let mut created = std::collections::BTreeMap::new();
        runtime().block_on(async {
            clock.begin();
            let state = server(&store);
            // A direct authenticated request, including forged kernel properties.
            created.insert("timer", sim_now());
            let created_row = request(&state, "POST", "/tdata/Jobs", json!({"Id":"timer", "first_event_at":"1900-01-01T00:00:00Z", "last_event_by":"forged"})).await;
            let first = created_row["first_event_at"].as_str().expect("creation sets first_event_at");
            assert_eq!(chrono::DateTime::parse_from_rfc3339(first).unwrap().with_timezone(&chrono::Utc), created["timer"]);
            request(&state, "POST", "/tdata/Jobs('timer')/Jobs.Start", json!({
                "first_event_at":"1900-01-01T00:00:00Z", "last_event_at":"1900-01-01T00:00:00Z", "last_event_via":"timer", "last_event_by":"mallory"
            })).await;
            let row = request(&state, "GET", "/tdata/Jobs('timer')", Value::Null).await;
            assert_eq!(metadata(&row), json!([sim_now(), "request", "ann"]), "seed {seed}: request");
            assert!(row["fields"].get("last_event_by").is_none(), "forgery is removed");
            assert!(row["fields"].get("first_event_at").is_none(), "creation time cannot be a parameter: {row}");
            // The timer must replace the previous sender and timestamp.
            tokio::time::advance(Duration::from_secs(180)).await;
            settle().await;
            let row = request(&state, "GET", "/tdata/Jobs('timer')", Value::Null).await;
            assert_eq!(row["status"], "Expired", "seed {seed}: timer fired");
            assert_eq!(metadata(&row), json!([sim_now(), "timer", "ann"]), "seed {seed}: timer");
            expected.push(("timer", metadata(&row)));
            let history = request(&state, "GET", "/observe/entities/Job/timer/history", Value::Null).await;
            let events = history["events"].as_array().unwrap();
            assert_eq!(events.last().unwrap()["via"], "timer");
            assert_eq!(events.last().unwrap()["by"], "ann");
            assert_eq!(events[1]["via"], "request");
            assert_eq!(events[1]["by"], "ann");
            created.insert("request", sim_now());
            request(&state, "POST", "/tdata/Jobs", json!({"Id":"request"})).await;
            request(&state, "POST", "/tdata/Jobs('request')/Jobs.Start", json!({})).await;
            for method in ["PATCH", "PUT"] {
                request(&state, method, "/tdata/Jobs('request')", json!({
                    "first_event_at":"1900-01-01T00:00:00Z", "last_event_at":"1900-01-01T00:00:00Z", "last_event_via":"timer", "last_event_by":"mallory",
                    "moved_by":"app-owned"
                })).await;
                let row = request(&state, "GET", "/tdata/Jobs('request')", Value::Null).await;
                assert_eq!(metadata(&row), json!([sim_now(), "request", "ann"]), "seed {seed}: {method}");
                assert_eq!(row["fields"]["moved_by"], "app-owned");
                assert!(row["fields"].get("last_event_by").is_none());
                assert!(row["fields"].get("first_event_at").is_none());
            }
            let row = request(&state, "GET", "/tdata/Jobs('request')", Value::Null).await;
            expected.push(("request", metadata(&row)));
            tokio::time::advance(Duration::from_secs(1 + rng.next_bound(60) as u64)).await;
            created.insert("reaction", sim_now());
            request(&state, "POST", "/tdata/Jobs", json!({"Id":"reaction"})).await;
            tokio::time::advance(Duration::from_millis(250)).await;
            request(&state, "POST", "/tdata/Jobs('reaction')/Jobs.React", json!({})).await;
            settle().await;
            let row = request(&state, "GET", "/tdata/Jobs('reaction')", Value::Null).await;
            assert_eq!(row["status"], "Done", "seed {seed}: reaction fired");
            assert_eq!(metadata(&row), json!([sim_now(), "reaction", "ann"]), "seed {seed}: reaction");
            expected.push(("reaction", metadata(&row)));
            let history = request(&state, "GET", "/observe/entities/Job/reaction/history", Value::Null).await;
            let event = history["events"].as_array().unwrap().last().unwrap();
            assert_eq!(event["via"], "reaction");
            assert_eq!(event["by"], "ann");
            let rows = request(&state, "GET", "/tdata/Jobs?$orderby=last_event_at%20desc", Value::Null).await;
            assert_eq!(rows["value"][0]["entity_id"], "reaction");
            let rows = request(&state, "GET", "/tdata/Jobs?$filter=last_event_via%20eq%20'timer'", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 1);
            assert_eq!(rows["value"][0]["entity_id"], "timer");
            let rows = request(&state, "GET", "/tdata/Jobs?$filter=last_event_at%20gt%20'2026-01-01T00:03:00Z'", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 1, "seed {seed}");
            assert_eq!(rows["value"][0]["entity_id"], "reaction");
            let rows = request(&state, "GET", "/tdata/Jobs?$filter=last_event_by%20eq%20'ann'", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 3, "seed {seed}: ann moved all three, by request, timer and reaction");
            let rows = request(&state, "GET", "/tdata/Jobs?$filter=last_event_at%20eq%20'2026-01-01T01:03:00%2B01:00'", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 2, "equivalent offset is the same instant");
            let rows = request(&state, "GET", "/tdata/Jobs?$select=entity_id,last_event_at,last_event_via,last_event_by", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 3);
            for row in rows["value"].as_array().unwrap() {
                let expected = expected.iter().find(|(id, _)| row["entity_id"] == *id).unwrap();
                assert_eq!(metadata(row), expected.1);
            }
            // Move the oldest record last: creation order must stay stable.
            tokio::time::advance(Duration::from_millis(137)).await;
            request(&state, "POST", "/tdata/Jobs('timer')/Jobs.Finish", json!({})).await;
            let row = request(&state, "GET", "/tdata/Jobs('timer')", Value::Null).await;
            expected[0].1 = metadata(&row);
            let rows = request(&state, "GET", "/tdata/Jobs?$orderby=first_event_at%20asc&$select=entity_id,first_event_at", Value::Null).await;
            let rows = rows["value"].as_array().unwrap();
            assert_eq!(rows.iter().map(|r| r["entity_id"].as_str().unwrap()).collect::<Vec<_>>(), ["timer", "request", "reaction"]);
            for row in rows {
                let id = row["entity_id"].as_str().unwrap();
                let raw = row["first_event_at"].as_str().expect("creation timestamp");
                assert!(raw.contains('.') && raw.ends_with('Z'), "fractional UTC: {raw}");
                assert_eq!(chrono::DateTime::parse_from_rfc3339(raw).unwrap().with_timezone(&chrono::Utc), created[id]);
            }
            let rows = request(&state, "GET", "/tdata/Jobs?$filter=first_event_at%20eq%20'2026-01-01T01:00:00.000%2B01:00'", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 1);
            assert_eq!(rows["value"][0]["entity_id"], "timer");
            // Snapshots omit history: all three values, including ann's identity,
            // must live independently. The reaction record replays its journal.
            for id in ["timer", "request"] {
                let entity = state.get_tenant_entity_state(&TenantId::default(), "Job", id).await.unwrap().state;
                let mut snapshot = serde_json::to_value(&entity).unwrap();
                snapshot["events"] = json!([]);
                if id == "request" {
                    // A pre-feature snapshot must recover creation from sequence 1,
                    // independently of its saved tail and the catalog back-fill.
                    snapshot.as_object_mut().unwrap().remove("first_event_at");
                }
                store.save_snapshot(&format!("default:Job:{id}"), entity.sequence_nr, &serde_json::to_vec(&snapshot).unwrap()).await.unwrap();
            }
            clock.end(Duration::from_secs(10));
        });
        runtime().block_on(async {
            clock.begin();
            let state = server(&store);
            for (id, expected) in expected {
                // Hydrate on a fresh process, then read via OData.
                let restored = state
                    .get_tenant_entity_state(&TenantId::default(), "Job", id)
                    .await
                    .unwrap()
                    .state;
                assert_eq!(
                    metadata(&serde_json::to_value(&restored).unwrap()),
                    expected,
                    "seed {seed}: recovered state for {id}, independent of the catalog"
                );
                let first = serde_json::to_value(&restored).unwrap()["first_event_at"]
                    .as_str()
                    .expect("restored creation timestamp")
                    .to_owned();
                assert_eq!(
                    chrono::DateTime::parse_from_rfc3339(&first)
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                    created[id]
                );
                let row =
                    request(&state, "GET", &format!("/tdata/Jobs('{id}')"), Value::Null).await;
                assert_eq!(metadata(&row), expected, "seed {seed}: restored {id}");
            }
            request(&state, "DELETE", "/tdata/Jobs('request')", Value::Null).await;
            let history = request(
                &state,
                "GET",
                "/observe/entities/Job/request/history",
                Value::Null,
            )
            .await;
            let event = history["events"].as_array().unwrap().last().unwrap();
            assert_eq!(event["via"], "request");
            assert_eq!(event["by"], "ann");
            clock.end(Duration::ZERO);
        });
    }
}

#[test]
fn dst_history_names_whom_an_agent_acted_for() {
    // The contexts the bearer layer builds from verified tokens: an agent's
    // token (`client_id`, `agent_type`, `sub`) and a person's (`sub` only).
    let agent = || {
        SecurityContext::from_verified_jwt(
            "hub-client",
            PrincipalKind::Agent,
            Some("claude-code"),
            Some("ann"),
            Some("contributor"),
            None,
        )
    };
    let ann = || {
        SecurityContext::from_verified_jwt("ann", PrincipalKind::Customer, None, None, None, None)
    };
    for seed in 0..16 {
        let clock = Arc::new(process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        runtime().block_on(async {
            clock.begin();
            let store = SimEventStore::no_faults(seed);
            let state = server(&store);
            request_as(&state, agent(), "POST", "/tdata/Jobs", json!({"Id":"job"})).await;
            let start = "/tdata/Jobs('job')/Jobs.Start";
            // Forged origin parameters stay parameters.
            let forged = json!({"for":"mallory", "agent_type":"forged"});
            request_as(&state, agent(), "POST", start, forged).await;
            let poke = "/tdata/Jobs('job')/Jobs.Poke";
            request_as(&state, ann(), "POST", poke, json!({})).await;
            tokio::time::advance(Duration::from_secs(180)).await;
            settle().await;
            let history = request(
                &state,
                "GET",
                "/observe/entities/Job/job/history",
                Value::Null,
            )
            .await;
            let rows: Vec<Value> = history["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| {
                    json!([
                        event["action"],
                        event["via"],
                        event["by"],
                        event["for"],
                        event["agent_type"]
                    ])
                })
                .collect();
            assert_eq!(
                rows,
                vec![
                    json!(["Created", "request", "hub-client", "ann", "claude-code"]),
                    json!(["Start", "request", "hub-client", "ann", "claude-code"]),
                    json!(["Poke", "request", "ann", null, null]),
                    json!(["Expire", "timer", "hub-client", "ann", "claude-code"]),
                ],
                "seed {seed}: {history}"
            );
            clock.end(Duration::ZERO);
        });
    }
}

#[test]
fn dst_spawned_creation_and_initial_action_are_reactions() {
    let spec = SPEC.replace(
        "to = \"Expired\"",
        "to = \"Expired\"\neffect = [\"spawn('Job', 'Child', child_id)\"]",
    ) + r#"
[[state]]
name = "child_id"
type = "string"
initial = ""
[[action]]
name = "Spawn"
from = ["Idle"]
to = "Done"
effect = ["spawn('Job', 'Child', child_id)"]
[[action]]
name = "Child"
from = ["Idle"]
to = "Done"
"#;
    for seed in 0..16 {
        let clock = Arc::new(process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        runtime().block_on(async {
            clock.begin();
            let store = SimEventStore::no_faults(seed);
            let state = server_with_spec(&store, &spec);
            for (id, action) in [("request-parent", "Spawn"), ("timer-parent", "Start")] {
                request(&state, "POST", "/tdata/Jobs", json!({"Id":id})).await;
                request(
                    &state,
                    "POST",
                    &format!("/tdata/Jobs('{id}')/Jobs.{action}"),
                    json!({}),
                )
                .await;
                if action == "Start" {
                    tokio::time::advance(Duration::from_secs(180)).await;
                }
                settle().await;
                let parent =
                    request(&state, "GET", &format!("/tdata/Jobs('{id}')"), Value::Null).await;
                let child = parent["fields"]["child_id"]
                    .as_str()
                    .expect("spawned child");
                let row = request(
                    &state,
                    "GET",
                    &format!("/tdata/Jobs('{child}')"),
                    Value::Null,
                )
                .await;
                assert_eq!(row["status"], "Done", "seed {seed}: {action}");
                assert_eq!(row["last_event_via"], "reaction");
                assert_eq!(row["last_event_by"], "ann");
                assert!(row["first_event_at"].is_string());
                let history = request(
                    &state,
                    "GET",
                    &format!("/observe/entities/Job/{child}/history"),
                    Value::Null,
                )
                .await;
                let events = history["events"].as_array().unwrap();
                assert_eq!(events.len(), 2);
                for event in events {
                    assert_eq!(event["via"], "reaction", "seed {seed}: {action}");
                    assert_eq!(event["by"], "ann");
                }
            }
            clock.end(Duration::ZERO);
        });
    }
}
