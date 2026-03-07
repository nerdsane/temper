//! Style constants for the agent CLI.
//!
//! Provides consistent terminal styling using the `console` crate.

#![allow(dead_code)]

use console::Style;

/// Style for section headings (bold cyan).
pub fn heading() -> Style {
    Style::new().cyan().bold()
}

/// Style for labels like "Model:", "Role:" (bold).
pub fn label() -> Style {
    Style::new().bold()
}

/// Style for tool names (yellow bold).
pub fn tool_name() -> Style {
    Style::new().yellow().bold()
}

/// Style for error messages (red bold).
pub fn error() -> Style {
    Style::new().red().bold()
}

/// Style for governance / authorization messages (magenta bold).
pub fn governance() -> Style {
    Style::new().magenta().bold()
}

/// Style for dimmed secondary text (timestamps, IDs).
pub fn dim() -> Style {
    Style::new().dim()
}

/// Style for success indicators (green).
pub fn success() -> Style {
    Style::new().green()
}

/// Style for the REPL prompt (cyan bold).
pub fn prompt() -> Style {
    Style::new().cyan().bold()
}
