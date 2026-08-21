#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::secrets::vault::SecretsVault;
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "resource_authorization/secret_authorization.rs"]
mod secret_authorization;

const TENANT: &str = "default";

async fn state_with_turso(name: &str) -> (ServerState, TursoEventStore, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let db_url = format!("file:{}", temp.path().join("metadata.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create Turso test store");
    let mut state = ServerState::from_registry(ActorSystem::new(name), SpecRegistry::new());
    state.data_dir = temp.path().join("data");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store, temp)
}

fn customer_security_context(id: &str) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: id.to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: format!("resource-auth-{id}"),
    }
}

fn json_request(
    method: Method,
    uri: &str,
    body: serde_json::Value,
    tenant: &str,
    principal_id: &str,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            customer_security_context(principal_id),
        ));
    request
}

async fn seed_file(
    store: &TursoEventStore,
    entity_type: &str,
    id: &str,
    hash: &str,
    body: &[u8],
    source_file_id: Option<&str>,
) {
    let mut fields = serde_json::json!({
        "content_hash": hash,
        "mime_type": "text/markdown",
        "has_content": true,
    });
    if let Some(source_file_id) = source_file_id {
        fields["file_id"] = serde_json::Value::String(source_file_id.to_string());
    }
    store
        .upsert_query_projection(TENANT, entity_type, id, "Ready", &fields, 1)
        .await
        .expect("seed file projection");
    store
        .put_blob(&format!("temper-fs/{hash}"), body)
        .await
        .expect("seed file blob");
}

#[tokio::test]
async fn batch_file_reads_authorize_every_exact_resource() {
    let (state, store, _temp) = state_with_turso("file-resource-auth").await;
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"
permit(
  principal == Customer::"reader",
  action == Action::"read",
  resource == File::"file-a"
);
permit(
  principal == Customer::"reader",
  action == Action::"read",
  resource == File::"file-large"
);
permit(
  principal == Customer::"reader",
  action == Action::"read",
  resource == FileVersion::"version-a"
);
"#,
        )
        .expect("file policy should parse");
    state
        .authz
        .reload_tenant_policies("other-tenant", "")
        .expect("other tenant should default-deny");
    seed_file(&store, "File", "file-a", "sha256:filea", b"file-a", None).await;
    let oversized_text = vec![b'x'; 2 * 1024 * 1024 + 1];
    seed_file(
        &store,
        "File",
        "file-large",
        "sha256:filelarge",
        &oversized_text,
        None,
    )
    .await;
    seed_file(
        &store,
        "FileVersion",
        "version-a",
        "sha256:versiona",
        b"version-a",
        Some("file-a"),
    )
    .await;
    let app = build_router(state);

    let allowed = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-text-batch",
            serde_json::json!({"file_ids": ["file-a"]}),
            TENANT,
            "reader",
        ))
        .await
        .expect("allowed request should run");
    assert_eq!(allowed.status(), StatusCode::OK);

    let duplicate = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-text-batch",
            serde_json::json!({"file_ids": ["file-a", "file-a"]}),
            TENANT,
            "reader",
        ))
        .await
        .expect("duplicate request should be rejected");
    assert_eq!(duplicate.status(), StatusCode::BAD_REQUEST);

    let too_many_ids = (0..101)
        .map(|index| format!("file-{index}"))
        .collect::<Vec<_>>();
    let oversized_batch = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-text-batch",
            serde_json::json!({"file_ids": too_many_ids}),
            TENANT,
            "reader",
        ))
        .await
        .expect("oversized batch should be rejected");
    assert_eq!(oversized_batch.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let oversized_item = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-text-batch",
            serde_json::json!({"file_ids": ["file-large"]}),
            TENANT,
            "reader",
        ))
        .await
        .expect("oversized buffered item should be rejected");
    assert_eq!(oversized_item.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let partial_batch = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-text-batch",
            serde_json::json!({"file_ids": ["file-a", "file-b"]}),
            TENANT,
            "reader",
        ))
        .await
        .expect("partially unauthorized request should run");
    assert_eq!(partial_batch.status(), StatusCode::FORBIDDEN);

    let wrong_principal = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-text-batch",
            serde_json::json!({"file_ids": ["file-a"]}),
            TENANT,
            "intruder",
        ))
        .await
        .expect("wrong-principal request should run");
    assert_eq!(wrong_principal.status(), StatusCode::FORBIDDEN);

    let wrong_tenant = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-text-batch",
            serde_json::json!({"file_ids": ["file-a"]}),
            "other-tenant",
            "reader",
        ))
        .await
        .expect("wrong-tenant request should run");
    assert_eq!(wrong_tenant.status(), StatusCode::FORBIDDEN);

    let version_allowed = app
        .oneshot(json_request(
            Method::POST,
            "/api/files/read-version-text-batch",
            serde_json::json!({"file_version_ids": ["version-a"]}),
            TENANT,
            "reader",
        ))
        .await
        .expect("version request should run");
    assert_eq!(version_allowed.status(), StatusCode::OK);
}

#[tokio::test]
async fn authorize_api_evaluates_the_claimed_resource_id_not_context_spoofing() {
    let (state, _store, _temp) = state_with_turso("authorize-exact-resource").await;
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"
permit(
  principal == Customer::"reader",
  action == Action::"inspect",
  resource == Tool::"target"
);
"#,
        )
        .expect("exact-resource policy should parse");
    let app = build_router(state);

    let allowed = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/authorize",
            serde_json::json!({
                "agent_id": "reader",
                "action": "inspect",
                "resource_type": "Tool",
                "resource_id": "target",
                "context": {"id": "attacker-override", "classification": "public"}
            }),
            TENANT,
            "reader",
        ))
        .await
        .expect("authorization request should run");
    assert_eq!(allowed.status(), StatusCode::OK);
    let allowed_body = axum::body::to_bytes(allowed.into_body(), 64 * 1024)
        .await
        .expect("read allowed response");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&allowed_body).expect("allowed response JSON")
            ["allowed"],
        true
    );

    let denied = app
        .oneshot(json_request(
            Method::POST,
            "/api/authorize",
            serde_json::json!({
                "agent_id": "reader",
                "action": "inspect",
                "resource_type": "Tool",
                "resource_id": "other",
                "context": {"id": "target"}
            }),
            TENANT,
            "reader",
        ))
        .await
        .expect("authorization request should run");
    assert_eq!(denied.status(), StatusCode::OK);
    let denied_body = axum::body::to_bytes(denied.into_body(), 64 * 1024)
        .await
        .expect("read denied response");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&denied_body).expect("denied response JSON")["allowed"],
        false
    );
}

#[path = "resource_authorization/artifact_authorization.rs"]
mod artifact_authorization;
