//! Spinner factory functions for the agent CLI.
//!
//! Thin wrappers around `indicatif::ProgressBar` with themed styles.

use console::style;
use indicatif::{ProgressBar, ProgressStyle};

/// Create a "thinking" spinner for LLM calls.
pub fn thinking() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!(
            "  {} {{spinner}} {{msg}}",
            style("thinking").cyan().bold()
        ))
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

/// Create a spinner for tool execution.
pub fn tool(name: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!(
            "  {} {{spinner}} {}",
            style("tool").yellow().bold(),
            style(name).yellow()
        ))
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

/// Create a spinner for governance / authorization wait.
pub fn governance(decision_id: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!(
            "  {} {{spinner}} Waiting for approval: {}",
            style("governance").magenta().bold(),
            style(decision_id).dim()
        ))
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["◐", "◓", "◑", "◒"]),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(250));
    pb
}

/// Create a progress bar for task execution in autonomous mode.
pub fn task_progress(index: usize, total: usize, title: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template(&format!(
            "  {} {{spinner}} [{index}/{total}] {title}",
            style("task").blue().bold(),
        ))
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}
