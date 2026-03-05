//! Temper Agent Executor — headless agent runner.
//!
//! Watches for Agent entities in Working state via SSE, claims them by
//! PATCHing `executor_id`, and runs them through [`AgentRunner`].
//! Child agents created via `SpawnChild` are picked up by the same watch loop.
//!
//! # Features
//!
//! - **Health endpoint**: Axum HTTP server on `--health-port` returning executor status
//! - **Graceful shutdown**: `Ctrl-C` → stop accepting agents → drain active → exit
//! - **Detach mode**: `--detach` for daemonization with PID file
//!
//! # Usage
//!
//! ```bash
//! temper-executor --temper-url http://localhost:4200 --max-concurrent 3
//! temper-executor --detach --health-port 4201
//! ```

mod agent_type;
mod daemon;
mod health;
mod schedule;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::StreamExt;
use temper_sdk::TemperClient;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::agent_type::run_agent;
use crate::daemon::{cleanup_pid_file, daemonize};
use crate::health::{HealthState, run_health_server};
use crate::schedule::run_schedule_ticker;

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

    /// Port for the health check HTTP endpoint. Disabled if not set.
    #[arg(long)]
    health_port: Option<u16>,

    /// Detach as a background daemon. Writes PID to ~/.local/state/temper/executor.pid.
    #[arg(long)]
    detach: bool,
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

    // Handle --detach: double-fork daemonization.
    if cli.detach {
        daemonize()?;
    }

    let exec_id = executor_id();
    let active_agents = Arc::new(AtomicUsize::new(0));
    let shutting_down = Arc::new(AtomicBool::new(false));

    info!(
        executor_id = %exec_id,
        url = %cli.temper_url,
        tenant = %cli.tenant,
        max_concurrent = cli.max_concurrent,
        tool_mode = %cli.tool_mode,
        "Starting Temper executor"
    );

    let semaphore = Arc::new(Semaphore::new(cli.max_concurrent));

    let health_state = HealthState {
        executor_id: exec_id.clone(),
        max_concurrent: cli.max_concurrent,
        active_agents: active_agents.clone(),
        shutting_down: shutting_down.clone(),
    };

    // Launch health server if port specified.
    let health_handle = if let Some(port) = cli.health_port {
        let state = health_state.clone();
        Some(tokio::spawn(run_health_server(port, state)))
    } else {
        None
    };

    // Main loop with graceful shutdown via tokio::select!
    let shutdown = shutting_down.clone();
    let event_loop = async {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            if let Err(e) =
                run_event_loop(&cli, &exec_id, &semaphore, &active_agents, &shutdown).await
            {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                error!("Event loop error: {e}. Reconnecting in 5 seconds...");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    };

    // Schedule ticker: evaluates cron expressions for Active schedules.
    let ticker_shutdown = shutting_down.clone();
    let ticker_url = cli.temper_url.clone();
    let ticker_tenant = cli.tenant.clone();
    let schedule_ticker = async {
        run_schedule_ticker(&ticker_url, &ticker_tenant, &ticker_shutdown).await;
    };

    tokio::select! {
        _ = event_loop => {
            info!("Event loop exited");
        }
        _ = schedule_ticker => {
            info!("Schedule ticker exited");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
            shutting_down.store(true, Ordering::Relaxed);
        }
    }

    // Graceful drain: wait for active agents to finish.
    info!("Draining active agents...");
    let drain_start = std::time::Instant::now(); // determinism-ok: executor timing
    let drain_timeout = std::time::Duration::from_secs(300);
    while active_agents.load(Ordering::Relaxed) > 0 {
        if drain_start.elapsed() > drain_timeout {
            warn!(
                active = active_agents.load(Ordering::Relaxed),
                "Drain timeout reached, shutting down anyway"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    if let Some(h) = health_handle {
        h.abort();
    }

    // Clean up PID file if we daemonized.
    if cli.detach {
        cleanup_pid_file();
    }

    info!("Executor shutdown complete");
    Ok(())
}

/// Connect to the SSE event stream and process agent events.
async fn run_event_loop(
    cli: &Cli,
    exec_id: &str,
    semaphore: &Arc<Semaphore>,
    active_agents: &Arc<AtomicUsize>,
    shutting_down: &Arc<AtomicBool>,
) -> Result<()> {
    let client = TemperClient::new(&cli.temper_url, &cli.tenant);

    info!("Connecting to SSE event stream...");
    let stream = client
        .events_stream()
        .await
        .context("Failed to connect to SSE event stream")?;

    let mut stream = Box::pin(stream);

    info!("Connected. Watching for agents...");

    while let Some(event_result) = stream.next().await {
        if shutting_down.load(Ordering::Relaxed) {
            info!("Shutdown requested, stopping event loop");
            break;
        }

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
        let active = active_agents.clone();

        active.fetch_add(1, Ordering::Relaxed);

        tokio::spawn(async move {
            let _permit = permit; // Held until task completes.

            if let Err(e) = run_agent(&temper_url, &tenant, &agent_id, &tool_mode, &model).await {
                error!(agent_id = %agent_id, "Agent execution failed: {e}");
            }

            active.fetch_sub(1, Ordering::Relaxed);
        });
    }

    anyhow::bail!("SSE stream ended unexpectedly")
}
