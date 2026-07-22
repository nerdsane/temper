use std::collections::HashMap;

use temper_authz::SecurityContext;
use temper_store_turso::TursoEventStore;

use super::*;

fn sqlite_test_url(test_name: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "temper-recovery-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    format!("file:{}", path.display())
}

#[tokio::test]
async fn recover_cedar_policies_activates_granular_policy_rows() {
    let store = TursoEventStore::new(&sqlite_test_url("granular-policies"), None)
        .await
        .expect("create test store");
    let policy = r#"
permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  context.module == "build_session_message"
};
"#;
    store
        .save_policy("default", "katagami-curation-wasm", policy, "test")
        .await
        .expect("save granular policy");

    let state = PlatformState::new(None);
    recover_cedar_policies(&state, &store).await;

    let mut resource_attrs = HashMap::new();
    resource_attrs.insert(
        "id".to_string(),
        serde_json::json!("__trigger__:Submit:build_session_message"),
    );
    resource_attrs.insert(
        "module".to_string(),
        serde_json::json!("build_session_message"),
    );

    let decision = state.server.authz.authorize_for_tenant(
        "default",
        &SecurityContext::from_resolved_identity("wasm-module", "wasm_module", None),
        "http_call",
        "HttpEndpoint",
        &resource_attrs,
    );

    assert!(
        decision.is_allowed(),
        "granular policy rows should be active after recovery, got {decision:?}"
    );
    assert!(
        state
            .server
            .authz
            .get_tenant_policy_text("default")
            .expect("tenant policy text")
            .contains("build_session_message")
    );
}

#[tokio::test]
async fn recover_cedar_policies_prefers_primary_row_over_legacy_blob() {
    let store = TursoEventStore::new(&sqlite_test_url("primary-policy-recovery"), None)
        .await
        .expect("create test store");
    store
        .upsert_tenant_policy(
            "default",
            r#"permit(principal, action == Action::"legacy_only", resource);"#,
        )
        .await
        .expect("save legacy policy");
    store
        .save_policy(
            "default",
            "primary",
            r#"permit(principal, action == Action::"read", resource);"#,
            "test",
        )
        .await
        .expect("save primary policy");
    store
        .save_policy(
            "default",
            "katagami-curation-wasm",
            r#"
permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  context.module == "build_session_message"
};
"#,
            "test",
        )
        .await
        .expect("save granular policy");

    let state = PlatformState::new(None);
    recover_cedar_policies(&state, &store).await;

    let tenant_text = state
        .server
        .authz
        .get_tenant_policy_text("default")
        .expect("tenant policy text");
    assert!(
        tenant_text.contains("build_session_message"),
        "granular app policy should be appended to primary policy"
    );
    assert!(
        !tenant_text.contains("legacy_only"),
        "legacy blob should be skipped when durable primary policy row exists"
    );
}

#[tokio::test]
async fn recover_cedar_policies_prefers_any_granular_generation_over_legacy_cache() {
    let store = TursoEventStore::new(&sqlite_test_url("owned-policy-recovery"), None)
        .await
        .expect("create test store");
    store
        .upsert_tenant_policy(
            "default",
            r#"permit(principal, action == Action::"legacy_only", resource);"#,
        )
        .await
        .expect("save legacy compatibility cache");
    store
        .save_policy(
            "default",
            "owner-a",
            r#"permit(principal, action == Action::"granular_only", resource);"#,
            "os-app:owner-a",
        )
        .await
        .expect("save canonical owned row");

    let state = PlatformState::new(None);
    recover_cedar_policies(&state, &store).await;

    let tenant_text = state
        .server
        .authz
        .get_tenant_policy_text("default")
        .expect("tenant policy text");
    assert!(tenant_text.contains("granular_only"));
    assert!(
        !tenant_text.contains("legacy_only"),
        "restart must not revive a compatibility aggregate once any granular generation exists"
    );
}

#[tokio::test]
async fn recover_cedar_policies_clears_previously_active_generation_when_all_rows_disabled() {
    let store = TursoEventStore::new(&sqlite_test_url("disabled-policy-recovery"), None)
        .await
        .expect("create test store");
    let legacy = r#"permit(principal, action == Action::"legacy_only", resource);"#;
    store
        .upsert_tenant_policy("default", legacy)
        .await
        .expect("save legacy compatibility cache");
    store
        .save_policy(
            "default",
            "owner-a",
            r#"permit(principal, action == Action::"granular_only", resource);"#,
            "os-app:owner-a",
        )
        .await
        .expect("save canonical owned row");
    store
        .toggle_policy_enabled("default", "owner-a", false)
        .await
        .expect("disable canonical row");

    let state = PlatformState::new(None);
    state
        .server
        .authz
        .reload_tenant_policies("default", legacy)
        .expect("preload previous generation");
    state
        .server
        .tenant_policies
        .write()
        .expect("policy cache lock")
        .insert("default".to_string(), legacy.to_string());

    recover_cedar_policies(&state, &store).await;

    assert_eq!(
        state
            .server
            .authz
            .get_tenant_policy_text("default")
            .unwrap_or_default(),
        "",
        "canonical empty generation must revoke the previously active policy"
    );
    assert_eq!(
        state
            .server
            .tenant_policies
            .read()
            .expect("policy cache lock")
            .get("default")
            .map(String::as_str),
        Some("")
    );
}
