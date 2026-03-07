//! Shared Monty sandbox infrastructure for Temper.
//!
//! Provides JSON/Monty conversion, helper utilities, HTTP dispatch for
//! `temper.*` methods, and a generic sandbox runner. Used by both
//! `temper-mcp`.

pub mod convert;
pub mod dispatch;
pub mod helpers;
pub mod http;
pub mod repl;
pub mod runner;
