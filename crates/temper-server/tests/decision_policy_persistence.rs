use std::collections::HashMap;

use axum::body::Body;
use axum::http::Request;
use temper_authz::{
    ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope, SecurityContext,
};
use temper_runtime::ActorSystem;
use temper_server::authz::{
    DecisionPolicyInstall, PolicyEntryUpsert, install_decision_policy,
    load_and_activate_tenant_policies, publish_policy_snapshot, refresh_policy_snapshot_if_stale,
    upsert_policy_entries, verify_active_policy_exactly_once,
};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::CsdlDocument;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

fn empty_state(name: &str) -> ServerState {
    ServerState::new(
        ActorSystem::new(name),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    )
}

fn session_customer_policy(session_id: &str) -> String {
    temper_authz::generate_cedar_from_matrix(
        "customer-1",
        "Customer",
        "read",
        "Order",
        "order-1",
        &PolicyScopeMatrix {
            principal: PrincipalScope::ThisAgent,
            action: ActionScope::ThisAction,
            resource: ResourceScope::ThisResource,
            duration: DurationScope::Session,
            agent_type_value: None,
            role_value: None,
            session_id: Some(session_id.to_string()),
        },
    )
    .expect("fixture policy should generate")
}

#[tokio::test]
async fn decision_policy_is_immutable_idempotent_and_single_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", dir.path().join("policies.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create local Turso store");
    let mut state = empty_state("decision-policy-install");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let policy = session_customer_policy("session-allowed");

    assert!(matches!(
        install_decision_policy(&state, "tenant-a", "decision:pd-1", &policy, "reviewer",)
            .await
            .expect("first install should succeed"),
        DecisionPolicyInstall::Created {
            publication_version: 1
        }
    ));
    assert!(matches!(
        install_decision_policy(&state, "tenant-a", "decision:pd-1", &policy, "reviewer",)
            .await
            .expect("exact retry should succeed"),
        DecisionPolicyInstall::AlreadyPresent {
            publication_version: 1
        }
    ));
    verify_active_policy_exactly_once(&state, "tenant-a", &policy)
        .expect("one exact policy should be active");

    let widened = session_customer_policy("session-other");
    let error = install_decision_policy(&state, "tenant-a", "decision:pd-1", &widened, "reviewer")
        .await
        .expect_err("one decision id must not be overwritten with a new scope");
    assert!(error.contains("different approved content"));

    let rows = state
        .policy_store()
        .expect("policy store")
        .load_policies_for_tenant("tenant-a")
        .await
        .expect("load durable rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].policy_id, "decision:pd-1");
    assert_eq!(rows[0].cedar_text, policy);

    let mut restarted = empty_state("decision-policy-restart");
    restarted.set_storage_stack(StorageStack::from_turso(store));
    load_and_activate_tenant_policies(&restarted, "tenant-a")
        .await
        .expect("restart policy activation");
    verify_active_policy_exactly_once(&restarted, "tenant-a", &policy)
        .expect("restart should activate exactly one durable policy row");

    let allowed = SecurityContext::from_headers(&[
        (
            "X-Temper-Principal-Id".to_string(),
            "customer-1".to_string(),
        ),
        (
            "X-Temper-Principal-Kind".to_string(),
            "customer".to_string(),
        ),
        (
            "X-Temper-Ctx-SessionId".to_string(),
            "session-allowed".to_string(),
        ),
    ]);
    let denied = SecurityContext::from_headers(&[
        (
            "X-Temper-Principal-Id".to_string(),
            "customer-1".to_string(),
        ),
        (
            "X-Temper-Principal-Kind".to_string(),
            "customer".to_string(),
        ),
        (
            "X-Temper-Ctx-SessionId".to_string(),
            "session-other".to_string(),
        ),
    ]);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("order-1"));
    assert!(
        restarted
            .authz
            .authorize_for_tenant("tenant-a", &allowed, "read", "Order", &attrs)
            .is_allowed()
    );
    assert!(
        !restarted
            .authz
            .authorize_for_tenant("tenant-a", &denied, "read", "Order", &attrs)
            .is_allowed()
    );
}

#[tokio::test]
async fn decision_policy_install_requires_durable_storage() {
    let state = empty_state("decision-policy-no-store");
    let error = install_decision_policy(
        &state,
        "tenant-a",
        "decision:pd-1",
        &session_customer_policy("session-allowed"),
        "reviewer",
    )
    .await
    .expect_err("approval cannot silently skip durable policy persistence");
    assert!(error.contains("durable policy store is not configured"));
    assert!(state.authz.get_tenant_policy_text("tenant-a").is_none());
}

#[tokio::test]
async fn concurrent_server_instances_cannot_overwrite_one_decision_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!(
        "file:{}",
        dir.path().join("concurrent-policies.db").display()
    );
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create local Turso store");
    let mut first_state = empty_state("decision-policy-concurrent-a");
    first_state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let mut second_state = empty_state("decision-policy-concurrent-b");
    second_state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let first_policy = session_customer_policy("session-a");
    let second_policy = session_customer_policy("session-b");

    let (first, second) = tokio::join!(
        install_decision_policy(
            &first_state,
            "tenant-a",
            "decision:pd-concurrent",
            &first_policy,
            "reviewer-a",
        ),
        install_decision_policy(
            &second_state,
            "tenant-a",
            "decision:pd-concurrent",
            &second_policy,
            "reviewer-b",
        ),
    );

    assert_ne!(first.is_ok(), second.is_ok());
    let rejected = first.as_ref().err().or_else(|| second.as_ref().err());
    assert!(
        rejected.is_some_and(|error| error.contains("different approved content")),
        "losing installer must report immutable-content conflict: {rejected:?}"
    );
    let rows = store
        .load_policies_for_tenant("tenant-a")
        .await
        .expect("load immutable decision row");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].policy_id, "decision:pd-concurrent");
    assert!(rows[0].cedar_text == first_policy || rows[0].cedar_text == second_policy);
}

#[tokio::test]
async fn final_policy_removal_publishes_default_deny_and_survives_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", dir.path().join("final-row.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create local Turso store");
    let mut state = empty_state("final-policy-row");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let policy = session_customer_policy("session-allowed");
    upsert_policy_entries(
        &state,
        "tenant-a",
        &[PolicyEntryUpsert {
            policy_id: "primary",
            cedar_text: &policy,
            created_by: "reviewer",
        }],
    )
    .await
    .expect("publish one permit");

    let allowed = SecurityContext::from_headers(&[
        (
            "X-Temper-Principal-Id".to_string(),
            "customer-1".to_string(),
        ),
        (
            "X-Temper-Principal-Kind".to_string(),
            "customer".to_string(),
        ),
        (
            "X-Temper-Ctx-SessionId".to_string(),
            "session-allowed".to_string(),
        ),
    ]);
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!("order-1"));
    assert!(
        state
            .authz
            .authorize_for_tenant("tenant-a", &allowed, "read", "Order", &attrs)
            .is_allowed()
    );

    let current = state
        .policy_store()
        .expect("policy store")
        .load_policy_snapshot("tenant-a")
        .await
        .expect("load current snapshot");
    assert_eq!(current.version, 1);
    let empty = publish_policy_snapshot(&state, "tenant-a", current.version, vec![])
        .await
        .expect("publish authoritative empty snapshot");
    assert_eq!(empty.version, 2);
    assert!(state.authz.get_tenant_policy_text("tenant-a").as_deref() == Some(""));
    assert!(
        !state
            .authz
            .authorize_for_tenant("tenant-a", &allowed, "read", "Order", &attrs)
            .is_allowed()
    );

    let mut restarted = empty_state("final-policy-row-restart");
    restarted.set_storage_stack(StorageStack::from_turso(store));
    assert_eq!(
        load_and_activate_tenant_policies(&restarted, "tenant-a")
            .await
            .expect("restart empty activation"),
        2
    );
    assert!(
        !restarted
            .authz
            .authorize_for_tenant("tenant-a", &allowed, "read", "Order", &attrs)
            .is_allowed()
    );
}

#[tokio::test]
async fn delayed_snapshot_reader_cannot_downgrade_newer_local_activation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", dir.path().join("version-guard.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create local Turso store");
    let mut state = empty_state("policy-version-guard");
    state.set_storage_stack(StorageStack::from_turso(store));
    let old_policy = session_customer_policy("session-old");
    upsert_policy_entries(
        &state,
        "tenant-a",
        &[PolicyEntryUpsert {
            policy_id: "old",
            cedar_text: &old_policy,
            created_by: "old-writer",
        }],
    )
    .await
    .expect("publish durable v1");

    let newer_policy = session_customer_policy("session-new");
    state
        .authz
        .reload_tenant_policies_named("tenant-a", &[("newer".to_string(), newer_policy.clone())])
        .expect("activate simulated newer replica state");
    state
        .tenant_policy_versions
        .write()
        .expect("version lock")
        .insert("tenant-a".to_string(), 2);

    assert_eq!(
        refresh_policy_snapshot_if_stale(&state, "tenant-a")
            .await
            .expect("delayed v1 load is harmless"),
        2
    );
    assert_eq!(
        state.authz.get_tenant_policy_text("tenant-a").as_deref(),
        Some(newer_policy.as_str())
    );
}

#[tokio::test]
async fn request_ingress_converges_a_stale_replica_to_durable_default_deny() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", dir.path().join("replica-refresh.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create shared Turso store");
    let mut stale = empty_state("policy-stale-replica");
    stale.set_storage_stack(StorageStack::from_turso(store.clone()));
    let mut writer = empty_state("policy-writer-replica");
    writer.set_storage_stack(StorageStack::from_turso(store));
    let policy = session_customer_policy("session-allowed");
    upsert_policy_entries(
        &writer,
        "tenant-a",
        &[PolicyEntryUpsert {
            policy_id: "permit",
            cedar_text: &policy,
            created_by: "writer",
        }],
    )
    .await
    .expect("publish v1 permit");

    let request = || {
        Request::get("/tdata")
            .header("x-tenant-id", "tenant-a")
            .body(Body::empty())
            .expect("build convergence request")
    };
    let _ = temper_server::build_router(stale.clone())
        .oneshot(request())
        .await
        .expect("first replica request");
    assert_eq!(
        stale
            .tenant_policy_versions
            .read()
            .expect("version lock")
            .get("tenant-a"),
        Some(&1)
    );

    let current = writer
        .policy_store()
        .expect("writer policy store")
        .load_policy_snapshot("tenant-a")
        .await
        .expect("load v1");
    publish_policy_snapshot(&writer, "tenant-a", current.version, vec![])
        .await
        .expect("publish v2 default deny");
    assert_eq!(
        stale.authz.get_tenant_policy_text("tenant-a").as_deref(),
        Some(policy.as_str()),
        "stale replica remains permissive until its next request"
    );

    let _ = temper_server::build_router(stale.clone())
        .oneshot(request())
        .await
        .expect("second replica request");
    assert_eq!(
        stale
            .tenant_policy_versions
            .read()
            .expect("version lock")
            .get("tenant-a"),
        Some(&2)
    );
    assert_eq!(
        stale.authz.get_tenant_policy_text("tenant-a").as_deref(),
        Some("")
    );
}

#[tokio::test]
async fn empty_durable_snapshot_retains_only_configured_baseline_authority() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("file:{}", dir.path().join("baseline.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create baseline store");
    let mut state = empty_state("policy-baseline");
    state.set_storage_stack(StorageStack::from_turso(store));
    let baseline = r#"
        permit(
            principal == Admin::"operator",
            action == Action::"manage",
            resource == ControlPlane::"system"
        );
    "#;
    state
        .set_tenant_policy_baseline("tenant-a", baseline)
        .expect("configure immutable baseline");

    assert_eq!(
        refresh_policy_snapshot_if_stale(&state, "tenant-a")
            .await
            .expect("activate authoritative empty mutable snapshot"),
        0
    );
    let operator = SecurityContext::from_headers(&[
        ("x-temper-principal-id".to_string(), "operator".to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ]);
    let outsider = SecurityContext::from_headers(&[
        ("x-temper-principal-id".to_string(), "outsider".to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
    ]);
    let attrs = HashMap::from([("id".to_string(), serde_json::json!("system"))]);
    assert!(
        state
            .authz
            .authorize_for_tenant("tenant-a", &operator, "manage", "ControlPlane", &attrs)
            .is_allowed()
    );
    assert!(
        !state
            .authz
            .authorize_for_tenant("tenant-a", &outsider, "manage", "ControlPlane", &attrs)
            .is_allowed()
    );
}

#[cfg(feature = "observe")]
#[tokio::test]
async fn path_tenant_authorization_converges_even_when_header_names_another_tenant() {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!(
        "file:{}",
        dir.path().join("path-tenant-refresh.db").display()
    );
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create shared path-tenant store");
    let mut stale = empty_state("path-tenant-stale");
    stale.set_storage_stack(StorageStack::from_turso(store.clone()));
    let mut writer = empty_state("path-tenant-writer");
    writer.set_storage_stack(StorageStack::from_turso(store));
    let permit = r#"
        permit(
            principal == Agent::"agent-b",
            action == Action::"manage_policies",
            resource is PolicySet
        );
    "#;
    upsert_policy_entries(
        &writer,
        "tenant-b",
        &[PolicyEntryUpsert {
            policy_id: "manage",
            cedar_text: permit,
            created_by: "writer",
        }],
    )
    .await
    .expect("publish tenant-b permit");
    refresh_policy_snapshot_if_stale(&stale, "tenant-b")
        .await
        .expect("seed stale replica permit");
    let current = writer
        .policy_store()
        .expect("writer policy store")
        .load_policy_snapshot("tenant-b")
        .await
        .expect("load tenant-b v1");
    publish_policy_snapshot(&writer, "tenant-b", current.version, vec![])
        .await
        .expect("revoke tenant-b permit");

    let response = temper_server::build_router(stale.clone())
        .oneshot(
            Request::get("/api/tenants/tenant-b/policies")
                .header("x-tenant-id", "tenant-a")
                .header("x-temper-principal-id", "agent-b")
                .header("x-temper-principal-kind", "agent")
                .body(Body::empty())
                .expect("build mismatched-tenant request"),
        )
        .await
        .expect("path-tenant authorization response");
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(
        stale
            .tenant_policy_versions
            .read()
            .expect("version lock")
            .get("tenant-b"),
        Some(&2)
    );
}
