//! Strict OData requests use the actual HTTP router and PostgreSQL actor runtime.
use std::collections::HashMap;
use std::sync::Arc;

use axum::{body::Body, http::Request, middleware::Next};
use deadpool_postgres::Pool;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use temper_actor_runtime::{ActorHandle, ActorSystem, SchedulerConfig, SpecDrivenActor};
use temper_runtime::tenant::TenantId;
use temper_server::{
    ServerState, build_router,
    registry::{EntityVerificationResult, SpecRegistry, VerificationStatus},
};
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;

const CSDL: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const SPEC: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
strict_action_params = true
[[state]]
name = "Notes"
type = "string"
initial = "draft note"
[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
params = ["Notes"]
"#;

async fn pool() -> (Pool, Option<ContainerAsync<Postgres>>) {
    let mut config = deadpool_postgres::Config::new();
    let container = if let Ok(url) = std::env::var("TEMPER_ACTOR_TEST_DATABASE_URL") {
        let parsed: tokio_postgres::Config = url.parse().unwrap();
        assert!(
            parsed.get_hosts().iter().all(|host| matches!(host,
            tokio_postgres::config::Host::Tcp(name) if name == "127.0.0.1" || name == "localhost"))
        );
        assert!(
            parsed
                .get_dbname()
                .is_some_and(|name| name.starts_with("temper_test_"))
        );
        config.url = Some(url);
        None
    } else {
        let container = Postgres::default().start().await.unwrap();
        config.host = Some(container.get_host().await.unwrap().to_string());
        config.port = Some(container.get_host_port_ipv4(5432).await.unwrap());
        config.user = Some("postgres".into());
        config.password = Some("postgres".into());
        config.dbname = Some("postgres".into());
        Some(container)
    };
    let pool = config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();
    temper_actor_runtime::schema::create_tables(&pool.get().await.unwrap())
        .await
        .unwrap();
    (pool, container)
}

async fn actor_state(pool: &Pool, handle: &ActorHandle) -> Vec<u8> {
    pool.get()
        .await
        .unwrap()
        .query_one(
            temper_actor_runtime::schema::LOAD_ACTOR,
            &[&handle.namespace, &handle.actor_type],
        )
        .await
        .unwrap()
        .get("state")
}

async fn message_count(pool: &Pool, namespace: &str) -> i64 {
    pool.get()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM odp_temper.actor_messages WHERE namespace = $1",
            &[&namespace],
        )
        .await
        .unwrap()
        .get(0)
}

async fn test_identity(mut request: Request<Body>, next: Next) -> axum::response::Response {
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "strict-test",
                "test-agent",
                None,
            ),
        ));
    next.run(request).await
}

#[tokio::test]
async fn strict_postgres_http_preserves_the_contract_and_acknowledges_only_enqueueing() {
    let (pool, _container) = pool().await;
    let actors = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    actors
        .register(Arc::new(
            SpecDrivenActor::from_ioa(SPEC, HashMap::new()).unwrap(),
        ))
        .await
        .unwrap();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Order", SPEC)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-09-06T00:00:00Z".into(),
        }),
    );
    let mut state = ServerState::from_pg_registry(actors.clone(), registry);
    state.actor_backed_types.insert("Order".into());
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    // Authentication is a local fixture; router, policy, storage and actor execution are real.
    let router = build_router(state).layer(axum::middleware::from_fn(test_identity));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = Client::new();
    let id = uuid::Uuid::new_v4().to_string();
    let rejected_id = uuid::Uuid::new_v4().to_string();
    let handle = ActorHandle::new(format!("default/{id}"), "Order");
    let entity_url = format!("{base}/tdata/Orders('{id}')");
    let action_url = format!("{entity_url}/Temper.SubmitOrder");

    let response = client
        .post(format!("{base}/tdata/Orders"))
        .json(&json!({"id": rejected_id, "Notes": "forged"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        actors
            .load_state(&format!("default/{rejected_id}"), "Order")
            .await
            .unwrap()
            .is_none()
    );
    let response = client
        .post(format!("{base}/tdata/Orders"))
        .json(&json!({"id": id}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let initial = actor_state(&pool, &handle).await;
    let initial_json: Value = serde_json::from_slice(&initial).unwrap();
    assert_eq!(initial_json["fields"]["Notes"], "draft note");
    for body in ["{", "null", "[]", r#"{"Notes":"allowed","forged":true}"#] {
        let response = client
            .post(&action_url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "accepted invalid body: {body}"
        );
        assert_eq!(message_count(&pool, &handle.namespace).await, 0);
        assert_eq!(actor_state(&pool, &handle).await, initial);
    }
    for method in [
        reqwest::Method::PATCH,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        let response = client
            .request(method, &entity_url)
            .json(&json!({"Notes":"forged"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(actor_state(&pool, &handle).await, initial);
    }
    let response = client
        .post(&action_url)
        .json(&json!({"Notes":"requested"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let receipt: Value = response.json().await.unwrap();
    assert_eq!(receipt["accepted"], true);
    let message_id = receipt["message_id"]
        .as_i64()
        .expect("accepted request must identify the queued message");
    assert!(message_id > 0);
    assert_eq!(message_count(&pool, &handle.namespace).await, 1);
    assert_eq!(actor_state(&pool, &handle).await, initial);
    let before: Value = client
        .get(&entity_url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["status"], "Draft");
    actors.activate_now(&handle).await.unwrap();
    let after: Value = client
        .get(&entity_url)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["status"], "Submitted");
    assert_eq!(after["fields"]["Notes"], "requested");
    let cursor: i64 = pool.get().await.unwrap().query_one(
        "SELECT last_msg_id FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = 'Order'",
        &[&handle.namespace]).await.unwrap().get(0);
    assert_eq!(cursor, message_id);
    server.abort();
}
