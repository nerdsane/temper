//! Tool registry trait and implementations.
//!
//! Tools are the capabilities an agent can invoke during execution.
//! [`ToolRegistry`] abstracts over different tool sets:
//! - [`LocalToolRegistry`]: file I/O, shell, + entity operations (for CLI/local executor)
//! - [`TemperToolRegistry`]: entity CRUD only (for sandboxed executors)

pub mod local;
pub mod temper;

use anyhow::Result;
use serde_json::Value;

/// A tool definition sent to the LLM.
#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Tool name used in LLM tool-call requests.
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: Value,
}

/// Result of executing a tool.
#[derive(Debug)]
pub enum ToolResult {
    /// Tool succeeded with output text.
    Success(String),
    /// Tool execution failed with an error message.
    Error(String),
}

/// Cedar resource mapping for a tool invocation.
#[derive(Debug)]
pub struct CedarMapping {
    /// The Cedar resource type (e.g., "FileSystem", "Shell", "Entity").
    pub resource_type: String,
    /// The Cedar action (e.g., "read", "write", "execute").
    pub action: String,
    /// The Cedar resource ID.
    pub resource_id: String,
}

/// Trait for pluggable tool registries.
///
/// Implementations define which tools are available to an agent and how
/// they are executed.
#[async_trait::async_trait]
pub trait ToolRegistry: Send + Sync {
    /// List all available tool definitions (sent to the LLM).
    fn list_tools(&self) -> Vec<ToolDef>;

    /// Execute a tool by name with the given input.
    async fn execute(&self, name: &str, input: Value) -> Result<ToolResult>;

    /// Map a tool invocation to a Cedar resource for authorization.
    fn to_cedar(&self, name: &str, input: &Value) -> CedarMapping;
}
