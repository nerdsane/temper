//! Typed error enum for the temper-mcp crate.
//!
//! Replaces the previous mix of `anyhow::Error` and bare `String` errors with
//! a single `McpError` type covering transport, protocol, tool, and runtime
//! categories. Each variant carries enough context to map deterministically to
//! a JSON-RPC error code at the protocol boundary.

use serde_json::Value;

/// Unified error type for temper-mcp operations.
///
/// All internal layers return `Result<T, McpError>`. The protocol boundary
/// in `protocol.rs` converts these into JSON-RPC error envelopes via the
/// [`McpError::json_rpc_code`] method.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    // ── Transport ────────────────────────────────────────────────────────
    /// Failed to reach the Temper HTTP server or read its response.
    #[error("{0}")]
    Transport(String),

    // ── Protocol ─────────────────────────────────────────────────────────
    /// Malformed JSON on the wire (JSON-RPC parse error, code -32700).
    #[error("parse error: {0}")]
    ParseError(String),

    /// Structurally valid JSON but not a valid JSON-RPC request (-32600).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Unknown JSON-RPC method (-32601).
    #[error("method not found: {0}")]
    MethodNotFound(String),

    /// Invalid method parameters (-32602).
    #[error("{0}")]
    InvalidParams(String),

    // ── Tool ─────────────────────────────────────────────────────────────
    /// An error that originated inside a tool call (e.g. HTTP 4xx/5xx from
    /// the Temper server, missing arguments, unknown tool method).
    /// These are returned as MCP tool-level `isError: true` content, **not**
    /// as JSON-RPC error envelopes.
    #[error("{0}")]
    Tool(String),

    // ── Runtime ──────────────────────────────────────────────────────────
    /// Sandbox execution errors (Monty VM failures, OS-call blocks, etc.).
    #[error("{0}")]
    Runtime(String),

    /// JSON serialization/deserialization failure.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Spec loading or configuration error.
    #[error("{0}")]
    Config(String),
}

/// Result alias used throughout the crate.
pub type McpResult<T> = std::result::Result<T, McpError>;

impl McpError {
    /// Map this error to a JSON-RPC error code.
    ///
    /// Only protocol-level errors produce JSON-RPC error envelopes.
    /// Tool and Runtime errors are surfaced as `isError: true` tool content.
    pub(crate) fn json_rpc_code(&self) -> i64 {
        match self {
            Self::ParseError(_) => -32700,
            Self::InvalidRequest(_) => -32600,
            Self::MethodNotFound(_) => -32601,
            Self::InvalidParams(_) => -32602,
            Self::Transport(_)
            | Self::Tool(_)
            | Self::Runtime(_)
            | Self::Serialization(_)
            | Self::Config(_) => -32603,
        }
    }

    /// Convert this error into a JSON-RPC error envelope.
    pub(crate) fn into_json_rpc_envelope(self, id: Option<Value>) -> Value {
        let code = self.json_rpc_code();
        let message = self.to_string();
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.unwrap_or(Value::Null),
            "error": {
                "code": code,
                "message": message,
            },
        })
    }
}

impl From<std::io::Error> for McpError {
    fn from(err: std::io::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

impl From<reqwest::Error> for McpError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.to_string())
    }
}
