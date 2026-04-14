//! Built-in tool definitions and local executor for the Crucible
//! agent toolset.
//!
//! This module provides:
//! - JSON Schema definitions for the 6 built-in tools (bash, read,
//!   write, edit, glob, grep) matching Anthropic's
//!   `agent_toolset_20260401`.
//! - A synchronous `execute_tool` entry point that dispatches by name
//!   and returns stdout/stderr or an error message.
//!
//! The sidecar (`crucible-chat watch`) calls `execute_tool` directly
//! for Local environments. Modal environments route through the
//! Python tool server (`modal_bridge/server.py`) instead.

use crate::chat::anthropic::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

/// Maximum bytes of output we return from any single tool invocation.
/// Keeps context manageable for the LLM.
const MAX_OUTPUT_BYTES: usize = 100_000;

/// Maximum number of glob results returned.
const MAX_GLOB_RESULTS: usize = 1000;

// ── Public types ────────────────────────────────────────────────

/// The result of executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

impl ToolResult {
    fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self {
            output: msg.into(),
            is_error: true,
        }
    }
}

// ── Tool definitions ───────────────────────────────────────────

/// Return JSON Schema definitions for the 6 built-in tools.
/// These match the `agent_toolset_20260401` surface.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "bash".into(),
            description: "Execute a bash command and return stdout/stderr.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            name: "read".into(),
            description: "Read a file and return its contents with line numbers.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Absolute path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (0-based)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["file_path"]
            }),
        },
        ToolDefinition {
            name: "write".into(),
            description: "Write content to a file, creating directories as needed.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Absolute path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write"
                    }
                },
                "required": ["file_path", "content"]
            }),
        },
        ToolDefinition {
            name: "edit".into(),
            description: "Replace a unique string in a file with a new string.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Absolute path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The text to find (must be unique in the file)"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The replacement text"
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        },
        ToolDefinition {
            name: "glob".into(),
            description: "Find files matching a glob pattern.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern (e.g. '**/*.rs')"
                    },
                    "path": {
                        "type": "string",
                        "description": "Base directory to search in (default: current dir)"
                    }
                },
                "required": ["pattern"]
            }),
        },
        ToolDefinition {
            name: "grep".into(),
            description: "Search file contents for a regex pattern.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for"
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search (default: current dir)"
                    },
                    "include": {
                        "type": "string",
                        "description": "File glob to filter (e.g. '*.rs')"
                    }
                },
                "required": ["pattern"]
            }),
        },
        // ── Memory tools ─────────────────────────────────────────
        ToolDefinition {
            name: "memory_list".into(),
            description: "List memories in the knowledge base, optionally filtered by path prefix. Returns paths and sizes (not content).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory_store_id": {
                        "type": "string",
                        "description": "Memory store ID (e.g. 'ms-kb')"
                    },
                    "path_prefix": {
                        "type": "string",
                        "description": "Filter by path prefix (e.g. '/raw/', '/wiki/concepts/'). Default: list all."
                    }
                },
                "required": ["memory_store_id"]
            }),
        },
        ToolDefinition {
            name: "memory_read".into(),
            description: "Read a specific memory's full content by its entity ID.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "The memory entity ID to read"
                    }
                },
                "required": ["memory_id"]
            }),
        },
        ToolDefinition {
            name: "memory_write".into(),
            description: "Create or update a memory in the knowledge base. If a memory with the same path exists, update it; otherwise create a new one.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory_store_id": {
                        "type": "string",
                        "description": "Memory store ID (e.g. 'ms-kb')"
                    },
                    "path": {
                        "type": "string",
                        "description": "File-like path (e.g. '/raw/my-article.md', '/wiki/concepts/transformers.md')"
                    },
                    "content": {
                        "type": "string",
                        "description": "The text content to store"
                    }
                },
                "required": ["memory_store_id", "path", "content"]
            }),
        },
        ToolDefinition {
            name: "memory_search".into(),
            description: "Search memory contents for a text query. Returns matching memories with content previews.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "memory_store_id": {
                        "type": "string",
                        "description": "Memory store ID (e.g. 'ms-kb')"
                    },
                    "query": {
                        "type": "string",
                        "description": "Text to search for in memory contents"
                    }
                },
                "required": ["memory_store_id", "query"]
            }),
        },
        // ── System tools ──────────────────────────────────────────
        ToolDefinition {
            name: "trigger_session".into(),
            description: "Post a user.message to another session, triggering its agent to process it. Use this to trigger lint runs or delegate tasks to other agents.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "The session ID to trigger (e.g. 's-l' for the lint session)"
                    },
                    "message": {
                        "type": "string",
                        "description": "The message to send to the session"
                    }
                },
                "required": ["session_id", "message"]
            }),
        },
    ]
}

// ── Executor ────────────────────────────────────────────────────

/// Execute a built-in tool by name with the given JSON arguments.
///
/// This is a blocking function — the caller should wrap it in
/// `spawn_blocking` when called from async code.
pub fn execute_tool(name: &str, args: &serde_json::Value) -> ToolResult {
    match name {
        "bash" => tool_bash(args),
        "read" => tool_read(args),
        "write" => tool_write(args),
        "edit" => tool_edit(args),
        "glob" => tool_glob(args),
        "grep" => tool_grep(args),
        _ => ToolResult::err(format!("unknown tool: {name}")),
    }
}

// ── Tool implementations ────────────────────────────────────────

fn tool_bash(args: &serde_json::Value) -> ToolResult {
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return ToolResult::err("bash: missing required parameter `command`");
    };
    match std::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
    {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(&stderr);
            }
            truncate_string(&mut text, MAX_OUTPUT_BYTES);
            if output.status.success() {
                ToolResult::ok(text)
            } else {
                ToolResult {
                    output: text,
                    is_error: !output.status.success(),
                }
            }
        }
        Err(e) => ToolResult::err(format!("bash: failed to execute: {e}")),
    }
}

fn tool_read(args: &serde_json::Value) -> ToolResult {
    let Some(file_path) = args.get("file_path").and_then(|v| v.as_str()) else {
        return ToolResult::err("read: missing required parameter `file_path`");
    };
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => return ToolResult::err(format!("read: {file_path}: {e}")),
    };
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let mut out = String::new();
    let lines: Vec<&str> = content.lines().collect();
    let end = match limit {
        Some(l) => (offset + l).min(lines.len()),
        None => lines.len(),
    };
    for (i, line) in lines.iter().enumerate().skip(offset).take(end - offset) {
        out.push_str(&format!("{}\t{}\n", i + 1, line));
    }
    truncate_string(&mut out, MAX_OUTPUT_BYTES);
    ToolResult::ok(out)
}

fn tool_write(args: &serde_json::Value) -> ToolResult {
    let Some(file_path) = args.get("file_path").and_then(|v| v.as_str()) else {
        return ToolResult::err("write: missing required parameter `file_path`");
    };
    let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
        return ToolResult::err("write: missing required parameter `content`");
    };
    let path = Path::new(file_path);
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("write: failed to create directory: {e}"));
            }
        }
    }
    match std::fs::write(file_path, content) {
        Ok(()) => ToolResult::ok(format!("wrote {} bytes to {file_path}", content.len())),
        Err(e) => ToolResult::err(format!("write: {file_path}: {e}")),
    }
}

fn tool_edit(args: &serde_json::Value) -> ToolResult {
    let Some(file_path) = args.get("file_path").and_then(|v| v.as_str()) else {
        return ToolResult::err("edit: missing required parameter `file_path`");
    };
    let Some(old_string) = args.get("old_string").and_then(|v| v.as_str()) else {
        return ToolResult::err("edit: missing required parameter `old_string`");
    };
    let Some(new_string) = args.get("new_string").and_then(|v| v.as_str()) else {
        return ToolResult::err("edit: missing required parameter `new_string`");
    };
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => return ToolResult::err(format!("edit: {file_path}: {e}")),
    };
    let count = content.matches(old_string).count();
    if count == 0 {
        return ToolResult::err(format!("edit: `old_string` not found in {file_path}"));
    }
    if count > 1 {
        return ToolResult::err(format!(
            "edit: `old_string` found {count} times in {file_path} — must be unique"
        ));
    }
    let new_content = content.replacen(old_string, new_string, 1);
    match std::fs::write(file_path, &new_content) {
        Ok(()) => ToolResult::ok(format!("edited {file_path}")),
        Err(e) => ToolResult::err(format!("edit: failed to write {file_path}: {e}")),
    }
}

fn tool_glob(args: &serde_json::Value) -> ToolResult {
    let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
        return ToolResult::err("glob: missing required parameter `pattern`");
    };
    let base = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let full_pattern = if pattern.starts_with('/') {
        pattern.to_string()
    } else {
        format!("{base}/{pattern}")
    };
    match glob::glob(&full_pattern) {
        Ok(entries) => {
            let mut out = String::new();
            let mut count = 0;
            for entry in entries {
                if count >= MAX_GLOB_RESULTS {
                    out.push_str(&format!("... (truncated at {MAX_GLOB_RESULTS} results)\n"));
                    break;
                }
                match entry {
                    Ok(path) => {
                        out.push_str(&path.display().to_string());
                        out.push('\n');
                        count += 1;
                    }
                    Err(e) => {
                        out.push_str(&format!("(error: {e})\n"));
                        count += 1;
                    }
                }
            }
            if out.is_empty() {
                out.push_str("(no matches)\n");
            }
            ToolResult::ok(out)
        }
        Err(e) => ToolResult::err(format!("glob: invalid pattern: {e}")),
    }
}

fn tool_grep(args: &serde_json::Value) -> ToolResult {
    let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
        return ToolResult::err("grep: missing required parameter `pattern`");
    };
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let include = args.get("include").and_then(|v| v.as_str());

    let mut cmd = std::process::Command::new("grep");
    cmd.arg("-rn").arg(pattern).arg(path);
    if let Some(inc) = include {
        cmd.arg("--include").arg(inc);
    }
    match cmd.output() {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            if text.is_empty() && !output.status.success() {
                // grep returns exit 1 on no matches — not an error
                return ToolResult::ok("(no matches)\n".to_string());
            }
            truncate_string(&mut text, MAX_OUTPUT_BYTES);
            ToolResult::ok(text)
        }
        Err(e) => ToolResult::err(format!("grep: failed to execute: {e}")),
    }
}

// ── Remote tool executor (tool server) ─────────────────────────

/// Execute a tool via the remote Python tool server. Used for Modal
/// environments where tools must run inside a sandbox container.
pub async fn execute_tool_remote(
    tool_server_url: &str,
    name: &str,
    args: &serde_json::Value,
    environment_id: &str,
) -> ToolResult {
    let url = format!("{}/tools/{}", tool_server_url.trim_end_matches('/'), name);
    let body = serde_json::json!({
        "arguments": args,
        "environment_id": environment_id,
    });
    let client = reqwest::Client::new();
    match client
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return ToolResult::err(format!("tool server {status}: {text}"));
            }
            match serde_json::from_str::<ToolResult>(&text) {
                Ok(r) => r,
                Err(_) => ToolResult::ok(text),
            }
        }
        Err(e) => ToolResult::err(format!("tool server unreachable: {e}")),
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn truncate_string(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        s.truncate(max_bytes);
        s.push_str("\n... (output truncated)");
    }
}

// ── Unit tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn bash_runs_echo() {
        let r = execute_tool("bash", &json!({"command": "echo hello"}));
        assert!(!r.is_error, "output: {}", r.output);
        assert!(r.output.contains("hello"), "output: {}", r.output);
    }

    #[test]
    fn bash_missing_command_errors() {
        let r = execute_tool("bash", &json!({}));
        assert!(r.is_error);
        assert!(r.output.contains("missing"), "{}", r.output);
    }

    #[test]
    fn bash_failing_command_sets_is_error() {
        let r = execute_tool("bash", &json!({"command": "false"}));
        assert!(r.is_error);
    }

    #[test]
    fn read_reads_file() {
        let dir = std::env::temp_dir().join("crucible_test_read");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let r = execute_tool("read", &json!({"file_path": path.to_str().unwrap()}));
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("1\tline1"), "{}", r.output);
        assert!(r.output.contains("2\tline2"), "{}", r.output);
        assert!(r.output.contains("3\tline3"), "{}", r.output);
    }

    #[test]
    fn read_with_offset_and_limit() {
        let dir = std::env::temp_dir().join("crucible_test_read_ol");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_ol.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let r = execute_tool(
            "read",
            &json!({"file_path": path.to_str().unwrap(), "offset": 1, "limit": 2}),
        );
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("2\tb"), "{}", r.output);
        assert!(r.output.contains("3\tc"), "{}", r.output);
        assert!(!r.output.contains("1\ta"), "{}", r.output);
        assert!(!r.output.contains("4\td"), "{}", r.output);
    }

    #[test]
    fn read_missing_file_errors() {
        let r = execute_tool(
            "read",
            &json!({"file_path": "/tmp/crucible_nonexistent_file_abc123"}),
        );
        assert!(r.is_error);
    }

    #[test]
    fn write_creates_file() {
        let dir = std::env::temp_dir().join("crucible_test_write");
        let path = dir.join("sub/new.txt");
        let _ = std::fs::remove_file(&path);
        let r = execute_tool(
            "write",
            &json!({"file_path": path.to_str().unwrap(), "content": "hello world"}),
        );
        assert!(!r.is_error, "{}", r.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn edit_replaces_unique_string() {
        let dir = std::env::temp_dir().join("crucible_test_edit");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("edit.txt");
        std::fs::write(&path, "foo bar baz").unwrap();
        let r = execute_tool(
            "edit",
            &json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "bar",
                "new_string": "qux"
            }),
        );
        assert!(!r.is_error, "{}", r.output);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "foo qux baz");
    }

    #[test]
    fn edit_rejects_non_unique() {
        let dir = std::env::temp_dir().join("crucible_test_edit_dup");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dup.txt");
        std::fs::write(&path, "aaa aaa").unwrap();
        let r = execute_tool(
            "edit",
            &json!({
                "file_path": path.to_str().unwrap(),
                "old_string": "aaa",
                "new_string": "bbb"
            }),
        );
        assert!(r.is_error);
        assert!(r.output.contains("2 times"), "{}", r.output);
    }

    #[test]
    fn glob_finds_files() {
        let dir = std::env::temp_dir().join("crucible_test_glob");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::File::create(dir.join("a.txt"));
        let _ = std::fs::File::create(dir.join("b.txt"));
        let r = execute_tool(
            "glob",
            &json!({"pattern": "*.txt", "path": dir.to_str().unwrap()}),
        );
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("a.txt"), "{}", r.output);
        assert!(r.output.contains("b.txt"), "{}", r.output);
    }

    #[test]
    fn grep_finds_pattern() {
        let dir = std::env::temp_dir().join("crucible_test_grep");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("g.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "hello world").unwrap();
        writeln!(f, "goodbye world").unwrap();
        let r = execute_tool(
            "grep",
            &json!({"pattern": "hello", "path": dir.to_str().unwrap()}),
        );
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("hello world"), "{}", r.output);
    }

    #[test]
    fn unknown_tool_errors() {
        let r = execute_tool("nonexistent", &json!({}));
        assert!(r.is_error);
        assert!(r.output.contains("unknown tool"), "{}", r.output);
    }
}
