//! Tool definitions, Cedar resource mapping, and local execution.
//!
//! Each tool maps to a Cedar resource type and action. The agent's LLM
//! receives tool schemas; execution happens locally on the CLI side.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;

/// Tool metadata sent to the LLM as tool schemas.
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
}

/// Cedar mapping for a tool invocation.
pub struct CedarMapping {
    pub resource_type: String,
    pub action: String,
    pub resource_id: String,
}

/// Result of executing a tool locally.
pub enum ToolResult {
    /// Tool succeeded with output text.
    Success(String),
    /// Tool execution failed.
    Error(String),
    /// Tool was denied by Cedar policy.
    Denied {
        decision_id: String,
        tool_call_id: String,
    },
}

/// All tool definitions sent to the Anthropic Messages API.
pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "file_read",
            description: "Read the contents of a file at the given path.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative file path to read."
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "file_write",
            description: "Write content to a file at the given path. Creates the file if it doesn't exist.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative file path to write."
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file."
                    }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "file_list",
            description: "List files and directories at the given path.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path to list. Defaults to current directory."
                    }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "shell_execute",
            description: "Execute a shell command and return its output.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute."
                    }
                },
                "required": ["command"]
            }),
        },
    ]
}

/// Map a tool name + input to a Cedar resource type, action, and resource ID.
pub fn tool_to_cedar(tool_name: &str, tool_input: &serde_json::Value) -> CedarMapping {
    match tool_name {
        "file_read" => CedarMapping {
            resource_type: "FileSystem".to_string(),
            action: "read".to_string(),
            resource_id: tool_input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        },
        "file_write" => CedarMapping {
            resource_type: "FileSystem".to_string(),
            action: "write".to_string(),
            resource_id: tool_input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        },
        "file_list" => CedarMapping {
            resource_type: "FileSystem".to_string(),
            action: "list".to_string(),
            resource_id: tool_input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".")
                .to_string(),
        },
        "shell_execute" => CedarMapping {
            resource_type: "Shell".to_string(),
            action: "execute".to_string(),
            resource_id: tool_input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        },
        _ => CedarMapping {
            resource_type: "Unknown".to_string(),
            action: tool_name.to_string(),
            resource_id: "unknown".to_string(),
        },
    }
}

/// Execute a tool locally and return the output.
pub async fn execute_tool(tool_name: &str, tool_input: &serde_json::Value) -> Result<String> {
    match tool_name {
        "file_read" => {
            let path = tool_input
                .get("path")
                .and_then(|v| v.as_str())
                .context("file_read: missing 'path' parameter")?;
            tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("Failed to read file: {path}"))
        }
        "file_write" => {
            let path = tool_input
                .get("path")
                .and_then(|v| v.as_str())
                .context("file_write: missing 'path' parameter")?;
            let content = tool_input
                .get("content")
                .and_then(|v| v.as_str())
                .context("file_write: missing 'content' parameter")?;
            if let Some(parent) = Path::new(path).parent() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
            tokio::fs::write(path, content)
                .await
                .with_context(|| format!("Failed to write file: {path}"))?;
            Ok(format!("Written {len} bytes to {path}", len = content.len()))
        }
        "file_list" => {
            let path = tool_input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("."); // ci-ok: CLI tool, not server
            let mut entries = Vec::new();
            let mut dir = tokio::fs::read_dir(path)
                .await
                .with_context(|| format!("Failed to list directory: {path}"))?;
            while let Some(entry) = dir
                .next_entry()
                .await
                .with_context(|| format!("Failed reading entry in: {path}"))?
            {
                let name = entry.file_name().to_string_lossy().to_string();
                let file_type = entry.file_type().await.ok();
                let suffix = if file_type.as_ref().is_some_and(|ft| ft.is_dir()) {
                    "/"
                } else {
                    ""
                };
                entries.push(format!("{name}{suffix}"));
            }
            entries.sort();
            Ok(entries.join("\n"))
        }
        "shell_execute" => {
            let command = tool_input
                .get("command")
                .and_then(|v| v.as_str())
                .context("shell_execute: missing 'command' parameter")?;
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .await
                .with_context(|| format!("Failed to execute command: {command}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut result = String::new();
            if !stdout.is_empty() {
                result.push_str(&stdout);
            }
            if !stderr.is_empty() {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str("stderr: ");
                result.push_str(&stderr);
            }
            if !output.status.success() {
                result.push_str(&format!("\nexit code: {}", output.status));
            }
            Ok(result)
        }
        other => anyhow::bail!("Unknown tool: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_to_cedar_file_read() {
        let input = json!({"path": "/tmp/test.txt"});
        let mapping = tool_to_cedar("file_read", &input);
        assert_eq!(mapping.resource_type, "FileSystem");
        assert_eq!(mapping.action, "read");
        assert_eq!(mapping.resource_id, "/tmp/test.txt");
    }

    #[test]
    fn test_tool_to_cedar_file_write() {
        let input = json!({"path": "/tmp/out.txt", "content": "hello"});
        let mapping = tool_to_cedar("file_write", &input);
        assert_eq!(mapping.resource_type, "FileSystem");
        assert_eq!(mapping.action, "write");
        assert_eq!(mapping.resource_id, "/tmp/out.txt");
    }

    #[test]
    fn test_tool_to_cedar_file_list() {
        let input = json!({"path": "/tmp"});
        let mapping = tool_to_cedar("file_list", &input);
        assert_eq!(mapping.resource_type, "FileSystem");
        assert_eq!(mapping.action, "list");
        assert_eq!(mapping.resource_id, "/tmp");
    }

    #[test]
    fn test_tool_to_cedar_shell_execute() {
        let input = json!({"command": "ls -la"});
        let mapping = tool_to_cedar("shell_execute", &input);
        assert_eq!(mapping.resource_type, "Shell");
        assert_eq!(mapping.action, "execute");
        assert_eq!(mapping.resource_id, "ls -la");
    }

    #[test]
    fn test_tool_to_cedar_unknown() {
        let input = json!({});
        let mapping = tool_to_cedar("unknown_tool", &input);
        assert_eq!(mapping.resource_type, "Unknown");
        assert_eq!(mapping.action, "unknown_tool");
    }

    #[test]
    fn test_tool_definitions_count() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 4);
        assert_eq!(defs[0].name, "file_read");
        assert_eq!(defs[1].name, "file_write");
        assert_eq!(defs[2].name, "file_list");
        assert_eq!(defs[3].name, "shell_execute");
    }
}
