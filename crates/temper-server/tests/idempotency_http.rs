//! An `Idempotency-Key` on a bound action, as a caller retrying a write sees
//! it: every request goes through the `/tdata` router to a server over a Turso
//! file.
//!
//! A retry under the key is answered with the first answer and runs nothing.
//! The key refuses any other request on the record. The server keeps each
//! answer for an hour and within a byte budget; after that a retry is answered
//! with the record as it is, and still runs nothing, because the record
//! remembers its last 1,000 keys, across a restart too. A key the record has
//! forgotten is a new request again.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{LogicalClock, RealIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::idempotency::{IDEMPOTENCY_TTL_SECS, IdempotencyCache};
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Retry" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Ticket">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="text" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Tickets" EntityType="Temper.Retry.Ticket"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Note` counts every note it takes, so a retry that ran again counts twice.
/// Its `text` lands in the record's fields, so an answer shows which request
/// it was taken from.
const TICKET_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Draft", "Open"]
initial = "Draft"

[[state]]
name = "notes"
type = "counter"
initial = 0

[[action]]
name = "Note"
kind = "input"
from = ["Draft"]
to = "Draft"
params = ["text", "tags"]
effect = ["notes += 1"]

[[action]]
name = "Open"
kind = "input"
from = ["Draft"]
to = "Open"
"#;

/// A server over the Turso file at `db`, as a process starting on it would
/// be. `cache` replaces the server's reply cache, to give it another budget.
async fn server(db: &Path, cache: Option<IdempotencyCache>) -> Router {
    let store = TursoEventStore::new(&format!("file:{}", db.display()), None)
        .await
        .expect("open the Turso file");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parses"),
        CSDL_XML.to_string(),
        &[("Ticket", TICKET_IOA)],
    );
    // HTTP writes wait for a spec's verification; mark it passed.
    registry.set_verification_status(
        &TenantId::default(),
        "Ticket",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-28T00:00:00Z".to_string(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new("idempotency-http"), registry);
    if let Some(cache) = cache {
        state.idempotency_cache = Arc::new(cache);
    }
    state.set_storage_stack(StorageStack::from_turso(store));
    state.rebuild_reaction_dispatcher();
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy loads");
    build_router(state)
}

/// The caller every request comes from.
fn signed_in(mut request: Request<Body>) -> Request<Body> {
    let security_context = temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "retrying-caller".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "idempotency-http".to_string(),
        cause: None,
        credential: None,
    };
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context,
        ));
    request
}

/// The answer's status and body.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(signed_in(request))
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .expect("the answer's body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn create(app: &Router, id: &str) {
    let request = Request::post("/tdata/Tickets")
        .header("Content-Type", "application/json")
        .body(Body::from(json!({ "id": id }).to_string()))
        .unwrap();
    let (status, body) = send(app, request).await;
    assert_eq!(status, StatusCode::CREATED, "create {id}: {body}");
}

/// Send `action` on ticket `id` with `body` under `key`.
async fn act(app: &Router, id: &str, action: &str, key: &str, body: &Value) -> (StatusCode, Value) {
    let request = Request::post(format!("/tdata/Tickets('{id}')/Temper.Retry.{action}"))
        .header("Content-Type", "application/json")
        .header("Idempotency-Key", key)
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, request).await
}

/// `Note` on ticket `id` under `key`, answered 200.
async fn note(app: &Router, id: &str, key: &str, body: &Value) -> Value {
    let (status, answer) = act(app, id, "Note", key, body).await;
    assert_eq!(status, StatusCode::OK, "Note {key} on {id}: {answer}");
    answer
}

async fn ticket(app: &Router, id: &str) -> Value {
    let request = Request::get(format!("/tdata/Tickets('{id}')"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, request).await;
    assert_eq!(status, StatusCode::OK, "read {id}: {body}");
    body
}

fn notes(record: &Value) -> u64 {
    record["counters"]["notes"].as_u64().unwrap_or(0)
}

fn text(record: &Value) -> &str {
    record["fields"]["text"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn a_retry_under_its_key_gets_the_first_answer_and_runs_once() {
    let dir = tempfile::tempdir().expect("a database of its own");
    let app = server(&dir.path().join("retry.db"), None).await;
    create(&app, "t1").await;

    let body = json!({ "text": "first", "tags": { "a": 1, "b": [1, "x"] } });
    let first = note(&app, "t1", "k1", &body).await;
    assert_eq!(notes(&first), 1, "{first}");

    // The same body with its keys in another order is the same request.
    let reordered = json!({ "tags": { "b": [1, "x"], "a": 1 }, "text": "first" });
    let again = note(&app, "t1", "k1", &reordered).await;
    assert_eq!(notes(&again), 1, "answered, not run again: {again}");

    // Another key is another request, and runs.
    let second = note(&app, "t1", "k2", &json!({ "text": "second" })).await;
    assert_eq!(notes(&second), 2, "{second}");

    // A retry after the record moved on gets the first answer, not the record
    // as it is now.
    let late = note(&app, "t1", "k1", &body).await;
    assert_eq!(notes(&late), 1, "the first answer: {late}");
    assert_eq!(text(&late), "first", "{late}");

    // A key belongs to its record: on another record it is a new request.
    create(&app, "t2").await;
    let other = note(&app, "t2", "k1", &body).await;
    assert_eq!(other["entity_id"], "t2", "{other}");
    assert_eq!(notes(&other), 1, "{other}");

    let record = ticket(&app, "t1").await;
    assert_eq!(notes(&record), 2, "each request ran once: {record}");
    assert_eq!(text(&record), "second", "{record}");
}

#[tokio::test]
async fn a_key_sent_with_another_body_or_action_is_refused() {
    let dir = tempfile::tempdir().expect("a database of its own");
    let app = server(&dir.path().join("refused.db"), None).await;
    create(&app, "t1").await;
    note(&app, "t1", "k1", &json!({ "text": "first" })).await;

    for (action, body) in [
        ("Note", json!({ "text": "another body" })),
        ("Open", json!({})),
    ] {
        let (status, answer) = act(&app, "t1", action, "k1", &body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action} under k1: {answer}");
        assert_eq!(answer["error"]["code"], "IdempotencyKeyReused", "{answer}");
        assert_eq!(
            answer["error"]["action"], "Note",
            "names the key's request: {answer}"
        );
        let message = answer["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("already used on this entity for a different request"),
            "{answer}"
        );
    }

    let record = ticket(&app, "t1").await;
    assert_eq!(record["status"], "Draft", "Open did not run: {record}");
    assert_eq!(notes(&record), 1, "the other Note did not run: {record}");
    assert_eq!(text(&record), "first", "{record}");
}

#[tokio::test]
async fn a_key_keeps_its_request_across_a_restart() {
    let dir = tempfile::tempdir().expect("a database of its own");
    let db = dir.path().join("restart.db");
    let body = json!({ "text": "first" });
    {
        let app = server(&db, None).await;
        create(&app, "t1").await;
        note(&app, "t1", "k1", &body).await;
    }

    // A new server on the same file: no answer is cached, the record's
    // journal still holds the key.
    let app = server(&db, None).await;
    let retry = note(&app, "t1", "k1", &body).await;
    assert_eq!(notes(&retry), 1, "not run again after the restart: {retry}");

    let (status, answer) = act(&app, "t1", "Open", "k1", &json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"]["code"], "IdempotencyKeyReused", "{answer}");
    assert_eq!(answer["error"]["action"], "Note", "{answer}");

    let next = note(&app, "t1", "k2", &json!({ "text": "second" })).await;
    assert_eq!(notes(&next), 2, "a new key runs: {next}");
    let record = ticket(&app, "t1").await;
    assert_eq!(record["status"], "Draft", "{record}");
    assert_eq!(notes(&record), 2, "{record}");
}

#[tokio::test]
async fn an_hour_later_a_retry_gets_the_record_as_it_is_and_still_runs_once() {
    // The server's clock, moved by hand. Every task of this test runs on its
    // thread, the server's too.
    let clock = Arc::new(LogicalClock::with_delta_ms(1_000));
    let _clock = install_sim_context(clock.clone(), Arc::new(RealIdGen));

    let dir = tempfile::tempdir().expect("a database of its own");
    let app = server(&dir.path().join("window.db"), None).await;
    create(&app, "t1").await;
    let body = json!({ "text": "first" });
    note(&app, "t1", "k1", &body).await;
    note(&app, "t1", "k2", &json!({ "text": "second" })).await;

    let within = note(&app, "t1", "k1", &body).await;
    assert_eq!(
        notes(&within),
        1,
        "within the hour, the first answer: {within}"
    );
    assert_eq!(text(&within), "first", "{within}");

    clock.advance_by(IDEMPOTENCY_TTL_SECS as u64 + 1);
    let after = note(&app, "t1", "k1", &body).await;
    assert_eq!(notes(&after), 2, "the record as it is now: {after}");
    assert_eq!(text(&after), "second", "{after}");

    let record = ticket(&app, "t1").await;
    assert_eq!(notes(&record), 2, "the retry did not run again: {record}");
}

/// A `Note` body whose text starts with `key` and is about `bytes` long. An
/// answer holding 16 KB of it costs the cache about 17 KB, so three fit in
/// 64 KiB and a fourth does not.
fn sized_text(key: &str, bytes: usize) -> Value {
    json!({ "text": format!("{key}:{}", "x".repeat(bytes)) })
}

/// An answer's note count and the start of its text, for failure messages.
fn head(record: &Value) -> String {
    let text: String = text(record).chars().take(8).collect();
    format!("notes {} text {text:?}", notes(record))
}

#[tokio::test]
async fn a_full_reply_cache_forgets_the_oldest_answers_and_the_retry_still_runs_once() {
    const BUDGET: usize = 64 * 1024;
    let dir = tempfile::tempdir().expect("a database of its own");
    let app = server(
        &dir.path().join("budget.db"),
        Some(IdempotencyCache::with_budget_bytes(BUDGET)),
    )
    .await;
    create(&app, "t1").await;
    for key in ["k1", "k2", "k3", "k4"] {
        note(&app, "t1", key, &sized_text(key, 16_000)).await;
    }

    // A recent answer is kept: the retry gets it.
    let kept = note(&app, "t1", "k3", &sized_text("k3", 16_000)).await;
    assert_eq!(notes(&kept), 3, "k3 gets its own answer: {}", head(&kept));
    assert!(text(&kept).starts_with("k3:"), "{}", head(&kept));

    // The oldest answer made room: the retry gets the record as it is.
    let dropped = note(&app, "t1", "k1", &sized_text("k1", 16_000)).await;
    assert_eq!(notes(&dropped), 4, "{}", head(&dropped));
    assert!(text(&dropped).starts_with("k4:"), "{}", head(&dropped));

    // An answer bigger than the whole budget is never kept. (Under 128 KB, so
    // the field stays in the record rather than moving to a blob.)
    create(&app, "t2").await;
    let big_body = sized_text("big", BUDGET + 8_000);
    note(&app, "t2", "big", &big_body).await;
    note(&app, "t2", "small", &json!({ "text": "small" })).await;
    let big = note(&app, "t2", "big", &big_body).await;
    assert_eq!(notes(&big), 2, "{}", head(&big));
    assert_eq!(text(&big), "small", "{}", head(&big));

    assert_eq!(notes(&ticket(&app, "t1").await), 4, "no retry ran again");
    assert_eq!(notes(&ticket(&app, "t2").await), 2, "no retry ran again");
}

#[tokio::test]
async fn a_key_the_record_has_forgotten_runs_as_a_new_request() {
    // A cache that keeps no answer, so the record's own key list decides.
    let dir = tempfile::tempdir().expect("a database of its own");
    let app = server(
        &dir.path().join("forgotten.db"),
        Some(IdempotencyCache::with_budget_bytes(0)),
    )
    .await;
    create(&app, "t1").await;
    let body = json!({ "text": "first" });
    note(&app, "t1", "k1", &body).await;
    let retry = note(&app, "t1", "k1", &body).await;
    assert_eq!(
        notes(&retry),
        1,
        "remembered while among the last 1,000: {retry}"
    );

    // 1,000 newer keys push k1 (and the create's key) out of the record's list.
    for index in 0..1_000 {
        let key = format!("d{index}");
        note(&app, "t1", &key, &json!({ "text": key })).await;
    }
    assert_eq!(notes(&ticket(&app, "t1").await), 1_001);

    let forgotten = note(&app, "t1", "k1", &body).await;
    assert_eq!(
        notes(&forgotten),
        1_002,
        "k1 ran again: {}",
        notes(&forgotten)
    );
    let newest = note(&app, "t1", "d999", &json!({ "text": "d999" })).await;
    assert_eq!(notes(&newest), 1_002, "d999 is still remembered");
    assert_eq!(notes(&ticket(&app, "t1").await), 1_002);
}
