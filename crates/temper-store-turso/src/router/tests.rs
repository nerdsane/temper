use super::*;
use libsql::params;

#[tokio::test]
async fn test_router_local_dev() {
    let dir = tempfile::tempdir().expect("tempdir");
    let platform_path = dir.path().join("platform.db");
    let platform_url = format!("file:{}", platform_path.display());

    let router = TenantStoreRouter::new(
        &platform_url,
        None,
        Some(dir.path().join("tenants").to_string_lossy().to_string()),
    )
    .await
    .expect("router creation");

    // No tenants initially.
    let tenants = router.list_tenants().await.expect("list");
    assert!(tenants.is_empty());

    // Register a tenant.
    let _store = router.register_tenant("alpha").await.expect("register");

    // Verify it's registered.
    let tenants = router.list_tenants().await.expect("list");
    assert_eq!(tenants, vec!["alpha"]);

    // Write and read back through the router.
    let persistence_id = "alpha:Order:order-1";
    let events = vec![PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "OrderCreated".to_string(),
        payload: serde_json::json!({"status": "Draft"}),
        metadata: temper_runtime::persistence::EventMetadata {
            event_id: uuid::Uuid::nil(),
            causation_id: uuid::Uuid::nil(),
            correlation_id: uuid::Uuid::nil(),
            timestamp: chrono::Utc::now(),
            actor_id: "test".to_string(),
        },
    }];
    let seq = router
        .append(persistence_id, 0, &events)
        .await
        .expect("append");
    assert_eq!(seq, 1);

    let read_back = router.read_events(persistence_id, 0).await.expect("read");
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].event_type, "OrderCreated");

    // System tenant routes to platform DB.
    let sys_store = router
        .store_for_tenant("temper-system")
        .await
        .expect("system");
    // The platform store should work (it has the entity schema too).
    let entity_ids = sys_store
        .list_entity_ids("temper-system")
        .await
        .expect("list");
    assert!(entity_ids.is_empty());
}

#[tokio::test]
async fn test_tenant_user_management() {
    let dir = tempfile::tempdir().expect("tempdir");
    let platform_path = dir.path().join("platform.db");
    let platform_url = format!("file:{}", platform_path.display());

    let router = TenantStoreRouter::new(&platform_url, None, None)
        .await
        .expect("router");

    // Add users.
    router
        .add_tenant_user("alpha", "github:alice", "admin")
        .await
        .expect("add user");
    router
        .add_tenant_user("alpha", "github:bob", "member")
        .await
        .expect("add user");
    router
        .add_tenant_user("beta", "github:alice", "member")
        .await
        .expect("add user");

    // Query by user.
    let alice_tenants = router
        .tenants_for_user("github:alice")
        .await
        .expect("query");
    assert_eq!(alice_tenants.len(), 2);

    let bob_tenants = router.tenants_for_user("github:bob").await.expect("query");
    assert_eq!(bob_tenants.len(), 1);
    assert_eq!(bob_tenants[0].tenant_id, "alpha");

    // Remove user.
    router
        .remove_tenant_user("alpha", "github:bob")
        .await
        .expect("remove");
    let bob_tenants = router.tenants_for_user("github:bob").await.expect("query");
    assert!(bob_tenants.is_empty());
}

#[tokio::test]
async fn test_ensure_tenant_reconnects_existing_registry_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let platform_path = dir.path().join("platform.db");
    let tenant_db_path = dir.path().join("preprovisioned.db");
    let platform_url = format!("file:{}", platform_path.display());
    let tenant_db_url = format!("file:{}", tenant_db_path.display());

    let router = TenantStoreRouter::new(&platform_url, None, None)
        .await
        .expect("router");

    let conn = router.platform_store().connection().expect("conn");
    conn.execute(
        "INSERT INTO tenant_registry (tenant_id, turso_db_url, turso_auth_token)
             VALUES (?1, ?2, ?3)",
        params!["preprovisioned", tenant_db_url, Option::<String>::None],
    )
    .await
    .expect("insert registry row");

    let existed = router
        .ensure_tenant("preprovisioned")
        .await
        .expect("ensure existing tenant");
    assert!(existed, "existing tenant should report already existed");

    let connected = router.connected_tenants().await;
    assert!(
        connected.contains(&"preprovisioned".to_string()),
        "tenant should be connected after ensure_tenant"
    );
}

#[tokio::test]
async fn test_ensure_tenant_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let platform_path = dir.path().join("platform.db");
    let platform_url = format!("file:{}", platform_path.display());

    let router = TenantStoreRouter::new(
        &platform_url,
        None,
        Some(dir.path().join("tenants").to_string_lossy().to_string()),
    )
    .await
    .expect("router creation");

    let first = router.ensure_tenant("repeat").await.expect("first ensure");
    let second = router.ensure_tenant("repeat").await.expect("second ensure");

    assert!(!first, "first ensure should provision the tenant");
    assert!(
        second,
        "second ensure should be idempotent and reuse tenant"
    );

    let tenants = router.list_tenants().await.expect("list tenants");
    assert_eq!(tenants, vec!["repeat".to_string()]);
}

#[cfg(feature = "cloud")]
#[tokio::test]
async fn test_provision_cloud_database_recovers_from_conflict() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let org = "acme";
    let db_name = "temper-alpha";

    Mock::given(method("POST"))
        .and(path(format!("/v1/organizations/{org}/databases")))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "database already exists"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/v1/organizations/{org}/databases/{db_name}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "database": {
                "hostname": "alpha.db.turso.io"
            }
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!(
            "/v1/organizations/{org}/databases/{db_name}/auth/tokens"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "jwt": "token-123"
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let platform_path = dir.path().join("platform.db");
    let platform_url = format!("file:{}", platform_path.display());

    let mut router = TenantStoreRouter::new(&platform_url, None, None)
        .await
        .expect("router");
    router = router.with_cloud_config("api-token".to_string(), org.to_string(), None);
    router.turso_api_base_url = server.uri();

    let (db_url, auth_token) = router
        .provision_cloud_database("alpha", "api-token", org)
        .await
        .expect("provision should recover from 409");

    assert_eq!(db_url, "libsql://alpha.db.turso.io");
    assert_eq!(auth_token.as_deref(), Some("token-123"));
}
