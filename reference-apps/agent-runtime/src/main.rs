/// Agent Runtime reference app.
///
/// Demonstrates the PG-backed actor runtime integrated with temper-server:
///
/// 1. PG ActorSystem runs Process, AgentDefinition, Message as spec-driven actors.
/// 2. All entity types are actor-backed (PG as source of truth).
/// 3. Scheduler polls PG mailbox and activates actors on incoming messages.
/// 4. OData API: POST /tdata/AgentDefinition → POST /tdata/Process → POST /tdata/Process(id)/StartProcess
///
/// Usage:
///   DATABASE_URL=postgres://localhost/temper_dev cargo run --bin agent-runtime
///
/// Postgres schema (first run):
///   psql $DATABASE_URL -f ../../crates/temper-actor-runtime/schema.sql
use std::collections::HashSet;
use std::sync::Arc;

use deadpool_postgres::Config as PgConfig;
use temper_actor_runtime::{ActorSystem, SchedulerConfig};
use temper_agents::{register_agent_actors, register_real_integrations};
use temper_server::ServerState;

const PORT: u16 = 8080;
const TENANT: &str = "demo";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "agent_runtime=info,temper_agents=info,temper_server=warn".into()
            }),
        )
        .with_target(true)
        .compact()
        .init();

    // ─── 1. Build PG pool ────────────────────────────────────────────────────
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/temper_dev".to_string());

    let mut cfg = PgConfig::new();
    // Parse simple postgres://user:pass@host/db URL.
    let url = url::Url::parse(&db_url).expect("invalid DATABASE_URL");
    cfg.host = url.host_str().map(|s| s.to_string());
    cfg.port = url.port();
    cfg.user = if url.username().is_empty() {
        None
    } else {
        Some(url.username().to_string())
    };
    cfg.password = url.password().map(|s| s.to_string());
    cfg.dbname = Some(url.path().trim_start_matches('/').to_string());

    let pool = cfg
        .create_pool(None, tokio_postgres::NoTls)
        .expect("failed to create PG pool");

    tracing::info!("PG pool created for {db_url}");

    // ─── 2. Actor system ─────────────────────────────────────────────────────
    let actor_system = Arc::new(ActorSystem::new(
        pool.clone(),
        SchedulerConfig {
            poll_interval: std::time::Duration::from_millis(100),
            batch_size: 32,
        },
    ));

    // Register spec-driven actors (Process, AgentDefinition, Message, ContextManager, ...).
    register_agent_actors(&actor_system)
        .await
        .expect("failed to register agent actors");

    // Register real LLM + ToolExecutor integrations (reads AI_GATEWAY_TOKEN from env).
    let http = reqwest::Client::new();
    register_real_integrations(&actor_system, http, None)
        .await
        .expect("failed to register integrations");

    tracing::info!("Actor system ready");

    // ─── 3. Build CSDL ───────────────────────────────────────────────────────
    let csdl_xml = build_csdl();
    let csdl = temper_spec::csdl::parse_csdl(&csdl_xml).expect("invalid CSDL");
    let mut registry = temper_server::registry::SpecRegistry::new();

    let ioa_sources: Vec<(&str, &str)> = vec![
        ("Process", temper_agents::PROCESS_SPEC),
        ("AgentDefinition", temper_agents::AGENT_DEFINITION_SPEC),
        ("Message", temper_agents::MESSAGE_SPEC),
        ("ContextManager", temper_agents::CONTEXT_MANAGER_SPEC),
        ("ToolRouter", temper_agents::TOOL_ROUTER_SPEC),
        ("Compactor", temper_agents::COMPACTOR_SPEC),
    ];
    registry.register_tenant(TENANT, csdl.clone(), csdl_xml.clone(), &ioa_sources);

    for (entity_type, _) in &ioa_sources {
        registry.set_verification_status(
            &temper_runtime::tenant::TenantId::from(TENANT),
            entity_type,
            temper_server::registry::VerificationStatus::Completed(
                temper_server::registry::EntityVerificationResult {
                    all_passed: true,
                    levels: vec![],
                    verified_at: chrono::Utc::now().to_rfc3339(),
                },
            ),
        );
    }

    // ─── 4. Server state ─────────────────────────────────────────────────────
    let mut state = ServerState::from_registry(actor_system.clone(), registry);

    // All entity types are actor-backed (PG is the source of truth).
    state.actor_backed_types = [
        "Process",
        "AgentDefinition",
        "Message",
        "ContextManager",
        "ToolRouter",
        "Compactor",
        "LlmIntegration",
        "ContextAssemblerIntegration",
        "ToolExecutorIntegration",
        "CompactionIntegration",
        "ToolRegistry",
        "ToolDefinition",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<HashSet<_>>();

    // ─── 5. Start scheduler ──────────────────────────────────────────────────
    let scheduler_system = actor_system.clone();
    tokio::spawn(async move {
        // poll_once is on ActorSystem directly
        tracing::info!("Scheduler started (poll interval: 100ms)");
        loop {
            if let Err(e) = scheduler_system.poll_once().await {
                tracing::warn!(error = %e, "scheduler poll error");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    // ─── 6. Start HTTP server ────────────────────────────────────────────────
    let router = temper_server::build_router(state);
    let addr = format!("0.0.0.0:{PORT}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Agent runtime listening on http://{addr}");
    axum::serve(listener, router).await?;
    Ok(())
}

fn build_csdl() -> String {
    let entity_types = [
        "Process",
        "AgentDefinition",
        "Message",
        "ToolDefinition",
        "ContextManager",
        "ToolRouter",
        "Compactor",
    ];
    let mut et_xml = String::new();
    let mut es_xml = String::new();
    for name in &entity_types {
        et_xml.push_str(&format!(
            r#"<EntityType Name="{name}"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="Status" Type="Edm.String"/><Property Name="Fields" Type="Edm.String"/></EntityType>"#
        ));
        es_xml.push_str(&format!(
            r#"<EntitySet Name="{name}" EntityType="agent.{name}"/>"#
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices>
<Schema Namespace="agent" xmlns="http://docs.oasis-open.org/odata/ns/edm">
{et_xml}
<EntityContainer Name="DefaultContainer">{es_xml}</EntityContainer>
</Schema>
</edmx:DataServices>
</edmx:Edmx>"#
    )
}
