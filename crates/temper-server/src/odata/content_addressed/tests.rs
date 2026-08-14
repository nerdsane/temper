use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use sha1::Digest as _;
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt as _;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::blob_store::{BlobIngestBudget, BlobIngestProgressPolicy};
use crate::blobs::FIELD_OVERFLOW_REF_KEY;
use crate::secrets::vault::SecretsVault;
use crate::state::ServerState;

const BLOB_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.RawIngestTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Blob">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="RepositoryId" Type="Edm.String" Nullable="false"/>
        <Property Name="Size" Type="Edm.Int64" Nullable="false"/>
        <Property Name="Content" Type="Edm.Binary" Nullable="false"/>
        <Property Name="CanonicalBytes" Type="Edm.Binary" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="CreatedAt" Type="Edm.DateTimeOffset" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Blobs" EntityType="Temper.RawIngestTest.Blob"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const BLOB_IOA: &str = r#"
[automaton]
name = "Blob"
states = ["Durable"]
initial = "Durable"

[[action]]
name = "Create"
kind = "input"
from = ["Durable"]
to = "Durable"
params = ["RepositoryId", "Size", "Content", "CanonicalBytes", "CreatedAt"]
"#;

fn security_context() -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: "raw-ingest-test".to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "raw-ingest-test".to_string(),
    }
}

async fn authenticate(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context(),
        ));
    next.run(request).await
}

fn test_state() -> (ServerState, tempfile::TempDir) {
    let csdl = parse_csdl(BLOB_CSDL).expect("Blob test CSDL");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Blob".to_string(), BLOB_IOA.to_string());
    let mut state = ServerState::with_specs(
        ActorSystem::new("raw-blob-ingest-http"),
        csdl,
        BLOB_CSDL.to_string(),
        specs,
    )
    .expect("Blob test state");
    let data_dir = tempfile::tempdir().expect("raw Blob data dir");
    state.data_dir = data_dir.path().to_path_buf();
    allow_blob_create(&state);
    (state, data_dir)
}

fn allow_blob_create(state: &ServerState) {
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
permit(principal, action == Action::"create", resource is Blob);
permit(principal, action == Action::"read", resource is Blob);
"#,
        )
        .expect("install Blob create policy");
}

fn app(state: ServerState) -> Router {
    crate::router::build_router(state).layer(axum::middleware::from_fn(authenticate))
}

fn git_blob_id(body: &[u8]) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(format!("blob {}\0", body.len()).as_bytes());
    hasher.update(body);
    format!("{:x}", hasher.finalize())
}

fn ingest_request(body: Body, declared_len: usize, expected_id: &str) -> Request<Body> {
    Request::post("/tdata/Blobs/Temper.IngestRaw")
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", declared_len.to_string())
        .header("X-Expected-Object-Id", expected_id)
        .header("X-Repository-Id", "repository-1")
        .body(body)
        .expect("raw Blob request")
}

fn counted_body(bytes: &'static [u8], polls: Arc<AtomicUsize>) -> Body {
    Body::from_stream(futures_util::stream::once(async move {
        polls.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(Bytes::from_static(bytes))
    }))
}

#[tokio::test]
async fn raw_ingest_cors_preflight_allows_required_headers() {
    let (state, _data_dir) = test_state();
    let response = app(state)
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/tdata/Blobs/Temper.IngestRaw")
                .header("Origin", "https://client.example")
                .header("Access-Control-Request-Method", "POST")
                .header(
                    "Access-Control-Request-Headers",
                    "content-type,x-repository-id,x-expected-object-id",
                )
                .body(Body::empty())
                .expect("preflight request"),
        )
        .await
        .expect("preflight response");

    assert_eq!(response.status(), StatusCode::OK);
    let allowed = response
        .headers()
        .get("access-control-allow-headers")
        .and_then(|value| value.to_str().ok())
        .expect("allowed CORS headers")
        .to_ascii_lowercase();
    assert!(allowed.contains("x-repository-id"));
    assert!(allowed.contains("x-expected-object-id"));
}

async fn staging_entry_count(path: &std::path::Path) -> usize {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return 0;
    }
    let mut entries = tokio::fs::read_dir(path).await.expect("read staging dir");
    let mut count = 0;
    while entries
        .next_entry()
        .await
        .expect("next staging entry")
        .is_some()
    {
        count += 1;
    }
    count
}

#[tokio::test]
async fn raw_ingest_streams_overflow_fields_and_returns_metadata_only() {
    let (state, _data_dir) = test_state();
    let body = b"abc";
    let object_id = git_blob_id(body);
    let response = app(state.clone())
        .oneshot(ingest_request(
            Body::from(body.as_slice()),
            body.len(),
            &object_id,
        ))
        .await
        .expect("raw Blob response");

    assert_eq!(response.status(), StatusCode::CREATED);
    let response_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response body");
    let response_json: serde_json::Value =
        serde_json::from_slice(&response_bytes).expect("response JSON");
    assert_eq!(response_json["fields"]["Id"], object_id);
    assert!(response_json["fields"].get("Content").is_none());
    assert!(response_json["fields"].get("CanonicalBytes").is_none());

    let persisted = state
        .get_tenant_entity_state(&TenantId::default(), "Blob", &object_id)
        .await
        .expect("persisted Blob");
    assert!(
        persisted.state.fields["Content"]
            .get(FIELD_OVERFLOW_REF_KEY)
            .is_some()
    );
    assert!(
        persisted.state.fields["CanonicalBytes"]
            .get(FIELD_OVERFLOW_REF_KEY)
            .is_some()
    );

    let mut fields = persisted.state.fields.clone();
    crate::blobs::hydrate_blob_refs_for_tenant(&state, &TenantId::default(), &mut fields).await;
    assert_eq!(fields["Content"], "YWJj");
    assert_eq!(fields["CanonicalBytes"], "YmxvYiAzAGFiYw==");
}

#[tokio::test]
async fn blob_property_value_preserves_small_inline_media() {
    let (state, _data_dir) = test_state();
    let body = b"abc";
    let object_id = git_blob_id(body);
    state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "Blob",
            &object_id,
            serde_json::json!({
                "Id": object_id,
                "RepositoryId": "repository-1",
                "Size": body.len() as i64,
                "Content": "YWJj",
                "CanonicalBytes": "YmxvYiAzAGFiYw==",
                "Status": "Durable",
                "CreatedAt": "2026-01-01T00:00:00Z",
            }),
        )
        .await
        .expect("create inline Blob");
    let router = app(state);

    let content = router
        .clone()
        .oneshot(
            Request::get(format!("/tdata/Blobs('{object_id}')/Content/$value"))
                .body(Body::empty())
                .expect("content request"),
        )
        .await
        .expect("content response");
    assert_eq!(content.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(content.into_body(), 4)
            .await
            .expect("inline content"),
        body.as_slice()
    );

    let canonical = router
        .oneshot(
            Request::get(format!("/tdata/Blobs('{object_id}')/CanonicalBytes/$value"))
                .body(Body::empty())
                .expect("canonical request"),
        )
        .await
        .expect("canonical response");
    assert_eq!(canonical.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(canonical.into_body(), 11)
            .await
            .expect("inline canonical"),
        b"blob 3\0abc".as_slice()
    );
}

#[tokio::test]
async fn unauthorized_raw_ingest_does_not_poll_body() {
    let (state, _data_dir) = test_state();
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action == Action::"read", resource is Blob);"#,
        )
        .expect("install deny-create policy");
    let polls = Arc::new(AtomicUsize::new(0));
    let response = app(state.clone())
        .oneshot(ingest_request(
            counted_body(b"abc", polls.clone()),
            3,
            &git_blob_id(b"abc"),
        ))
        .await
        .expect("authorization response");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(
        state
            .list_entity_ids(&TenantId::default(), "Blob")
            .is_empty()
    );
}

#[tokio::test]
async fn declared_size_over_budget_does_not_poll_body() {
    let (mut state, _data_dir) = test_state();
    state.raw_blob_ingest_budget = BlobIngestBudget::new(2, 1);
    let polls = Arc::new(AtomicUsize::new(0));
    let response = app(state.clone())
        .oneshot(ingest_request(
            counted_body(b"abc", polls.clone()),
            3,
            &git_blob_id(b"abc"),
        ))
        .await
        .expect("budget response");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(
        state
            .list_entity_ids(&TenantId::default(), "Blob")
            .is_empty()
    );
}

#[tokio::test]
async fn wrong_digest_cleans_staging_and_creates_no_entity() {
    let (state, data_dir) = test_state();
    let response = app(state.clone())
        .oneshot(ingest_request(Body::from("abc"), 3, &git_blob_id(b"abd")))
        .await
        .expect("digest response");

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        state
            .list_entity_ids(&TenantId::default(), "Blob")
            .is_empty()
    );
    assert_eq!(
        staging_entry_count(&data_dir.path().join("blobs/.ingest-staging")).await,
        0
    );
}

#[tokio::test]
async fn short_and_long_bodies_clean_staging_and_create_no_entity() {
    for (body, declared_len, expected_status) in [
        ("ab", 3usize, StatusCode::BAD_REQUEST),
        ("abc", 2usize, StatusCode::BAD_REQUEST),
    ] {
        let (state, data_dir) = test_state();
        let expected_id = if declared_len == 3 {
            git_blob_id(b"abc")
        } else {
            git_blob_id(b"ab")
        };
        let response = app(state.clone())
            .oneshot(ingest_request(
                Body::from(body.to_string()),
                declared_len,
                &expected_id,
            ))
            .await
            .expect("length response");
        assert_eq!(response.status(), expected_status);
        assert!(
            state
                .list_entity_ids(&TenantId::default(), "Blob")
                .is_empty()
        );
        assert_eq!(
            staging_entry_count(&data_dir.path().join("blobs/.ingest-staging")).await,
            0
        );
    }
}

#[tokio::test]
async fn concurrent_declared_bytes_cannot_exceed_budget() {
    let (mut state, data_dir) = test_state();
    state.raw_blob_ingest_budget = BlobIngestBudget::new(3, 1);
    let router = app(state.clone());
    let first_polls = Arc::new(AtomicUsize::new(0));
    let first_counter = first_polls.clone();
    let first_body = Body::from_stream(async_stream::stream! {
        first_counter.fetch_add(1, Ordering::SeqCst);
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"a"));
        std::future::pending::<()>().await;
    });
    let first = tokio::spawn(router.clone().oneshot(ingest_request(
        first_body,
        3,
        &git_blob_id(b"abc"),
    )));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while first_polls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first upload should start");

    let rejected_polls = Arc::new(AtomicUsize::new(0));
    let second = router
        .oneshot(ingest_request(
            counted_body(b"xyz", rejected_polls.clone()),
            3,
            &git_blob_id(b"xyz"),
        ))
        .await
        .expect("concurrent budget response");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(rejected_polls.load(Ordering::SeqCst), 0);

    first.abort();
    let _ = first.await;
    tokio::task::yield_now().await;
    assert_eq!(
        staging_entry_count(&data_dir.path().join("blobs/.ingest-staging")).await,
        0
    );
}

mod failure_and_streaming;
