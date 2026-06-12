use super::*;
use crate::wide_event::otel::event_error_message;

fn sample_event() -> WideEvent {
    from_transition(TransitionInput {
        tenant: "default",
        entity_type: "Order",
        entity_id: "order-123",
        operation: "SubmitOrder",
        from_status: "Draft",
        to_status: "Submitted",
        success: true,
        duration_ns: 5_000_000,
        params: &serde_json::json!({"ShippingAddressId": "addr-1"}),
        item_count: 2,
        trace_id: "trace-abc",
    })
}

#[test]
fn test_wide_event_from_transition() {
    let event = sample_event();
    assert_eq!(event.event_kind, EventKind::Transition);
    assert_eq!(event.entity_type, "Order");
    assert_eq!(event.operation, "SubmitOrder");
    assert_eq!(event.tags["tenant"], "default");
    assert_eq!(event.tags["entity_type"], "Order");
    assert_eq!(event.tags["success"], "true");
    assert_eq!(event.measurements["transition_count"], 1.0);
    assert_eq!(event.attributes["entity_id"], "order-123");
    assert_eq!(event.attributes["tenant"], "default");
}

#[test]
fn test_emit_span_noop() {
    emit_span(&sample_event());
}

#[test]
fn test_emit_metrics_noop() {
    emit_metrics(&sample_event());
}

#[test]
fn test_cost_decoupling() {
    let event = sample_event();
    assert!(!event.tags.contains_key("entity_id"));
    assert!(!event.tags.contains_key("params"));
    assert!(event.attributes.contains_key("entity_id"));
    assert!(event.attributes.contains_key("params"));
}

#[test]
fn test_failed_transition_marks_error() {
    let event = from_transition(TransitionInput {
        tenant: "default",
        entity_type: "Order",
        entity_id: "order-456",
        operation: "SubmitOrder",
        from_status: "Draft",
        to_status: "Draft",
        success: false,
        duration_ns: 1_000_000,
        params: &serde_json::json!({}),
        item_count: 0,
        trace_id: "trace-def",
    });
    assert!(!event.success);
    assert_eq!(event.tags["success"], "false");
}

#[test]
fn test_wasm_invocation_event() {
    let event = from_wasm_invocation(WasmInvocationInput {
        module_name: "weather_module",
        trigger_action: "CheckWeather",
        entity_type: "Task",
        entity_id: "task-1",
        tenant: "tenant-a",
        success: true,
        duration_ns: 2_000_000,
        error: None,
    });
    assert_eq!(event.event_kind, EventKind::WasmInvocation);
    assert_eq!(event.tags["module_name"], "weather_module");
    assert_eq!(event.tags["success"], "true");
    assert!(!event.tags.contains_key("entity_id"));
    assert_eq!(event.attributes["entity_id"], "task-1");
    assert_eq!(event.measurements["invocation_count"], 1.0);
}

#[test]
fn test_wasm_invocation_with_error() {
    let event = from_wasm_invocation(WasmInvocationInput {
        module_name: "weather_module",
        trigger_action: "CheckWeather",
        entity_type: "Task",
        entity_id: "task-1",
        tenant: "tenant-a",
        success: false,
        duration_ns: 3_000_000,
        error: Some("module panicked"),
    });
    assert!(!event.success);
    assert_eq!(event.attributes["error"], "module panicked");
    assert_eq!(event.attributes["error.message"], "module panicked");
    assert_eq!(event.attributes["error.type"], "wasm_invocation_error");
    assert_eq!(
        event_error_message(&event).as_deref(),
        Some("module panicked")
    );
}

#[test]
fn test_authz_decision_event() {
    let event = from_authz_decision(AuthzDecisionInput {
        action: "SubmitOrder",
        resource_type: "Order",
        principal_kind: "user",
        decision: "Allow",
        duration_ns: 500_000,
        tenant: "tenant-b",
    });
    assert_eq!(event.event_kind, EventKind::AuthzDecision);
    assert_eq!(event.tags["decision"], "Allow");
    assert!(event.success);
    assert!(!event.tags.contains_key("principal_kind"));
    assert_eq!(event.attributes["principal_kind"], "user");
}

#[test]
fn test_authz_deny_decision() {
    let event = from_authz_decision(AuthzDecisionInput {
        action: "DeleteOrder",
        resource_type: "Order",
        principal_kind: "user",
        decision: "Deny",
        duration_ns: 800_000,
        tenant: "tenant-b",
    });
    assert!(!event.success);
}

#[test]
fn test_invariant_check_event() {
    let event = from_invariant_check(InvariantCheckInput {
        invariant_name: "order_total_positive",
        entity_type: "Order",
        entity_id: "order-99",
        tenant: "tenant-c",
        check_count: 3,
        outcome: "converged",
        duration_ns: 1_500_000,
    });
    assert_eq!(event.event_kind, EventKind::InvariantCheck);
    assert_eq!(event.tags["outcome"], "converged");
    assert!(event.success);
    assert!(!event.tags.contains_key("entity_id"));
    assert_eq!(event.attributes["entity_id"], "order-99");
}

#[test]
fn test_invariant_check_failed() {
    let event = from_invariant_check(InvariantCheckInput {
        invariant_name: "stock_non_negative",
        entity_type: "Inventory",
        entity_id: "inv-5",
        tenant: "tenant-c",
        check_count: 10,
        outcome: "failed",
        duration_ns: 5_000_000,
    });
    assert!(!event.success);
}

#[test]
fn test_llm_call_event() {
    let event = from_llm_call(LlmCallInput {
        provider: "anthropic",
        model: "claude-sonnet-4-6",
        operation: "chat",
        entity_type: "Session",
        entity_id: "sess-1",
        session_id: "sess-1",
        success: true,
        duration_ns: 3_000_000_000,
        input_tokens: 150,
        output_tokens: 50,
        stop_reason: "end_turn",
        system_instructions: Some(r#"[{"type":"text","content":"be concise"}]"#),
        input_messages: Some(r#"[{"role":"user","content":"hello"}]"#),
        output_messages: Some(r#"[{"role":"assistant","content":"hi"}]"#),
        trace_id: "trace-llm",
        error: None,
    });
    assert_eq!(event.event_kind, EventKind::LlmCall);
    assert_eq!(event.tags["gen_ai.system"], "anthropic");
    assert_eq!(event.tags["gen_ai.request.model"], "claude-sonnet-4-6");
    assert_eq!(event.tags["gen_ai.operation.name"], "chat");
    assert_eq!(event.tags["gen_ai.response.finish_reasons"], "end_turn");
    assert!(event.success);
    assert_eq!(event.measurements["gen_ai.usage.input_tokens"], 150.0);
    assert_eq!(event.measurements["gen_ai.usage.output_tokens"], 50.0);
    assert_eq!(event.attributes["gen_ai.conversation.id"], "sess-1");
    assert_eq!(
        event.attributes["gen_ai.system_instructions"],
        r#"[{"type":"text","content":"be concise"}]"#
    );
    assert_eq!(
        event.attributes["gen_ai.input.messages"],
        r#"[{"role":"user","content":"hello"}]"#
    );
    assert_eq!(
        event.attributes["gen_ai.output.messages"],
        r#"[{"role":"assistant","content":"hi"}]"#
    );
    assert!(!event.tags.contains_key("entity_id"));
}

#[test]
fn test_llm_call_failure() {
    let event = from_llm_call(LlmCallInput {
        provider: "openrouter",
        model: "gpt-4",
        operation: "chat",
        entity_type: "Session",
        entity_id: "sess-2",
        session_id: "sess-2",
        success: false,
        duration_ns: 500_000,
        input_tokens: 100,
        output_tokens: 0,
        stop_reason: "",
        system_instructions: None,
        input_messages: None,
        output_messages: None,
        trace_id: "trace-fail",
        error: Some("rate limit exceeded"),
    });
    assert!(!event.success);
    assert_eq!(event.attributes["error"], "rate limit exceeded");
}

#[test]
fn test_tool_call_event() {
    let event = from_tool_call(ToolCallInput {
        tool_name: "temper_create",
        tool_call_id: Some("toolu_123"),
        entity_type: "Session",
        entity_id: "sess-1",
        session_id: "sess-1",
        tool_arguments: Some(r#"{"entity":"Task"}"#),
        tool_result: Some(r#"{"id":"task-1"}"#),
        success: true,
        duration_ns: 200_000_000,
        trace_id: "trace-tool",
        error: None,
    });
    assert_eq!(event.event_kind, EventKind::ToolCall);
    assert_eq!(event.tags["gen_ai.operation.name"], "execute_tool");
    assert_eq!(event.tags["gen_ai.tool.name"], "temper_create");
    assert!(event.success);
    assert_eq!(event.attributes["gen_ai.conversation.id"], "sess-1");
    assert_eq!(event.attributes["gen_ai.tool.call.id"], "toolu_123");
    assert_eq!(
        event.attributes["gen_ai.tool.call.arguments"],
        r#"{"entity":"Task"}"#
    );
    assert_eq!(
        event.attributes["gen_ai.tool.call.result"],
        r#"{"id":"task-1"}"#
    );
    assert!(!event.tags.contains_key("entity_id"));
}

#[test]
fn test_tool_call_failure() {
    let event = from_tool_call(ToolCallInput {
        tool_name: "sandbox_bash",
        tool_call_id: None,
        entity_type: "Session",
        entity_id: "sess-3",
        session_id: "sess-3",
        tool_arguments: None,
        tool_result: None,
        success: false,
        duration_ns: 1_000_000,
        trace_id: "trace-tool-fail",
        error: Some("sandbox timeout"),
    });
    assert!(!event.success);
    assert_eq!(event.attributes["error"], "sandbox timeout");
}

#[test]
fn test_emit_span_all_event_kinds() {
    let events = vec![
        sample_event(),
        from_wasm_invocation(WasmInvocationInput {
            module_name: "m",
            trigger_action: "a",
            entity_type: "T",
            entity_id: "id",
            tenant: "t",
            success: true,
            duration_ns: 0,
            error: None,
        }),
        from_authz_decision(AuthzDecisionInput {
            action: "a",
            resource_type: "T",
            principal_kind: "user",
            decision: "Allow",
            duration_ns: 0,
            tenant: "t",
        }),
        from_invariant_check(InvariantCheckInput {
            invariant_name: "inv",
            entity_type: "T",
            entity_id: "id",
            tenant: "t",
            check_count: 1,
            outcome: "converged",
            duration_ns: 0,
        }),
        from_llm_call(LlmCallInput {
            provider: "anthropic",
            model: "claude-sonnet-4-6",
            operation: "chat",
            entity_type: "Session",
            entity_id: "s",
            session_id: "s",
            success: true,
            duration_ns: 0,
            input_tokens: 0,
            output_tokens: 0,
            stop_reason: "end_turn",
            system_instructions: None,
            input_messages: None,
            output_messages: None,
            trace_id: "",
            error: None,
        }),
        from_tool_call(ToolCallInput {
            tool_name: "temper_get",
            tool_call_id: None,
            entity_type: "Session",
            entity_id: "s",
            session_id: "s",
            tool_arguments: None,
            tool_result: None,
            success: true,
            duration_ns: 0,
            trace_id: "",
            error: None,
        }),
    ];
    for event in &events {
        emit_span(event);
        emit_metrics(event);
    }
}
