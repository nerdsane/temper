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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State as AxumState;
use axum::response::Json;
use axum::routing::get;
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

/// Shared state for the health endpoint.
#[derive(Clone)]
struct HealthState {
    executor_id: String,
    max_concurrent: usize,
    active_agents: Arc<AtomicUsize>,
    shutting_down: Arc<AtomicBool>,
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

/// Health check endpoint handler.
async fn health_handler(AxumState(state): AxumState<HealthState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": if state.shutting_down.load(Ordering::Relaxed) { "draining" } else { "healthy" },
        "executor_id": state.executor_id,
        "active_agents": state.active_agents.load(Ordering::Relaxed),
        "max_concurrent": state.max_concurrent,
    }))
}

/// Run the health check HTTP server.
async fn run_health_server(port: u16, state: HealthState) -> Result<()> {
    let app = axum::Router::new()
        .route("/health", get(health_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .context("Failed to bind health port")?;
    info!(port = port, "Health endpoint listening");
    axum::serve(listener, app)
        .await
        .context("Health server error")?;
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

/// Default system prompt when no AgentType is configured.
const DEFAULT_SYSTEM_PROMPT: &str = "You are a Temper agent. Accomplish your assigned goal \
    using the tools available to you. Report results clearly.\n\n\
    ## Delegation\n\
    For complex tasks, you can delegate sub-tasks to child agents:\n\
    - `spawn_child_agent(role, goal, model)` — spawns a child that runs autonomously\n\
    - `check_children_status()` — check progress of all spawned children\n\
    You cannot complete until all children have finished (Completed or Failed).";

/// Resolve an AgentType entity for the given agent, returning (system_prompt, tool_set, model).
///
/// Falls back to CLI defaults when the agent has no agent_type_id or the AgentType
/// entity is not found.
async fn resolve_agent_type(
    client: &TemperClient,
    agent: &serde_json::Value,
    default_tool_mode: &str,
    default_model: &str,
) -> (String, String, String) {
    let agent_type_id = agent
        .get("agent_type_id")
        .or_else(|| agent.get("fields").and_then(|f| f.get("agent_type_id")))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if agent_type_id.is_empty() {
        return (
            DEFAULT_SYSTEM_PROMPT.to_string(),
            default_tool_mode.to_string(),
            default_model.to_string(),
        );
    }

    match client.get("AgentTypes", agent_type_id).await {
        Ok(at) => {
            let resolve = |key: &str, default: &str| -> String {
                at.get(key)
                    .or_else(|| at.get("fields").and_then(|f| f.get(key)))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(default)
                    .to_string()
            };
            let prompt = resolve("system_prompt", DEFAULT_SYSTEM_PROMPT);
            let tool_set = resolve("tool_set", default_tool_mode);
            let model = resolve("model", default_model);
            info!(
                agent_type_id = %agent_type_id,
                model = %model,
                tool_set = %tool_set,
                "Resolved AgentType"
            );
            (prompt, tool_set, model)
        }
        Err(e) => {
            warn!(
                agent_type_id = %agent_type_id,
                "Failed to resolve AgentType: {e}. Using defaults."
            );
            (
                DEFAULT_SYSTEM_PROMPT.to_string(),
                default_tool_mode.to_string(),
                default_model.to_string(),
            )
        }
    }
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

    // Fetch agent entity to resolve AgentType.
    let agent = client.get("Agents", agent_id).await?;
    let (system_prompt, resolved_tool_mode, resolved_model) =
        resolve_agent_type(&client, &agent, tool_mode, model).await;

    let provider = AnthropicProvider::new(&resolved_model)?;

    let tools: Box<dyn temper_agent_runtime::ToolRegistry> = match resolved_tool_mode.as_str() {
        "temper" => Box::new(TemperToolRegistry::new(TemperClient::new(
            temper_url, tenant,
        ))),
        _ => Box::new(LocalToolRegistry::new(TemperClient::new(
            temper_url, tenant,
        ))),
    };

    let principal_id = std::sync::Arc::new(std::sync::Mutex::new(Some(agent_id.to_string())));
    let runner = AgentRunner::new(client, Box::new(provider), tools, principal_id);
    runner.resume(agent_id, &system_prompt).await?;

    info!(agent_id = %agent_id, "Agent execution completed");
    Ok(())
}

/// Schedule ticker: periodically evaluates Active schedules and fires due ones.
///
/// Runs on a 60-second interval. For each Active Schedule entity, parses the
/// cron expression and fires `Schedule.Fire` if the cron is due. The Fire action's
/// spawn effect creates an Agent entity, which the SSE event loop picks up.
async fn run_schedule_ticker(temper_url: &str, tenant: &str, shutting_down: &Arc<AtomicBool>) {
    use chrono::Utc; // determinism-ok: executor process, not simulation-visible
    use cron::Schedule;
    use std::str::FromStr;

    let client = TemperClient::new(temper_url, tenant);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

    loop {
        interval.tick().await;
        if shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // Query Active schedules.
        let schedules = match client
            .list_filtered("Schedules", "status eq 'Active'")
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to query schedules: {e}");
                continue;
            }
        };

        let now = Utc::now(); // determinism-ok: executor process

        for sched in schedules {
            let field = |key: &str| -> &str {
                sched
                    .get(key)
                    .or_else(|| sched.get("fields").and_then(|f: &serde_json::Value| f.get(key)))
                    .and_then(|v: &serde_json::Value| v.as_str())
                    .unwrap_or_default()
            };
            let sched_id = field("id");
            let cron_expr = field("cron_expr");
            let last_run = field("last_run");
            let run_count: u64 = field("run_count").parse().unwrap_or(0);
            let max_runs: u64 = field("max_runs").parse().unwrap_or(0);

            // Check max_runs (0 = unlimited).
            if max_runs > 0 && run_count >= max_runs {
                // Auto-complete the schedule.
                if let Err(e) = client
                    .action("Schedules", sched_id, "Complete", serde_json::json!({}))
                    .await
                {
                    warn!(schedule_id = %sched_id, "Failed to complete schedule: {e}");
                }
                continue;
            }

            // Parse cron expression.
            let schedule = match Schedule::from_str(cron_expr) {
                Ok(s) => s,
                Err(e) => {
                    warn!(schedule_id = %sched_id, cron = %cron_expr, "Invalid cron expression: {e}");
                    continue;
                }
            };

            // Check if due: find next occurrence after last_run (or epoch if never run).
            let last = if last_run.is_empty() {
                chrono::DateTime::<Utc>::MIN_UTC
            } else {
                last_run
                    .parse::<chrono::DateTime<Utc>>()
                    .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC)
            };

            let next = schedule.after(&last).next();
            let is_due = next.is_some_and(|n| n <= now);

            if !is_due {
                continue;
            }

            // Resolve agent params from the schedule entity.
            let resolve = |key: &str| -> String {
                sched
                    .get(key)
                    .or_else(|| sched.get("fields").and_then(|f| f.get(key)))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };

            let agent_role = resolve("agent_role");
            let goal_template = resolve("goal_template");
            let agent_type_id = resolve("agent_type_id");
            let now_str = now.to_rfc3339();

            info!(
                schedule_id = %sched_id,
                role = %agent_role,
                "Firing schedule"
            );

            if let Err(e) = client
                .action(
                    "Schedules",
                    sched_id,
                    "Fire",
                    serde_json::json!({
                        "last_run": now_str,
                        "role": agent_role,
                        "goal": goal_template,
                        "model": "claude-sonnet-4-6",
                        "agent_type_id": agent_type_id,
                    }),
                )
                .await
            {
                warn!(schedule_id = %sched_id, "Failed to fire schedule: {e}");
            }
        }
    }
}

/// Double-fork daemonization with PID file.
fn daemonize() -> Result<()> {
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    // Create PID file directory.
    let pid_dir = dirs_pid_dir();
    fs::create_dir_all(&pid_dir).context("Failed to create PID directory")?;

    // Fork: the child continues, the parent exits.
    // We use a re-exec approach instead of raw fork for safety.
    let args: Vec<String> = std::env::args().filter(|a| a != "--detach").collect();
    let child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("Failed to spawn daemon process")?;

    // Write PID file.
    let pid_file = pid_dir.join("executor.pid");
    fs::write(&pid_file, child.id().to_string())
        .context("Failed to write PID file")?;
    eprintln!("Executor daemonized. PID={}, PID file={}", child.id(), pid_file.display());

    // The parent exits immediately.
    std::process::exit(0);
}

/// Clean up PID file on shutdown.
fn cleanup_pid_file() {
    let pid_file = dirs_pid_dir().join("executor.pid");
    if pid_file.exists() {
        std::fs::remove_file(&pid_file).ok();
    }
}

/// PID file directory: ~/.local/state/temper/
fn dirs_pid_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()); // determinism-ok: executor process
    std::path::PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("temper")
}
