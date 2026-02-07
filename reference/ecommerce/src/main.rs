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

    if args.len() >= 2 && args[1] == "agent" {
        if args.len() >= 3 {
            // Single-shot mode: run one prompt and exit
            let prompt = args[2..].join(" ");
            run_agent_single(&prompt).await;
        } else {
            // Interactive mode: conversational REPL
            run_agent_interactive().await;
        }
    } else if args.len() >= 2 && args[1] == "analyze" {
        // Trajectory analysis mode
        run_analysis().await;
    } else {
        run_server().await;
    }
}

async fn run_agent_single(prompt: &str) {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set for agent mode");

    let clickhouse_url = std::env::var("CLICKHOUSE_URL").ok();
    let mut agent = agent::CustomerAgent::new("http://localhost:3000", &api_key);

    if let Some(ref ch_url) = clickhouse_url {
        agent.set_clickhouse(ch_url);
    }

    let response = agent.handle(prompt).await;
    println!("\nAgent: {response}");
}

async fn run_agent_interactive() {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set for agent mode");

    let clickhouse_url = std::env::var("CLICKHOUSE_URL").ok();
    let mut agent = agent::CustomerAgent::new("http://localhost:3000", &api_key);

    if let Some(ref ch_url) = clickhouse_url {
        agent.set_clickhouse(ch_url);
    }

    println!("Temper E-Commerce Agent (type 'quit' to exit)");
    println!("Server must be running on localhost:3000\n");

    let stdin = std::io::stdin();
    loop {
        print!("You: ");
        use std::io::Write;
        std::io::stdout().flush().unwrap();

        let mut input = String::new();
        if stdin.read_line(&mut input).is_err() || input.trim().is_empty() {
            continue;
        }

        let input = input.trim();
        if input == "quit" || input == "exit" {
            println!("Goodbye!");
            break;
        }

        let response = agent.handle(input).await;
        println!("Agent: {response}\n");
    }
}

async fn run_analysis() {
    let clickhouse_url = std::env::var("CLICKHOUSE_URL")
        .unwrap_or_else(|_| "http://localhost:8123".into());

    println!("Analyzing trajectories from ClickHouse ({clickhouse_url})...\n");

    use temper_observe::ObservabilityStore as _;
    let store = temper_observe::ClickHouseStore::new(&clickhouse_url);

    // Query trajectory spans
    let result = store.query_spans(
        "SELECT trace_id, operation, status, duration_ns FROM spans WHERE service = 'temper-agent' ORDER BY start_time DESC LIMIT 50",
        &[],
    ).await;

    match result {
        Ok(rs) => {
            println!("Recent agent spans: {} rows", rs.len());
            for row in &rs.rows {
                let trace = row.get("trace_id").and_then(|v| v.as_str()).unwrap_or("?");
                let op = row.get("operation").and_then(|v| v.as_str()).unwrap_or("?");
                let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  {trace} | {op} | {status}");
            }
        }
        Err(e) => println!("Query failed: {e}"),
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
