use super::*;
use std::sync::Arc;
use temper_server::storage::{PolicyStore, PolicyStoreRow};

// The real store supplies reads; the environment refuses each attempted write.
struct FailedWrites(TursoEventStore);

#[async_trait::async_trait]
impl PolicyStore for FailedWrites {
    async fn save_policy(&self, _: &str, _: &str, _: &str, _: &str) -> Result<bool, String> {
        Err("injected policy write failure".into())
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        PolicyStore::load_policies_for_tenant(&self.0, tenant).await
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        panic!("unexpected cross-tenant read")
    }

    async fn toggle_policy_enabled(&self, _: &str, _: &str, _: bool) -> Result<bool, String> {
        panic!("unexpected enable mutation")
    }

    async fn update_policy_text(&self, _: &str, _: &str, _: &str, _: &str) -> Result<bool, String> {
        panic!("unexpected update")
    }

    async fn replace_policy_if_hash(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<bool, String> {
        panic!("unexpected conditional replacement")
    }

    async fn replace_tenant_policies(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), String> {
        panic!("unexpected whole-set replacement")
    }

    async fn delete_policy(&self, _: &str, _: &str) -> Result<(), String> {
        panic!("unexpected delete")
    }
}

fn policy_request(method: &str, route: &str, body: serde_json::Value) -> Request<Body> {
    request_with_context(
        method,
        &format!("/api/tenants/default/policies/{route}"),
        Body::from(body.to_string()),
        TENANT,
        SecurityContext::from_resolved_identity("operator", "operator", None),
    )
}

async fn seed(state: &ServerState, store: &TursoEventStore) -> String {
    let base = format!("{OPERATOR_POLICY}\n{GUARD_PERMIT}");
    store
        .save_policy(TENANT, "base", &base, "system")
        .await
        .unwrap();
    store
        .save_policy(TENANT, "guard", GUARD_V1, "system")
        .await
        .unwrap();
    let live = format!("{base}\n{GUARD_V1}");
    state.authz.reload_tenant_policies(TENANT, &live).unwrap();
    state
        .tenant_policies
        .write()
        .unwrap()
        .insert(TENANT.into(), live.clone());
    live
}

fn allowed(state: &ServerState, role: &str) -> bool {
    state
        .authz
        .authorize_for_tenant(TENANT, &writer(role), "Write", "Thing", &Default::default())
        .is_allowed()
}

#[tokio::test]
async fn failed_policy_writes_preserve_live_restrictions() {
    // Exhaust all affected save paths: replace an ID, create an ID, add a rule.
    for (route, body) in [
        (
            "create",
            serde_json::json!({"policy_id": "guard", "cedar_text": GUARD_V2}),
        ),
        (
            "create",
            serde_json::json!({"policy_id": "new", "cedar_text": GUARD_V2}),
        ),
        ("rules", serde_json::json!({"rule": GUARD_V2})),
    ] {
        let (mut state, store, _temp) = policy_state().await;
        let before = seed(&state, &store).await;
        let mut storage = StorageStack::from_turso(store.clone());
        storage.policies = Some(Arc::new(FailedWrites(store.clone())));
        state.set_storage_stack(storage);
        let response = build_router(state.clone())
            .oneshot(policy_request("POST", route, body))
            .await
            .unwrap();
        // Check the actual authorization outcome as well as engine/cache/rows.
        assert!(
            !allowed(&state, "first-guard"),
            "failed {route} removed a live forbid"
        );
        assert!(
            allowed(&state, "second-guard"),
            "failed {route} activated posted text"
        );
        assert_eq!(state.authz.get_tenant_policy_text(TENANT).unwrap(), before);
        assert_eq!(state.tenant_policies.read().unwrap()[TENANT], before);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let rows = store.load_policies_for_tenant(TENANT).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .find(|row| row.policy_id == "guard")
                .unwrap()
                .cedar_text,
            GUARD_V1
        );
    }
}

#[tokio::test]
async fn disabled_policy_creates_stay_disabled_until_enabled() {
    let (state, store, _temp) = policy_state().await;
    seed(&state, &store).await;
    let app = build_router(state.clone());
    let disabled = app
        .clone()
        .oneshot(policy_request(
            "PATCH",
            "entry/guard",
            serde_json::json!({"enabled": false}),
        ))
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::OK);
    let count = statements(&state.authz.get_tenant_policy_text(TENANT).unwrap());

    for text in [GUARD_V1, GUARD_V2, GUARD_V2] {
        let response = app
            .clone()
            .oneshot(policy_request(
                "POST",
                "create",
                serde_json::json!({"policy_id": "guard", "cedar_text": text}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(allowed(&state, "first-guard"));
        assert!(allowed(&state, "second-guard"));
        assert_eq!(
            statements(&state.authz.get_tenant_policy_text(TENANT).unwrap()),
            count
        );
        let rows = store.load_policies_for_tenant(TENANT).await.unwrap();
        let guard = rows.iter().find(|row| row.policy_id == "guard").unwrap();
        assert!(!guard.enabled);
        assert_eq!(guard.cedar_text, text);
    }

    let enabled = app
        .oneshot(policy_request(
            "PATCH",
            "entry/guard",
            serde_json::json!({"enabled": true}),
        ))
        .await
        .unwrap();
    assert_eq!(enabled.status(), StatusCode::OK);
    assert!(allowed(&state, "first-guard"));
    assert!(!allowed(&state, "second-guard"));
    assert_eq!(
        statements(&state.authz.get_tenant_policy_text(TENANT).unwrap()),
        count + 1
    );
}

#[tokio::test]
async fn unchanged_live_create_neither_saves_nor_reloads() {
    let (mut state, store, _temp) = policy_state().await;
    seed(&state, &store).await;
    let rows = store.load_policies_for_tenant(TENANT).await.unwrap();
    let named = rows
        .iter()
        .map(|row| (row.policy_id.clone(), row.cedar_text.clone()))
        .collect::<Vec<_>>();
    state
        .authz
        .reload_tenant_policies_named(TENANT, &named)
        .unwrap();
    let decision = |state: &ServerState| {
        state.authz.authorize_for_tenant(
            TENANT,
            &writer("second-guard"),
            "Write",
            "Thing",
            &Default::default(),
        )
    };
    let before_ids = decision(&state).policy_ids().to_vec();
    assert_eq!(before_ids.len(), 1);
    assert!(before_ids[0].starts_with("default:base:"));
    let mut storage = StorageStack::from_turso(store.clone());
    storage.policies = Some(Arc::new(FailedWrites(store)));
    state.set_storage_stack(storage);
    let response = build_router(state.clone())
        .oneshot(policy_request(
            "POST",
            "create",
            serde_json::json!({"policy_id": "guard", "cedar_text": GUARD_V1}),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "no write should be attempted"
    );
    // A flattening reload would replace the named policy ID with policyN.
    assert_eq!(decision(&state).policy_ids(), before_ids);
}

#[tokio::test]
async fn disabled_create_preserves_statements_held_by_an_enabled_row() {
    let (state, store, _temp) = policy_state().await;
    seed(&state, &store).await;
    // Distinct text avoids the store's duplicate-row suppression.
    let shared = format!("{GUARD_V1}\n{GUARD_V2}");
    store
        .save_policy(TENANT, "shared", &shared, "system")
        .await
        .unwrap();
    let app = build_router(state.clone());
    let response = app
        .clone()
        .oneshot(policy_request(
            "PATCH",
            "entry/guard",
            serde_json::json!({"enabled": false}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for text in [GUARD_V1, GUARD_V2] {
        let response = app
            .clone()
            .oneshot(policy_request(
                "POST",
                "create",
                serde_json::json!({"policy_id": "guard", "cedar_text": text}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(
            !allowed(&state, "first-guard"),
            "another row still holds the old forbid"
        );
        assert!(
            !allowed(&state, "second-guard"),
            "another row holds the posted forbid"
        );
    }
}
