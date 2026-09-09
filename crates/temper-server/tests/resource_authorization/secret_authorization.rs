use super::*;

#[tokio::test]
async fn secret_crud_uses_manage_secrets_for_exact_tenant_and_key() {
    let (mut state, _store, _temp) = state_with_turso("secret-resource-auth").await;
    state.secrets_vault = Some(std::sync::Arc::new(SecretsVault::new(&[12_u8; 32])));
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"
permit(
  principal == Customer::"secret-manager",
  action == Action::"manage_secrets",
  resource
) when {
  resource == Secret::"api-key" || resource == Secret::"__keys__"
};
"#,
        )
        .expect("secret policy should parse");
    let state_for_check = state.clone();
    let app = build_router(state);

    let denied = app
        .clone()
        .oneshot(json_request(
            Method::PUT,
            "/api/tenants/default/secrets/api-key",
            serde_json::json!({"value": "denied"}),
            TENANT,
            "intruder",
        ))
        .await
        .expect("denied secret request should run");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let wrong_key = app
        .clone()
        .oneshot(json_request(
            Method::PUT,
            "/api/tenants/default/secrets/other-key",
            serde_json::json!({"value": "denied"}),
            TENANT,
            "secret-manager",
        ))
        .await
        .expect("wrong-key secret request should run");
    assert_eq!(wrong_key.status(), StatusCode::FORBIDDEN);

    let wrong_tenant = app
        .clone()
        .oneshot(json_request(
            Method::PUT,
            "/api/tenants/default/secrets/api-key",
            serde_json::json!({"value": "denied"}),
            "other-tenant",
            "secret-manager",
        ))
        .await
        .expect("wrong-tenant secret request should run");
    assert_eq!(wrong_tenant.status(), StatusCode::UNAUTHORIZED);

    let allowed = app
        .clone()
        .oneshot(json_request(
            Method::PUT,
            "/api/tenants/default/secrets/api-key",
            serde_json::json!({"value": "stored-value"}),
            TENANT,
            "secret-manager",
        ))
        .await
        .expect("allowed secret request should run");
    assert_eq!(allowed.status(), StatusCode::NO_CONTENT);

    let listed = app
        .clone()
        .oneshot(json_request(
            Method::GET,
            "/api/tenants/default/secrets",
            serde_json::Value::Null,
            TENANT,
            "secret-manager",
        ))
        .await
        .expect("list secret request should run");
    assert_eq!(listed.status(), StatusCode::OK);

    let denied_delete = app
        .clone()
        .oneshot(json_request(
            Method::DELETE,
            "/api/tenants/default/secrets/api-key",
            serde_json::Value::Null,
            TENANT,
            "intruder",
        ))
        .await
        .expect("denied delete request should run");
    assert_eq!(denied_delete.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        state_for_check
            .secrets_vault
            .as_ref()
            .and_then(|vault| vault.get_secret(TENANT, "api-key"))
            .as_deref(),
        Some("stored-value")
    );

    let deleted = app
        .oneshot(json_request(
            Method::DELETE,
            "/api/tenants/default/secrets/api-key",
            serde_json::Value::Null,
            TENANT,
            "secret-manager",
        ))
        .await
        .expect("allowed delete request should run");
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}
