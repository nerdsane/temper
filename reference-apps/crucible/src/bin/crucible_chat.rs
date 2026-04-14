//! `crucible-chat` — Crucible agent CLI.
//!
//! Subcommands:
//!
//! - `seed` — create Environment + ManagedAgent + Session
//! - `send <session-id> <message>` — post a user message (rejects if
//!   session is Running)
//! - `watch <session-id>` — long-running poller that detects new user
//!   messages, drives the LLM turn loop, and transitions the session
//!   through Running → Idle. Supports interrupt detection.
//! - `interrupt <session-id>` — post a `user.interrupt` event to stop
//!   the agent mid-turn
//! - `respond <session-id>` — run one turn without posting a new
//!   message (legacy, for curl walkthrough compatibility)

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use crucible_reference::chat::anthropic::{Model, ResolvedModel, model_from_env};
use crucible_reference::chat::responder::{RespondRequest, respond};
use crucible_reference::chat::seed::{SeedOptions, seed};
use crucible_reference::chat::temper_client::{SessionEventRow, TemperClient};

#[derive(Debug, Parser)]
#[command(name = "crucible-chat", about = "Crucible reference agent CLI")]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
struct CommonArgs {
    /// Base URL of the running `temper serve` instance.
    #[arg(long, default_value = "http://127.0.0.1:3000", global = true)]
    server: String,

    /// Tenant to target.
    #[arg(long, default_value = "crucible", global = true)]
    tenant: String,

    /// Pick a model provider (`anthropic` or `openai`).
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Override `ANTHROPIC_API_KEY` from the environment.
    #[arg(long, global = true)]
    anthropic_api_key: Option<String>,

    /// Override `OPENAI_API_KEY` from the environment.
    #[arg(long, global = true)]
    openai_api_key: Option<String>,

    /// Override `OPENAI_BASE_URL` for any OpenAI-compatible endpoint.
    #[arg(long, global = true)]
    openai_base_url: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a fresh Environment + ManagedAgent + Session.
    Seed {
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        agent_id: Option<String>,
        #[arg(long)]
        environment_id: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        system: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },

    /// Post a user message. Rejects if session is Running.
    Send { session_id: String, message: String },

    /// Poll for new user messages and drive the agent loop.
    Watch {
        session_id: String,
        /// Poll interval in seconds.
        #[arg(long, default_value = "1")]
        poll_interval: u64,
    },

    /// Post a user.interrupt event to stop the agent mid-turn.
    Interrupt {
        session_id: String,
        /// Optional redirect message to send after interrupting.
        #[arg(long)]
        message: Option<String>,
    },

    /// Run one turn without posting a new user message (legacy).
    Respond { session_id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    // determinism-ok: CLI binary, not a sim-visible crate.
    if let Some(ref p) = cli.common.provider {
        unsafe { std::env::set_var("CRUCIBLE_RESPONDER_PROVIDER", p) };
    }
    if let Some(ref key) = cli.common.anthropic_api_key {
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", key) };
    }
    if let Some(ref key) = cli.common.openai_api_key {
        unsafe { std::env::set_var("OPENAI_API_KEY", key) };
    }
    if let Some(ref base) = cli.common.openai_base_url {
        unsafe { std::env::set_var("OPENAI_BASE_URL", base) };
    }

    let temper = TemperClient::new(&cli.common.server, &cli.common.tenant);

    match cli.command {
        Command::Seed {
            session_id,
            agent_id,
            environment_id,
            name,
            system,
            model,
        } => {
            let mut opts = SeedOptions::default();
            opts.session_id = session_id;
            opts.agent_id = agent_id;
            opts.environment_id = environment_id;
            if let Some(n) = name {
                opts.agent_name = n;
            }
            if let Some(s) = system {
                opts.system_prompt = s;
            }
            if let Some(m) = model {
                opts.model_id = m;
            }
            let outcome = seed(&temper, opts).await.context("seed failed")?;
            println!("environment_id={}", outcome.environment_id);
            println!("agent_id={}", outcome.agent_id);
            println!("session_id={}", outcome.session_id);
            Ok(())
        }

        Command::Send {
            session_id,
            message,
        } => {
            // Reject if session is Running (match Anthropic's behavior).
            let session = temper.get_session(&session_id).await?;
            if session.status == "Running" {
                return Err(anyhow!(
                    "Session is Running. Use `interrupt` to stop the current turn, \
                     or wait for it to finish."
                ));
            }
            if session.status == "Terminated" || session.status == "Archived" {
                return Err(anyhow!(
                    "Session is {} — cannot send messages.",
                    session.status
                ));
            }

            // POST the user.message event.
            let events = temper.list_session_events(&session_id, 500).await?;
            let next_seq = events.iter().map(|e| e.sequence).max().unwrap_or(-1) + 1;
            let now = chrono_now();
            let row = SessionEventRow {
                id: format!("ev-send-{session_id}-{next_seq}"),
                session_id: session_id.clone(),
                sequence: next_seq,
                kind: "user.message".to_string(),
                created_at: now.clone(),
                processed_at: Some(now),
                content: Some(
                    serde_json::json!({
                        "blocks": [{"type": "text", "text": message}]
                    })
                    .to_string(),
                ),
                ..blank_event()
            };
            temper.create_session_event(&row).await?;
            println!("Message sent (seq={next_seq}). Use `watch` to drive the agent loop.");
            Ok(())
        }

        Command::Watch {
            session_id,
            poll_interval,
        } => {
            let resolved = model_from_env().context("resolving model provider")?;
            match resolved {
                ResolvedModel::Anthropic(m) => {
                    watch_loop(&temper, &m, &session_id, poll_interval).await
                }
                ResolvedModel::OpenAI(m) => {
                    watch_loop(&temper, &m, &session_id, poll_interval).await
                }
                ResolvedModel::Mock(m) => watch_loop(&temper, &m, &session_id, poll_interval).await,
            }
        }

        Command::Interrupt {
            session_id,
            message,
        } => {
            let events = temper.list_session_events(&session_id, 500).await?;
            let next_seq = events.iter().map(|e| e.sequence).max().unwrap_or(-1) + 1;
            let now = chrono_now();

            // POST user.interrupt
            let row = SessionEventRow {
                id: format!("ev-int-{session_id}-{next_seq}"),
                session_id: session_id.clone(),
                sequence: next_seq,
                kind: "user.interrupt".to_string(),
                created_at: now.clone(),
                processed_at: Some(now.clone()),
                content: Some("{}".to_string()),
                ..blank_event()
            };
            temper.create_session_event(&row).await?;
            println!("Interrupt sent (seq={next_seq}).");

            // Optionally post a redirect message.
            if let Some(msg) = message {
                let row = SessionEventRow {
                    id: format!("ev-int-{session_id}-{}", next_seq + 1),
                    session_id: session_id.clone(),
                    sequence: next_seq + 1,
                    kind: "user.message".to_string(),
                    created_at: now.clone(),
                    processed_at: Some(now),
                    content: Some(
                        serde_json::json!({
                            "blocks": [{"type": "text", "text": msg}]
                        })
                        .to_string(),
                    ),
                    ..blank_event()
                };
                temper.create_session_event(&row).await?;
                println!("Redirect message sent (seq={}).", next_seq + 1);
            }
            Ok(())
        }

        Command::Respond { session_id } => run_turn(&temper, &session_id, None).await,
    }
}

/// The polling watch loop. Runs until the process is killed.
async fn watch_loop<M: Model>(
    temper: &TemperClient,
    model: &M,
    session_id: &str,
    poll_interval: u64,
) -> Result<()> {
    eprintln!("[watch] Watching session {session_id} (poll every {poll_interval}s)");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;

        // Check session status.
        let session = temper.get_session(session_id).await?;
        match session.status.as_str() {
            "Terminated" | "Archived" => {
                eprintln!("[watch] Session is {} — exiting.", session.status);
                return Ok(());
            }
            "Running" => {
                // Already running (another turn in flight or manual action).
                continue;
            }
            _ => {}
        }

        // Load full history and check if there's a pending user message.
        let events = temper.list_session_events(session_id, 500).await?;
        if !has_pending_user_message(&events) {
            continue;
        }

        // Drive the turn.
        eprintln!("[watch] Pending user message detected. Running agent turn...");
        let outcome = respond(
            temper,
            model,
            RespondRequest {
                session_id,
                new_user_message: None,
            },
        )
        .await;

        match outcome {
            Ok(result) => {
                println!("Agent: {}", result.assistant_text);
                eprintln!(
                    "[watch] Turn complete. input_tokens={} output_tokens={}",
                    result.input_tokens, result.output_tokens
                );
            }
            Err(e) => {
                eprintln!("[watch] Turn failed: {e:#}");
            }
        }
    }
}

/// Check if there's a user.message that hasn't been responded to yet.
/// A message is "pending" if it appears after the last session.status_idle
/// event (or if there's no idle event at all).
fn has_pending_user_message(events: &[SessionEventRow]) -> bool {
    let last_idle_seq = events
        .iter()
        .filter(|e| e.kind == "session.status_idle")
        .map(|e| e.sequence)
        .max()
        .unwrap_or(-1);

    events
        .iter()
        .any(|e| e.kind == "user.message" && e.sequence > last_idle_seq)
}

async fn run_turn(temper: &TemperClient, session_id: &str, new_user: Option<&str>) -> Result<()> {
    let resolved = model_from_env().context("resolving model provider")?;
    let outcome = match resolved {
        ResolvedModel::Anthropic(m) => dispatch(temper, &m, session_id, new_user).await?,
        ResolvedModel::OpenAI(m) => dispatch(temper, &m, session_id, new_user).await?,
        ResolvedModel::Mock(m) => dispatch(temper, &m, session_id, new_user).await?,
    };
    println!("Agent: {}", outcome.assistant_text);
    eprintln!(
        "[usage] input_tokens={} output_tokens={}",
        outcome.input_tokens, outcome.output_tokens
    );
    Ok(())
}

async fn dispatch<M: Model>(
    temper: &TemperClient,
    model: &M,
    session_id: &str,
    new_user: Option<&str>,
) -> Result<crucible_reference::chat::responder::RespondOutcome> {
    respond(
        temper,
        model,
        RespondRequest {
            session_id,
            new_user_message: new_user,
        },
    )
    .await
}

fn blank_event() -> SessionEventRow {
    SessionEventRow {
        id: String::new(),
        session_id: String::new(),
        sequence: 0,
        kind: String::new(),
        created_at: String::new(),
        processed_at: None,
        content: None,
        stop_reason: None,
        stop_reason_event_ids: None,
        model_request_start_id: None,
        is_error: None,
        model_input_tokens: None,
        model_output_tokens: None,
        model_cache_creation_input_tokens: None,
        model_cache_read_input_tokens: None,
        model_speed: None,
        tool_name: None,
        tool_use_id: None,
        session_thread_id: None,
    }
}

fn chrono_now() -> String {
    // Simple ISO-8601 timestamp from system clock.
    // determinism-ok: CLI binary, not sim-visible.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let secs = (now / 1000) as i64;
    let ms = now % 1000;
    // Reuse the Howard Hinnant algorithm from responder.rs
    let (y, mo, d, h, mi, s) = crucible_reference::chat::responder::epoch_to_civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
