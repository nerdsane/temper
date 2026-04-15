//! Deprecated shim — use `temper_sandbox::repl` directly.

#[deprecated(note = "Use temper_sandbox::repl directly")]
pub use temper_sandbox::repl::ReplConfig;

#[deprecated(note = "Use temper_sandbox::repl::run_repl directly")]
pub use temper_sandbox::repl::run_repl;
