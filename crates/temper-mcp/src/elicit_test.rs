//! Unit tests for the elicitation protocol pieces (`elicit.rs`).

use super::*;
use serde_json::json;

#[test]
fn is_client_response_classifies_messages() {
    // A response to a server→client request: id + result, no method.
    assert!(is_client_response(&json!({
        "jsonrpc": "2.0", "id": 1, "result": {"action": "accept"}
    })));
    // An error response is also a response.
    assert!(is_client_response(&json!({
        "jsonrpc": "2.0", "id": 1, "error": {"code": -1, "message": "no"}
    })));
    // A client request has a method — never routed as a response.
    assert!(!is_client_response(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {}
    })));
    // A notification has no id.
    assert!(!is_client_response(&json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    })));
    // A null id is not a correlatable response.
    assert!(!is_client_response(&json!({
        "jsonrpc": "2.0", "id": null, "result": {}
    })));
}

#[tokio::test]
async fn client_requester_correlates_response_by_id() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pending = PendingClientRequests::default();
    let requester = ClientRequester::new(tx, pending.clone());

    let request_fut = requester.request(
        "elicitation/create",
        json!({"message": "hi"}),
        std::time::Duration::from_secs(5),
    );
    let answer = async {
        let sent = rx.recv().await.expect("request emitted");
        assert_eq!(sent["method"], "elicitation/create");
        assert_eq!(sent["jsonrpc"], "2.0");
        let id = sent["id"].clone();
        // An unrelated id does not resolve the request.
        assert!(!pending.resolve(json!({"jsonrpc": "2.0", "id": 999_999, "result": {}})));
        assert!(pending.resolve(json!({
            "jsonrpc": "2.0", "id": id, "result": {"action": "decline"}
        })));
    };
    let (response, ()) = tokio::join!(request_fut, answer);
    let response = response.expect("correlated response");
    assert_eq!(response["result"]["action"], "decline");
}

#[tokio::test]
async fn client_requester_times_out_and_clears_pending() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let pending = PendingClientRequests::default();
    let requester = ClientRequester::new(tx, pending.clone());

    let result = requester
        .request(
            "elicitation/create",
            json!({}),
            std::time::Duration::from_millis(10),
        )
        .await;
    assert_eq!(result.unwrap_err(), ClientRequestError::Timeout);
    // The pending slot is cleared: a late response no longer matches.
    assert!(!pending.resolve(json!({"jsonrpc": "2.0", "id": 1, "result": {}})));
}

#[tokio::test]
async fn client_requester_reports_closed_channel() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    let requester = ClientRequester::new(tx, PendingClientRequests::default());
    let result = requester
        .request(
            "elicitation/create",
            json!({}),
            std::time::Duration::from_secs(1),
        )
        .await;
    assert_eq!(result.unwrap_err(), ClientRequestError::Closed);
}

#[test]
fn denial_extraction_requires_status_and_decision_id() {
    let denial = json!({
        "status": "authorization_denied",
        "decision_id": "PD-abc",
        "reason": "Cedar denied: CancelOrder on Order('o1')",
    });
    let extracted = denial_from_dispatch_value("demo", &denial).expect("denial");
    assert_eq!(extracted.tenant, "demo");
    assert_eq!(extracted.decision_id, "PD-abc");
    assert!(extracted.reason.contains("CancelOrder"));

    // pending_decision is accepted as the id field too.
    let alt = json!({"status": "authorization_denied", "pending_decision": "PD-x"});
    assert_eq!(
        denial_from_dispatch_value("demo", &alt)
            .expect("denial")
            .decision_id,
        "PD-x"
    );

    // A denial without a decision id is not actionable.
    assert!(
        denial_from_dispatch_value("demo", &json!({"status": "authorization_denied"})).is_none()
    );
    // Ordinary results are ignored.
    assert!(denial_from_dispatch_value("demo", &json!({"status": "ok"})).is_none());
    assert!(denial_from_dispatch_value("demo", &json!("text")).is_none());
}

#[test]
fn elicitation_params_shape_matches_mcp_spec() {
    let denial = DeniedDecision {
        tenant: "demo".to_string(),
        decision_id: "PD-abc".to_string(),
        reason: "Cedar denied: CancelOrder on Order('o1')".to_string(),
    };
    let params = elicitation_params(&denial, Some("checkout-bot"));

    let message = params["message"].as_str().expect("message");
    assert!(message.contains("PD-abc"));
    assert!(message.contains("demo"));
    assert!(message.contains("checkout-bot"));
    assert!(message.contains("CancelOrder"));

    // Restricted flat-object schema with a single enum string field.
    assert_eq!(params["requestedSchema"]["type"], "object");
    assert_eq!(params["requestedSchema"]["required"], json!(["decision"]));
    let field = &params["requestedSchema"]["properties"]["decision"];
    assert_eq!(field["type"], "string");
    assert_eq!(
        field["enum"],
        json!(["approve_narrow", "approve_broad", "deny", "leave_pending"])
    );
    assert_eq!(
        field["enumNames"].as_array().map(|names| names.len()),
        Some(4),
        "each enum value needs a display name"
    );
}

#[test]
fn parse_elicit_choice_fails_closed() {
    let accept = |decision: &str| {
        json!({"jsonrpc": "2.0", "id": 1, "result": {
            "action": "accept", "content": {"decision": decision}
        }})
    };
    assert_eq!(
        parse_elicit_choice(&accept("approve_narrow")),
        Some(ElicitChoice::ApproveNarrow)
    );
    assert_eq!(
        parse_elicit_choice(&accept("approve_broad")),
        Some(ElicitChoice::ApproveBroad)
    );
    assert_eq!(
        parse_elicit_choice(&accept("deny")),
        Some(ElicitChoice::Deny)
    );
    assert_eq!(
        parse_elicit_choice(&accept("leave_pending")),
        Some(ElicitChoice::LeavePending)
    );

    // Decline and cancel never resolve the decision.
    let decline = json!({"jsonrpc": "2.0", "id": 1, "result": {"action": "decline"}});
    assert_eq!(parse_elicit_choice(&decline), None);
    let cancel = json!({"jsonrpc": "2.0", "id": 1, "result": {"action": "cancel"}});
    assert_eq!(parse_elicit_choice(&cancel), None);

    // Accept with garbage content, missing content, or an unknown choice
    // stays unresolved — never a default approval.
    assert_eq!(parse_elicit_choice(&accept("approve_everything")), None);
    let no_content = json!({"jsonrpc": "2.0", "id": 1, "result": {"action": "accept"}});
    assert_eq!(parse_elicit_choice(&no_content), None);
    let error = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "no"}});
    assert_eq!(parse_elicit_choice(&error), None);
}

#[test]
fn scope_json_matches_policy_scope_matrix() {
    use temper_authz::{ActionScope, PolicyScopeMatrix, PrincipalScope, ResourceScope};

    let narrow: PolicyScopeMatrix =
        serde_json::from_value(narrow_scope()).expect("narrow scope deserializes");
    assert_eq!(narrow.principal, PrincipalScope::ThisAgent);
    assert_eq!(narrow.action, ActionScope::ThisAction);
    assert_eq!(narrow.resource, ResourceScope::ThisResource);
    temper_authz::validate_policy_scope_matrix(&narrow).expect("narrow scope valid");

    let broad: PolicyScopeMatrix =
        serde_json::from_value(broad_scope()).expect("broad scope deserializes");
    assert_eq!(broad.principal, PrincipalScope::ThisAgent);
    assert_eq!(broad.action, ActionScope::AllActionsOnType);
    assert_eq!(broad.resource, ResourceScope::AnyOfType);
    temper_authz::validate_policy_scope_matrix(&broad).expect("broad scope valid");
}

#[test]
fn elicit_flag_defaults_enabled_and_honors_off_switch() {
    assert!(elicit_flag_enabled(None));
    assert!(elicit_flag_enabled(Some("1")));
    assert!(elicit_flag_enabled(Some("yes")));
    assert!(!elicit_flag_enabled(Some("0")));
    assert!(!elicit_flag_enabled(Some("false")));
    assert!(!elicit_flag_enabled(Some(" FALSE ")));
    assert!(!elicit_flag_enabled(Some("off")));
    assert!(!elicit_flag_enabled(Some("no")));
}

#[test]
fn elicit_timeout_parses_with_default() {
    assert_eq!(elicit_timeout_secs(None), 120);
    assert_eq!(elicit_timeout_secs(Some("30")), 30);
    assert_eq!(
        elicit_timeout_secs(Some("0")),
        120,
        "zero falls back to default"
    );
    assert_eq!(elicit_timeout_secs(Some("nope")), 120);
}

#[test]
fn annotate_merges_into_object_and_wraps_scalars() {
    let annotation = json!({"approval": "granted by human via elicitation", "decision_id": "PD-1"});

    // Object results get the annotation merged in.
    let merged = annotate_tool_result(
        Ok(r#"{"status":"authorization_denied","decision_id":"PD-1"}"#.to_string()),
        annotation.clone(),
    )
    .expect("ok");
    let merged: Value = serde_json::from_str(&merged).expect("json");
    assert_eq!(merged["status"], "authorization_denied");
    assert_eq!(merged["approval"], "granted by human via elicitation");

    // Non-object results are wrapped, keeping the annotation top-level.
    let wrapped =
        annotate_tool_result(Ok("\"just text\"".to_string()), annotation.clone()).expect("ok");
    let wrapped: Value = serde_json::from_str(&wrapped).expect("json");
    assert_eq!(wrapped["result"], "just text");
    assert_eq!(wrapped["decision_id"], "PD-1");

    // Error results stay errors with the annotation appended.
    let err = annotate_tool_result(Err(anyhow::anyhow!("KeyError: 'x'")), annotation)
        .expect_err("stays an error");
    let text = err.to_string();
    assert!(text.contains("KeyError"));
    assert!(text.contains("human-elicitation"));
    assert!(text.contains("PD-1"));
}
