//! On-call agent triage benchmarks — through the OData HTTP API.
//!
//! Every action goes through the full stack an agent hits:
//!   HTTP request → axum routing → OData path parsing → Cedar authz
//!   → actor dispatch → TransitionTable evaluation → event persistence
//!   → JSON response serialization
//!
//! Two groups:
//! - **inmemory**: full HTTP stack, no Postgres
//! - **postgres**: full HTTP stack + Postgres persistence (requires `DATABASE_URL`)
//!
//! Run with:
//!   cargo bench -p oncall-reference --bench agent_triage
//!   DATABASE_URL=postgres://... cargo bench -p oncall-reference --bench agent_triage

use std::collections::BTreeMap;

use criterion::{Criterion, criterion_group, criterion_main};
use hyper::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../specs/model.csdl.xml");
const PAGE_IOA: &str = include_str!("../specs/page.ioa.toml");
const ESCALATION_POLICY_IOA: &str = include_str!("../specs/escalation_policy.ioa.toml");
const REMEDIATION_IOA: &str = include_str!("../specs/remediation.ioa.toml");
const POSTMORTEM_IOA: &str = include_str!("../specs/postmortem.ioa.toml");

const NS: &str = "Temper.OnCall";

/// Unique run prefix to avoid Postgres entity ID collisions across runs.
fn run_prefix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("b{ts}")
}

fn oncall_sources() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("Page".to_string(), PAGE_IOA.to_string());
    m.insert(
        "EscalationPolicy".to_string(),
        ESCALATION_POLICY_IOA.to_string(),
    );
    m.insert("Remediation".to_string(), REMEDIATION_IOA.to_string());
    m.insert("Postmortem".to_string(), POSTMORTEM_IOA.to_string());
    m
}

fn build_inmemory_state(name: &str) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let system = ActorSystem::new(name);
    ServerState::with_specs(system, csdl, CSDL_XML.to_string(), oncall_sources())
        .expect("oncall specs should parse")
}

fn build_pg_state(rt: &tokio::runtime::Runtime, name: &str) -> Option<ServerState> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = rt.block_on(async {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(10)
            .connect(&url)
            .await
            .ok()
    })?;
    rt.block_on(async {
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .ok()
    })?;
    let store = PostgresEventStore::new(pool);
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let system = ActorSystem::new(name);
    Some(
        ServerState::with_persistence(system, csdl, CSDL_XML.to_string(), oncall_sources(), store)
            .expect("oncall specs should parse"),
    )
}

/// POST a bound action through the OData API and assert success.
async fn post_action(
    app: &axum::Router,
    entity_set: &str,
    entity_id: &str,
    action: &str,
    body: serde_json::Value,
) {
    let uri = format!("/tdata/{entity_set}('{entity_id}')/{NS}.{action}");
    let req = Request::post(&uri)
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    if status != StatusCode::OK && status != StatusCode::CREATED {
        let body = axum::body::to_bytes(resp.into_body(), 10_000)
            .await
            .unwrap_or_default();
        let body_str = String::from_utf8_lossy(&body);
        panic!("POST {uri} returned {status}: {body_str}");
    }
}

/// Full on-call agent triage through the OData HTTP API — ~10 POST requests
/// across 4 entity types simulating an incident response flow.
async fn agent_triage_http(app: &axum::Router, id: &str) {
    let page_id = format!("page-{id}");
    let rem_id = format!("rem-{id}");
    let pm_id = format!("pm-{id}");

    // --- Page: Triggered → Investigating → Remediated → Resolved ---
    post_action(
        app,
        "Pages",
        &page_id,
        "AssignAgent",
        serde_json::json!({"AgentId": "agent-tier-1"}),
    )
    .await;
    post_action(
        app,
        "Pages",
        &page_id,
        "StartInvestigation",
        serde_json::json!({}),
    )
    .await;

    // --- Remediation: Proposed → Approved → Executing → Succeeded ---
    post_action(
        app,
        "Remediations",
        &rem_id,
        "AutoApprove",
        serde_json::json!({}),
    )
    .await;
    post_action(
        app,
        "Remediations",
        &rem_id,
        "Execute",
        serde_json::json!({}),
    )
    .await;
    post_action(
        app,
        "Remediations",
        &rem_id,
        "Succeed",
        serde_json::json!({}),
    )
    .await;

    // --- Page: Remediated → Resolved ---
    post_action(
        app,
        "Pages",
        &page_id,
        "StartRemediation",
        serde_json::json!({}),
    )
    .await;
    post_action(app, "Pages", &page_id, "Resolve", serde_json::json!({})).await;

    // --- Postmortem: Draft → InReview → Approved → Published ---
    post_action(
        app,
        "Postmortems",
        &pm_id,
        "AddRootCause",
        serde_json::json!({"RootCauseDescription": "Memory leak in cache layer"}),
    )
    .await;
    post_action(
        app,
        "Postmortems",
        &pm_id,
        "SubmitForReview",
        serde_json::json!({}),
    )
    .await;
    post_action(
        app,
        "Postmortems",
        &pm_id,
        "ApprovePostmortem",
        serde_json::json!({}),
    )
    .await;
    post_action(app, "Postmortems", &pm_id, "Publish", serde_json::json!({})).await;
}

// ===========================================================================
// In-memory: full HTTP stack, no Postgres
// ===========================================================================

fn bench_inmemory(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("inmemory");

    // Single triage (~11 HTTP requests, 3 entity types)
    {
        let app = build_router(build_inmemory_state("bench-mem"));
        let mut id = 0u64;
        group.bench_function("agent_triage", |b| {
            b.iter(|| {
                id += 1;
                rt.block_on(async {
                    agent_triage_http(&app, &format!("t-{id}")).await;
                })
            })
        });
    }

    // 10 concurrent agent triages
    {
        let app = build_router(build_inmemory_state("bench-mem-10"));
        let mut batch = 0u64;
        group.bench_function("10_concurrent_triages", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(10);
                    for i in 0..10 {
                        let a = app.clone();
                        let tid = format!("t-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_triage_http(&a, &tid).await;
                        }));
                    }
                    for h in handles {
                        h.await.unwrap();
                    }
                })
            })
        });
    }

    // 100 concurrent agent triages
    {
        let app = build_router(build_inmemory_state("bench-mem-100"));
        let mut batch = 0u64;
        group.bench_function("100_concurrent_triages", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(100);
                    for i in 0..100 {
                        let a = app.clone();
                        let tid = format!("t-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_triage_http(&a, &tid).await;
                        }));
                    }
                    for h in handles {
                        h.await.unwrap();
                    }
                })
            })
        });
    }

    group.finish();
}

// ===========================================================================
// Postgres: full HTTP stack + persistence
// ===========================================================================

fn bench_postgres(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let Some(state) = build_pg_state(&rt, "bench-pg") else {
        eprintln!(">>> Skipping Postgres benchmarks: set DATABASE_URL to enable");
        return;
    };

    let app = build_router(state);
    let mut group = c.benchmark_group("postgres");
    let pfx = run_prefix();

    // Single triage with full persistence
    {
        let mut id = 0u64;
        group.bench_function("agent_triage", |b| {
            b.iter(|| {
                id += 1;
                let tid = format!("{pfx}-{id}");
                rt.block_on(async {
                    agent_triage_http(&app, &tid).await;
                })
            })
        });
    }

    // 10 concurrent agent triages with persistence
    {
        let mut batch = 0u64;
        group.bench_function("10_concurrent_triages", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(10);
                    for i in 0..10 {
                        let a = app.clone();
                        let tid = format!("{pfx}-c10-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_triage_http(&a, &tid).await;
                        }));
                    }
                    for h in handles {
                        h.await.unwrap();
                    }
                })
            })
        });
    }

    // 100 concurrent agent triages with persistence
    {
        let mut batch = 0u64;
        group.bench_function("100_concurrent_triages", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(100);
                    for i in 0..100 {
                        let a = app.clone();
                        let tid = format!("{pfx}-c100-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_triage_http(&a, &tid).await;
                        }));
                    }
                    for h in handles {
                        h.await.unwrap();
                    }
                })
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_inmemory, bench_postgres);
criterion_main!(benches);
