use super::*;

const RELEASE_READ_POLICY: &str = r#"
permit(
  principal is Agent,
  action == Action::"read_policies",
  resource == PolicySet::"default"
) when {
  principal.agent_type == "release" &&
  principal.agentTypeVerified == true
};
"#;

fn release() -> SecurityContext {
    SecurityContext::from_resolved_identity("release", "release", None)
}

fn operator() -> SecurityContext {
    SecurityContext::from_resolved_identity("operator", "operator", None)
}

async fn status_of(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
    who: SecurityContext,
) -> StatusCode {
    app.clone()
        .oneshot(request_with_context(
            method,
            uri,
            Body::from(body.to_string()),
            TENANT,
            who,
        ))
        .await
        .expect("policy request runs")
        .status()
}

/// `read_policies` lets a caller list the tenant's Cedar rows and nothing
/// more: every write still needs `manage_policies`, which still lists too.
#[tokio::test]
async fn read_policies_lists_rows_but_cannot_change_them() {
    let (state, store, _temp) = policy_state().await;
    let live = format!("{OPERATOR_POLICY}\n{RELEASE_READ_POLICY}");
    store
        .save_policy(TENANT, "primary", &live, "system")
        .await
        .expect("seed primary row");
    state
        .authz
        .reload_tenant_policies(TENANT, &live)
        .expect("install operator and release policies");
    let app = build_router(state);
    let list = "/api/tenants/default/policies/list";

    let listed = app
        .clone()
        .oneshot(request_with_context(
            "GET",
            list,
            Body::empty(),
            TENANT,
            release(),
        ))
        .await
        .expect("release list runs");
    assert_eq!(listed.status(), StatusCode::OK);
    let body = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .expect("list body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("list is JSON");
    assert_eq!(body["total"], 1, "release sees the tenant's rows: {body}");
    assert_eq!(body["policies"][0]["policy_id"], "primary");

    let rule =
        serde_json::json!({ "policy_id": "release-made", "cedar_text": RELEASE_READ_POLICY })
            .to_string();
    let writes = [
        (
            "POST",
            "/api/tenants/default/policies/create",
            rule.as_str(),
        ),
        (
            "PATCH",
            "/api/tenants/default/policies/entry/primary",
            r#"{"enabled":false}"#,
        ),
        ("DELETE", "/api/tenants/default/policies/entry/primary", ""),
    ];
    for (method, uri, body) in writes {
        assert_eq!(
            status_of(&app, method, uri, body, release()).await,
            StatusCode::FORBIDDEN,
            "read_policies alone must not {method} {uri}"
        );
    }
    let rows = store
        .load_policies_for_tenant(TENANT)
        .await
        .expect("rows after refused writes");
    assert_eq!(rows.len(), 1, "refused writes changed nothing: {rows:?}");
    assert!(rows[0].enabled && rows[0].cedar_text == live);

    assert_eq!(
        status_of(&app, "GET", list, "", operator()).await,
        StatusCode::OK,
        "manage_policies still lists"
    );
    let stranger = SecurityContext::from_resolved_identity("stranger", "stranger", None);
    assert_eq!(
        status_of(&app, "GET", list, "", stranger).await,
        StatusCode::FORBIDDEN,
        "listing still needs read_policies or manage_policies"
    );
}
