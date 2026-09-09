//! Security regression tests for the OTS trajectory routes (ARN-187).
//!
//! `GET`/`POST /api/ots/trajectories` previously read the raw `X-Tenant-Id`
//! header and selected that tenant's store without authorization, so any
//! caller could read another tenant's full agent-execution traces or forge
//! writes by setting a header. These tests lock in the credential gate
//! (ADR-0157): identity and tenant come only from the typed request context,
//! authorization from `require_observe_auth`.
//!
//! Ported from the ARN-187 branch (`claude/arn-187-ots-auth-gate`, PR #347)
//! and adapted: the app-level `{tenant}::{id}` key prefix it tested is
//! superseded by the store-level `(tenant, trajectory_id)` primary key.
//!
//! NOTE: behind the `observe` feature; a bare `cargo test -p temper-server`
//! filters these out.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use tower::ServiceExt;

use crate::ServerState;
use crate::registry::SpecRegistry;

/// Enforcing baseline: only `Admin` principals are permitted anything, so the
/// deny path is real rather than the permissive test default.
const ADMIN_ONLY_POLICY: &str = r#"permit(principal is Admin, action, resource);"#;

/// A minimal well-formed OTS trajectory payload.
fn sample_trajectory() -> String {
    serde_json::json!({
        "trajectory_id": "traj-route-1",
        "version": "0.1.0",
        "metadata": {
            "task_description": "t",
            "timestamp_start": "2026-01-01T00:00:00Z",
            "agent_id": "a1",
            "outcome": "success",
            "human_reviewed": false,
        },
        "context": {},
    })
    .to_string()
}

fn enforcing_state() -> ServerState {
    let state = ServerState::from_registry(ActorSystem::new("test-ots-auth"), SpecRegistry::new());
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("baseline policy should parse");
    state
        .authz
        .reload_tenant_policies("tenant-a", ADMIN_ONLY_POLICY)
        .expect("tenant-a baseline policy should parse");
    state
}

fn app(state: ServerState) -> Router {
    Router::new()
        .nest("/api", crate::api::build_api_router())
        .with_state(state)
}

fn with_auth(
    mut request: Request<Body>,
    tenant: &str,
    security_context: SecurityContext,
) -> Request<Body> {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            security_context,
        ));
    request
}

fn admin_auth() -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: "admin-1".to_string(),
            kind: PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "ots-admin-test".to_string(),
    }
}

fn agent_auth(id: &str) -> SecurityContext {
    SecurityContext::from_resolved_identity(id, "worker", Some("session-1"))
}

/// Exploit (a): an anonymous caller spoofs a victim tenant via `X-Tenant-Id`
/// and reads its OTS traces. Must be refused at the edge.
#[tokio::test]
async fn unauthenticated_get_ots_trajectories_is_denied() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(
            Request::get("/api/ots/trajectories")
                .header("X-Tenant-Id", "victim-tenant")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Exploit (b): an anonymous caller forges an OTS trajectory into a victim
/// tenant. Must be refused at the edge.
#[tokio::test]
async fn unauthenticated_post_ots_trajectory_is_denied() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(
            Request::post("/api/ots/trajectories")
                .header("content-type", "application/json")
                .header("X-Tenant-Id", "victim-tenant")
                .body(Body::from(sample_trajectory()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn raw_admin_headers_do_not_authorize_ots_trajectories() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(
            Request::get("/api/ots/trajectories")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Temper-Principal-Id", "admin-1")
                .header("X-Tenant-Id", "victim-tenant")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "raw identity headers must not create an authenticated OTS context"
    );
}

/// A properly-authorized same-tenant GET still succeeds — the gate must not
/// over-block legitimate callers.
#[tokio::test]
async fn authorized_admin_get_ots_trajectories_succeeds() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(with_auth(
            Request::get("/api/ots/trajectories")
                .body(Body::empty())
                .unwrap(),
            "tenant-a",
            admin_auth(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Out-of-range limits are rejected before any store work (ARN-187 delta:
/// strict rejection rather than a silent clamp).
#[tokio::test]
async fn out_of_range_ots_list_limits_are_rejected_before_store_query() {
    let app = app(enforcing_state());
    for query in ["limit=-1", "limit=0", "limit=501"] {
        let resp = app
            .clone()
            .oneshot(with_auth(
                Request::get(format!("/api/ots/trajectories?{query}").as_str())
                    .body(Body::empty())
                    .unwrap(),
                "tenant-a",
                admin_auth(),
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{query} must not reach the storage adapter"
        );
    }
}

/// A properly-authorized same-tenant POST still succeeds.
#[tokio::test]
async fn authorized_admin_post_ots_trajectory_succeeds() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(with_auth(
            Request::post("/api/ots/trajectories")
                .header("content-type", "application/json")
                .body(Body::from(sample_trajectory()))
                .unwrap(),
            "tenant-a",
            admin_auth(),
        ))
        .await
        .unwrap();
    // With no durable metadata store configured the handler short-circuits to
    // `201 Created`; the point is that an authorized write is not refused.
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// Cross-tenant isolation on the read path: the tenant is the credential's,
/// and a spoofed `X-Tenant-Id` naming another tenant changes nothing.
#[tokio::test]
async fn tenant_scoped_credential_cannot_read_other_tenant() {
    let state = ServerState::from_registry(
        ActorSystem::new("test-ots-cross-tenant"),
        SpecRegistry::new(),
    );
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("baseline policy should parse");
    // tenant-a grants its agents read access; tenant-b has no loaded tenant
    // policy set, so a non-System principal there fails closed
    // (NoMatchingPermit) — the global baseline is never consulted.
    state
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal is Agent, action == Action::"read_trajectories", resource is OtsTrajectory);"#,
        )
        .expect("tenant-a policy should parse");
    let app = app(state);

    let same_tenant = app
        .clone()
        .oneshot(with_auth(
            Request::get("/api/ots/trajectories")
                .body(Body::empty())
                .unwrap(),
            "tenant-a",
            agent_auth("agent-a"),
        ))
        .await
        .unwrap();
    assert_eq!(same_tenant.status(), StatusCode::OK);

    // The spoofed header must neither redirect the read into tenant-b nor
    // change the Cedar tenant: the request still runs as tenant-a.
    let cross_tenant = app
        .oneshot(with_auth(
            Request::get("/api/ots/trajectories")
                .header("X-Tenant-Id", "tenant-b")
                .body(Body::empty())
                .unwrap(),
            "tenant-a",
            agent_auth("agent-a"),
        ))
        .await
        .unwrap();
    assert_eq!(
        cross_tenant.status(),
        StatusCode::OK,
        "spoofed X-Tenant-Id must not override the credential-bound tenant"
    );
}

/// Cross-tenant isolation on the write path: same property for ingestion.
#[tokio::test]
async fn tenant_scoped_credential_cannot_write_other_tenant() {
    let state = ServerState::from_registry(
        ActorSystem::new("test-ots-cross-tenant-write"),
        SpecRegistry::new(),
    );
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("baseline policy should parse");
    // tenant-a grants its agents write access; tenant-b has no loaded tenant
    // policy set, so a non-System principal there fails closed.
    state
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal is Agent, action == Action::"write_trajectories", resource is OtsTrajectory);"#,
        )
        .expect("tenant-a policy should parse");
    let app = app(state);

    let same_tenant = app
        .clone()
        .oneshot(with_auth(
            Request::post("/api/ots/trajectories")
                .header("content-type", "application/json")
                .body(Body::from(sample_trajectory()))
                .unwrap(),
            "tenant-a",
            agent_auth("agent-a"),
        ))
        .await
        .unwrap();
    assert_eq!(same_tenant.status(), StatusCode::CREATED);

    let cross_tenant = app
        .oneshot(with_auth(
            Request::post("/api/ots/trajectories")
                .header("content-type", "application/json")
                .header("X-Tenant-Id", "tenant-b")
                .body(Body::from(sample_trajectory()))
                .unwrap(),
            "tenant-a",
            agent_auth("agent-a"),
        ))
        .await
        .unwrap();
    assert_eq!(
        cross_tenant.status(),
        StatusCode::CREATED,
        "spoofed X-Tenant-Id must not override the credential-bound tenant"
    );
}
