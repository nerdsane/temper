//! LLM Caller — WASM module for calling LLM providers (Anthropic/OpenRouter/Mock).
//!
//! Reads conversation from TemperFS File entity (via $value endpoint) when
//! `conversation_file_id` is set, otherwise falls back to inline entity state.
//! Calls the LLM, appends the response, writes back to TemperFS, and returns
//! a dynamic callback action based on the LLM's response:
//! - `ProcessToolCalls` if the response contains tool_use blocks
//! - `RecordResult` if the response is an end_turn
//! - `Fail` if the turn budget is exceeded
//!
//! Supported modes:
//! - Anthropic API key (`x-api-key`)
//! - Anthropic OAuth token (`Authorization: Bearer sk-ant-oat...`)
//! - OpenRouter API key (`Authorization: Bearer`, OpenAI-compatible schema)
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;
use session_tree_lib::SessionTree;

/// Entry point — NOT using `temper_module!` because we need dynamic callback actions.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "llm_caller: starting");

        // Read entity state
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

        // Check turn budget
        let turn_count = fields
            .get("turn_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let max_turns = fields
            .get("max_turns")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(20);

        if turn_count >= max_turns {
            set_success_result(
                "Fail",
                &json!({ "error_message": format!("turn budget exhausted ({turn_count}/{max_turns})") }),
            );
            return Ok(());
        }

        // Read configuration
        let model = fields
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-20250514");
        let provider_raw = fields
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic");
        let provider = normalize_provider(provider_raw);
        let tools_enabled = fields
            .get("tools_enabled")
            .and_then(|v| v.as_str())
            .unwrap_or("read,write,edit,bash");
        // `system_prompt` is the Anthropic API system parameter (agent persona/behavior).
        // `user_message` is the actual user task from the Provision action.
        let system_prompt = fields
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let user_message = fields
            .get("user_message")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let sandbox_url = fields
            .get("sandbox_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let workdir = fields
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace");

        // Resolve provider credentials from integration config.
        let api_key = if provider == "mock" {
            String::new()
        } else {
            resolve_provider_api_key(&ctx, &provider)?
        };
        if provider != "mock" && is_unresolved_secret_template(&api_key) {
            return Err(format!(
                "provider={provider} api key is unresolved secret template: '{api_key}'. \
set tenant secret and retry"
            ));
        }
        let anthropic_api_url = ctx
            .config
            .get("anthropic_api_url")
            .cloned()
            .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".to_string());
        let openrouter_api_url = ctx
            .config
            .get("openrouter_api_url")
            .cloned()
            .unwrap_or_else(|| "https://openrouter.ai/api/v1/chat/completions".to_string());
        let anthropic_auth_mode = ctx
            .config
            .get("anthropic_auth_mode")
            .cloned()
            .unwrap_or_else(|| "auto".to_string());
        let openrouter_site_url = ctx
            .config
            .get("openrouter_site_url")
            .cloned()
            .unwrap_or_default();
        let openrouter_app_name = ctx
            .config
            .get("openrouter_app_name")
            .cloned()
            .unwrap_or_else(|| "temper-agent".to_string());

        if provider != "mock" && api_key.is_empty() {
            return Err(format!(
                "missing API key for provider={provider}. expected secrets: \
anthropic_api_key (or api_key) for anthropic, openrouter_api_key (or api_key) for openrouter"
            ));
        }

        // TemperFS conversation storage
        let conversation_file_id = fields
            .get("conversation_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let temper_api_url = resolve_temper_api_url(&ctx, &fields);
        let tenant = &ctx.tenant;

        // Session tree fields (Pi architecture)
        let session_file_id = fields
            .get("session_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_leaf_id = fields
            .get("session_leaf_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Soul and steering fields
        let soul_id = fields
            .get("soul_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let max_follow_ups: i64 = fields
            .get("max_follow_ups")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let reserve_tokens: usize = fields
            .get("reserve_tokens")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(20000);

        // Read conversation — from TemperFS if file_id set, else inline state.
        // First turn uses `user_message` (the actual user task from Provision).
        // `system_prompt` is always sent as the Anthropic system parameter, never as a message.
        if user_message.is_empty() {
            return Err("user_message is empty — nothing to send to the LLM".to_string());
        }
        let first_turn_content = user_message;

        // Determine which session storage to use
        let use_session_tree = !session_file_id.is_empty() && !session_leaf_id.is_empty();

        let (mut messages, mut session_tree) = if use_session_tree {
            let session_jsonl = read_session_from_temperfs(&ctx, &temper_api_url, tenant, session_file_id)?;
            if session_jsonl.is_empty() {
                // First turn — tree was just created by sandbox_provisioner but empty
                let tree = SessionTree::from_jsonl(&session_jsonl);
                let msgs = vec![json!({ "role": "user", "content": first_turn_content })];
                (msgs, Some(tree))
            } else {
                let tree = SessionTree::from_jsonl(&session_jsonl);
                let msgs = tree.build_context(session_leaf_id);
                if msgs.is_empty() {
                    (vec![json!({ "role": "user", "content": first_turn_content })], Some(tree))
                } else {
                    (msgs, Some(tree))
                }
            }
        } else if !conversation_file_id.is_empty() {
            // Legacy flat JSON mode
            let msgs = read_conversation_from_temperfs(
                &ctx, &temper_api_url, tenant, conversation_file_id, first_turn_content,
            )?;
            (msgs, None)
        } else {
            // Inline state
            let conversation_json = fields.get("conversation").and_then(|v| v.as_str()).unwrap_or("");
            if conversation_json.is_empty() {
                (vec![json!({ "role": "user", "content": first_turn_content })], None)
            } else {
                (serde_json::from_str(conversation_json).unwrap_or_else(|_| {
                    vec![json!({ "role": "user", "content": first_turn_content })]
                }), None)
            }
        };

        // Build tool definitions based on tools_enabled
        let tools = build_tool_definitions(tools_enabled, sandbox_url, workdir);

        // Check compaction threshold (Pi architecture)
        if use_session_tree {
            if let Some(ref tree) = session_tree {
                let context_tokens = tree.estimate_tokens(session_leaf_id);
                // Model context windows (approximate)
                let context_window: usize = if model.contains("opus") { 200000 }
                    else if model.contains("haiku") { 200000 }
                    else { 200000 }; // sonnet default
                if context_tokens > context_window.saturating_sub(reserve_tokens) {
                    ctx.log("info", &format!(
                        "llm_caller: context_tokens ({}) exceeds threshold ({}), triggering compaction",
                        context_tokens, context_window.saturating_sub(reserve_tokens)
                    ));
                    set_success_result("NeedsCompaction", &json!({
                        "context_tokens": context_tokens,
                        "session_leaf_id": session_leaf_id,
                    }));
                    return Ok(());
                }
            }
        }

        // System prompt assembly (Pi architecture):
        // 1. Soul content (from AgentSoul entity via TemperFS)
        // 2. system_prompt override (from Configure action)
        // 3. Available skills XML block
        // 4. Memory context
        let assembled_system_prompt = assemble_system_prompt(
            &ctx, &temper_api_url, tenant, soul_id, system_prompt,
        )?;

        emit_progress_ignore(
            &ctx,
            json!({
                "kind": "prompt_assembled",
                "message": "system prompt assembled",
                "system_prompt": assembled_system_prompt,
            }),
        );
        let mock_hang = provider == "mock" && mock_plan_requests_hang(&messages);
        if !mock_hang {
            let _ = send_heartbeat(&ctx, &temper_api_url, tenant);
        }
        emit_progress_ignore(
            &ctx,
            json!({
                "kind": "llm_request_started",
                "message": format!("calling provider={provider} model={model}"),
            }),
        );

        // Call LLM API
        let response = match provider.as_str() {
            "mock" => call_mock(&ctx, &messages, &assembled_system_prompt, &tools)?,
            "anthropic" => call_anthropic(
                &ctx,
                &api_key,
                &anthropic_api_url,
                model,
                &assembled_system_prompt,
                &messages,
                &tools,
                &anthropic_auth_mode,
            )?,
            "openrouter" => call_openrouter(
                &ctx,
                &api_key,
                &openrouter_api_url,
                model,
                &assembled_system_prompt,
                &messages,
                &tools,
                &openrouter_site_url,
                &openrouter_app_name,
            )?,
            other => return Err(format!("unsupported LLM provider: {other}")),
        };

        ctx.log(
            "info",
            &format!(
                "llm_caller: got response, stop_reason={}",
                response.stop_reason
            ),
        );
        emit_progress_ignore(
            &ctx,
            json!({
                "kind": "llm_response",
                "message": format!("provider returned stop_reason={}", response.stop_reason),
                "stop_reason": response.stop_reason.clone(),
            }),
        );

        // Append assistant response to conversation
        messages.push(json!({
            "role": "assistant",
            "content": response.content,
        }));

        // Write updated conversation to TemperFS (if file_id set) or pass inline
        let updated_conversation = serde_json::to_string(&messages).unwrap_or_default();

        if !conversation_file_id.is_empty() {
            write_conversation_to_temperfs(
                &ctx,
                &temper_api_url,
                tenant,
                conversation_file_id,
                &updated_conversation,
            )?;
        }

        // For TemperFS mode, don't pass conversation inline (it's in the File)
        let conv_param = if conversation_file_id.is_empty() {
            Some(updated_conversation.clone())
        } else {
            None
        };

        // Route based on stop_reason
        match response.stop_reason.as_str() {
            "tool_use" => {
                // Extract tool_use blocks
                let tool_calls: Vec<Value> = response
                    .content
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter(|block| block.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
                    .cloned()
                    .collect();

                // Update session tree if in tree mode
                let new_leaf = if use_session_tree {
                    if let Some(ref mut tree) = session_tree {
                        let parent = session_leaf_id;
                        let (leaf, _) = tree.append_assistant_message(
                            parent,
                            &response.content,
                            response.output_tokens as usize,
                        );
                        let updated_jsonl = tree.to_jsonl();
                        write_session_to_temperfs(&ctx, &temper_api_url, tenant, session_file_id, &updated_jsonl)?;
                        Some(leaf)
                    } else { None }
                } else { None };

                let tool_calls_json = serde_json::to_string(&tool_calls).unwrap_or_default();
                let mut params = json!({
                    "pending_tool_calls": tool_calls_json,
                    "input_tokens": response.input_tokens,
                    "output_tokens": response.output_tokens,
                });
                if let Some(leaf) = new_leaf {
                    params["session_leaf_id"] = json!(leaf);
                }
                if let Some(ref conv) = conv_param {
                    params["conversation"] = json!(conv);
                }
                set_success_result("ProcessToolCalls", &params);
            }
            "end_turn" | "stop" => {
                let result_text = response
                    .content
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            block.get("text").and_then(|v| v.as_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                // Update session tree if in tree mode
                if use_session_tree {
                    if let Some(ref mut tree) = session_tree {
                        let parent = session_leaf_id;
                        let (new_leaf, _) = tree.append_assistant_message(
                            parent,
                            &response.content,
                            response.output_tokens as usize,
                        );
                        let updated_jsonl = tree.to_jsonl();
                        write_session_to_temperfs(&ctx, &temper_api_url, tenant, session_file_id, &updated_jsonl)?;

                        // Route through steering check if follow-ups are enabled
                        if max_follow_ups > 0 {
                            set_success_result("CheckSteering", &json!({
                                "result": result_text,
                                "session_leaf_id": new_leaf,
                                "input_tokens": response.input_tokens,
                                "output_tokens": response.output_tokens,
                            }));
                        } else {
                            let params = json!({
                                "result": result_text,
                                "session_leaf_id": new_leaf,
                                "input_tokens": response.input_tokens,
                                "output_tokens": response.output_tokens,
                            });
                            set_success_result("RecordResult", &params);
                        }
                    }
                } else {
                    // Legacy mode — direct to RecordResult
                    let mut params = json!({
                        "result": result_text,
                        "input_tokens": response.input_tokens,
                        "output_tokens": response.output_tokens,
                    });
                    if let Some(ref conv) = conv_param {
                        params["conversation"] = json!(conv);
                    }
                    set_success_result("RecordResult", &params);
                }
            }
            other => {
                set_success_result(
                    "Fail",
                    &json!({ "error_message": format!("unexpected stop_reason: {other}") }),
                );
            }
        }

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

/// Parsed LLM response.
struct LlmResponse {
    content: Value,
    stop_reason: String,
    input_tokens: i64,
    output_tokens: i64,
}

fn normalize_provider(provider: &str) -> String {
    let norm = provider.trim().to_ascii_lowercase();
    if norm == "open_router" {
        "openrouter".to_string()
    } else {
        norm
    }
}

fn is_unresolved_secret_template(value: &str) -> bool {
    value.contains("{secret:")
}

fn first_non_empty(values: &[Option<String>]) -> String {
    for v in values.iter().flatten() {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    String::new()
}

fn resolve_provider_api_key(ctx: &Context, provider: &str) -> Result<String, String> {
    let key = match provider {
        "anthropic" => first_non_empty(&[
            ctx.config.get("anthropic_api_key").cloned(),
            ctx.config.get("api_key").cloned(),
        ]),
        "openrouter" => first_non_empty(&[
            ctx.config.get("openrouter_api_key").cloned(),
            ctx.config.get("api_key").cloned(),
        ]),
        other => return Err(format!("unsupported LLM provider: {other}")),
    };
    Ok(key)
}

fn call_mock(
    ctx: &Context,
    messages: &[Value],
    assembled_system_prompt: &str,
    _tools: &[Value],
) -> Result<LlmResponse, String> {
    ctx.log("info", "llm_caller: using deterministic mock provider");
    if mock_plan_requests_hang(messages) {
        simulate_mock_hang(ctx)?;
        return Err("mock hang scenario finished without heartbeat".to_string());
    }

    let assistant_turns = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .count();

    if let Some(step) = extract_mock_plan(messages)
        .and_then(|steps| steps.get(assistant_turns).cloned())
    {
        return build_mock_step_response(messages, assembled_system_prompt, assistant_turns, &step);
    }

    let latest_user = latest_user_text(messages);
    let text = resolve_mock_template(
        latest_user
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("mock provider completed"),
        assembled_system_prompt,
        latest_user.as_deref().unwrap_or(""),
    );
    Ok(mock_text_response(messages, text))
}

fn extract_mock_signal_summary(messages: &[Value]) -> Result<Value, String> {
    for message in messages.iter().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let raw = message
            .get("content")
            .map(stringify_content)
            .unwrap_or_default();
        if raw.trim().is_empty() {
            continue;
        }
        return serde_json::from_str::<Value>(&raw)
            .map_err(|e| format!("mock provider expected JSON signal summary: {e}"));
    }
    Err("mock provider could not find a user JSON payload".to_string())
}

fn build_mock_analysis(signal_summary: &Value) -> Value {
    let legacy_unmet_intents = signal_summary
        .get("legacy_unmet_intents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let intent_candidates = signal_summary
        .get("intent_evidence")
        .and_then(|value| value.get("intent_candidates"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let workaround_patterns = signal_summary
        .get("intent_evidence")
        .and_then(|value| value.get("workaround_patterns"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let abandonment_patterns = signal_summary
        .get("intent_evidence")
        .and_then(|value| value.get("abandonment_patterns"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let policy_suggestions = signal_summary
        .get("policy_suggestions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let feature_requests = signal_summary
        .get("feature_requests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let agents = signal_summary
        .get("agents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut existing_keys = collect_existing_dedupe_keys(signal_summary);
    let mut findings = Vec::<Value>::new();

    for candidate in intent_candidates.iter().take(4) {
        let issue_title = lookup_string(
            candidate,
            &[
                "recommended_issue_title",
                "intent_title",
                "title",
                "intent_statement",
            ],
        )
        .unwrap_or_else(|| "Enable unmet intent".to_string());
        let symptom_title = lookup_string(candidate, &["symptom_title", "problem_statement"])
            .unwrap_or_else(|| "Observed symptom".to_string());
        let intent_title = lookup_string(candidate, &["intent_title", "recommended_issue_title"])
            .unwrap_or_else(|| issue_title.clone());
        let intent = lookup_string(candidate, &["intent_statement", "sample_intent"])
            .unwrap_or_else(|| intent_title.clone());
        let recommendation = lookup_string(candidate, &["recommendation"])
            .unwrap_or_else(|| format!("Add direct support for {intent_title}."));
        let volume = lookup_u64(
            candidate,
            &["failure_count", "workaround_count", "abandonment_count", "total_count"],
        )
        .unwrap_or(1);
        let success_rate = lookup_f64(candidate, &["success_rate"]).unwrap_or(0.0);
        let trend = if lookup_u64(candidate, &["abandonment_count"]).unwrap_or(0) > 0 {
            "growing"
        } else {
            "stable"
        };
        let kind = lookup_string(candidate, &["suggested_kind"]).unwrap_or_else(|| {
            if lookup_u64(candidate, &["workaround_count"]).unwrap_or(0) > 0 {
                "workaround".to_string()
            } else {
                "missing_capability".to_string()
            }
        });
        let dedupe_key = lookup_string(candidate, &["intent_key"]).unwrap_or_else(|| {
            normalize_key(&format!("intent:{intent_title}:{issue_title}"))
        });
        if existing_keys.contains(&normalize_key(&issue_title))
            || existing_keys.contains(&normalize_key(&intent_title))
            || existing_keys.contains(&dedupe_key)
        {
            continue;
        }
        existing_keys.insert(normalize_key(&issue_title));
        existing_keys.insert(normalize_key(&intent_title));
        existing_keys.insert(dedupe_key.clone());

        findings.push(json!({
            "kind": kind,
            "symptom_title": symptom_title,
            "intent_title": intent_title.clone(),
            "recommended_issue_title": issue_title.clone(),
            "title": issue_title,
            "intent": intent,
            "recommendation": recommendation,
            "priority_score": lookup_f64(candidate, &["priority_score"]).unwrap_or((0.50_f64 + (volume as f64 / 25.0)).min(0.9)),
            "volume": volume,
            "success_rate": success_rate,
            "trend": trend,
            "requires_spec_change": lookup_string(candidate, &["suggested_kind"]).unwrap_or_default() != "governance_gap",
            "problem_statement": lookup_string(candidate, &["problem_statement"])
                .unwrap_or_else(|| format!("{intent_title} is not directly supported today.")),
            "root_cause": format!("Recent trajectory evidence for '{}' clusters around '{}'.", intent_title, symptom_title),
            "spec_diff": recommendation,
            "acceptance_criteria": [
                format!("Users or agents can complete '{}' directly.", intent_title),
                "Observed failure/workaround patterns drop after the change."
            ],
            "dedupe_key": dedupe_key,
            "evidence": candidate.clone(),
        }));
    }

    for unmet in legacy_unmet_intents.iter().take(2) {
        let entity_type = lookup_string(unmet, &["entity_type"]).unwrap_or_else(|| "UnknownEntity".to_string());
        let action = lookup_string(unmet, &["action"]).unwrap_or_else(|| "UnknownAction".to_string());
        let error_pattern = lookup_string(unmet, &["error_pattern"]).unwrap_or_else(|| "UnknownError".to_string());
        let failure_count = lookup_u64(unmet, &["failure_count", "count"]).unwrap_or(1);
        let recommendation = lookup_string(unmet, &["recommendation"])
            .unwrap_or_else(|| format!("Add or repair {entity_type}.{action} handling."));
        let intent = lookup_string(unmet, &["sample_intent"])
            .unwrap_or_else(|| format!("Complete {action} on {entity_type}"));
        let intent_title = format!("Enable {}", humanize_issue_focus(&intent));
        let title = intent_title.clone();
        let dedupe_key = normalize_key(&format!("unmet:{entity_type}:{action}:{error_pattern}"));
        if existing_keys.contains(&normalize_key(&title)) || existing_keys.contains(&dedupe_key) {
            continue;
        }
        existing_keys.insert(normalize_key(&title));
        existing_keys.insert(dedupe_key.clone());

        let priority = (0.55_f64 + (failure_count as f64 / 20.0)).min(0.95);
        findings.push(json!({
            "kind": "missing_capability",
            "symptom_title": format!("{action} hits {error_pattern} on {entity_type}"),
            "intent_title": intent_title,
            "recommended_issue_title": title.clone(),
            "title": title,
            "intent": intent,
            "recommendation": recommendation,
            "priority_score": priority,
            "volume": failure_count,
            "success_rate": 0.0,
            "trend": "growing",
            "requires_spec_change": true,
            "problem_statement": format!("Users are trying to {action} on {entity_type}, but the capability is currently blocked by {error_pattern}."),
            "root_cause": format!("The current spec and implementation do not cover the requested {entity_type} workflow."),
            "spec_diff": format!("Add or extend {entity_type} support so agents can execute {action} without {error_pattern}."),
            "acceptance_criteria": [
                format!("Agents can execute {action} on {entity_type} without the current {error_pattern} failure."),
                "Observe metrics show the unmet-intent failure count drops after deployment."
            ],
            "dedupe_key": dedupe_key,
            "evidence": unmet.clone(),
        }));
    }

    for suggestion in policy_suggestions.iter().take(2) {
        let description = lookup_string(suggestion, &["description"])
            .unwrap_or_else(|| "Relax an over-restrictive policy path".to_string());
        let denial_count = lookup_u64(suggestion, &["denial_count", "count"]).unwrap_or(1);
        let title = if description.is_empty() {
            "Resolve repeated policy denials".to_string()
        } else {
            description.clone()
        };
        let dedupe_key = normalize_key(&format!("policy:{title}"));
        if existing_keys.contains(&normalize_key(&title)) || existing_keys.contains(&dedupe_key) {
            continue;
        }
        existing_keys.insert(normalize_key(&title));
        existing_keys.insert(dedupe_key.clone());

        findings.push(json!({
            "kind": "governance_gap",
            "symptom_title": title.clone(),
            "intent_title": "Enable direct issue workflow progression for worker agents",
            "recommended_issue_title": "Enable worker agents to move issues into todo",
            "title": "Enable worker agents to move issues into todo",
            "intent": "Complete the blocked workflow without repeated Cedar denials.",
            "recommendation": description,
            "priority_score": (0.45_f64 + (denial_count as f64 / 25.0)).min(0.85),
            "volume": denial_count,
            "success_rate": 0.0,
            "trend": "stable",
            "requires_spec_change": false,
            "problem_statement": "The intended issue-workflow outcome is blocked by repeated policy denials on the same transition.",
            "root_cause": "Authorization rules are narrower than actual usage patterns.",
            "spec_diff": "Adjust Cedar policy or app capabilities to align authorized behavior with real demand.",
            "acceptance_criteria": [
                "The repeated denial pattern is no longer observed for the intended workflow.",
                "Any widened policy remains scoped to the minimum required principals and resources."
            ],
            "dedupe_key": dedupe_key,
            "evidence": suggestion.clone(),
        }));
    }

    if findings.is_empty() {
        for feature in feature_requests.iter().take(1) {
            let description = lookup_string(feature, &["description"])
                .unwrap_or_else(|| "Address a repeated feature request".to_string());
            let frequency = lookup_u64(feature, &["frequency", "count"]).unwrap_or(1);
            let title = format!("Enable {}", humanize_issue_focus(&description));
            let dedupe_key = normalize_key(&format!("feature:{description}"));
            if existing_keys.contains(&normalize_key(&title)) || existing_keys.contains(&dedupe_key) {
                continue;
            }
            findings.push(json!({
                "kind": "workaround",
                "symptom_title": format!("Feature requests keep accumulating for {description}"),
                "intent_title": title.clone(),
                "recommended_issue_title": title.clone(),
                "title": title,
                "intent": description,
                "recommendation": description,
                "priority_score": (0.40_f64 + (frequency as f64 / 25.0)).min(0.8),
                "volume": frequency,
                "success_rate": 0.2,
                "trend": "stable",
                "requires_spec_change": false,
                "problem_statement": "Users are repeatedly asking for the same outcome outside the supported path.",
                "root_cause": "The feature is not part of the current product surface.",
                "spec_diff": "Review whether the capability should graduate into the main spec.",
                "acceptance_criteria": [
                    "The requested capability is either planned explicitly or closed with a documented rationale.",
                    "Duplicate feature requests no longer accumulate without a disposition."
                ],
                "dedupe_key": dedupe_key,
                "evidence": feature.clone(),
            }));
        }
    }

    if findings.is_empty() {
        for agent in agents.iter().take(1) {
            let agent_id = lookup_string(agent, &["agent_id", "id"]).unwrap_or_else(|| "unknown-agent".to_string());
            let total_actions = lookup_u64(agent, &["total_actions"]).unwrap_or(0);
            let success_rate = lookup_f64(agent, &["success_rate"]).unwrap_or(0.0);
            if total_actions == 0 {
                continue;
            }
            let title = format!("Reduce workflow friction for {agent_id}");
            let dedupe_key = normalize_key(&format!("friction:{agent_id}"));
            if existing_keys.contains(&normalize_key(&title)) || existing_keys.contains(&dedupe_key) {
                continue;
            }
            findings.push(json!({
                "kind": "friction",
                "symptom_title": format!("{agent_id} needs too many steps to complete common work"),
                "intent_title": title.clone(),
                "recommended_issue_title": title.clone(),
                "title": title,
                "intent": format!("Let {agent_id} complete common tasks with fewer steps."),
                "recommendation": "Review the top repeated workflow and collapse the multi-step sequence into a higher-level capability.",
                "priority_score": 0.35,
                "volume": total_actions,
                "success_rate": success_rate,
                "trend": "stable",
                "requires_spec_change": false,
                "problem_statement": "A high-volume workflow still requires too many manual steps.",
                "root_cause": "The current API surface is low-level relative to real usage patterns.",
                "spec_diff": "Consider a composed action that captures the common workflow directly.",
                "acceptance_criteria": [
                    "The workflow requires fewer state transitions than before.",
                    "Agent success rate stays stable or improves after the simplification."
                ],
                "dedupe_key": dedupe_key,
                "evidence": agent.clone(),
            }));
        }
    }

    let tenant = lookup_string(signal_summary, &["tenant"]).unwrap_or_else(|| "unknown-tenant".to_string());
    let summary = format!(
        "Mock evolution analysis for tenant {tenant}: {} intent candidates, {} workaround patterns, {} abandonment patterns, {} policy suggestions, {} feature requests, {} agent summaries, {} findings emitted.",
        intent_candidates.len(),
        workaround_patterns.len(),
        abandonment_patterns.len(),
        policy_suggestions.len(),
        feature_requests.len(),
        agents.len(),
        findings.len()
    );

    json!({
        "summary": summary,
        "findings": findings,
    })
}

fn collect_existing_dedupe_keys(signal_summary: &Value) -> std::collections::BTreeSet<String> {
    let mut keys = std::collections::BTreeSet::new();

    if let Some(issues) = signal_summary.get("issues").and_then(Value::as_array) {
        for issue in issues {
            if let Some(title) = lookup_string(issue, &["Title", "title", "name"]) {
                keys.insert(normalize_key(&title));
            }
            if let Some(dedupe_key) = lookup_string(issue, &["DedupeKey", "dedupe_key"]) {
                keys.insert(normalize_key(&dedupe_key));
            }
        }
    }

    if let Some(records) = signal_summary.get("recent_records").and_then(Value::as_array) {
        for record in records {
            if let Some(title) = lookup_string(record, &["title", "description", "problem_statement"]) {
                keys.insert(normalize_key(&title));
            }
        }
    }

    keys
}

fn lookup_string(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(candidate) = value.get(*key) else {
            continue;
        };
        if let Some(text) = candidate.as_str() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        } else if candidate.is_number() || candidate.is_boolean() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn lookup_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        let Some(candidate) = value.get(*key) else {
            continue;
        };
        if let Some(number) = candidate.as_u64() {
            return Some(number);
        }
        if let Some(number) = candidate.as_i64() {
            if number >= 0 {
                return Some(number as u64);
            }
        }
        if let Some(text) = candidate.as_str() {
            if let Ok(number) = text.trim().parse::<u64>() {
                return Some(number);
            }
        }
    }
    None
}

fn lookup_f64(value: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        let Some(candidate) = value.get(*key) else {
            continue;
        };
        if let Some(number) = candidate.as_f64() {
            return Some(number);
        }
        if let Some(text) = candidate.as_str() {
            if let Ok(number) = text.trim().parse::<f64>() {
                return Some(number);
            }
        }
    }
    None
}

fn normalize_key(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn humanize_issue_focus(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "unmet intent".to_string();
    }
    trimmed
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_lowercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn detect_anthropic_oauth_mode(api_key: &str, auth_mode: &str) -> bool {
    match auth_mode.trim().to_ascii_lowercase().as_str() {
        "oauth" => true,
        "api_key" => false,
        _ => api_key.starts_with("sk-ant-oat"),
    }
}

/// Call Anthropic Messages API.
fn call_anthropic(
    ctx: &Context,
    api_key: &str,
    api_url: &str,
    model: &str,
    system_prompt: &str,
    messages: &[Value],
    tools: &[Value],
    anthropic_auth_mode: &str,
) -> Result<LlmResponse, String> {
    // Detect OAuth token (sk-ant-oat-*) vs standard API key
    let is_oauth = detect_anthropic_oauth_mode(api_key, anthropic_auth_mode);

    // OAuth tokens enforce a fixed system prompt when tools are present.
    // Custom system instructions are prepended to the first user message instead.
    let (effective_system, effective_messages) = if is_oauth {
        let oauth_system = "You are Claude Code, Anthropic's official CLI for Claude.".to_string();
        let mut msgs = messages.to_vec();
        if !system_prompt.is_empty() {
            if let Some(first) = msgs.first_mut() {
                if let Some(content) = first.get("content").and_then(|v| v.as_str()) {
                    let combined = format!("[System instructions: {system_prompt}]\n\n{content}");
                    first["content"] = json!(combined);
                }
            }
        }
        (oauth_system, msgs)
    } else {
        (system_prompt.to_string(), messages.to_vec())
    };

    let mut body = json!({
        "model": model,
        "max_tokens": 4096,
        "messages": effective_messages,
    });

    if !effective_system.is_empty() {
        body["system"] = json!(effective_system);
    }

    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }

    let body_str =
        serde_json::to_string(&body).map_err(|e| format!("JSON serialize error: {e}"))?;

    ctx.log(
        "info",
        &format!(
            "llm_caller: calling Anthropic API, model={model}, oauth={is_oauth}, messages={}, url={api_url}",
            messages.len(),
        ),
    );

    // Build auth headers — OAuth tokens use Bearer + beta header
    let headers = if is_oauth {
        vec![
            ("authorization".to_string(), format!("Bearer {api_key}")),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            (
                "anthropic-beta".to_string(),
                "oauth-2025-04-20,computer-use-2025-01-24".to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
            ("user-agent".to_string(), "claude-cli/2.1.75".to_string()),
            ("x-app".to_string(), "cli".to_string()),
        ]
    } else {
        vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]
    };

    // Retry on transient API errors (500, 529, and 400 with vague "Error" message)
    let mut last_err = String::new();
    let mut resp = None;
    for attempt in 0..5 {
        if attempt > 0 {
            ctx.log(
                "warn",
                &format!(
                    "llm_caller: retrying (attempt {}/5), last error: {last_err}",
                    attempt + 1
                ),
            );
        }
        match ctx.http_call("POST", api_url, &headers, &body_str) {
            Ok(r) if r.status == 200 => {
                resp = Some(r);
                break;
            }
            Ok(r) if r.status == 500 || r.status == 529 => {
                last_err = format!("HTTP {}: {}", r.status, &r.body[..r.body.len().min(200)]);
                continue;
            }
            Ok(r) if r.status == 400 && r.body.contains("\"message\":\"Error\"") => {
                // Transient 400 with vague error message — retry
                last_err = format!("HTTP 400 (transient): {}", &r.body[..r.body.len().min(200)]);
                continue;
            }
            Ok(r) => {
                return Err(format!(
                    "Anthropic API returned {}: {}",
                    r.status,
                    &r.body[..r.body.len().min(500)]
                ));
            }
            Err(e) => {
                last_err = e;
                continue;
            }
        }
    }
    let resp = resp.ok_or_else(|| format!("Anthropic API failed after 5 attempts: {last_err}"))?;

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse LLM response: {e}"))?;

    let stop_reason = parsed
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn")
        .to_string();

    let content = parsed.get("content").cloned().unwrap_or(json!([]));

    // Extract token usage from Anthropic response
    let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    ctx.log(
        "info",
        &format!("llm_caller: usage: input={input_tokens}, output={output_tokens}"),
    );

    Ok(LlmResponse {
        content,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

/// Call OpenRouter Chat Completions API (OpenAI-compatible schema).
fn call_openrouter(
    ctx: &Context,
    api_key: &str,
    api_url: &str,
    model: &str,
    system_prompt: &str,
    messages: &[Value],
    tools: &[Value],
    site_url: &str,
    app_name: &str,
) -> Result<LlmResponse, String> {
    let mut or_messages = Vec::<Value>::new();
    if !system_prompt.is_empty() {
        or_messages.push(json!({
            "role": "system",
            "content": system_prompt,
        }));
    }
    or_messages.extend(convert_messages_to_openrouter(messages));

    let openai_tools = convert_tools_to_openrouter(tools);
    let mut body = json!({
        "model": model,
        "messages": or_messages,
        "max_tokens": 4096,
    });
    if !openai_tools.is_empty() {
        body["tools"] = json!(openai_tools);
        body["tool_choice"] = json!("auto");
    }

    let body_str =
        serde_json::to_string(&body).map_err(|e| format!("JSON serialize error: {e}"))?;

    let mut headers = vec![
        ("authorization".to_string(), format!("Bearer {api_key}")),
        ("content-type".to_string(), "application/json".to_string()),
    ];
    if !site_url.trim().is_empty() {
        headers.push(("HTTP-Referer".to_string(), site_url.trim().to_string()));
    }
    if !app_name.trim().is_empty() {
        headers.push(("X-Title".to_string(), app_name.trim().to_string()));
    }

    ctx.log(
        "info",
        &format!(
            "llm_caller: calling OpenRouter API, model={model}, messages={}, url={api_url}",
            messages.len(),
        ),
    );

    let mut last_err = String::new();
    let mut resp = None;
    for attempt in 0..5 {
        if attempt > 0 {
            ctx.log(
                "warn",
                &format!(
                    "llm_caller: openrouter retry (attempt {}/5), last error: {last_err}",
                    attempt + 1
                ),
            );
        }
        match ctx.http_call("POST", api_url, &headers, &body_str) {
            Ok(r) if r.status == 200 => {
                resp = Some(r);
                break;
            }
            Ok(r) if matches!(r.status, 429 | 500 | 502 | 503 | 504) => {
                last_err = format!("HTTP {}: {}", r.status, &r.body[..r.body.len().min(200)]);
                continue;
            }
            Ok(r) => {
                return Err(format!(
                    "OpenRouter API returned {}: {}",
                    r.status,
                    &r.body[..r.body.len().min(500)]
                ));
            }
            Err(e) => {
                last_err = e;
                continue;
            }
        }
    }
    let resp = resp.ok_or_else(|| format!("OpenRouter API failed after 5 attempts: {last_err}"))?;

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse OpenRouter response: {e}"))?;
    let choice = parsed
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));

    let mut content_blocks = Vec::<Value>::new();
    let text = extract_openrouter_text(&message);
    if !text.is_empty() {
        content_blocks.push(json!({
            "type": "text",
            "text": text,
        }));
    }

    let mut has_tool_calls = false;
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (idx, tc) in tool_calls.iter().enumerate() {
            let fn_name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool");
            let call_id = tc
                .get("id")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("or_tool_{}", idx + 1));
            let args_str = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input = serde_json::from_str::<Value>(args_str).unwrap_or(json!({}));

            content_blocks.push(json!({
                "type": "tool_use",
                "id": call_id,
                "name": fn_name,
                "input": input,
            }));
            has_tool_calls = true;
        }
    }

    let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
    let input_tokens = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_i64())
        .or_else(|| usage.get("input_tokens").and_then(|v| v.as_i64()))
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .or_else(|| usage.get("output_tokens").and_then(|v| v.as_i64()))
        .unwrap_or(0);

    let stop_reason = if has_tool_calls {
        "tool_use".to_string()
    } else {
        "end_turn".to_string()
    };

    Ok(LlmResponse {
        content: Value::Array(content_blocks),
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

fn extract_openrouter_text(message: &Value) -> String {
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(arr) = message.get("content").and_then(Value::as_array) {
        let mut chunks = Vec::<String>::new();
        for item in arr {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                chunks.push(text.to_string());
            } else if let Some(text) = item.get("content").and_then(Value::as_str) {
                chunks.push(text.to_string());
            }
        }
        return chunks.join("\n");
    }
    String::new()
}

fn stringify_content(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        s.to_string()
    } else {
        value.to_string()
    }
}

fn emit_progress_ignore(ctx: &Context, payload: Value) {
    let _ = ctx.emit_progress(&payload);
}

fn send_heartbeat(ctx: &Context, temper_api_url: &str, tenant: &str) -> Result<(), String> {
    let url = format!(
        "{temper_api_url}/tdata/TemperAgents('{}')/Temper.Agent.TemperAgent.Heartbeat",
        ctx.entity_id
    );
    let body = json!({ "last_heartbeat_at": "alive" });
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];
    let _ = ctx.http_call("POST", &url, &headers, &body.to_string())?;
    Ok(())
}

fn mock_plan_requests_hang(messages: &[Value]) -> bool {
    if let Some(steps) = extract_mock_plan(messages)
        && steps
            .iter()
            .any(|step| step.get("mode").and_then(Value::as_str) == Some("hang"))
    {
        return true;
    }
    latest_user_text(messages)
        .map(|text| text.contains("[mock-hang]"))
        .unwrap_or(false)
}

fn simulate_mock_hang(ctx: &Context) -> Result<(), String> {
    let base_url = temper_api_url(ctx);
    let url = format!(
        "{base_url}/observe/entities/{}/{}/wait?statuses=__never__&timeout_ms=10000&poll_ms=250",
        ctx.entity_type, ctx.entity_id
    );
    let headers = vec![
        ("x-tenant-id".to_string(), ctx.tenant.clone()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    let _ = ctx.http_call("GET", &url, &headers, "")?;
    Ok(())
}

fn extract_mock_plan(messages: &[Value]) -> Option<Vec<Value>> {
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let raw = stringify_content(message.get("content").unwrap_or(&Value::Null));
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(steps) = parsed.get("steps").and_then(Value::as_array) {
            return Some(steps.clone());
        }
        if let Some(steps) = parsed
            .get("mock_plan")
            .and_then(|value| value.get("steps"))
            .and_then(Value::as_array)
        {
            return Some(steps.clone());
        }
    }
    None
}

fn build_mock_step_response(
    messages: &[Value],
    assembled_system_prompt: &str,
    assistant_turns: usize,
    step: &Value,
) -> Result<LlmResponse, String> {
    if step.get("mode").and_then(Value::as_str) == Some("hang") {
        return Ok(mock_text_response(messages, "mock hang placeholder".to_string()));
    }

    let mut content = Vec::<Value>::new();
    if let Some(text) = step.get("text").and_then(Value::as_str) {
        let resolved = resolve_mock_template(
            text,
            assembled_system_prompt,
            latest_user_text(messages).as_deref().unwrap_or(""),
        );
        if !resolved.is_empty() {
            content.push(json!({ "type": "text", "text": resolved }));
        }
    }

    if let Some(tool_calls) = step.get("tool_calls").and_then(Value::as_array) {
        for (index, tool_call) in tool_calls.iter().enumerate() {
            let name = tool_call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown_tool");
            let input = tool_call.get("input").cloned().unwrap_or_else(|| json!({}));
            let id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("mock-tool-{assistant_turns}-{index}"));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }

    if content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        let output_len = serde_json::to_string(&content).unwrap_or_default().len() as i64;
        return Ok(LlmResponse {
            content: Value::Array(content),
            stop_reason: "tool_use".to_string(),
            input_tokens: estimate_message_tokens(messages),
            output_tokens: output_len,
        });
    }

    let final_text = step
        .get("final_text")
        .or_else(|| step.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("mock provider completed");
    Ok(mock_text_response(
        messages,
        resolve_mock_template(
            final_text,
            assembled_system_prompt,
            latest_user_text(messages).as_deref().unwrap_or(""),
        ),
    ))
}

fn mock_text_response(messages: &[Value], text: String) -> LlmResponse {
    LlmResponse {
        content: json!([{ "type": "text", "text": text.clone() }]),
        stop_reason: "end_turn".to_string(),
        input_tokens: estimate_message_tokens(messages),
        output_tokens: text.len() as i64,
    }
}

fn estimate_message_tokens(messages: &[Value]) -> i64 {
    messages
        .iter()
        .map(|message| {
            message
                .get("content")
                .map(stringify_content)
                .unwrap_or_default()
                .len() as i64
        })
        .sum::<i64>()
}

fn latest_user_text(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|message| stringify_content(message.get("content").unwrap_or(&Value::Null)))
}

fn resolve_mock_template(template: &str, assembled_system_prompt: &str, latest_user: &str) -> String {
    let mut text = template.to_string();
    text = text.replace("{{latest_user}}", latest_user);
    text = text.replace("{{memory_block}}", &extract_tag_block(assembled_system_prompt, "agent_memory"));
    text = text.replace(
        "{{memory_keys}}",
        &extract_memory_keys(assembled_system_prompt).join(", "),
    );
    text = text.replace(
        "{{memory_count}}",
        &extract_memory_keys(assembled_system_prompt).len().to_string(),
    );
    text = text.replace("{{skills_block}}", &extract_tag_block(assembled_system_prompt, "available_skills"));
    text
}

fn extract_tag_block(text: &str, tag: &str) -> String {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let Some(start) = text.find(&start_tag) else {
        return String::new();
    };
    let Some(end) = text[start..].find(&end_tag) else {
        return String::new();
    };
    text[start..start + end + end_tag.len()].to_string()
}

fn extract_memory_keys(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let marker = "key=\"";
            let start = line.find(marker)? + marker.len();
            let rest = &line[start..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

fn convert_messages_to_openrouter(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::<Value>::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = msg.get("content").cloned().unwrap_or(json!(""));

        match content {
            Value::String(text) => {
                out.push(json!({
                    "role": role,
                    "content": text,
                }));
            }
            Value::Array(blocks) => {
                if role == "assistant" {
                    let mut text_chunks = Vec::<String>::new();
                    let mut tool_calls = Vec::<Value>::new();
                    for (idx, block) in blocks.iter().enumerate() {
                        match block.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    text_chunks.push(t.to_string());
                                }
                            }
                            "tool_use" => {
                                let id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| format!("tool_{}", idx + 1));
                                let name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown_tool");
                                let input = block.get("input").cloned().unwrap_or(json!({}));
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": input.to_string(),
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }

                    let mut assistant = json!({
                        "role": "assistant",
                        "content": text_chunks.join("\n"),
                    });
                    if !tool_calls.is_empty() {
                        assistant["tool_calls"] = json!(tool_calls);
                    }
                    out.push(assistant);
                } else if role == "user" {
                    let mut user_text = Vec::<String>::new();
                    for block in &blocks {
                        match block.get("type").and_then(Value::as_str).unwrap_or("") {
                            "tool_result" => {
                                let tool_call_id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown_tool_call");
                                let content = stringify_content(
                                    block.get("content").unwrap_or(&Value::String(String::new())),
                                );
                                out.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tool_call_id,
                                    "content": content,
                                }));
                            }
                            "text" => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    user_text.push(t.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                    if !user_text.is_empty() {
                        out.push(json!({
                            "role": "user",
                            "content": user_text.join("\n"),
                        }));
                    }
                } else {
                    out.push(json!({
                        "role": role,
                        "content": Value::Array(blocks),
                    }));
                }
            }
            other => {
                out.push(json!({
                    "role": role,
                    "content": other,
                }));
            }
        }
    }
    out
}

fn convert_tools_to_openrouter(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::<Value>::new();
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let parameters = tool
            .get("input_schema")
            .cloned()
            .unwrap_or(json!({"type": "object", "properties": {}}));
        out.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": parameters,
            }
        }));
    }
    out
}

/// Build tool definitions for the LLM based on enabled tools.
fn build_tool_definitions(tools_enabled: &str, sandbox_url: &str, workdir: &str) -> Vec<Value> {
    let enabled: Vec<&str> = tools_enabled.split(',').map(str::trim).collect();
    let mut tools = Vec::new();

    if enabled.contains(&"read") {
        tools.push(json!({
            "name": "read",
            "description": format!("Read a file from the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to read" }
                },
                "required": ["path"]
            }
        }));
    }

    if enabled.contains(&"write") {
        tools.push(json!({
            "name": "write",
            "description": format!("Write a file to the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to write" },
                    "content": { "type": "string", "description": "File content" }
                },
                "required": ["path", "content"]
            }
        }));
    }

    if enabled.contains(&"edit") {
        tools.push(json!({
            "name": "edit",
            "description": format!("Edit a file in the sandbox at {sandbox_url} using search-and-replace. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to edit" },
                    "old_string": { "type": "string", "description": "Text to find" },
                    "new_string": { "type": "string", "description": "Text to replace with" }
                },
                "required": ["path", "old_string", "new_string"]
            }
        }));
    }

    if enabled.contains(&"bash") {
        tools.push(json!({
            "name": "bash",
            "description": format!("Run a bash command in the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" }
                },
                "required": ["command"]
            }
        }));
    }

    if enabled.contains(&"read_entity") {
        tools.push(json!({
            "name": "read_entity",
            "description": "Read a TemperFS-backed entity content file by file_id.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_id": { "type": "string", "description": "TemperFS File entity ID" }
                },
                "required": ["file_id"]
            }
        }));
    }

    if enabled.contains(&"save_memory") {
        tools.push(json!({
            "name": "save_memory",
            "description": "Persist a memory entry scoped to the agent soul.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string" },
                    "content": { "type": "string" },
                    "memory_type": { "type": "string" }
                },
                "required": ["key", "content"]
            }
        }));
    }

    if enabled.contains(&"recall_memory") {
        tools.push(json!({
            "name": "recall_memory",
            "description": "Recall memories matching a key or content substring.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }
        }));
    }

    if enabled.contains(&"spawn_agent") {
        tools.push(json!({
            "name": "spawn_agent",
            "description": "Create, configure, and provision a child TemperAgent.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "task": { "type": "string" },
                    "model": { "type": "string" },
                    "provider": { "type": "string" },
                    "max_turns": { "type": "integer" },
                    "tools": { "type": "string" },
                    "soul_id": { "type": "string" },
                    "background": { "type": "boolean" }
                },
                "required": ["task"]
            }
        }));
        tools.push(json!({
            "name": "list_agents",
            "description": "List child agents spawned by this agent.",
            "input_schema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }));
        tools.push(json!({
            "name": "abort_agent",
            "description": "Cancel a child agent by ID.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" }
                },
                "required": ["agent_id"]
            }
        }));
        tools.push(json!({
            "name": "steer_agent",
            "description": "Queue a steering message for a child agent.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["agent_id", "message"]
            }
        }));
    }

    if enabled.contains(&"run_coding_agent") {
        tools.push(json!({
            "name": "run_coding_agent",
            "description": "Run a coding agent CLI command inside the sandbox.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_type": { "type": "string" },
                    "task": { "type": "string" },
                    "workdir": { "type": "string" },
                    "background": { "type": "boolean" }
                },
                "required": ["agent_type", "task"]
            }
        }));
    }

    if enabled.contains(&"logfire_query") {
        tools.push(json!({
            "name": "logfire_query",
            "description": "Query Logfire observability data with either raw SQL or built-in intent-analysis patterns. Use this to inspect failure clusters, retries, alternate success paths, and abandonment evidence before producing final findings.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "Raw SQL query to run against Logfire records or metrics tables. Optional when query_kind is provided." },
                    "query_kind": { "type": "string", "description": "Optional built-in pattern query: recent_events, intent_failure_cluster, workflow_retries, alternate_success_paths, intent_abandonment" },
                    "service_name": { "type": "string", "description": "Optional service filter. Defaults to temper-platform." },
                    "environment": { "type": "string", "description": "Optional deployment_environment filter, e.g. local" },
                    "entity_type": { "type": "string", "description": "Optional entity/resource filter for built-in query kinds" },
                    "action": { "type": "string", "description": "Optional action filter for built-in query kinds" },
                    "intent_text": { "type": "string", "description": "Optional intent text filter for built-in query kinds" },
                    "agent_id": { "type": "string", "description": "Optional agent identifier filter for built-in query kinds" },
                    "lookback_minutes": { "type": "integer", "description": "Optional recency window for built-in query kinds. Defaults to 240." },
                    "min_timestamp": { "type": "string", "description": "Optional ISO timestamp lower bound" },
                    "max_timestamp": { "type": "string", "description": "Optional ISO timestamp upper bound" },
                    "limit": { "type": "integer", "description": "Optional row limit, clamped to 200" },
                    "row_oriented": { "type": "boolean", "description": "Return JSON rows instead of columns. Defaults to true." }
                },
                "required": []
            }
        }));
    }

    if enabled.contains(&"save_memory") {
        tools.push(json!({
            "name": "save_memory",
            "description": "Save a memory for future agent sessions. Memories persist across runs.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Unique key for this memory" },
                    "content": { "type": "string", "description": "Memory content (markdown)" },
                    "memory_type": { "type": "string", "enum": ["user", "feedback", "project", "reference"], "description": "Type of memory" }
                },
                "required": ["key", "content", "memory_type"]
            }
        }));
    }

    if enabled.contains(&"recall_memory") {
        tools.push(json!({
            "name": "recall_memory",
            "description": "Search and recall memories from previous sessions.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query to find relevant memories" }
                },
                "required": ["query"]
            }
        }));
    }

    if enabled.contains(&"spawn_agent") {
        tools.push(json!({
            "name": "spawn_agent",
            "description": "Spawn a child TemperAgent to handle a subtask. The child runs autonomously and returns its result.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "Optional deterministic child agent ID" },
                    "task": { "type": "string", "description": "The task for the child agent" },
                    "model": { "type": "string", "description": "LLM model to use (optional, defaults to parent's model)" },
                    "provider": { "type": "string", "description": "LLM provider to use (optional, defaults to parent's provider)" },
                    "max_turns": { "type": "integer", "description": "Maximum turns for the child (optional, default 20)" },
                    "tools": { "type": "string", "description": "Comma-separated tools to enable (optional, defaults to parent's tools)" },
                    "soul_id": { "type": "string", "description": "Soul ID to use (optional, defaults to parent's soul)" },
                    "background": { "type": "boolean", "description": "If true, return after provisioning without waiting for completion" }
                },
                "required": ["task"]
            }
        }));
    }

    if enabled.contains(&"list_agents") {
        tools.push(json!({
            "name": "list_agents",
            "description": "List child agents spawned by this agent and their status.",
            "input_schema": {
                "type": "object",
                "properties": {},
                "required": []
            }
        }));
    }

    if enabled.contains(&"steer_agent") {
        tools.push(json!({
            "name": "steer_agent",
            "description": "Send a follow-up message to a child agent mid-run.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "The child agent entity ID" },
                    "message": { "type": "string", "description": "The steering message to inject" }
                },
                "required": ["agent_id", "message"]
            }
        }));
    }

    if enabled.contains(&"abort_agent") {
        tools.push(json!({
            "name": "abort_agent",
            "description": "Cancel a running child agent.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "The child agent entity ID to cancel" }
                },
                "required": ["agent_id"]
            }
        }));
    }

    if enabled.contains(&"read_entity") {
        tools.push(json!({
            "name": "read_entity",
            "description": "Read a TemperFS file by ID. Use this to load skill content, soul documents, or any other entity-backed file.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file_id": { "type": "string", "description": "The TemperFS File entity ID to read" }
                },
                "required": ["file_id"]
            }
        }));
    }

    if enabled.contains(&"run_coding_agent") {
        tools.push(json!({
            "name": "run_coding_agent",
            "description": "Spawn a coding agent CLI process (Claude Code, Codex, Pi, OpenCode) in the sandbox.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "agent_type": { "type": "string", "enum": ["claude-code", "codex", "pi", "opencode"], "description": "Which coding agent CLI to use" },
                    "task": { "type": "string", "description": "The task for the coding agent" },
                    "workdir": { "type": "string", "description": "Working directory in the sandbox (optional)" },
                    "background": { "type": "boolean", "description": "Run in background (default: false)" }
                },
                "required": ["agent_type", "task"]
            }
        }));
    }

    tools
}

/// Read conversation messages from TemperFS File entity via $value endpoint.
fn read_conversation_from_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
    user_message: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    match ctx.http_call("GET", &url, &headers, "") {
        Ok(resp) if resp.status == 200 => {
            let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({"messages": []}));
            let messages = parsed
                .get("messages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if messages.is_empty() {
                // First turn — initialize with user message
                Ok(vec![json!({ "role": "user", "content": user_message })])
            } else {
                Ok(messages)
            }
        }
        Ok(resp) if resp.status == 404 => {
            // File has no content yet — first turn
            ctx.log(
                "info",
                "llm_caller: TemperFS file has no content, initializing",
            );
            Ok(vec![json!({ "role": "user", "content": user_message })])
        }
        Ok(resp) => {
            ctx.log(
                "warn",
                &format!(
                    "llm_caller: TemperFS read failed (HTTP {}), falling back to inline",
                    resp.status
                ),
            );
            Ok(vec![json!({ "role": "user", "content": user_message })])
        }
        Err(e) => {
            ctx.log(
                "warn",
                &format!("llm_caller: TemperFS read error: {e}, falling back to inline"),
            );
            Ok(vec![json!({ "role": "user", "content": user_message })])
        }
    }
}

/// Write conversation messages to TemperFS File entity via $value endpoint.
fn write_conversation_to_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
    conversation_json: &str,
) -> Result<(), String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];

    // Wrap messages array in the TemperFS conversation format
    let body = format!("{{\"messages\":{conversation_json}}}");

    let resp = ctx.http_call("PUT", &url, &headers, &body)?;
    if resp.status >= 200 && resp.status < 300 {
        ctx.log(
            "info",
            &format!(
                "llm_caller: wrote conversation to TemperFS ({} bytes)",
                body.len()
            ),
        );
        Ok(())
    } else {
        Err(format!(
            "TemperFS $value write failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(200)]
        ))
    }
}

/// Read session JSONL from TemperFS.
fn read_session_from_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
) -> Result<String, String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status == 200 {
        Ok(resp.body)
    } else if resp.status == 404 {
        Ok(String::new())
    } else {
        Err(format!("TemperFS session read failed (HTTP {})", resp.status))
    }
}

/// Write session JSONL to TemperFS.
fn write_session_to_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
    jsonl: &str,
) -> Result<(), String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("content-type".to_string(), "text/plain".to_string()),
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ];
    let resp = ctx.http_call("PUT", &url, &headers, jsonl)?;
    if resp.status >= 200 && resp.status < 300 {
        Ok(())
    } else {
        Err(format!("TemperFS session write failed (HTTP {})", resp.status))
    }
}

/// Assemble the full system prompt from soul + override + skills + memory.
fn assemble_system_prompt(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    soul_id: &str,
    system_prompt_override: &str,
) -> Result<String, String> {
    let mut parts: Vec<String> = Vec::new();

    // 1. Soul content
    if !soul_id.is_empty() {
        match load_soul_content(ctx, temper_api_url, tenant, soul_id) {
            Ok(content) if !content.is_empty() => parts.push(content),
            Ok(_) => ctx.log("warn", "assemble_system_prompt: soul content is empty"),
            Err(e) => ctx.log("warn", &format!("assemble_system_prompt: failed to load soul: {e}")),
        }
    }

    // 2. System prompt override
    if !system_prompt_override.is_empty() {
        parts.push(system_prompt_override.to_string());
    }

    // 3. Available skills
    if !soul_id.is_empty() {
        match load_skills_block(ctx, temper_api_url, tenant) {
            Ok(block) if !block.is_empty() => parts.push(block),
            Ok(_) => {}
            Err(e) => ctx.log("warn", &format!("assemble_system_prompt: failed to load skills: {e}")),
        }
    }

    // 4. Memory context
    if !soul_id.is_empty() {
        match load_memory_block(ctx, temper_api_url, tenant, soul_id) {
            Ok(block) if !block.is_empty() => parts.push(block),
            Ok(_) => {}
            Err(e) => ctx.log("warn", &format!("assemble_system_prompt: failed to load memory: {e}")),
        }
    }

    // Fall back to bare system_prompt if nothing loaded
    if parts.is_empty() {
        return Ok(system_prompt_override.to_string());
    }

    Ok(parts.join("\n\n"))
}

/// Load soul content from AgentSoul entity.
fn load_soul_content(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    soul_id: &str,
) -> Result<String, String> {
    let url = format!("{temper_api_url}/tdata/AgentSouls('{soul_id}')");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status != 200 {
        return Err(format!("soul read failed (HTTP {})", resp.status));
    }
    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
    let content_file_id = entity_field_str(&parsed, &["ContentFileId"]).unwrap_or("");
    if content_file_id.is_empty() {
        return Ok(String::new());
    }
    // Read from TemperFS
    let file_url = format!("{temper_api_url}/tdata/Files('{content_file_id}')/$value");
    let resp2 = ctx.http_call("GET", &file_url, &headers, "")?;
    if resp2.status == 200 { Ok(resp2.body) } else { Ok(String::new()) }
}

/// Load active skills as an XML block for the system prompt.
fn load_skills_block(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
) -> Result<String, String> {
    let url = format!("{temper_api_url}/tdata/AgentSkills?$filter=Status eq 'Active'");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status != 200 {
        return Ok(String::new());
    }
    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
    let skills = parsed.get("value").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if skills.is_empty() {
        return Ok(String::new());
    }
    let mut xml = String::from("<available_skills>\n");
    for skill in &skills {
        let name = entity_field_str(skill, &["Name"]).unwrap_or("unknown");
        let desc = entity_field_str(skill, &["Description"]).unwrap_or("");
        let file_id = entity_field_str(skill, &["ContentFileId"]).unwrap_or("");
        xml.push_str(&format!("  <skill name=\"{name}\" description=\"{desc}\" file_id=\"{file_id}\" />\n"));
    }
    xml.push_str("</available_skills>");
    Ok(xml)
}

/// Load agent memories as a context block for the system prompt.
fn load_memory_block(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    soul_id: &str,
) -> Result<String, String> {
    let url = format!(
        "{temper_api_url}/tdata/AgentMemorys?$filter=SoulId eq '{}' and Status eq 'Active'",
        soul_id
    );
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status != 200 {
        return Ok(String::new());
    }
    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or(json!({}));
    let memories = parsed.get("value").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    if memories.is_empty() {
        return Ok(String::new());
    }
    let mut block = String::from("<agent_memory>\n");
    for mem in &memories {
        let key = entity_field_str(mem, &["Key"]).unwrap_or("unknown");
        let content = entity_field_str(mem, &["Content"]).unwrap_or("");
        let mem_type = entity_field_str(mem, &["MemoryType"]).unwrap_or("reference");
        block.push_str(&format!("  <memory key=\"{key}\" type=\"{mem_type}\">\n    {content}\n  </memory>\n"));
    }
    block.push_str("</agent_memory>");
    Ok(block)
}

fn direct_field_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn entity_field_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    direct_field_str(value, keys).or_else(|| {
        value.get("fields")
            .and_then(|fields| direct_field_str(fields, keys))
    })
}

fn resolve_temper_api_url(ctx: &Context, fields: &Value) -> String {
    fields
        .get("temper_api_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| match ctx.config.get("temper_api_url").map(String::as_str) {
            Some(value) if !value.trim().is_empty() && !value.contains("{secret:") => {
                Some(value.to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}
