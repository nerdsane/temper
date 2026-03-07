//! E-commerce agent checkout benchmarks — through the OData HTTP API.
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
//!   cargo bench -p ecommerce-reference --bench agent_checkout
//!   DATABASE_URL=postgres://... cargo bench -p ecommerce-reference --bench agent_checkout

use std::collections::BTreeMap;

use criterion::{Criterion, criterion_group, criterion_main};
use hyper::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../specs/order.ioa.toml");
const PAYMENT_IOA: &str = include_str!("../specs/payment.ioa.toml");
const SHIPMENT_IOA: &str = include_str!("../specs/shipment.ioa.toml");

const NS: &str = "Temper.Example";

/// Unique run prefix to avoid Postgres entity ID collisions across runs.
fn run_prefix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("b{ts}")
}

fn ecommerce_sources() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("Order".to_string(), ORDER_IOA.to_string());
    m.insert("Payment".to_string(), PAYMENT_IOA.to_string());
    m.insert("Shipment".to_string(), SHIPMENT_IOA.to_string());
    m
}

fn build_inmemory_state(name: &str) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let system = ActorSystem::new(name);
    ServerState::with_specs(system, csdl, CSDL_XML.to_string(), ecommerce_sources())
        .expect("ecommerce specs should parse")
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
        ServerState::with_persistence(
            system,
            csdl,
            CSDL_XML.to_string(),
            ecommerce_sources(),
            store,
        )
        .expect("ecommerce specs should parse"),
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

/// Full e-commerce checkout through the OData HTTP API, exactly as an agent
/// would call it — 13 POST requests across 3 entity types.
async fn agent_checkout_http(app: &axum::Router, id: &str) {
    // --- Order: Draft → Submitted ---
    post_action(
        app,
        "Orders",
        id,
        "AddItem",
        serde_json::json!({"ProductId": "SKU-001", "Quantity": 2}),
    )
    .await;
    post_action(
        app,
        "Orders",
        id,
        "AddItem",
        serde_json::json!({"ProductId": "SKU-042", "Quantity": 1}),
    )
    .await;
    post_action(
        app,
        "Orders",
        id,
        "SubmitOrder",
        serde_json::json!({"ShippingAddressId": "addr-1", "PaymentMethod": "card"}),
    )
    .await;

    // --- Payment: Pending → Captured ---
    let pay_id = format!("pay-{id}");
    post_action(
        app,
        "Payments",
        &pay_id,
        "AuthorizePayment",
        serde_json::json!({}),
    )
    .await;
    post_action(
        app,
        "Payments",
        &pay_id,
        "CapturePayment",
        serde_json::json!({}),
    )
    .await;

    // --- Order: Confirmed → Shipped ---
    post_action(app, "Orders", id, "ConfirmOrder", serde_json::json!({})).await;
    post_action(app, "Orders", id, "ProcessOrder", serde_json::json!({})).await;
    post_action(
        app,
        "Orders",
        id,
        "ShipOrder",
        serde_json::json!({"Carrier": "FedEx", "TrackingNumber": "TRK-12345"}),
    )
    .await;

    // --- Shipment: Created → Delivered ---
    let ship_id = format!("ship-{id}");
    post_action(
        app,
        "Shipments",
        &ship_id,
        "ShipOrder",
        serde_json::json!({"Carrier": "FedEx", "TrackingNumber": "TRK-12345"}),
    )
    .await;
    post_action(
        app,
        "Shipments",
        &ship_id,
        "MarkInTransit",
        serde_json::json!({}),
    )
    .await;
    post_action(
        app,
        "Shipments",
        &ship_id,
        "MarkOutForDelivery",
        serde_json::json!({}),
    )
    .await;
    post_action(
        app,
        "Shipments",
        &ship_id,
        "DeliverShipment",
        serde_json::json!({}),
    )
    .await;

    // --- Order: Delivered ---
    post_action(app, "Orders", id, "DeliverOrder", serde_json::json!({})).await;
}

// ===========================================================================
// In-memory: full HTTP stack, no Postgres
// ===========================================================================

fn bench_inmemory(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("inmemory");

    // Single checkout (13 HTTP requests, 3 entities)
    {
        let app = build_router(build_inmemory_state("bench-mem"));
        let mut id = 0u64;
        group.bench_function("agent_checkout", |b| {
            b.iter(|| {
                id += 1;
                rt.block_on(async {
                    agent_checkout_http(&app, &format!("o-{id}")).await;
                })
            })
        });
    }

    // 10 concurrent agent checkouts
    {
        let app = build_router(build_inmemory_state("bench-mem-10"));
        let mut batch = 0u64;
        group.bench_function("10_concurrent_checkouts", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(10);
                    for i in 0..10 {
                        let a = app.clone();
                        let oid = format!("o-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_checkout_http(&a, &oid).await;
                        }));
                    }
                    for h in handles {
                        h.await.unwrap();
                    }
                })
            })
        });
    }

    // 100 concurrent agent checkouts
    {
        let app = build_router(build_inmemory_state("bench-mem-100"));
        let mut batch = 0u64;
        group.bench_function("100_concurrent_checkouts", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(100);
                    for i in 0..100 {
                        let a = app.clone();
                        let oid = format!("o-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_checkout_http(&a, &oid).await;
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

    // Single checkout with full persistence
    {
        let mut id = 0u64;
        group.bench_function("agent_checkout", |b| {
            b.iter(|| {
                id += 1;
                let oid = format!("{pfx}-{id}");
                rt.block_on(async {
                    agent_checkout_http(&app, &oid).await;
                })
            })
        });
    }

    // 10 concurrent agent checkouts with persistence
    {
        let mut batch = 0u64;
        group.bench_function("10_concurrent_checkouts", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(10);
                    for i in 0..10 {
                        let a = app.clone();
                        let oid = format!("{pfx}-c10-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_checkout_http(&a, &oid).await;
                        }));
                    }
                    for h in handles {
                        h.await.unwrap();
                    }
                })
            })
        });
    }

    // 100 concurrent agent checkouts with persistence
    {
        let mut batch = 0u64;
        group.bench_function("100_concurrent_checkouts", |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(100);
                    for i in 0..100 {
                        let a = app.clone();
                        let oid = format!("{pfx}-c100-{batch}-{i}");
                        handles.push(tokio::spawn(async move {
                            agent_checkout_http(&a, &oid).await;
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
