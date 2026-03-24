//! Context Compactor — WASM module for compacting long agent conversations.
//!
//! When the session tree exceeds the context window (minus reserve_tokens),
//! this module is triggered. It summarizes older messages using an LLM call
//! and replaces them with a compaction entry in the session tree.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use session_tree_lib::SessionTree;
use temper_wasm_sdk::prelude::*;
use wasm_helpers::{read_session_from_temperfs, resolve_temper_api_url, write_session_to_temperfs};

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "context_compactor: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        // Read compaction parameters
        let keep_recent_tokens: usize = fields
            .get("keep_recent_tokens")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(10000);

        let session_file_id = fields
            .get("session_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let session_leaf_id = fields
            .get("session_leaf_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if session_file_id.is_empty() || session_leaf_id.is_empty() {
            return Err("context_compactor: missing session_file_id or session_leaf_id".to_string());
        }

        let temper_api_url = resolve_temper_api_url(&ctx, &fields);
        let tenant = &ctx.tenant;

        // 1. Read session tree from TemperFS
        let session_jsonl = read_session_from_temperfs(&ctx, &temper_api_url, tenant, session_file_id)?;
        let mut tree = SessionTree::from_jsonl(&session_jsonl);

        ctx.log("info", &format!(
            "context_compactor: tree has {} entries, estimating tokens from leaf {}",
            tree.len(), session_leaf_id
        ));

        // 2. Find cut point
        let cut_point = match tree.find_cut_point(session_leaf_id, keep_recent_tokens) {
            Some(cp) => cp,
            None => {
                ctx.log("warn", "context_compactor: no valid cut point found, skipping compaction");
                set_success_result("CompactionComplete", &json!({
                    "session_leaf_id": session_leaf_id,
                    "context_tokens": tree.estimate_tokens(session_leaf_id),
                }));
                return Ok(());
            }
        };

        ctx.log("info", &format!("context_compactor: cut point at entry {}", cut_point));

        // 3. Build compaction prompt from messages being cut
        let messages_to_summarize = tree.build_context(&cut_point);
        if messages_to_summarize.is_empty() {
            ctx.log("warn", "context_compactor: no messages to summarize");
            set_success_result("CompactionComplete", &json!({
                "session_leaf_id": session_leaf_id,
                "context_tokens": tree.estimate_tokens(session_leaf_id),
            }));
            return Ok(());
        }

        let conversation_text = format_messages_for_summary(&messages_to_summarize);

        // 4. Call LLM for structured summary
        let compaction_model = fields
            .get("compaction_model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                fields.get("model").and_then(|v| v.as_str()).unwrap_or("claude-sonnet-4-20250514")
            });

        let api_key = ctx.config.get("api_key").cloned().unwrap_or_default();
        let provider = fields
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic");
        let summary = if provider.eq_ignore_ascii_case("mock") || api_key.trim().is_empty() {
            build_mock_summary(&conversation_text)
        } else {
            call_compaction_llm(&ctx, &api_key, compaction_model, &conversation_text)?
        };

        ctx.log("info", &format!(
            "context_compactor: generated summary ({} chars)",
            summary.len()
        ));

        // 5. Append compaction entry to session tree
        let (compaction_id, _line) = tree.append_compaction(session_leaf_id, &summary, &cut_point);

        // 6. Write updated session tree back to TemperFS
        let updated_jsonl = tree.to_jsonl();
        write_session_to_temperfs(&ctx, &temper_api_url, tenant, session_file_id, &updated_jsonl)?;

        // 7. Return CompactionComplete with new leaf pointing after compaction
        let new_token_estimate = tree.estimate_tokens(&compaction_id);
        set_success_result("CompactionComplete", &json!({
            "session_leaf_id": compaction_id,
            "context_tokens": new_token_estimate,
        }));

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

fn build_mock_summary(conversation_text: &str) -> String {
    let truncated: String = conversation_text.chars().take(600).collect();
    format!(
        "## Goal\nPreserve the active task.\n\n## Constraints & Preferences\nStay within the current workspace and existing agent context.\n\n## Progress\n- Done: Earlier conversation was compacted.\n- In Progress: Continue the active task with the remaining context.\n- Blocked: None.\n\n## Key Decisions\nUse the deterministic mock compaction path when no real model is configured.\n\n## Next Steps\nResume the agent loop after compaction.\n\n## Critical Context\n{}",
        truncated
    )
}

/// Format messages into a text block for the compaction LLM prompt.
fn format_messages_for_summary(messages: &[Value]) -> String {
    let mut text = String::new();
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
        let content = msg.get("content").cloned().unwrap_or(json!(""));
        let content_str = match content {
            Value::String(s) => s,
            Value::Array(arr) => {
                arr.iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            block.get("text").and_then(|v| v.as_str()).map(String::from)
                        } else if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                            Some(format!("[tool_use: {}]", block.get("name").and_then(|v| v.as_str()).unwrap_or("unknown")))
                        } else if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                            let content = block.get("content").and_then(|v| v.as_str()).unwrap_or("...");
                            let truncated = if content.len() > 200 { &content[..200] } else { content };
                            Some(format!("[tool_result: {}]", truncated))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => serde_json::to_string(&content).unwrap_or_default(),
        };
        text.push_str(&format!("## {role}\n{content_str}\n\n"));
    }
    text
}

/// Call the LLM with a compaction-specific system prompt.
fn call_compaction_llm(
    ctx: &Context,
    api_key: &str,
    model: &str,
    conversation_text: &str,
) -> Result<String, String> {
    let system_prompt = "You are a conversation compactor. Summarize the following conversation into a structured summary. Be concise but preserve all important context, decisions, and progress. Output the summary in this exact format:\n\n## Goal\n<what the user is trying to accomplish>\n\n## Constraints & Preferences\n<any stated constraints or preferences>\n\n## Progress\n- Done: <completed items>\n- In Progress: <current work>\n- Blocked: <blockers if any>\n\n## Key Decisions\n<important decisions made>\n\n## Next Steps\n<what should happen next>\n\n## Critical Context\n<anything that must not be forgotten>";

    let body = json!({
        "model": model,
        "max_tokens": 2048,
        "system": system_prompt,
        "messages": [{
            "role": "user",
            "content": format!("Summarize this conversation:\n\n{conversation_text}")
        }]
    });

    let is_oauth = api_key.contains("sk-ant-oat");
    let headers = if is_oauth {
        vec![
            ("authorization".to_string(), format!("Bearer {api_key}")),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]
    } else {
        vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]
    };

    let body_str = serde_json::to_string(&body).map_err(|e| format!("JSON serialize error: {e}"))?;

    let resp = ctx.http_call("POST", "https://api.anthropic.com/v1/messages", &headers, &body_str)?;
    if resp.status != 200 {
        return Err(format!(
            "Compaction LLM call failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(500)]
        ));
    }

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse compaction LLM response: {e}"))?;

    // Extract text from response
    let text = parsed
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(|v| v.as_str()) == Some("text")))
        .and_then(|b| b.get("text").and_then(|v| v.as_str()))
        .unwrap_or("Summary unavailable")
        .to_string();

    Ok(text)
}
