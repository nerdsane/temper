//! Unit tests for MCP runtime (ARN-222).

use super::*;

    use axum::{Router, extract::State, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn extract_trajectory_actions_from_temper_action_calls() {
        let code = r#"
result = temper.action("Issue", "issue-1", "PromoteToCritical", {"Reason": "prod incident"})
other = temper.action('Issue', 'issue-1', 'Assign', {'AgentId': 'agent-2'})
tenant = temper.action("gepa-tenant", "Issues", "11111111-1111-1111-1111-111111111111", "Reassign", {"NewAssigneeId": "agent-3"})
"#;

        let actions = extract_trajectory_actions_from_code(code);
        assert_eq!(actions.len(), 3);
        assert_eq!(
            actions[0].get("action").and_then(Value::as_str),
            Some("PromoteToCritical")
        );
        assert_eq!(
            actions[2]
                .get("params")
                .and_then(Value::as_object)
                .and_then(|m| m.get("NewAssigneeId"))
                .and_then(Value::as_str),
            Some("agent-3")
        );
    }

    #[test]
    fn normalize_python_literals_to_json() {
        let value = parse_python_json_value("{'enabled': True, 'reason': None, 'count': 2}")
            .expect("python dict should parse");
        assert_eq!(value["enabled"], serde_json::json!(true));
        assert_eq!(value["reason"], serde_json::Value::Null);
        assert_eq!(value["count"], serde_json::json!(2));
    }

    #[test]
    fn extract_temper_call_metadata_tracks_tenant_and_entity() {
        let code = r#"
await temper.action("tenant-a", "Issue", "i-1", "Assign", {"AgentId": "agent-1"})
await temper.create("tenant-b", "Task", {"Title": "x"})
"#;
        let metadata = extract_temper_call_metadata(code);
        assert!(
            metadata.iter().any(|m| {
                m.tenant.as_deref() == Some("tenant-a") && m.entity_type.as_deref() == Some("Issue")
            }),
            "expected tenant-a/Issue metadata"
        );
        assert!(
            metadata.iter().any(|m| {
                m.tenant.as_deref() == Some("tenant-b") && m.entity_type.as_deref() == Some("Task")
            }),
            "expected tenant-b/Task metadata"
        );
    }

    #[tokio::test]
    async fn finalize_trajectory_retries_retryable_ots_upload_failure() {
        async fn handler(State(attempts): State<Arc<AtomicUsize>>) -> StatusCode {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::ACCEPTED
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/api/ots/trajectories", post(handler))
            .with_state(attempts.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test server");
        });

        let mut ctx = RuntimeContext {
            base_url: format!("http://{addr}"),
            http: reqwest::Client::new(),
            agent_id: Some("agent".to_string()),
            agent_type: Some("test-agent".to_string()),
            session_id: Some("session".to_string()),
            api_key: None,
            identity_tenant: "tenant".to_string(),
            sandbox: temper_sandbox::runner::PersistentSandbox::new(&[("temper", "Temper", 1)]),
            trajectory: None,
            tenants_seen: BTreeMap::new(),
            entity_types_seen: BTreeMap::new(),
        };
        ctx.init_trajectory();

        ctx.finalize_trajectory().await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn char_safe_truncate_handles_multibyte() {
        let s = "日本語テスト";
        let out = char_safe_truncate(s, 2);
        assert!(out.starts_with("日本"), "{out}");
        assert!(out.contains("[truncated]"), "{out}");
        // Must not panic and must be valid UTF-8.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn primary_tenant_is_identity_not_majority() {
        let mut ctx = RuntimeContext {
            base_url: "http://127.0.0.1".into(),
            http: reqwest::Client::new(),
            agent_id: None,
            agent_type: None,
            session_id: None,
            api_key: None,
            identity_tenant: "bound-tenant".into(),
            sandbox: temper_sandbox::runner::PersistentSandbox::new(&[]),
            trajectory: None,
            tenants_seen: BTreeMap::from([
                ("other".into(), 99usize),
                ("bound-tenant".into(), 1usize),
            ]),
            entity_types_seen: BTreeMap::new(),
        };
        assert_eq!(ctx.primary_tenant(), "bound-tenant");
        // silence mut warning if any
        let _ = &mut ctx;
    }