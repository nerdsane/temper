//! ADR-0166 tests for the dispatch callback-param gate.
//!
//! Split from `wasm_test.rs`: these cover one thing — which private LLM
//! observability params survive for a tenant that has not opted into content
//! export, and how their values are bounded.

use super::*;

#[test]
fn strips_private_llm_observability_params_before_callback_dispatch() {
    let params = json!({
        "provider_response_file_id": "file-123",
        "input_tokens": 10,
        "_gen_ai_input_messages": "[{\"role\":\"user\"}]",
        "_gen_ai_output_messages": "[{\"role\":\"assistant\"}]",
        "_gen_ai_system_instructions": "system",
        "_gen_ai_provider": "anthropic",
        "_gen_ai_model": "claude-sonnet-4-6",
        "_gen_ai_finish_reason": "end_turn",
        "_gen_ai_llm_parent_span_id": "parent-span-private",
        "_dd_llmobs_tool_spans": "[]",
        "gen_ai_parent_trace_id": "trace-public",
        "gen_ai_llm_parent_span_id": "parent-span-public",
    });

    let stripped = strip_private_observability_params(params);

    assert_eq!(stripped["provider_response_file_id"], "file-123");
    assert_eq!(stripped["input_tokens"], 10);
    assert_eq!(stripped["gen_ai_parent_trace_id"], "trace-public");
    assert_eq!(stripped["gen_ai_llm_parent_span_id"], "parent-span-public");
    assert!(stripped.get("_gen_ai_input_messages").is_none());
    assert!(stripped.get("_gen_ai_output_messages").is_none());
    assert!(stripped.get("_gen_ai_system_instructions").is_none());
    assert!(stripped.get("_gen_ai_provider").is_none());
    assert!(stripped.get("_gen_ai_model").is_none());
    assert!(stripped.get("_gen_ai_finish_reason").is_none());
    assert!(stripped.get("_gen_ai_llm_parent_span_id").is_none());
    assert!(stripped.get("_dd_llmobs_tool_spans").is_none());
}

/// A key name cannot make an untrusted value into metadata. The sinks record
/// `_gen_ai_model` as `gen_ai.request.model`, which LLM Observability reads as
/// LLM data — so a module that returns its prompt under that key would export
/// content for a non-opted-in tenant under a semantic-convention name. The
/// other three channels bound these values; so must this one.
/// The dispatch gate is an allowlist: an unrecognised private observability
/// param must not survive just because it is not on a content list. A denylist
/// here would leave `_gen_ai_completion` in the map for any sink added later.
#[test]
fn unrecognized_private_observability_params_do_not_survive() {
    let mut params = json!({
        "_gen_ai_completion": "SECRET COMPLETION",
        "_gen_ai_prompt": "SECRET PROMPT",
        "_dd_llmobs_something_new": "SECRET",
        "_gen_ai_model": "claude-opus-4-8",
        "_gen_ai_llm_parent_span_id": "parent-span",
        "output": "ordinary action output",
    });

    redact_llm_content_params(&mut params, false);

    for dropped in [
        "_gen_ai_completion",
        "_gen_ai_prompt",
        "_dd_llmobs_something_new",
    ] {
        assert!(
            params.get(dropped).is_none(),
            "`{dropped}` must not survive an allowlist; got {params:?}"
        );
    }
    // Every key the content list names must also be gone.
    for content_key in LLM_CONTENT_PARAM_KEYS {
        assert!(
            params.get(content_key).is_none(),
            "{content_key} must be dropped"
        );
    }
    // Recognised metadata and ordinary output are untouched.
    assert_eq!(
        params.get("_gen_ai_model").and_then(Value::as_str),
        Some("claude-opus-4-8")
    );
    assert_eq!(
        params
            .get("_gen_ai_llm_parent_span_id")
            .and_then(Value::as_str),
        Some("parent-span"),
        "trace-correlation ids must survive or LLMObs spans lose their parent"
    );
    assert_eq!(
        params.get("output").and_then(Value::as_str),
        Some("ordinary action output"),
        "non-observability params are not this gate's business"
    );
}

#[test]
fn clamps_guest_supplied_llm_metadata_values_for_non_opted_in_tenant() {
    use temper_wasm::host_trait::MAX_REDACTED_LLM_METADATA_VALUE_BYTES;
    let prompt = "P".repeat(4096);
    let mut params = json!({
        "_gen_ai_model": prompt,
        "_gen_ai_provider": prompt,
        "_gen_ai_finish_reason": prompt,
        "input_tokens": 10,
    });

    redact_llm_content_params(&mut params, false);

    for key in ["_gen_ai_model", "_gen_ai_provider", "_gen_ai_finish_reason"] {
        let value = params
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{key} should survive, bounded"));
        assert!(
            value.len() <= MAX_REDACTED_LLM_METADATA_VALUE_BYTES,
            "{key} must be clamped, got {} bytes",
            value.len()
        );
    }
    assert_eq!(params.get("input_tokens").and_then(Value::as_u64), Some(10));

    // An opted-in tenant is not clamped.
    let mut exported = json!({ "_gen_ai_model": prompt });
    redact_llm_content_params(&mut exported, true);
    assert_eq!(
        exported.get("_gen_ai_model").and_then(Value::as_str),
        Some(prompt.as_str())
    );
}

#[test]
fn redacts_llm_content_params_for_non_opted_in_tenant() {
    let base = json!({
        "input_tokens": 10,
        "output_tokens": 20,
        "_gen_ai_provider": "anthropic",
        "_gen_ai_model": "claude-sonnet-4-6",
        "_gen_ai_finish_reason": "end_turn",
        "_gen_ai_llm_parent_span_id": "parent-span",
        "_gen_ai_input_messages": "[{\"role\":\"user\",\"content\":\"SECRET PROMPT\"}]",
        "_gen_ai_output_messages": "[{\"role\":\"assistant\",\"content\":\"SECRET REPLY\"}]",
        "_gen_ai_system_instructions": "SECRET SYSTEM",
        "_dd_llmobs_tool_spans": "[{\"arguments\":\"SECRET ARGS\",\"result\":\"SECRET RESULT\"}]",
    });

    // Non-opted-in tenant: content stripped, metadata preserved.
    let mut redacted = base.clone();
    redact_llm_content_params(&mut redacted, false);
    assert!(
        redacted.get("_gen_ai_input_messages").is_none(),
        "prompt must be redacted"
    );
    assert!(
        redacted.get("_gen_ai_output_messages").is_none(),
        "completion must be redacted"
    );
    assert!(
        redacted.get("_gen_ai_system_instructions").is_none(),
        "system prompt must be redacted"
    );
    assert!(
        redacted.get("_dd_llmobs_tool_spans").is_none(),
        "tool content must be redacted"
    );
    assert_eq!(redacted["input_tokens"], 10);
    assert_eq!(redacted["output_tokens"], 20);
    assert_eq!(redacted["_gen_ai_provider"], "anthropic");
    assert_eq!(redacted["_gen_ai_model"], "claude-sonnet-4-6");
    assert_eq!(redacted["_gen_ai_finish_reason"], "end_turn");
    assert_eq!(redacted["_gen_ai_llm_parent_span_id"], "parent-span");

    // Opted-in tenant: content preserved.
    let mut exported = base.clone();
    redact_llm_content_params(&mut exported, true);
    assert_eq!(
        exported["_gen_ai_input_messages"],
        "[{\"role\":\"user\",\"content\":\"SECRET PROMPT\"}]"
    );
    assert_eq!(
        exported["_dd_llmobs_tool_spans"],
        "[{\"arguments\":\"SECRET ARGS\",\"result\":\"SECRET RESULT\"}]"
    );
}
