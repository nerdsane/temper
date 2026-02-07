//! Temper Reference App: Agentic E-Commerce
//!
//! A fully agent-operated e-commerce backend serving OData v4 APIs.
//! Entity actors process real state machine transitions verified by DST.
//!
//! Modes:
//! - Without DATABASE_URL: in-memory only (actors lose state on restart)
//! - With DATABASE_URL: events persisted to Postgres (actors rebuild from journal)
//!
//! Run with: cargo run -p ecommerce

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

    tracing::info!("Temper Ecommerce — starting");

    let csdl = parse_csdl(CSDL_XML).expect("Failed to parse CSDL");
    tracing::info!(schemas = csdl.schemas.len(), "CSDL loaded");

    let system = ActorSystem::new("ecommerce");

    let mut tla_sources = HashMap::new();
    tla_sources.insert("Order".to_string(), ORDER_TLA.to_string());

    // Connect to Postgres if DATABASE_URL is set
    let state = match std::env::var("DATABASE_URL") {
        Ok(database_url) => {
            tracing::info!("Connecting to PostgreSQL...");
            let pool = sqlx::PgPool::connect(&database_url)
                .await
                .expect("Failed to connect to PostgreSQL");

            // Run migrations
            temper_store_postgres::migration::run_migrations(&pool)
                .await
                .expect("Failed to run migrations");
            tracing::info!("PostgreSQL connected, migrations complete");

            let store = PostgresEventStore::new(pool);
            ServerState::with_persistence(system, csdl, CSDL_XML.to_string(), tla_sources, store)
        }
        Err(_) => {
            tracing::warn!("DATABASE_URL not set — running in-memory mode (no persistence)");
            ServerState::with_tla(system, csdl, CSDL_XML.to_string(), tla_sources)
        }
    };

    let app = build_router(state);
    let addr = "0.0.0.0:3000";
    tracing::info!(addr, "OData v4 API listening");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
