use super::*;
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn vault() -> Arc<SecretsVault> {
    Arc::new(SecretsVault::new(&[0x31; 32]))
}

fn request() -> Value {
    json!({
        "model": "jev-latest",
        "state": "Please connect me with a human.",
        "questions": {
            "human_requested": {
                "type": "noul",
                "instructions": "Has the customer requested a human?"
            }
        }
    })
}

fn response() -> Value {
    json!({
        "model": "jev-test",
        "answers": {"human_requested": {"type": "noul", "noul": 0.95}},
        "usage": {"input_tokens": 12, "output_tokens": 3}
    })
}

#[tokio::test]
async fn uses_tenant_credential_and_native_request_shape() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    vault
        .cache_secret("tenant-b", TYPESAFE_SECRET_NAME, "key-b".to_string())
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("Authorization", "Bearer key-b"))
        .and(header("Content-Type", "application/json"))
        .and(body_json(request()))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()))
        .expect(1)
        .mount(&server)
        .await;
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    assert_eq!(
        provider.evaluate("tenant-b", &request()).await.unwrap(),
        response()
    );
}

#[tokio::test]
async fn refuses_platform_credential_and_other_tenants_secret() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_platform_secret(TYPESAFE_SECRET_NAME, "platform-key".to_string())
        .unwrap();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    assert!(
        provider
            .evaluate("tenant-b", &request())
            .await
            .unwrap_err()
            .contains("tenant credential")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_oversized_request_before_http() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    let mut request = request();
    request["state"] = json!("a".repeat(SYSTEM_ONE_REQUEST_BYTE_BUDGET));
    assert!(
        provider
            .evaluate("tenant-a", &request)
            .await
            .unwrap_err()
            .contains("request byte budget")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejects_oversized_response() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("a".repeat(SYSTEM_ONE_RESPONSE_BYTE_BUDGET + 1)),
        )
        .mount(&server)
        .await;
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    assert!(
        provider
            .evaluate("tenant-a", &request())
            .await
            .unwrap_err()
            .contains("response byte budget")
    );
}

#[tokio::test]
async fn redacts_provider_error_body() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("key-a private-customer-message"))
        .mount(&server)
        .await;
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    let error = provider.evaluate("tenant-a", &request()).await.unwrap_err();
    assert_eq!(error, "System One provider returned HTTP 401");
    assert!(!error.contains("key-a"));
    assert!(!error.contains("private-customer-message"));
}

#[tokio::test]
async fn rejects_redirect_without_forwarding_credential() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", destination.uri()))
        .mount(&server)
        .await;
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    let error = provider.evaluate("tenant-a", &request()).await.unwrap_err();
    assert_eq!(error, "System One provider returned HTTP 307");
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn exhausted_concurrency_budget_does_not_queue_or_send() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    let _permits = provider
        .evaluations
        .try_acquire_many(SYSTEM_ONE_CONCURRENCY_BUDGET as u32)
        .unwrap();
    assert!(
        provider
            .evaluate("tenant-a", &request())
            .await
            .unwrap_err()
            .contains("concurrency budget")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_json_response_has_no_body_in_error() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("private-customer-message"))
        .mount(&server)
        .await;
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    assert_eq!(
        provider.evaluate("tenant-a", &request()).await.unwrap_err(),
        "System One provider returned invalid JSON"
    );
}

#[tokio::test]
async fn delayed_provider_response_obeys_total_deadline() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret("tenant-a", TYPESAFE_SECRET_NAME, "key-a".to_string())
        .unwrap();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(response())
                .set_delay(SYSTEM_ONE_EVALUATION_TIMEOUT + Duration::from_secs(1)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    assert_eq!(
        provider.evaluate("tenant-a", &request()).await.unwrap_err(),
        "System One evaluation timed out"
    );
    assert_eq!(
        provider.evaluations.available_permits(),
        SYSTEM_ONE_CONCURRENCY_BUDGET,
        "timeouts must release their concurrency permits"
    );
}

#[tokio::test]
async fn invalid_authorization_header_is_redacted_and_not_sent() {
    let server = MockServer::start().await;
    let vault = vault();
    vault
        .cache_secret(
            "tenant-a",
            TYPESAFE_SECRET_NAME,
            "key-a\nprivate-value".to_string(),
        )
        .unwrap();
    let provider = TypesafeSystemOneProvider::for_test(vault, server.uri());
    assert_eq!(
        provider.evaluate("tenant-a", &request()).await.unwrap_err(),
        "System One provider transport failed"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
