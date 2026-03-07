//! Agent CLI user interface.
//!
//! [`AgentUI`] is a state machine that manages spinners, streaming text,
//! and markdown rendering for the `temper agent` command. It produces
//! `on_delta` and `on_event` callbacks for [`AgentRunner`].
//!
//! State machine:
//! ```text
//! Idle → Thinking (spinner) → Streaming (raw print) → Idle
//!                                  ↓ (tool_use returned)
//!                             ToolRunning (spinner) → back to Thinking
//! ```

pub mod markdown;
pub mod spinner;
pub mod theme;

use std::io::Write;
use std::sync::{Arc, Mutex};

use console::{Term, style};
use indicatif::ProgressBar;
use temper_agent_runtime::AgentEvent;

/// UI state machine states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiState {
    /// No active spinner or streaming.
    Idle,
    /// Thinking spinner is active (waiting for LLM).
    Thinking,
    /// Streaming text deltas from LLM.
    Streaming,
}

/// Shared mutable UI state behind a mutex.
struct UiInner {
    state: UiState,
    /// Active spinner (if any).
    spinner: Option<ProgressBar>,
    /// Whether this is a real terminal (not piped).
    is_term: bool,
    /// Terminal width for markdown rendering.
    term_width: usize,
}

/// Agent CLI user interface.
///
/// Thread-safe wrapper around spinner lifecycle and output formatting.
/// Create once, then call [`make_on_delta`] and [`make_event_handler`]
/// to get callbacks for [`AgentRunner`].
pub struct AgentUI {
    inner: Arc<Mutex<UiInner>>,
}

impl AgentUI {
    /// Create a new `AgentUI`, auto-detecting terminal capabilities.
    pub fn new() -> Self {
        let term = Term::stdout();
        let is_term = term.is_term();
        let term_width = if is_term { term.size().1 as usize } else { 80 };

        Self {
            inner: Arc::new(Mutex::new(UiInner {
                state: UiState::Idle,
                spinner: None,
                is_term,
                term_width,
            })),
        }
    }

    /// Whether we're running in a real terminal.
    #[allow(dead_code)]
    pub fn is_term(&self) -> bool {
        self.inner.lock().unwrap().is_term // ci-ok: infallible lock
    }

    /// Terminal width.
    #[allow(dead_code)]
    pub fn term_width(&self) -> usize {
        self.inner.lock().unwrap().term_width // ci-ok: infallible lock
    }

    /// Create the `on_delta` callback for [`AgentRunner`].
    ///
    /// On the first text delta, kills the thinking spinner and switches
    /// to streaming mode. Subsequent deltas are printed raw.
    pub fn make_on_delta(self: &Arc<Self>) -> Arc<dyn Fn(String) + Send + Sync> {
        let ui = Arc::clone(self);
        Arc::new(move |text| {
            let mut inner = ui.inner.lock().unwrap(); // ci-ok: infallible lock

            // First delta: kill spinner, switch to streaming.
            if inner.state == UiState::Thinking {
                if let Some(ref pb) = inner.spinner {
                    pb.finish_and_clear();
                }
                inner.spinner = None;
                inner.state = UiState::Streaming;
            }

            // Print raw text delta.
            print!("{text}");
            std::io::stdout().flush().ok();
        })
    }

    /// Create the `on_event` callback for [`AgentRunner`].
    pub fn make_event_handler(self: &Arc<Self>) -> Arc<dyn Fn(AgentEvent) + Send + Sync> {
        let ui = Arc::clone(self);
        Arc::new(move |event| {
            let mut inner = ui.inner.lock().unwrap(); // ci-ok: infallible lock

            match event {
                AgentEvent::LlmCallStart => {
                    // Start thinking spinner (only in terminal mode).
                    if inner.is_term {
                        let pb = spinner::thinking();
                        inner.spinner = Some(pb);
                    }
                    inner.state = UiState::Thinking;
                }
                AgentEvent::LlmCallEnd { full_text: _ } => {
                    // Kill any active spinner.
                    if let Some(ref pb) = inner.spinner {
                        pb.finish_and_clear();
                    }
                    inner.spinner = None;

                    // If we were streaming, just finish with a newline.
                    // The text was already printed by on_delta during streaming.
                    if inner.state == UiState::Streaming {
                        println!();
                    }
                    inner.state = UiState::Idle;
                }
                AgentEvent::ToolStart { name } => {
                    // Kill any active spinner first.
                    if let Some(ref pb) = inner.spinner {
                        pb.finish_and_clear();
                    }
                    if inner.is_term {
                        let pb = spinner::tool(&name);
                        inner.spinner = Some(pb);
                    } else {
                        eprintln!("  [tool] {name}...");
                    }
                }
                AgentEvent::ToolEnd {
                    name,
                    success,
                    duration_ms,
                } => {
                    if let Some(ref pb) = inner.spinner {
                        pb.finish_and_clear();
                    }
                    inner.spinner = None;

                    let status = if success {
                        style("done").green()
                    } else {
                        style("failed").red()
                    };
                    let duration = style(format!("{duration_ms}ms")).dim();
                    if inner.is_term {
                        eprintln!(
                            "  {} {} {} {}",
                            style("tool").yellow().bold(),
                            style(&name).yellow(),
                            status,
                            duration
                        );
                    } else {
                        eprintln!("  [tool] {name} {status} ({duration_ms}ms)");
                    }
                }
                AgentEvent::GovernanceAllowed { action, resource } => {
                    if inner.is_term {
                        let detail = if resource.is_empty() {
                            action.clone()
                        } else {
                            format!("{action} on {resource}")
                        };
                        eprintln!(
                            "  {} {}",
                            style("authz").green().bold(),
                            style(detail).green()
                        );
                    } else {
                        let detail = if resource.is_empty() {
                            action.clone()
                        } else {
                            format!("{action} on {resource}")
                        };
                        eprintln!("  [authz] allowed {detail}");
                    }
                }
                AgentEvent::GovernanceWait {
                    decision_id,
                    action,
                } => {
                    if let Some(ref pb) = inner.spinner {
                        pb.finish_and_clear();
                    }
                    if inner.is_term {
                        eprintln!(
                            "  {} {} needs approval: {}",
                            style("governance").magenta().bold(),
                            style(&action).magenta(),
                            style(&decision_id).dim()
                        );
                        let pb = spinner::governance(&decision_id);
                        inner.spinner = Some(pb);
                    } else {
                        eprintln!("  [governance] {action} needs approval: {decision_id}");
                    }
                }
                AgentEvent::GovernanceResolved { approved } => {
                    if let Some(ref pb) = inner.spinner {
                        pb.finish_and_clear();
                    }
                    inner.spinner = None;
                    let result = if approved {
                        style("approved").green()
                    } else {
                        style("denied").red()
                    };
                    if inner.is_term {
                        eprintln!("  {} {}", style("governance").magenta().bold(), result);
                    } else {
                        eprintln!("  [governance] {result}");
                    }
                }
            }
        })
    }

    /// Create an inline governance resolver for interactive approval prompts.
    ///
    /// Uses `dialoguer::Select` to prompt the user with scope choices.
    /// In non-TTY mode, returns `Wait` immediately (external-only approval).
    pub fn make_governance_resolver(
        self: &Arc<Self>,
    ) -> temper_agent_runtime::GovernanceResolverFn {
        let ui = Arc::clone(self);
        Arc::new(move |prompt: temper_agent_runtime::GovernancePrompt| {
            let is_term = ui.inner.lock().unwrap().is_term; // ci-ok: infallible lock
            if !is_term {
                return temper_agent_runtime::GovernanceDecision::Wait;
            }

            // Kill any active spinner before prompting.
            {
                let mut inner = ui.inner.lock().unwrap(); // ci-ok: infallible lock
                if let Some(ref pb) = inner.spinner {
                    pb.finish_and_clear();
                }
                inner.spinner = None;
            }

            eprintln!();
            eprintln!(
                "  {} {} needs approval",
                style("governance").magenta().bold(),
                style(format!(
                    "tools.{}(\"{}\")",
                    prompt.action, prompt.resource_id
                ))
                .magenta(),
            );
            eprintln!();
            eprintln!(
                "  {}  {}",
                style("Decision:").bold(),
                style(&prompt.decision_id).dim()
            );
            eprintln!(
                "  {}    {} on {}",
                style("Action:").bold(),
                prompt.action,
                prompt.resource_type
            );
            eprintln!("  {}  {}", style("Resource:").bold(), prompt.resource_id);
            eprintln!();

            let items = &[
                format!(
                    "narrow  — this exact agent + {} + {}",
                    prompt.action, prompt.resource_id
                ),
                format!(
                    "medium  — this agent + {} on any {}",
                    prompt.action, prompt.resource_type
                ),
                format!(
                    "broad   — this agent + any action on any {}",
                    prompt.resource_type
                ),
                "deny    — reject this request".to_string(),
                "wait    — approve in Observe UI instead".to_string(),
            ];

            let selection = dialoguer::Select::new()
                .with_prompt("Choose scope")
                .items(items)
                .default(0)
                .interact_opt();

            match selection {
                Ok(Some(0)) => temper_agent_runtime::GovernanceDecision::Approve {
                    scope: temper_agent_runtime::GovernanceScope::Narrow,
                },
                Ok(Some(1)) => temper_agent_runtime::GovernanceDecision::Approve {
                    scope: temper_agent_runtime::GovernanceScope::Medium,
                },
                Ok(Some(2)) => temper_agent_runtime::GovernanceDecision::Approve {
                    scope: temper_agent_runtime::GovernanceScope::Broad,
                },
                Ok(Some(3)) => temper_agent_runtime::GovernanceDecision::Deny,
                _ => temper_agent_runtime::GovernanceDecision::Wait,
            }
        })
    }

    // ── Styled output methods ──────────────────────────────────────────

    /// Print the interactive mode banner.
    pub fn print_banner(&self, model: &str, role: &str) {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            eprintln!(
                "\n  {} {}\n",
                style("Temper Agent").cyan().bold(),
                style("— interactive mode").dim()
            );
            eprintln!(
                "  {}  {}    {}  {}",
                style("Model:").bold(),
                model,
                style("Role:").bold(),
                role
            );
            eprintln!("  {}\n", style("Type your request. Ctrl+D to exit.").dim());
        } else {
            eprintln!("Temper Agent — interactive mode");
            eprintln!("Model: {model}  Role: {role}");
            eprintln!("Type your request. Ctrl+D to exit.\n");
        }
    }

    /// Print the agent ID after creation.
    pub fn print_agent_id(&self, agent_id: &str) {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            eprintln!("  {}  {}\n", style("Agent:").bold(), style(agent_id).dim());
        } else {
            eprintln!("Agent: {agent_id}\n");
        }
    }

    /// Print goal/role/model for autonomous mode.
    pub fn print_goal_info(&self, goal: &str, role: &str, model: &str) {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            eprintln!("\n  {}  {}", style("Goal:").bold(), style(goal).cyan());
            eprintln!(
                "  {}  {}    {}  {}",
                style("Role:").bold(),
                role,
                style("Model:").bold(),
                model
            );
            eprintln!();
        } else {
            eprintln!("Goal:  {goal}");
            eprintln!("Role:  {role}");
            eprintln!("Model: {model}");
            eprintln!();
        }
    }

    /// Print resuming message.
    pub fn print_resuming(&self, agent_id: &str) {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            eprintln!(
                "  {} {}",
                style("Resuming agent:").bold(),
                style(agent_id).dim()
            );
        } else {
            eprintln!("Resuming agent: {agent_id}");
        }
    }

    /// Print agent completion.
    pub fn print_completed(&self, agent_id: &str) {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            eprintln!(
                "\n  {} {}",
                style("Agent completed:").green().bold(),
                style(agent_id).dim()
            );
        } else {
            eprintln!("\nAgent completed: {agent_id}");
        }
    }

    /// Print a provider fallback warning.
    pub fn print_provider_fallback(&self, msg: &str) {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            eprintln!(
                "  {} {}",
                style("warn").yellow().bold(),
                style(msg).yellow()
            );
        } else {
            eprintln!("  {msg}");
        }
    }

    /// Print an error.
    pub fn print_error(&self, msg: &str) {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            eprintln!("  {} {}", style("error").red().bold(), msg);
        } else {
            eprintln!("Error: {msg}");
        }
    }

    /// Get the styled REPL prompt string.
    pub fn prompt_string(&self) -> String {
        let inner = self.inner.lock().unwrap(); // ci-ok: infallible lock
        if inner.is_term {
            format!("{} ", style("temper>").cyan().bold())
        } else {
            "temper> ".to_string()
        }
    }
}
