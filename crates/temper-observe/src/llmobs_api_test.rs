use super::*;

#[test]
fn converts_otel_messages_with_tool_calls_and_results() {
    let messages = convert_otel_messages_to_llmobs(
        r#"[
          {"role":"user","parts":[{"type":"text","content":"List sessions"}]},
          {"role":"assistant","parts":[{"type":"tool_call","id":"tool_123","name":"temper.list_sessions","arguments":{"top":3}}]},
          {"role":"tool","parts":[{"type":"tool_call_response","id":"tool_123","result":{"sessions":["s1","s2"]}}]}
        ]"#,
    )
    .unwrap();

    assert_eq!(messages[0]["content"], "List sessions");
    assert_eq!(messages[1]["tool_calls"][0]["name"], "temper.list_sessions");
    assert_eq!(messages[2]["tool_results"][0]["tool_id"], "tool_123");
}

#[test]
fn normalizes_hex_trace_and_span_ids() {
    assert_eq!(normalize_trace_id("0000000000000000000000000000000f"), "15");
    assert_eq!(normalize_span_id("000000000000000a"), "10");
}

#[test]
fn llm_span_payload_uses_span_name_and_supported_model_metadata() {
    let payload = build_llm_span_payload(&LlmSpanInput {
        service_name: "temperpaw",
        session_id: "session-1",
        trace_id: "0000000000000000000000000000000f",
        span_id: "000000000000000a",
        parent_span_id: None,
        agent_span_id: None,
        agent_start_ns: None,
        workflow_span_id: None,
        agent_name: None,
        workflow_name: None,
        span_name: "wasm:provider_caller",
        provider: "openai_codex",
        model: "gpt-5.4",
        system_instructions: None,
        input_messages_json: None,
        output_messages_json: None,
        input_tokens: 10,
        output_tokens: 20,
        finish_reason: Some("tool_use"),
        duration_ms: 1234,
        error_type: None,
    })
    .unwrap();
    let span = &payload["data"]["attributes"]["spans"][0];

    assert_eq!(span["name"], "wasm:provider_caller");
    assert_eq!(span["meta"]["model_name"], "gpt-5.4");
    assert_eq!(span["meta"]["model_provider"], "openai");
    assert_eq!(span["meta"]["metadata"]["model_name"], "gpt-5.4");
    assert_eq!(span["meta"]["metadata"]["model_provider"], "openai");

    let tags = span["tags"].as_array().unwrap();
    assert!(tags.iter().any(|tag| tag == "model_name:gpt-5.4"));
    assert!(tags.iter().any(|tag| tag == "model_provider:openai"));
}

#[test]
fn llm_span_payload_uses_parent_span_id_when_available() {
    let payload = build_llm_span_payload(&LlmSpanInput {
        service_name: "temperpaw",
        session_id: "session-1",
        trace_id: "0000000000000000000000000000000f",
        span_id: "000000000000000a",
        parent_span_id: Some("000000000000000b"),
        agent_span_id: None,
        agent_start_ns: None,
        workflow_span_id: None,
        agent_name: None,
        workflow_name: None,
        span_name: "wasm:provider_caller",
        provider: "openai_codex",
        model: "gpt-5.4",
        system_instructions: None,
        input_messages_json: None,
        output_messages_json: None,
        input_tokens: 10,
        output_tokens: 20,
        finish_reason: Some("stop"),
        duration_ms: 1234,
        error_type: None,
    })
    .unwrap();
    let span = &payload["data"]["attributes"]["spans"][0];

    assert_eq!(span["parent_id"], "11");
}

#[test]
fn llm_span_payload_defaults_agent_root_to_temperpaw_session_name() {
    let payload = build_llm_span_payload(&LlmSpanInput {
        service_name: "temperpaw",
        session_id: "session-1",
        trace_id: "0000000000000000000000000000000f",
        span_id: "000000000000000a",
        parent_span_id: None,
        agent_span_id: Some("000000000000000b"),
        agent_start_ns: None,
        workflow_span_id: None,
        agent_name: None,
        workflow_name: None,
        span_name: "wasm:provider_caller",
        provider: "openai_codex",
        model: "gpt-5.4",
        system_instructions: None,
        input_messages_json: None,
        output_messages_json: None,
        input_tokens: 10,
        output_tokens: 20,
        finish_reason: Some("stop"),
        duration_ms: 1234,
        error_type: None,
    })
    .unwrap();
    let spans = payload["data"]["attributes"]["spans"].as_array().unwrap();

    assert_eq!(spans[0]["name"], "temperpaw.agent.session");
}

#[test]
fn llm_span_payload_can_emit_agent_workflow_tree() {
    let payload = build_llm_span_payload(&LlmSpanInput {
        service_name: "temperpaw",
        session_id: "session-1",
        trace_id: "0000000000000000000000000000000f",
        span_id: "000000000000000a",
        parent_span_id: Some("000000000000000b"),
        agent_span_id: Some("000000000000000b"),
        agent_start_ns: None,
        workflow_span_id: Some("000000000000000c"),
        agent_name: Some("temperpaw.agent.session"),
        workflow_name: Some("Session.ContextReady"),
        span_name: "wasm:provider_caller",
        provider: "openai_codex",
        model: "gpt-5.4",
        system_instructions: None,
        input_messages_json: Some(
            r#"[{"role":"user","parts":[{"type":"text","content":"Debug this session"}]}]"#,
        ),
        output_messages_json: Some(
            r#"[{"role":"assistant","parts":[{"type":"text","content":"Done"}]}]"#,
        ),
        input_tokens: 10,
        output_tokens: 20,
        finish_reason: Some("stop"),
        duration_ms: 1234,
        error_type: None,
    })
    .unwrap();
    let spans = payload["data"]["attributes"]["spans"].as_array().unwrap();

    assert_eq!(spans.len(), 3);
    assert_eq!(spans[0]["name"], "temperpaw.agent.session");
    assert_eq!(spans[0]["parent_id"], "undefined");
    assert_eq!(spans[0]["span_id"], "11");
    assert_eq!(spans[0]["meta"]["kind"], "agent");
    assert_eq!(spans[0]["meta"]["metadata"]["session_id"], "session-1");
    assert_eq!(spans[0]["meta"]["input"]["value"], "Debug this session");
    assert_eq!(spans[0]["meta"]["output"]["value"], "Done");

    assert_eq!(spans[1]["name"], "Session.ContextReady");
    assert_eq!(spans[1]["parent_id"], "11");
    assert_eq!(spans[1]["span_id"], "12");
    assert_eq!(spans[1]["meta"]["kind"], "workflow");
    assert_eq!(spans[1]["meta"]["input"]["value"], "Debug this session");
    assert_eq!(spans[1]["meta"]["output"]["value"], "Done");
    assert!(spans[0]["meta"]["input"].get("messages").is_none());
    assert!(spans[1]["meta"]["input"].get("messages").is_none());

    assert_eq!(spans[2]["name"], "wasm:provider_caller");
    assert_eq!(spans[2]["parent_id"], "12");
    assert_eq!(spans[2]["span_id"], "10");
    assert_eq!(spans[2]["meta"]["kind"], "llm");
    assert_eq!(
        spans[2]["meta"]["input"]["messages"][0]["content"],
        "Debug this session"
    );
    assert_eq!(spans[2]["meta"]["output"]["messages"][0]["content"], "Done");
}
