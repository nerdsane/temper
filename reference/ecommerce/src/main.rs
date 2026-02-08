//! Temper Reference App: Agentic E-Commerce
//!
//! Modes:
//! - `cargo run -p ecommerce` — start the OData server
//! - `cargo run -p ecommerce -- agent` — interactive conversational agent
//! - `cargo run -p ecommerce -- agent "prompt"` — single-shot agent
//! - `cargo run -p ecommerce -- analyze` — trajectory analysis + evolution records

mod agent;
mod prompt_registry;
mod sentinel;

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
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info,temper=debug".into()))
        .init();

    // Initialise OTEL tracing if OTLP_ENDPOINT is set
    let _otel_guard = std::env::var("OTLP_ENDPOINT").ok().and_then(|endpoint| {
        match temper_observe::otel::init_tracing(&endpoint, "temper-ecommerce") {
            Ok(guard) => Some(guard),
            Err(e) => {
                tracing::warn!(error = %e, "failed to initialise OTEL tracing");
                None
            }
        }
    });

    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 2 && args[1] == "agent" {
        if args.len() >= 3 {
            run_agent_single(&args[2..].join(" ")).await;
        } else {
            run_agent_interactive().await;
        }
    } else if args.len() >= 2 && args[1] == "analyze" {
        run_analysis().await;
    } else {
        run_server().await;
    }
}

async fn run_agent_single(prompt: &str) {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    let mut agent = agent::CustomerAgent::new("http://localhost:3000", &api_key);
    let response = agent.handle(prompt).await;
    println!("\nAgent: {response}");
}

async fn run_agent_interactive() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY required");
    let mut agent = agent::CustomerAgent::new("http://localhost:3000", &api_key);

    println!("Temper E-Commerce Agent (type 'quit' to exit)");
    println!("Server must be running on localhost:3000\n");

    loop {
        print!("You: ");
        use std::io::Write;
        std::io::stdout().flush().unwrap();

        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() || input.trim().is_empty() { continue; }
        let input = input.trim();
        if input == "quit" || input == "exit" { println!("Goodbye!"); break; }

        let response = agent.handle(input).await;
        println!("Agent: {response}\n");
    }
}

async fn run_analysis() {
    use temper_observe::ObservabilityStore as _;
    use temper_evolution::{
        RecordHeader, RecordType, ObservationRecord, ObservationClass,
        InsightRecord, InsightSignal,
        RecordStore, compute_priority_score, classify_insight, generate_digest,
    };

    let ch_url = std::env::var("CLICKHOUSE_URL").unwrap_or_else(|_| "http://localhost:8123".into());
    let store = temper_observe::ClickHouseStore::new(&ch_url);
    let records = RecordStore::new();

    println!("============================================");
    println!("  TEMPER TRAJECTORY ANALYSIS");
    println!("============================================\n");

    // 1. Total spans
    let total_count = store.query_spans("SELECT count(*) as cnt FROM spans WHERE service = 'temper-agent'", &[])
        .await.ok().and_then(|rs| rs.rows.first()?.get("cnt")?.as_u64()).unwrap_or(0);
    println!("Total trajectory spans: {total_count}\n");

    // 2. Operations breakdown
    println!("Operations:");
    if let Ok(rs) = store.query_spans(
        "SELECT operation, count(*) as cnt FROM spans WHERE service = 'temper-agent' GROUP BY operation ORDER BY cnt DESC", &[],
    ).await {
        for row in &rs.rows {
            let op = row.get("operation").and_then(|v| v.as_str()).unwrap_or("?");
            let cnt = row.get("cnt").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("  {op}: {cnt}");
        }
    }
    println!();

    // 3. User intents
    println!("User intents:");
    if let Ok(rs) = store.query_spans(
        "SELECT JSONExtractString(attributes, 'user_intent') as intent, count(*) as cnt FROM spans WHERE service = 'temper-agent' AND operation = 'trajectory.complete' GROUP BY intent ORDER BY cnt DESC", &[],
    ).await {
        for row in &rs.rows {
            let intent = row.get("intent").and_then(|v| v.as_str()).unwrap_or("?");
            let cnt = row.get("cnt").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("  \"{intent}\" — {cnt}x");
        }
    }
    println!();

    // 4. Generate evolution records
    if total_count > 0 {
        let obs = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "trajectory-analyzer"),
            source: "trajectory-analyzer".into(),
            classification: ObservationClass::Trajectory,
            evidence_query: "SELECT operation, count(*) FROM spans WHERE service = 'temper-agent' GROUP BY operation".into(),
            threshold_field: None, threshold_value: None,
            observed_value: Some(total_count as f64),
            context: serde_json::json!({"total_spans": total_count}),
        };
        println!("O-Record: {}", obs.header.id);
        records.insert_observation(obs);

        // Detect unmet "split order" intent
        let split_signal = InsightSignal {
            intent: "split order into shipments".into(),
            volume: 1, success_rate: 0.0, trend: "new".into(), growth_rate: None,
        };
        let insight = InsightRecord {
            header: RecordHeader::new(RecordType::Insight, "trajectory-analyzer"),
            category: classify_insight(&split_signal),
            signal: split_signal.clone(),
            recommendation: "Add SplitOrder action to Order entity".into(),
            priority_score: compute_priority_score(&split_signal),
        };
        println!("I-Record: {} (category: {:?}, priority: {:.2})\n", insight.header.id, insight.category, insight.priority_score);
        records.insert_insight(insight);

        // 5. Product intelligence digest
        let insights = records.ranked_insights();
        if !insights.is_empty() {
            println!("{}", generate_digest(&insights));
        }
    } else {
        println!("No trajectory data yet. Run the agent first to generate data.");
    }
}

async fn run_server() {
    tracing::info!("Temper Ecommerce — starting server");
    let csdl = parse_csdl(CSDL_XML).expect("Failed to parse CSDL");
    let system = ActorSystem::new("ecommerce");

    let mut tla_sources = HashMap::new();
    tla_sources.insert("Order".to_string(), ORDER_TLA.to_string());

    let state = match std::env::var("DATABASE_URL") {
        Ok(url) => {
            let pool = sqlx::PgPool::connect(&url).await.expect("Failed to connect to PostgreSQL");
            temper_store_postgres::migration::run_migrations(&pool).await.expect("Migration failed");
            tracing::info!("PostgreSQL connected");
            ServerState::with_persistence(system, csdl, CSDL_XML.to_string(), tla_sources, PostgresEventStore::new(pool))
        }
        Err(_) => {
            tracing::warn!("DATABASE_URL not set — in-memory mode");
            ServerState::with_tla(system, csdl, CSDL_XML.to_string(), tla_sources)
        }
    };

    let app = build_router(state);
    let addr = "0.0.0.0:3000";
    tracing::info!(addr, "OData v4 API listening");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    // Spawn sentinel if ClickHouse is configured
    if let Ok(ch_url) = std::env::var("CLICKHOUSE_URL") {
        let ch = ch_url.clone();
        tokio::spawn(async move { sentinel::run_sentinel(&ch, 30).await });
    }

    axum::serve(listener, app).await.unwrap();
}
