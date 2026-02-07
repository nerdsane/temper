//! Temper Reference App: Agentic E-Commerce
//!
//! Modes:
//! - `cargo run -p ecommerce` — start the OData server
//! - `cargo run -p ecommerce -- agent "Create an order with a widget"` — run the LLM agent
//!
//! Environment:
//! - DATABASE_URL: Postgres connection (optional, falls back to in-memory)
//! - ANTHROPIC_API_KEY: Required for agent mode

mod agent;

use std::collections::HashMap;

use temper_runtime::ActorSystem;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;

const CSDL_XML: &str = include_str!("../specs/model.csdl.xml");
const ORDER_TLA: &str = include_str!("../specs/order.tla");

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,temper=debug".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 3 && args[1] == "agent" {
        // Agent mode: run the LLM agent with the given prompt
        let prompt = args[2..].join(" ");
        run_agent(&prompt).await;
    } else {
        // Server mode: start the OData HTTP server
        run_server().await;
    }
}

async fn run_agent(prompt: &str) {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set for agent mode");

    tracing::info!(prompt, "Starting agent");

    let mut agent = agent::CustomerAgent::new("http://localhost:3000", &api_key);
    let response = agent.handle(prompt).await;

    println!("\nAgent: {response}");

    if let Some(order_id) = &agent.last_order_id {
        println!("(Last order ID: {order_id})");
    }
}

async fn run_server() {
    tracing::info!("Temper Ecommerce — starting server");

    let csdl = parse_csdl(CSDL_XML).expect("Failed to parse CSDL");
    tracing::info!(schemas = csdl.schemas.len(), "CSDL loaded");

    let system = ActorSystem::new("ecommerce");

    let mut tla_sources = HashMap::new();
    tla_sources.insert("Order".to_string(), ORDER_TLA.to_string());

    let state = match std::env::var("DATABASE_URL") {
        Ok(database_url) => {
            tracing::info!("Connecting to PostgreSQL...");
            let pool = sqlx::PgPool::connect(&database_url)
                .await
                .expect("Failed to connect to PostgreSQL");

            temper_store_postgres::migration::run_migrations(&pool)
                .await
                .expect("Failed to run migrations");
            tracing::info!("PostgreSQL connected, migrations complete");

            let store = PostgresEventStore::new(pool);
            ServerState::with_persistence(system, csdl, CSDL_XML.to_string(), tla_sources, store)
        }
        Err(_) => {
            tracing::warn!("DATABASE_URL not set — running in-memory mode");
            ServerState::with_tla(system, csdl, CSDL_XML.to_string(), tla_sources)
        }
    };

    let app = build_router(state);
    let addr = "0.0.0.0:3000";
    tracing::info!(addr, "OData v4 API listening");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
