//! Store setup, HTTP requests, and assertions for conditional secret writes.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::secrets::vault::SecretsVault;
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

/// Tenant used by this isolated test server.
const TENANT: &str = "default";
/// Principal permitted to manage test secrets.
pub(super) const WRITER: &str = "secret-writer";
/// Writers competing under the same precondition.
pub(super) const CONCURRENT_WRITERS: usize = 8;

/// A server whose secrets live in a real store. Dropping it drops the store.
pub(super) struct Backend {
    state: ServerState,
    _temp: Option<tempfile::TempDir>,
    postgres: Option<(sqlx::PgPool, String)>,
}

impl Backend {
    /// Create a file-backed Turso server.
    pub(super) async fn turso(name: &str) -> Self {
        let temp = tempfile::tempdir().expect("temporary test directory");
        let db_url = format!("file:{}", temp.path().join("metadata.db").display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create Turso test store");
        let mut state = ServerState::from_registry(ActorSystem::new(name), SpecRegistry::new());
        state.data_dir = temp.path().join("data");
        state.set_storage_stack(StorageStack::from_turso(store));
        Self::with_policy(state, Some(temp), None)
    }

    /// Create a Postgres server in a fresh schema.
    pub(super) async fn postgres(name: &str) -> Self {
        let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
        let options: PgConnectOptions = url.parse().expect("PostgreSQL URL");
        let pool = PgPoolOptions::new()
            .max_connections(CONCURRENT_WRITERS as u32 + 2)
            .connect_with(options.options([("search_path", "pg_catalog")]))
            .await
            .expect("connect to PostgreSQL");
        let schema_name = format!(
            "secret_cas_{}",
            temper_runtime::scheduler::sim_uuid().simple()
        );
        let schema = PostgresSchema::new(schema_name.clone()).expect("schema name");
        migration::run_migrations_in_schema(&pool, &schema)
            .await
            .expect("migrate PostgreSQL schema");
        let store = PostgresEventStore::with_schema(pool.clone(), schema);
        let mut state = ServerState::from_registry(ActorSystem::new(name), SpecRegistry::new());
        let data_dir = tempfile::tempdir().expect("temporary test directory");
        state.data_dir = data_dir.path().join("data");
        state.set_storage_stack(StorageStack::from_postgres(store));
        Self::with_policy(state, Some(data_dir), Some((pool, schema_name)))
    }

    fn with_policy(
        mut state: ServerState,
        temp: Option<tempfile::TempDir>,
        postgres: Option<(sqlx::PgPool, String)>,
    ) -> Self {
        state.secrets_vault = Some(std::sync::Arc::new(SecretsVault::new(&[21_u8; 32])));
        state
            .authz
            .reload_tenant_policies(
                TENANT,
                &format!(
                    r#"
permit(
  principal == Customer::"{WRITER}",
  action == Action::"manage_secrets",
  resource
);
"#
                ),
            )
            .expect("secret policy should parse");
        Self {
            state,
            _temp: temp,
            postgres,
        }
    }

    /// Remove the disposable Postgres schema, if present.
    pub(super) async fn finish(self) {
        if let Some((pool, schema_name)) = self.postgres {
            sqlx::query(&format!("DROP SCHEMA \"{schema_name}\" CASCADE"))
                .execute(&pool)
                .await
                .expect("drop test schema");
            pool.close().await;
        }
    }

    /// Build the real HTTP router for this store.
    pub(super) fn router(&self) -> Router {
        build_router(self.state.clone())
    }

    /// The value the store holds for `key`, read back from the store rather
    /// than from this server's in-memory cache.
    pub(super) async fn stored_value(&self, key: &str) -> Option<String> {
        let vault = self.state.secrets_vault.as_ref().expect("vault");
        vault.remove_secret(TENANT, key);
        self.state
            .load_tenant_secrets(TENANT)
            .await
            .expect("load secrets from the store");
        vault.get_secret(TENANT, key)
    }
}

fn security_context(id: &str) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: id.to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: format!("secret-cas-{id}"),
        cause: None,
        credential: None,
    }
}

fn request(
    method: Method,
    uri: &str,
    body: Option<&str>,
    principal: &str,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = body
        .map(|value| serde_json::json!({ "value": value }).to_string())
        .unwrap_or_default();
    let mut request = builder
        .body(Body::from(body))
        .expect("request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(TENANT),
            security_context(principal),
        ));
    request
}

/// What a PUT answered: its status, its `ETag` header and its body.
#[derive(Debug)]
pub(super) struct PutOutcome {
    /// Response status.
    pub(super) status: StatusCode,
    /// Response etag, if present.
    pub(super) etag: Option<String>,
    /// Response body.
    pub(super) body: String,
    /// Value submitted by this writer.
    pub(super) value: String,
}

/// Send a secret PUT through the real router.
pub(super) async fn put(
    app: &Router,
    key: &str,
    value: &str,
    principal: &str,
    headers: &[(&str, &str)],
) -> PutOutcome {
    let response = app
        .clone()
        .oneshot(request(
            Method::PUT,
            &format!("/api/tenants/{TENANT}/secrets/{key}"),
            Some(value),
            principal,
            headers,
        ))
        .await
        .expect("secret PUT should run");
    let status = response.status();
    let etag = response
        .headers()
        .get("etag")
        .map(|value| value.to_str().expect("ETag is ASCII").to_string());
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read PUT body");
    PutOutcome {
        status,
        etag,
        body: String::from_utf8_lossy(&body).into_owned(),
        value: value.to_string(),
    }
}

/// The etag the secret listing reports for `key`.
pub(super) async fn listed_etag(app: &Router, key: &str) -> Option<String> {
    let response = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/api/tenants/{TENANT}/secrets"),
            None,
            WRITER,
            &[],
        ))
        .await
        .expect("secret list should run");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read list body");
    let listing: serde_json::Value = serde_json::from_slice(&body).expect("list JSON");
    assert!(
        listing["keys"]
            .as_array()
            .expect("keys stay a list of names")
            .iter()
            .any(|name| name == key),
        "the listing still names {key}: {listing}"
    );
    listing["etags"][key].as_str().map(str::to_string)
}

/// Assert a successful write and return its strong etag.
pub(super) fn written(outcome: &PutOutcome) -> String {
    assert_eq!(
        outcome.status,
        StatusCode::NO_CONTENT,
        "write should succeed: {outcome:?}"
    );
    let etag = outcome.etag.clone().expect("a successful PUT returns ETag");
    assert!(
        etag.len() > 2 && etag.starts_with('"') && etag.ends_with('"'),
        "ETag is a quoted strong validator: {etag}"
    );
    etag
}

/// Assert a failed precondition without revealing the current etag.
pub(super) fn refused(outcome: &PutOutcome, current_etag: &str) {
    assert_eq!(
        outcome.status,
        StatusCode::PRECONDITION_FAILED,
        "write should be refused: {outcome:?}"
    );
    assert_eq!(outcome.etag, None, "a 412 carries no ETag");
    assert!(
        !outcome.body.contains(current_etag.trim_matches('"')),
        "a 412 never reveals the current etag: {}",
        outcome.body
    );
}
