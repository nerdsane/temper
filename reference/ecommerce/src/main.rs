//! Temper Reference App: Agentic E-Commerce
//!
//! A fully agent-operated e-commerce backend serving OData v4 APIs.
//! The "frontend" is LLM agents that interact with this API on behalf of customers.
//!
//! Run with: cargo run -p ecommerce

use temper_runtime::ActorSystem;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../specs/model.csdl.xml");

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("info,temper=debug")
        .init();

    tracing::info!("Temper Ecommerce — starting");

    // Parse CSDL specification
    let csdl = parse_csdl(CSDL_XML).expect("Failed to parse CSDL specification");
    tracing::info!(
        schemas = csdl.schemas.len(),
        "CSDL loaded"
    );

    // Boot actor system
    let system = ActorSystem::new("ecommerce");

    // Build server state and router
    let state = ServerState::new(system, csdl, CSDL_XML.to_string());
    let app = build_router(state);

    // Bind and serve
    let addr = "0.0.0.0:3000";
    tracing::info!(addr, "OData v4 API listening");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
