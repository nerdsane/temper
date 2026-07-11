use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use tower::ServiceExt as _;

use crate::registry::SpecRegistry;
use crate::state::ServerState;

const KEY: &str = "field-overflow/sha256/shared.json";

fn install_blob_policy(state: &ServerState, tenant: &str) {
    state
        .authz
        .reload_tenant_policies(
            tenant,
            &format!(
                r#"
permit(
  principal == Agent::"blob-client",
  action == Action::"write_blob_object",
  resource == BlobObject::"{KEY}"
);
permit(
  principal == Agent::"blob-client",
  action == Action::"read_blob_object",
  resource == BlobObject::"{KEY}"
);
"#,
            ),
        )
        .expect("blob policy should parse");
}

fn authenticated_request(method: Method, path: &str, tenant: &str, body: Body) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(body)
        .expect("internal blob request");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("blob-client", "worker", None),
        ));
    request
}

fn claimed_admin_request(method: Method, path: &str, tenant: &str, body: Body) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(body)
        .expect("internal blob admin request");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext {
                principal: temper_authz::Principal {
                    id: "claimed-admin".to_string(),
                    kind: temper_authz::PrincipalKind::Admin,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "blob-admin-side-channel-test".to_string(),
            },
        ));
    request
}

#[tokio::test]
async fn internal_blob_http_storage_is_bound_to_the_authenticated_tenant() {
    let data_dir = tempfile::tempdir().expect("blob data directory");
    let mut state = ServerState::from_registry(
        ActorSystem::new("tenant-bound-internal-blob"),
        SpecRegistry::new(),
    );
    state.data_dir = data_dir.path().to_path_buf();
    for tenant in ["tenant-a", "tenant-b", "default"] {
        install_blob_policy(&state, tenant);
    }
    let app = crate::build_router(state.clone());
    let key_path = format!("/_internal/blobs/{KEY}");

    for (tenant, value) in [("tenant-a", "alpha"), ("tenant-b", "bravo")] {
        let response = app
            .clone()
            .oneshot(authenticated_request(
                Method::PUT,
                &key_path,
                tenant,
                Body::from(value),
            ))
            .await
            .expect("tenant blob write");
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{tenant}");
    }

    for (tenant, expected) in [
        ("tenant-a", b"alpha".as_slice()),
        ("tenant-b", b"bravo".as_slice()),
    ] {
        let response = app
            .clone()
            .oneshot(authenticated_request(
                Method::GET,
                &key_path,
                tenant,
                Body::empty(),
            ))
            .await
            .expect("tenant blob read");
        assert_eq!(response.status(), StatusCode::OK, "{tenant}");
        let bytes = to_bytes(response.into_body(), 32)
            .await
            .expect("bounded blob response");
        assert_eq!(bytes.as_ref(), expected, "{tenant}");
    }

    let default_response = app
        .oneshot(authenticated_request(
            Method::GET,
            &key_path,
            "default",
            Body::empty(),
        ))
        .await
        .expect("default tenant blob read");
    assert_eq!(default_response.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&TenantId::new("tenant-a"), KEY,)
            .await
            .expect("tenant A direct read"),
        Some(b"alpha".to_vec())
    );
    assert_eq!(
        state
            .get_blob_with_legacy_fallback(&TenantId::new("tenant-b"), KEY,)
            .await
            .expect("tenant B direct read"),
        Some(b"bravo".to_vec())
    );
}

#[tokio::test]
async fn internal_blob_http_requires_exact_object_authority() {
    let data_dir = tempfile::tempdir().expect("blob data directory");
    let mut state = ServerState::from_registry(
        ActorSystem::new("exact-internal-blob-auth"),
        SpecRegistry::new(),
    );
    state.data_dir = data_dir.path().to_path_buf();
    install_blob_policy(&state, "tenant-a");
    let app = crate::build_router(state);

    let allowed = app
        .clone()
        .oneshot(authenticated_request(
            Method::PUT,
            &format!("/_internal/blobs/{KEY}"),
            "tenant-a",
            Body::from("allowed"),
        ))
        .await
        .expect("allowed blob request should run");
    assert_eq!(allowed.status(), StatusCode::NO_CONTENT);

    let claimed_admin = app
        .clone()
        .oneshot(claimed_admin_request(
            Method::PUT,
            &format!("/_internal/blobs/{KEY}"),
            "tenant-a",
            Body::from("must-not-write"),
        ))
        .await
        .expect("claimed-admin blob request should run");
    assert_eq!(claimed_admin.status(), StatusCode::FORBIDDEN);

    for method in [Method::GET, Method::PUT] {
        let denied = app
            .clone()
            .oneshot(authenticated_request(
                method.clone(),
                "/_internal/blobs/field-overflow/sha256/sibling.json",
                "tenant-a",
                Body::from("denied"),
            ))
            .await
            .expect("denied blob request should run");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN, "{method}");
    }
}
