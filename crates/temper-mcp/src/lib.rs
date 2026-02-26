//! stdio MCP server exposing Temper Code Mode tools.

use std::path::PathBuf;

mod convert;
mod protocol;
mod runtime;
mod sandbox;
mod spec_loader;
mod tools;

pub use runtime::run_stdio_server;

#[cfg(test)]
use protocol::dispatch_json_value;
use runtime::RuntimeContext;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_SERVER_NAME: &str = "temper-mcp";

/// A single app spec source, loaded as `name=specs_dir` from CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub name: String,
    pub specs_dir: PathBuf,
}

/// Runtime config for the stdio MCP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpConfig {
    pub temper_port: Option<u16>,
    pub apps: Vec<AppConfig>,
    /// Agent principal ID for `X-Temper-Principal-Id` header. When set, all
    /// requests include agent identity headers for Cedar authorization.
    pub principal_id: Option<String>,
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
