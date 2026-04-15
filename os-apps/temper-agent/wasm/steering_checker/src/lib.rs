//! Steering Checker — WASM module for the two-loop steering architecture.
//!
//! When the LLM returns end_turn, this module is triggered (via CheckSteering).
//! It checks for queued steering messages and either:
//! - Injects the first queued message and returns ContinueWithSteering
//! - Returns FinalizeResult if no messages are queued
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
        ctx.log("info", "steering_checker: starting");

        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        // Read steering state
        let steering_messages_json = fields
            .get("steering_messages")
            .and_then(|v| v.as_str())
            .unwrap_or("[]");

        let mut steering_messages: Vec<Value> = serde_json::from_str(steering_messages_json)
            .unwrap_or_default();

        let follow_up_count = fields
            .get("follow_up_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let max_follow_ups: i64 = fields
            .get("max_follow_ups")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let session_file_id = fields
            .get("session_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_leaf_id = fields
            .get("session_leaf_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let temper_api_url = resolve_temper_api_url(&ctx, &fields);
        let tenant = &ctx.tenant;

        // Check if we have steering messages AND haven't hit the follow-up limit
        if !steering_messages.is_empty() && follow_up_count < max_follow_ups {
            // Dequeue the first steering message
            let msg = steering_messages.remove(0);
            let msg_content = msg.get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| msg.as_str().unwrap_or(""));

            ctx.log("info", &format!(
                "steering_checker: injecting steering message ({} remaining, follow_up {}/{})",
                steering_messages.len(), follow_up_count + 1, max_follow_ups
            ));

            // If session tree mode, inject into session tree
            if !session_file_id.is_empty() && !session_leaf_id.is_empty() {
                let session_jsonl = read_session_from_temperfs(&ctx, &temper_api_url, tenant, session_file_id)?;
                let mut tree = SessionTree::from_jsonl(&session_jsonl);

                // Append steering message as a user message in the tree
                let (new_leaf_id, _line) = tree.append_steering_message(
                    session_leaf_id,
                    msg_content,
                    estimate_tokens(msg_content),
                );

                // Write back
                let updated_jsonl = tree.to_jsonl();
                write_session_to_temperfs(&ctx, &temper_api_url, tenant, session_file_id, &updated_jsonl)?;

                // Update steering_messages in entity state (remove dequeued message)
                let updated_queue =
                    serde_json::to_string(&steering_messages).unwrap_or_else(|_| "[]".to_string());
                set_success_result("ContinueWithSteering", &json!({
                    "session_leaf_id": new_leaf_id,
                    "steering_messages": updated_queue,
                }));
            } else {
                // Inline conversation mode (legacy fallback)
                let conversation_json = fields
                    .get("conversation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("[]");
                let mut messages: Vec<Value> = serde_json::from_str(conversation_json).unwrap_or_default();
                messages.push(json!({
                    "role": "user",
                    "content": msg_content,
                }));
                let updated_conversation = serde_json::to_string(&messages).unwrap_or_default();

                set_success_result("ContinueWithSteering", &json!({
                    "conversation": updated_conversation,
                    "steering_messages": serde_json::to_string(&steering_messages)
                        .unwrap_or_else(|_| "[]".to_string()),
                }));
            }
        } else {
            // No steering messages or follow-up limit reached — finalize
            if follow_up_count >= max_follow_ups {
                ctx.log("info", &format!(
                    "steering_checker: follow-up limit reached ({}/{}), finalizing",
                    follow_up_count, max_follow_ups
                ));
            } else {
                ctx.log("info", "steering_checker: no steering messages, finalizing");
            }

            // Extract the result text from the last assistant message
            let result_text = extract_last_result(&ctx, &fields, &temper_api_url, tenant, session_file_id, session_leaf_id)?;

            set_success_result("FinalizeResult", &json!({
                "result": result_text,
                "session_leaf_id": session_leaf_id,
            }));
        }

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

/// Extract the last assistant text from the conversation for the result field.
fn extract_last_result(
    ctx: &Context,
    fields: &Value,
    temper_api_url: &str,
    tenant: &str,
    session_file_id: &str,
    session_leaf_id: &str,
) -> Result<String, String> {
    if !session_file_id.is_empty() && !session_leaf_id.is_empty() {
        let session_jsonl = read_session_from_temperfs(ctx, temper_api_url, tenant, session_file_id)?;
        let tree = SessionTree::from_jsonl(&session_jsonl);
        let messages = tree.build_context(session_leaf_id);

        // Find last assistant message
        for msg in messages.iter().rev() {
            if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                return Ok(extract_text_from_content(msg.get("content")));
            }
        }
        Ok(String::new())
    } else {
        let conversation_json = fields
            .get("conversation")
            .and_then(|v| v.as_str())
            .unwrap_or("[]");
        let messages: Vec<Value> = serde_json::from_str(conversation_json).unwrap_or_default();

        for msg in messages.iter().rev() {
            if msg.get("role").and_then(|v| v.as_str()) == Some("assistant") {
                return Ok(extract_text_from_content(msg.get("content")));
            }
        }
        Ok(String::new())
    }
}

/// Extract text from an assistant message content (handles both string and array formats).
fn extract_text_from_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            arr.iter()
                .filter_map(|block| {
                    if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                        block.get("text").and_then(|v| v.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => String::new(),
    }
}

/// Simple token estimate (4 chars per token).
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}
