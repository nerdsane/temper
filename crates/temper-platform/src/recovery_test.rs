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
async fn recovered_denial_names_the_source_file() {
    // ARN-286: a restart must not flatten policy provenance. After
    // recovery a denial still has to name the file and statement, so an
    // operator reading the decision log can open the policy that caused it.
    let store = TursoEventStore::new(&sqlite_test_url("named-denial"), None)
        .await
        .expect("create test store");
    store
        .save_policy(
            "katagami",
            "katagami-commons/art_style.cedar",
            concat!(
                "permit(principal is Customer, action == Action::\"read\", resource is ArtStyle);\n",
                "forbid(principal is Customer, action == Action::\"update\", resource is ArtStyle);\n",
            ),
            "test",
        )
        .await
        .expect("save granular policy");

    let state = PlatformState::new(None);
    recover_cedar_policies(&state, &store).await;

    let mut resource_attrs = HashMap::new();
    resource_attrs.insert("id".to_string(), serde_json::json!("as-1"));

    let mut ctx = SecurityContext::from_headers(&[]);
    ctx.principal.kind = temper_authz::PrincipalKind::Customer;
    ctx.principal.id = "user-1".to_string();

    let decision = state.server.authz.authorize_for_tenant(
        "katagami",
        &ctx,
        "update",
        "ArtStyle",
        &resource_attrs,
    );

    let temper_authz::AuthzDecision::Deny(denial) = decision else {
        panic!("forbid statement should deny the update, got {decision:?}");
    };
    assert_eq!(
        denial.to_string(),
        "denied by policy: katagami-commons/art_style.cedar#2"
    );
}

#[tokio::test]
async fn legacy_blob_does_not_duplicate_what_granular_rows_already_carry() {
    // Installs write both an aggregate tenant blob and per-file rows. Left
    // alone, recovery loads the app's statements twice — and a denial then
    // cites an unlabelled copy alongside the file it came from.
    let store = TursoEventStore::new(&sqlite_test_url("blob-dedup"), None)
        .await
        .expect("create test store");
    let app_policy =
        r#"forbid(principal is Customer, action == Action::"update", resource is ArtStyle);"#;
    let other_policy =
        r#"permit(principal is Customer, action == Action::"read", resource is ArtStyle);"#;

    store
        .upsert_tenant_policy("katagami", &format!("{other_policy}\n{app_policy}"))
        .await
        .expect("save legacy blob");
    store
        .save_policy(
            "katagami",
            "katagami-commons/art_style.cedar",
            app_policy,
            "test",
        )
        .await
        .expect("save granular policy");

    let state = PlatformState::new(None);
    recover_cedar_policies(&state, &store).await;

    let tenant_text = state
        .server
        .authz
        .get_tenant_policy_text("katagami")
        .expect("tenant policy text");
    assert_eq!(
        tenant_text.matches(app_policy).count(),
        1,
        "the app's statement must be loaded exactly once: {tenant_text}"
    );
    assert!(
        tenant_text.contains(other_policy),
        "statements only the blob carries must survive: {tenant_text}"
    );

    let mut resource_attrs = HashMap::new();
    resource_attrs.insert("id".to_string(), serde_json::json!("as-1"));
    let decision = state.server.authz.authorize_for_tenant(
        "katagami",
        &SecurityContext::from_headers(&[]),
        "update",
        "ArtStyle",
        &resource_attrs,
    );
    let temper_authz::AuthzDecision::Deny(denial) = decision else {
        panic!("forbid statement should deny the update, got {decision:?}");
    };
    assert_eq!(
        denial.to_string(),
        "denied by policy: katagami-commons/art_style.cedar#1",
        "the denial must cite the file, not an unlabelled duplicate"
    );
}
