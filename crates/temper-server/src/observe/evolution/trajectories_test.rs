//! Security regression tests for the OTS trajectory endpoints (ARN-187).
//!
//! The OTS list/ingest handlers (`GET`/`POST /api/ots/trajectories`) previously
//! read the raw `X-Tenant-Id` header and selected that tenant's store **without
//! any authorization check**, so any caller could read another tenant's full
//! agent-execution traces or forge writes into any tenant simply by setting a
//! header. These tests lock in the gate: both handlers now call
//! `require_observe_auth` and resolve the tenant via `observe_tenant_scope`,
//! matching the sibling `handle_trajectories` / `handle_unmet_intent` handlers.
//!
//! NOTE: these tests live behind the `observe` feature (the whole `observe`/
//! `api` module tree is `#[cfg(feature = "observe")]`), so they only compile
//! and run with `--features observe` (or `cargo test --workspace`, where
//! `temper-cli`/`temper-mcp` turn the feature on). A bare
//! `cargo test -p temper-server` silently filters them out.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;
use tower::ServiceExt;

use temper_runtime::ActorSystem;

use crate::ServerState;
use crate::registry::SpecRegistry;

/// Enforcing baseline policy: only `Admin` principals are permitted anything.
/// Non-admin (Customer/Agent) principals are denied unless a per-tenant policy
/// grants them access. Production never runs the permissive engine, so we swap
/// it out here to exercise the real deny path.
const ADMIN_ONLY_POLICY: &str = r#"permit(principal is Admin, action, resource);"#;

/// A minimal, well-formed OTS trajectory payload.
const SAMPLE_TRAJECTORY: &str =
    r#"{"metadata":{"trajectory_id":"traj-1","agent_id":"a1","outcome":"success"},"turns":[]}"#;

#[test]
fn ots_storage_trajectory_ids_are_tenant_scoped() {
    assert_eq!(
        super::tenant_scoped_ots_trajectory_id("tenant-a", "same-id"),
        "tenant-a::same-id"
    );
    assert_ne!(
        super::tenant_scoped_ots_trajectory_id("tenant-a", "same-id"),
        super::tenant_scoped_ots_trajectory_id("tenant-b", "same-id"),
        "tenants must not share a durable OTS trajectory primary key"
    );
}

/// Build a `ServerState` whose Cedar engine actually enforces (admin-only
/// baseline) instead of the permissive test default.
fn enforcing_state() -> ServerState {
    let system = ActorSystem::new("test-ots-auth");
    let state = ServerState::from_registry(system, SpecRegistry::new());
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("baseline policy should parse");
    state
}

/// Mount the management API (which owns `/ots/trajectories`) under `/api`.
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
    SecurityContext::from_resolved_identity("admin-1", "operator", None)
}

fn agent_auth(id: &str) -> SecurityContext {
    SecurityContext::from_resolved_identity(id, "worker", Some("session-1"))
}

/// Exploit (a): an anonymous caller spoofs a victim tenant via `X-Tenant-Id`
/// and reads its OTS traces. Before the fix this returned `200` with the
/// victim tenant's data; it must now be denied.
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
/// tenant. Before the fix this returned `202/201`; it must now be denied.
#[tokio::test]
async fn unauthenticated_post_ots_trajectory_is_denied() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(
            Request::post("/api/ots/trajectories")
                .header("content-type", "application/json")
                .header("X-Tenant-Id", "victim-tenant")
                .body(Body::from(SAMPLE_TRAJECTORY))
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

/// A properly-authorized (admin) same-tenant GET still succeeds — the gate must
/// not over-block legitimate callers.
#[tokio::test]
async fn authorized_admin_get_ots_trajectories_succeeds() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(
            with_auth(
                Request::get("/api/ots/trajectories")
                    .header("X-Tenant-Id", "tenant-a")
                    .body(Body::empty())
                    .unwrap(),
                "tenant-a",
                admin_auth(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "authorized admin read should still succeed"
    );
}

#[tokio::test]
async fn negative_ots_list_limit_is_rejected_before_store_query() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(with_auth(
            Request::get("/api/ots/trajectories?limit=-1")
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
        "negative limits must not reach the storage adapter"
    );
}

/// A properly-authorized (admin) same-tenant POST still succeeds.
#[tokio::test]
async fn authorized_admin_post_ots_trajectory_succeeds() {
    let app = app(enforcing_state());
    let resp = app
        .oneshot(
            with_auth(
                Request::post("/api/ots/trajectories")
                    .header("content-type", "application/json")
                    .header("X-Tenant-Id", "tenant-a")
                    .body(Body::from(SAMPLE_TRAJECTORY))
                    .unwrap(),
                "tenant-a",
                admin_auth(),
            ),
        )
        .await
        .unwrap();
    // With no durable metadata store configured the handler short-circuits to
    // `201 Created`; the point is that an authorized write is *not* forbidden.
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "authorized admin write should still succeed"
    );
}

/// Cross-tenant isolation: a credential scoped to `tenant-a` can read its own
/// trajectories but MUST NOT read `tenant-b`'s by spoofing the header.
#[tokio::test]
async fn tenant_scoped_credential_cannot_read_other_tenant() {
    let system = ActorSystem::new("test-ots-cross-tenant");
    let state = ServerState::from_registry(system, SpecRegistry::new());
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("baseline policy should parse");
    // `tenant-a` grants its agents read access to trajectories. `tenant-b`
    // grants this principal nothing (falls back to the admin-only baseline).
    state
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal is Agent, action == Action::"read_trajectories", resource is OtsTrajectory);"#,
        )
        .expect("tenant-a policy should parse");

    let app = app(state);

    // Same-tenant read (tenant-a agent → tenant-a) succeeds.
    let same_tenant = app
        .clone()
        .oneshot(
            with_auth(
                Request::get("/api/ots/trajectories")
                    .header("X-Tenant-Id", "tenant-a")
                    .body(Body::empty())
                    .unwrap(),
                "tenant-a",
                agent_auth("agent-a"),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        same_tenant.status(),
        StatusCode::OK,
        "same-tenant authorized read should succeed"
    );

    // Cross-tenant read (tenant-a agent + spoofed tenant-b header) still uses
    // the credential-bound tenant-a context.
    let cross_tenant = app
        .oneshot(
            with_auth(
                Request::get("/api/ots/trajectories")
                    .header("X-Tenant-Id", "tenant-b")
                    .body(Body::empty())
                    .unwrap(),
                "tenant-a",
                agent_auth("agent-a"),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        cross_tenant.status(),
        StatusCode::OK,
        "spoofed X-Tenant-Id must not override credential-bound tenant"
    );
}

/// Cross-tenant isolation on the write path: a credential scoped to `tenant-a`
/// can ingest its own trajectories but MUST NOT forge writes into `tenant-b`.
#[tokio::test]
async fn tenant_scoped_credential_cannot_write_other_tenant() {
    let system = ActorSystem::new("test-ots-cross-tenant-write");
    let state = ServerState::from_registry(system, SpecRegistry::new());
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("baseline policy should parse");
    // `tenant-a` grants its agents write access; `tenant-b` grants this
    // principal nothing (falls back to the admin-only baseline).
    state
        .authz
        .reload_tenant_policies(
            "tenant-a",
            r#"permit(principal is Agent, action == Action::"write_trajectories", resource is OtsTrajectory);"#,
        )
        .expect("tenant-a policy should parse");

    let app = app(state);

    // Same-tenant write (tenant-a agent → tenant-a) succeeds.
    let same_tenant = app
        .clone()
        .oneshot(
            with_auth(
                Request::post("/api/ots/trajectories")
                    .header("content-type", "application/json")
                    .header("X-Tenant-Id", "tenant-a")
                    .body(Body::from(SAMPLE_TRAJECTORY))
                    .unwrap(),
                "tenant-a",
                agent_auth("agent-a"),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        same_tenant.status(),
        StatusCode::CREATED,
        "same-tenant authorized write should succeed"
    );

    // Cross-tenant write (tenant-a agent + spoofed tenant-b header) still uses
    // the credential-bound tenant-a context.
    let cross_tenant = app
        .oneshot(
            with_auth(
                Request::post("/api/ots/trajectories")
                    .header("content-type", "application/json")
                    .header("X-Tenant-Id", "tenant-b")
                    .body(Body::from(SAMPLE_TRAJECTORY))
                    .unwrap(),
                "tenant-a",
                agent_auth("agent-a"),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        cross_tenant.status(),
        StatusCode::CREATED,
        "spoofed X-Tenant-Id must not override credential-bound tenant"
    );
}
