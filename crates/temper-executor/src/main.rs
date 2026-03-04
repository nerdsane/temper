//! Temper Agent Executor — headless agent runner.
//!
//! Watches for Agent entities in Working state via SSE, claims them by
//! PATCHing `executor_id`, and runs them through [`AgentRunner`].
//! Child agents created via `SpawnChild` are picked up by the same watch loop.
//!
//! # Usage
//!
//! ```bash
//! temper-executor --temper-url http://localhost:4200 --max-concurrent 3
//! ```

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::StreamExt;
use temper_agent_runtime::{AgentRunner, AnthropicProvider, LocalToolRegistry, TemperToolRegistry};
use temper_sdk::TemperClient;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

/// Headless agent executor for Temper.
///
/// Watches for Agent entities entering Working state and executes them
/// using the configured LLM provider and tool registry.
#[derive(Parser)]
#[command(name = "temper-executor", about = "Headless agent executor for Temper")]
struct Cli {
    /// Temper server URL.
    #[arg(long, default_value = "http://localhost:4200")]
    temper_url: String,

    /// Tenant ID for multi-tenant scoping.
    #[arg(long, default_value = "default")]
    tenant: String,

    /// Maximum number of concurrent agent runs.
    #[arg(long, default_value = "3")]
    max_concurrent: usize,

    /// Tool mode: "local" for file I/O + shell + entities, "temper" for entity-only.
    #[arg(long, default_value = "local")]
    tool_mode: String,

    /// LLM model to use for agent execution.
    #[arg(long, default_value = "claude-sonnet-4-6")]
    model: String,
}

/// Unique executor identity for claiming agents.
fn executor_id() -> String {
    format!("executor-{}-{}", hostname(), std::process::id())
}

/// Best-effort hostname for executor identity.
fn hostname() -> String {
    std::env::var("HOSTNAME") // determinism-ok: executor process, not simulation-visible
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let exec_id = executor_id();

    info!(
        executor_id = %exec_id,
        url = %cli.temper_url,
        tenant = %cli.tenant,
        max_concurrent = cli.max_concurrent,
        tool_mode = %cli.tool_mode,
        "Starting Temper executor"
    );

    let semaphore = Arc::new(Semaphore::new(cli.max_concurrent));

    // Main event loop: connect to SSE, watch for agents, claim and run them.
    loop {
        if let Err(e) = run_event_loop(&cli, &exec_id, &semaphore).await {
            error!("Event loop error: {e}. Reconnecting in 5 seconds...");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
}

/// Connect to the SSE event stream and process agent events.
async fn run_event_loop(cli: &Cli, exec_id: &str, semaphore: &Arc<Semaphore>) -> Result<()> {
    let client = TemperClient::new(&cli.temper_url, &cli.tenant);

    info!("Connecting to SSE event stream...");
    let stream = client
        .events_stream()
        .await
        .context("Failed to connect to SSE event stream")?;

    let mut stream = Box::pin(stream);

    info!("Connected. Watching for agents...");

    while let Some(event_result) = stream.next().await {
        let event = match event_result {
            Ok(e) => e,
            Err(e) => {
                warn!("SSE event parse error: {e}");
                continue;
            }
        };

        // Watch for Agent entities that just entered Working state.
        if event.entity_type != "Agents" {
            continue;
        }

        // Check if this is a Start action (transitions to Working state).
        if event.action != "Start" {
            continue;
        }

        let agent_id = event.entity_id.clone();
        info!(agent_id = %agent_id, "Detected agent in Working state");

        // Check if already claimed.
        let agent = match client.get("Agents", &agent_id).await {
            Ok(a) => a,
            Err(e) => {
                warn!(agent_id = %agent_id, "Failed to get agent: {e}");
                continue;
            }
        };

        let current_executor = agent
            .get("executor_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !current_executor.is_empty() {
            info!(
                agent_id = %agent_id,
                executor = %current_executor,
                "Agent already claimed"
            );
            continue;
        }

        // Claim the agent.
        if let Err(e) = client
            .patch(
                "Agents",
                &agent_id,
                serde_json::json!({ "executor_id": exec_id }),
            )
            .await
        {
            warn!(agent_id = %agent_id, "Failed to claim agent: {e}");
            continue;
        }

        info!(agent_id = %agent_id, "Claimed agent");

        // Spawn a task to run the agent.
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    agent_id = %agent_id,
                    "Max concurrent limit reached. Agent will be picked up later."
                );
                // Unclaim the agent so another executor can pick it up.
                if let Err(e) = client
                    .patch(
                        "Agents",
                        &agent_id,
                        serde_json::json!({ "executor_id": "" }),
                    )
                    .await
                {
                    warn!("Failed to unclaim agent: {e}");
                }
                continue;
            }
        };

        let temper_url = cli.temper_url.clone();
        let tenant = cli.tenant.clone();
        let tool_mode = cli.tool_mode.clone();
        let model = cli.model.clone();

        tokio::spawn(async move {
            let _permit = permit; // Held until task completes.

            if let Err(e) = run_agent(&temper_url, &tenant, &agent_id, &tool_mode, &model).await {
                error!(agent_id = %agent_id, "Agent execution failed: {e}");
            }
        });
    }

    anyhow::bail!("SSE stream ended unexpectedly")
}

/// Run a single agent to completion.
async fn run_agent(
    temper_url: &str,
    tenant: &str,
    agent_id: &str,
    tool_mode: &str,
    model: &str,
) -> Result<()> {
    info!(agent_id = %agent_id, "Starting agent execution");

    let client = TemperClient::new(temper_url, tenant);
    let provider = AnthropicProvider::new(model)?;

    let tools: Box<dyn temper_agent_runtime::ToolRegistry> = match tool_mode {
        "temper" => Box::new(TemperToolRegistry::new(TemperClient::new(
            temper_url, tenant,
        ))),
        _ => Box::new(LocalToolRegistry::new(TemperClient::new(
            temper_url, tenant,
        ))),
    };

    let principal_id = std::sync::Arc::new(std::sync::Mutex::new(Some(agent_id.to_string())));
    let runner = AgentRunner::new(client, Box::new(provider), tools, principal_id);
    runner.resume(agent_id).await?;

    info!(agent_id = %agent_id, "Agent execution completed");
    Ok(())
}
