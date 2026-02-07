//! Temper Reference App: Agentic E-Commerce
//!
//! A fully agent-operated e-commerce backend serving OData v4 APIs.
//! Entity actors process real state machine transitions verified by DST.
//!
//! Run with: cargo run -p ecommerce

use std::collections::HashMap;
use temper_runtime::ActorSystem;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../specs/model.csdl.xml");
const ORDER_TLA: &str = include_str!("../specs/order.tla");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info,temper=debug")
        .init();

    tracing::info!("Temper Ecommerce — starting");

    let csdl = parse_csdl(CSDL_XML).expect("Failed to parse CSDL");
    tracing::info!(schemas = csdl.schemas.len(), "CSDL loaded");

    let system = ActorSystem::new("ecommerce");

    // Load TLA+ specs for state machine-backed entities
    let mut tla_sources = HashMap::new();
    tla_sources.insert("Order".to_string(), ORDER_TLA.to_string());

    // Build state with transition tables from TLA+ (same tables verified by DST)
    let state = ServerState::with_tla(system, csdl, CSDL_XML.to_string(), tla_sources);
    let app = build_router(state);

    let addr = "0.0.0.0:3000";
    tracing::info!(addr, "OData v4 API listening — entity actors with real state machines");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
