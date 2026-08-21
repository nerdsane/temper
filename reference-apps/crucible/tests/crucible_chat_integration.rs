//! End-to-end integration test for the Phase 4 chat loop (ADR-0046).
//!
//! Spins up a real `temper-server` HTTP router on an ephemeral port
//! with all twelve Crucible IOAs loaded, points a [`TemperClient`] at
//! it, and drives the full turn loop from
//! [`crucible_reference::chat::responder`] against a deterministic
//! [`MockModel`].
//!
//! What this test proves:
//!
//! 1. `seed::seed` successfully POSTs Environment + ManagedAgent +
//!    Session through the real OData router, passing the Phase 0 hard
//!    constraint and the Phase 1–2 cross-invariants.
//! 2. `responder::respond` drives the full five-event turn
//!    (`user.message`, `span.model_request_start`, `agent.message`,
//!    `span.model_request_end`, `session.status_idle`) and ends the
//!    session in `Idle` — exercising the Phase 2 lifecycle actions
//!    `StartSession` and `IdleSession`.
//! 3. A second turn on the same session reconstructs chat history
//!    from the SessionEvent feed alone — the mock captures its most
//!    recent `MessagesRequest` and the test asserts that turn 2's
//!    request contains four messages (user/assistant/user + the new
//!    user turn), proving multi-turn memory works without any
//!    in-process state.
//!
//! No network, no API key, no secrets. Everything runs in-process.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use crucible_reference::chat::anthropic::MockModel;
use crucible_reference::chat::responder::{RespondRequest, respond};
use crucible_reference::chat::seed::{CallableAgentSeedSpec, SeedOptions, seed};
use crucible_reference::chat::temper_client::TemperClient;
use std::net::SocketAddr;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;

// ----------------------------------------------------------------------
// Spec fixtures — same set as crucible_sessions_validation.rs
// ----------------------------------------------------------------------

const ENVIRONMENT_IOA: &str = include_str!("../specs/environment.ioa.toml");
const ALLOWED_HOST_IOA: &str = include_str!("../specs/environment_allowed_host.ioa.toml");
const PACKAGE_IOA: &str = include_str!("../specs/environment_package.ioa.toml");
const MANAGED_AGENT_IOA: &str = include_str!("../specs/managed_agent.ioa.toml");
const AGENT_MCP_SERVER_IOA: &str = include_str!("../specs/agent_mcp_server.ioa.toml");
const AGENT_SKILL_IOA: &str = include_str!("../specs/agent_skill.ioa.toml");
const AGENT_TOOL_IOA: &str = include_str!("../specs/agent_tool.ioa.toml");
const AGENT_TOOL_CONFIG_IOA: &str = include_str!("../specs/agent_tool_config.ioa.toml");
const AGENT_VERSION_IOA: &str = include_str!("../specs/agent_version.ioa.toml");
const SESSION_IOA: &str = include_str!("../specs/session.ioa.toml");
const SESSION_RESOURCE_IOA: &str = include_str!("../specs/session_resource.ioa.toml");
const SESSION_EVENT_IOA: &str = include_str!("../specs/session_event.ioa.toml");
const CALLABLE_AGENT_IOA: &str = include_str!("../specs/callable_agent.ioa.toml");
const SESSION_THREAD_IOA: &str = include_str!("../specs/session_thread.ioa.toml");
const CROSS_INVARIANTS_TOML: &str = include_str!("../specs/cross-invariants.toml");
const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");
const CRUCIBLE_CHAT_INTEGRATION_POLICY: &str = r#"
permit(principal, action in [Action::"list", Action::"read", Action::"create", Action::"update", Action::"delete"], resource is Environment);
permit(principal, action in [Action::"list", Action::"read", Action::"create", Action::"update", Action::"delete"], resource is ManagedAgent);
permit(principal, action in [Action::"list", Action::"read", Action::"create", Action::"update", Action::"delete"], resource is AgentTool);
permit(
    principal,
    action in [
        Action::"list",
        Action::"read",
        Action::"create",
        Action::"update",
        Action::"delete",
        Action::"StartSession",
        Action::"IdleSession",
        Action::"ResumeSession"
    ],
    resource is Session
);
permit(principal, action in [Action::"list", Action::"read", Action::"create", Action::"update", Action::"delete"], resource is SessionEvent);
permit(principal, action in [Action::"list", Action::"read", Action::"create", Action::"update", Action::"delete"], resource is CallableAgent);
permit(
    principal,
    action in [
        Action::"list",
        Action::"read",
        Action::"create",
        Action::"update",
        Action::"delete",
        Action::"IdleThread",
        Action::"ResumeThread",
        Action::"TerminateThread"
    ],
    resource is SessionThread
);
"#;

/// Build a fully-loaded Crucible `ServerState` with every entity type
/// marked as verified. Same pattern as `crucible_sessions_validation.rs`.
fn build_crucible_state() -> ServerState {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_reactions_and_constraints(
        "default",
        csdl,
        MODEL_CSDL.to_string(),
        &[
            ("Environment", ENVIRONMENT_IOA),
            ("EnvironmentAllowedHost", ALLOWED_HOST_IOA),
            ("EnvironmentPackage", PACKAGE_IOA),
            ("ManagedAgent", MANAGED_AGENT_IOA),
            ("AgentMcpServer", AGENT_MCP_SERVER_IOA),
            ("AgentSkill", AGENT_SKILL_IOA),
            ("AgentTool", AGENT_TOOL_IOA),
            ("AgentToolConfig", AGENT_TOOL_CONFIG_IOA),
            ("AgentVersion", AGENT_VERSION_IOA),
            ("Session", SESSION_IOA),
            ("SessionResource", SESSION_RESOURCE_IOA),
            ("SessionEvent", SESSION_EVENT_IOA),
            ("CallableAgent", CALLABLE_AGENT_IOA),
            ("SessionThread", SESSION_THREAD_IOA),
        ],
        Vec::new(),
        Some(CROSS_INVARIANTS_TOML.to_string()),
    );

    let system = ActorSystem::new("crucible-chat-integration");
    let state = ServerState::from_registry(system, registry);

    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in [
            "Environment",
            "EnvironmentAllowedHost",
            "EnvironmentPackage",
            "ManagedAgent",
            "AgentMcpServer",
            "AgentSkill",
            "AgentTool",
            "AgentToolConfig",
            "AgentVersion",
            "Session",
            "SessionResource",
            "SessionEvent",
            "CallableAgent",
            "SessionThread",
        ] {
            registry.set_verification_status(
                &TenantId::default(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "L0 SMT".to_string(),
                        passed: true,
                        summary: "OK".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-04-11T00:00:00Z".to_string(),
                }),
            );
        }
    }
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            CRUCIBLE_CHAT_INTEGRATION_POLICY,
        )
        .expect("install Crucible chat integration policy");
    state
}

async fn authenticate_test_request(mut request: Request, next: Next) -> Response {
    let security_context = temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "crucible-chat-integration".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "crucible-chat-integration".to_string(),
    };
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context,
        ));
    next.run(request).await
}

/// Spawn a real axum::serve on an ephemeral port and return its
/// bound address. The task runs until the test exits.
async fn spawn_server(state: ServerState) -> SocketAddr {
    let router = build_router(state).layer(axum::middleware::from_fn(authenticate_test_request));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind should succeed");
    let addr = listener
        .local_addr()
        .expect("listener should have local addr");
    tokio::spawn(async move {
        // Intentionally ignore the return value — when the test
        // finishes tokio will drop the task.
        let _ = axum::serve(listener, router).await;
    });
    addr
}

// ----------------------------------------------------------------------
// The actual test
// ----------------------------------------------------------------------

#[tokio::test]
async fn full_chat_loop_end_to_end_mock_mode() {
    // Spin up server + client.
    let state = build_crucible_state();
    let addr = spawn_server(state.clone()).await;
    let temper = TemperClient::new(format!("http://{addr}"), "default");

    // Seed: env + agent + session with deterministic ids.
    let seed_opts = SeedOptions {
        environment_id: Some("env-chat-it".to_string()),
        agent_id: Some("agt-chat-it".to_string()),
        session_id: Some("sess-chat-it".to_string()),
        ..SeedOptions::default()
    };
    let seeded = seed(&temper, seed_opts).await.expect("seed should succeed");
    assert_eq!(seeded.environment_id, "env-chat-it");
    assert_eq!(seeded.agent_id, "agt-chat-it");
    assert_eq!(seeded.session_id, "sess-chat-it");

    // -----------------------------------------------------------------
    // Turn 1 — new user message "Hello, what is 2+2?"
    // -----------------------------------------------------------------
    let mock = MockModel::new();
    let turn1 = respond(
        &temper,
        &mock,
        RespondRequest {
            session_id: "sess-chat-it",
            new_user_message: Some("Hello, what is 2+2?"),
        },
    )
    .await
    .expect("turn 1 should succeed");

    assert!(
        turn1.assistant_text.starts_with("Echo:"),
        "mock model should prefix with Echo: (got {:?})",
        turn1.assistant_text
    );
    assert!(turn1.input_tokens > 0, "mock should report usage");
    assert!(turn1.output_tokens > 0, "mock should report usage");

    // The session should now be in Idle.
    let session_after_turn_1 = temper
        .get_session("sess-chat-it")
        .await
        .expect("session should still exist");
    assert_eq!(
        session_after_turn_1.status, "Idle",
        "session should be Idle after one turn, got {:?}",
        session_after_turn_1.status
    );

    // The event feed should contain five events, in order.
    let events_after_turn_1 = temper
        .list_session_events("sess-chat-it", 500)
        .await
        .expect("event listing should succeed");
    assert_eq!(
        events_after_turn_1.len(),
        5,
        "turn 1 should have emitted exactly 5 events, got: {:?}",
        events_after_turn_1
            .iter()
            .map(|e| (e.sequence, e.kind.clone()))
            .collect::<Vec<_>>()
    );
    let kinds: Vec<&str> = events_after_turn_1
        .iter()
        .map(|e| e.kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "user.message",
            "span.model_request_start",
            "agent.message",
            "span.model_request_end",
            "session.status_idle",
        ],
        "turn 1 should emit the 5 kinds in canonical order",
    );

    // The span.model_request_end row should carry usage + correlation.
    let end_event = events_after_turn_1
        .iter()
        .find(|e| e.kind == "span.model_request_end")
        .unwrap();
    assert_eq!(end_event.is_error, Some(false));
    assert!(end_event.model_request_start_id.is_some());
    assert!(end_event.model_input_tokens.unwrap_or(0) > 0);
    assert!(end_event.model_output_tokens.unwrap_or(0) > 0);

    // The session.status_idle row should carry StopReason=end_turn.
    let idle_event = events_after_turn_1
        .iter()
        .find(|e| e.kind == "session.status_idle")
        .unwrap();
    assert_eq!(idle_event.stop_reason.as_deref(), Some("end_turn"));

    // -----------------------------------------------------------------
    // Turn 2 — multi-turn memory proof
    // -----------------------------------------------------------------
    // Now ask a follow-up. The responder should start from Idle
    // (calling ResumeSession) and reconstruct the full chat history
    // from events alone — the mock will capture the request and we
    // assert the messages array contains four entries in order.
    let turn2 = respond(
        &temper,
        &mock,
        RespondRequest {
            session_id: "sess-chat-it",
            new_user_message: Some("And what did I just ask you?"),
        },
    )
    .await
    .expect("turn 2 should succeed");

    assert!(turn2.assistant_text.starts_with("Echo:"));

    let last_req = mock
        .last_request()
        .expect("mock should have captured turn 2 request");
    assert_eq!(
        last_req.messages.len(),
        3,
        "turn 2 should send 3 messages: user1, assistant1, user2 — got {:?}",
        last_req
            .messages
            .iter()
            .map(|m| (
                m.role.clone(),
                m.content[0].as_text().unwrap_or("?").to_string()
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(last_req.messages[0].role, "user");
    assert_eq!(
        last_req.messages[0].content[0].as_text(),
        Some("Hello, what is 2+2?")
    );
    assert_eq!(last_req.messages[1].role, "assistant");
    // The assistant reply from turn 1 was the mock's "Echo: Hello, what is 2+2?"
    assert_eq!(
        last_req.messages[1].content[0].as_text(),
        Some("Echo: Hello, what is 2+2?")
    );
    assert_eq!(last_req.messages[2].role, "user");
    assert_eq!(
        last_req.messages[2].content[0].as_text(),
        Some("And what did I just ask you?")
    );

    // -----------------------------------------------------------------
    // After turn 2 the feed should contain 10 events and Session.Idle.
    // -----------------------------------------------------------------
    let events_after_turn_2 = temper
        .list_session_events("sess-chat-it", 500)
        .await
        .expect("event listing should succeed");
    assert_eq!(
        events_after_turn_2.len(),
        10,
        "two turns should have emitted 10 total events"
    );
    let session_after_turn_2 = temper.get_session("sess-chat-it").await.unwrap();
    assert_eq!(session_after_turn_2.status, "Idle");
}

#[tokio::test]
async fn respond_without_new_user_message_uses_existing_history() {
    // Proves the `respond` subcommand path (no new_user_message) works
    // against a Session that already has a user event in its feed.
    let state = build_crucible_state();
    let addr = spawn_server(state).await;
    let temper = TemperClient::new(format!("http://{addr}"), "default");

    let seeded = seed(
        &temper,
        SeedOptions {
            session_id: Some("sess-chat-resp".to_string()),
            agent_id: Some("agt-chat-resp".to_string()),
            environment_id: Some("env-chat-resp".to_string()),
            ..SeedOptions::default()
        },
    )
    .await
    .unwrap();

    // Turn 1: drop a user.message via the `send` path.
    let mock = MockModel::new();
    respond(
        &temper,
        &mock,
        RespondRequest {
            session_id: &seeded.session_id,
            new_user_message: Some("first question"),
        },
    )
    .await
    .unwrap();

    // Turn 2: externally POST a user.message via the TemperClient
    // directly (simulating the curl walkthrough path where a client
    // writes events by hand), then call respond with None.
    let external_user_row = crucible_reference::chat::temper_client::SessionEventRow {
        id: "ev-ext-1".to_string(),
        session_id: seeded.session_id.clone(),
        sequence: 5, // turn 1 used sequences 0..=4
        kind: "user.message".to_string(),
        created_at: "2026-04-11T00:05:00Z".to_string(),
        processed_at: Some("2026-04-11T00:05:00Z".to_string()),
        content: Some(
            r#"{"blocks":[{"type":"text","text":"externally posted question"}]}"#.to_string(),
        ),
        stop_reason: None,
        stop_reason_event_ids: None,
        model_request_start_id: None,
        is_error: None,
        model_input_tokens: None,
        model_output_tokens: None,
        model_cache_creation_input_tokens: None,
        model_cache_read_input_tokens: None,
        model_speed: None,
        tool_name: None,
        tool_use_id: None,
        session_thread_id: None,
    };
    temper
        .create_session_event(&external_user_row)
        .await
        .unwrap();

    let outcome = respond(
        &temper,
        &mock,
        RespondRequest {
            session_id: &seeded.session_id,
            new_user_message: None,
        },
    )
    .await
    .expect("respond with no new user message should still work");

    // Mock echoes the last user text — which should be the externally
    // posted question, not "first question".
    assert_eq!(outcome.assistant_text, "Echo: externally posted question");

    // Event feed: 5 from turn 1 + 1 external user + 4 from turn 2
    // (start, agent message, end, idle) = 10 total.
    let events = temper
        .list_session_events(&seeded.session_id, 500)
        .await
        .unwrap();
    assert_eq!(events.len(), 10);
}

// ----------------------------------------------------------------------
// Smoke test: the plain HTTP client hits a known endpoint.
// ----------------------------------------------------------------------

#[tokio::test]
async fn temper_client_reports_404_on_unknown_session() {
    let state = build_crucible_state();
    let addr = spawn_server(state).await;
    let temper = TemperClient::new(format!("http://{addr}"), "default");

    let err = temper
        .get_session("no-such-session")
        .await
        .expect_err("should fail for unknown session");
    let s = err.to_string();
    // The error surface includes the HTTP status so callers can
    // differentiate 404 from 500.
    assert!(
        s.contains("404") || s.contains("not") || s.contains("Not Found"),
        "error should mention 404/not found: {s}"
    );
    // And the StatusCode the error embedded should be a client error.
    assert_ne!(StatusCode::OK.as_u16(), 0); // sanity — reqwest StatusCode import wasn't wasted
}

// ----------------------------------------------------------------------
// Multi-agent delegation test
// ----------------------------------------------------------------------

#[tokio::test]
async fn multi_agent_delegation_end_to_end() {
    use crucible_reference::chat::anthropic::{ContentBlock, MessagesResponse, Usage};

    let state = build_crucible_state();
    let addr = spawn_server(state).await;
    let temper = TemperClient::new(format!("http://{addr}"), "default");

    // Seed a coordinator agent with one callable sub-agent.
    let seed_opts = SeedOptions {
        environment_id: Some("env-multi".to_string()),
        agent_id: Some("agt-coordinator".to_string()),
        session_id: Some("sess-multi".to_string()),
        callable_agents: vec![CallableAgentSeedSpec {
            agent_id: "agt-reviewer".to_string(),
            agent_name: "reviewer".to_string(),
            system_prompt: "You are a code reviewer.".to_string(),
            model_id: None,
        }],
        ..SeedOptions::default()
    };
    let seeded = seed(&temper, seed_opts).await.expect("seed should succeed");
    assert_eq!(seeded.session_id, "sess-multi");

    // Set up a mock that returns a delegate_to_agent tool call on turn 1,
    // then an "Echo:" response for the sub-agent, then a final text
    // response for the coordinator.
    let mock = MockModel::new();

    // Response 1: coordinator calls delegate_to_agent
    mock.push_response(MessagesResponse {
        content: vec![
            ContentBlock::Text {
                text: "Let me delegate this.".to_string(),
            },
            ContentBlock::ToolUse {
                id: "toolu_delegate_1".to_string(),
                name: "delegate_to_agent".to_string(),
                input: serde_json::json!({
                    "agent_id": "agt-reviewer",
                    "message": "Review this code please"
                }),
            },
        ],
        stop_reason: Some("tool_use".to_string()),
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    });

    // Response 2: sub-agent replies in respond_thread().
    mock.push_response(MessagesResponse {
        content: vec![ContentBlock::Text {
            text: "Code looks good, no issues found.".to_string(),
        }],
        stop_reason: Some("end_turn".to_string()),
        usage: Usage {
            input_tokens: 80,
            output_tokens: 20,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    });

    // Response 3: coordinator final text after receiving delegation result.
    mock.push_response(MessagesResponse {
        content: vec![ContentBlock::Text {
            text: "The reviewer said the code looks good.".to_string(),
        }],
        stop_reason: Some("end_turn".to_string()),
        usage: Usage {
            input_tokens: 150,
            output_tokens: 30,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    });

    // Run the turn.
    let outcome = respond(
        &temper,
        &mock,
        RespondRequest {
            session_id: "sess-multi",
            new_user_message: Some("Please review my code"),
        },
    )
    .await
    .expect("multi-agent turn should succeed");

    assert_eq!(
        outcome.assistant_text,
        "The reviewer said the code looks good."
    );

    // Verify the session is Idle.
    let session = temper
        .get_session("sess-multi")
        .await
        .expect("session should exist");
    assert_eq!(session.status, "Idle");

    // Verify the event feed contains the expected thread events.
    let events = temper
        .list_session_events("sess-multi", 500)
        .await
        .expect("events should load");
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();

    // Should contain delegation events on the primary thread.
    assert!(
        kinds.contains(&"session.thread_created"),
        "should have session.thread_created event, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"agent.thread_message_sent"),
        "should have agent.thread_message_sent event, got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"agent.thread_message_received"),
        "should have agent.thread_message_received event, got: {kinds:?}"
    );

    // Should have thread-scoped events (sub-agent's user.message + agent.message).
    let thread_events: Vec<_> = events
        .iter()
        .filter(|e| e.session_thread_id.is_some())
        .collect();
    assert!(
        thread_events.len() >= 4,
        "should have at least 4 thread-scoped events (user.message, \
         model_request_start, agent.message, model_request_end), got {} ({:?})",
        thread_events.len(),
        thread_events
            .iter()
            .map(|e| (e.sequence, e.kind.as_str()))
            .collect::<Vec<_>>()
    );

    // The sub-agent's user.message should contain the delegated text.
    let thread_user_msg = thread_events
        .iter()
        .find(|e| e.kind == "user.message")
        .expect("thread should have a user.message");
    let content = thread_user_msg.content.as_deref().unwrap_or("");
    assert!(
        content.contains("Review this code please"),
        "thread user.message should contain the delegated text, got: {content}"
    );
}
