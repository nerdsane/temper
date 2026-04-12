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

use serde::{Deserialize, Serialize};
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
    let offset = args
        .get("offset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
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
        return ToolResult::err(format!(
            "edit: `old_string` not found in {file_path}"
        ));
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
    let base = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
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
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
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
