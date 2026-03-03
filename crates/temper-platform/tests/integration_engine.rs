//! Integration engine tests.
//!
//! Covers the full integration pipeline:
//! - IOA specs with `[[integration]]` sections parse correctly
//! - IntegrationRegistry lookups match parsed spec integrations
//! - WebhookDispatcher delivers events to a live mock server (wiremock)
//! - Verification cascade still passes for specs with integrations
//! - IntegrationEngine background task dispatches via channel

use std::collections::BTreeMap;

use chrono::Utc;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use temper_platform::integration::{
    IntegrationConfig, IntegrationEngine, IntegrationEvent, IntegrationRegistry, IntegrationStatus,
    RetryPolicy, WebhookConfig, WebhookDispatcher,
};
use temper_spec::automaton::parse_automaton;
use temper_verify::cascade::VerificationCascade;

const ORDER_IOA_WITH_INTEGRATIONS: &str =
    include_str!("../../../reference-apps/ecommerce/specs/order.ioa.toml");

// -----------------------------------------------------------------------
// Parser → Registry integration
// -----------------------------------------------------------------------

#[test]
fn parsed_integrations_populate_registry() {
    let automaton = parse_automaton(ORDER_IOA_WITH_INTEGRATIONS).expect("should parse");
    assert_eq!(automaton.integrations.len(), 3);

    // Build IntegrationConfigs from parsed Integration structs (production would
    // read deployment config; here we synthesize configs from the spec).
    let configs: Vec<IntegrationConfig> = automaton
        .integrations
        .iter()
        .map(|ig| IntegrationConfig {
            name: ig.name.clone(),
            trigger: ig.trigger.clone(),
            webhook: WebhookConfig {
                url: format!("https://example.com/{}", ig.name),
                method: "POST".to_string(),
                headers: BTreeMap::new(),
                timeout_ms: 5000,
            },
            retry: RetryPolicy::default(),
        })
        .collect();

    let registry = IntegrationRegistry::from_configs(configs);
    assert_eq!(registry.len(), 3);

    // SubmitOrder triggers notify_fulfillment
    let submit = registry.lookup("SubmitOrder");
    assert_eq!(submit.len(), 1);
    assert_eq!(submit[0].name, "notify_fulfillment");

    // ConfirmOrder triggers charge_payment
    let confirm = registry.lookup("ConfirmOrder");
    assert_eq!(confirm.len(), 1);
    assert_eq!(confirm[0].name, "charge_payment");

    // ShipOrder triggers notify_shipping
    let ship = registry.lookup("ShipOrder");
    assert_eq!(ship.len(), 1);
    assert_eq!(ship[0].name, "notify_shipping");

    // AddItem triggers nothing
    assert!(registry.lookup("AddItem").is_empty());
}

#[test]
fn spec_with_integrations_in_entity_spec() {
    let automaton = parse_automaton(ORDER_IOA_WITH_INTEGRATIONS).expect("should parse");
    // Ensure the automaton itself carries integration metadata
    assert_eq!(automaton.integrations[0].name, "notify_fulfillment");
    assert_eq!(automaton.integrations[0].trigger, "SubmitOrder");
    assert_eq!(automaton.integrations[0].integration_type, "webhook");
    assert_eq!(automaton.integrations[1].name, "charge_payment");
    assert_eq!(automaton.integrations[2].name, "notify_shipping");
}

// -----------------------------------------------------------------------
// Webhook dispatcher with wiremock
// -----------------------------------------------------------------------

fn test_event(event_name: &str) -> IntegrationEvent {
    IntegrationEvent {
        tenant: "test-tenant".to_string(),
        entity_type: "Order".to_string(),
        entity_id: "order-42".to_string(),
        event_name: event_name.to_string(),
        from_status: "Draft".to_string(),
        to_status: "Submitted".to_string(),
        params: json!({"item_count": 3}),
        timestamp: Utc::now(),
    }
}

#[tokio::test]
async fn webhook_dispatcher_success() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let config = IntegrationConfig {
        name: "test_hook".to_string(),
        trigger: "SubmitOrder".to_string(),
        webhook: WebhookConfig {
            url: format!("{}/webhook", server.uri()),
            method: "POST".to_string(),
            headers: BTreeMap::new(),
            timeout_ms: 5000,
        },
        retry: RetryPolicy {
            max_retries: 0,
            backoff_base_ms: 100,
        },
    };

    let dispatcher = WebhookDispatcher::new();
    let event = test_event("SubmitOrder");

    let result = dispatcher.dispatch(&config, &event).await;
    assert!(matches!(result.status, IntegrationStatus::Success));
}

#[tokio::test]
async fn webhook_dispatcher_retries_on_failure() {
    let server = MockServer::start().await;

    // Respond with 500 on every attempt — all retries should fail.
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3) // 1 initial + 2 retries
        .mount(&server)
        .await;

    let config = IntegrationConfig {
        name: "failing_hook".to_string(),
        trigger: "SubmitOrder".to_string(),
        webhook: WebhookConfig {
            url: format!("{}/webhook", server.uri()),
            method: "POST".to_string(),
            headers: BTreeMap::new(),
            timeout_ms: 5000,
        },
        retry: RetryPolicy {
            max_retries: 2,
            backoff_base_ms: 10, // Fast backoff for tests
        },
    };

    let dispatcher = WebhookDispatcher::new();
    let event = test_event("SubmitOrder");

    let result = dispatcher.dispatch(&config, &event).await;
    assert!(matches!(result.status, IntegrationStatus::Failed(_)));
}

#[tokio::test]
async fn webhook_dispatcher_sends_json_body() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/webhook"))
        .and(wiremock::matchers::header(
            "content-type",
            "application/json",
        ))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let config = IntegrationConfig {
        name: "json_hook".to_string(),
        trigger: "SubmitOrder".to_string(),
        webhook: WebhookConfig {
            url: format!("{}/webhook", server.uri()),
            method: "POST".to_string(),
            headers: BTreeMap::new(),
            timeout_ms: 5000,
        },
        retry: RetryPolicy {
            max_retries: 0,
            backoff_base_ms: 100,
        },
    };

    let dispatcher = WebhookDispatcher::new();
    let event = test_event("SubmitOrder");

    let result = dispatcher.dispatch(&config, &event).await;
    assert!(matches!(result.status, IntegrationStatus::Success));
}

#[tokio::test]
async fn webhook_dispatcher_custom_headers() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/webhook"))
        .and(wiremock::matchers::header("x-api-key", "secret-123"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let mut headers = BTreeMap::new();
    headers.insert("x-api-key".to_string(), "secret-123".to_string());

    let config = IntegrationConfig {
        name: "auth_hook".to_string(),
        trigger: "SubmitOrder".to_string(),
        webhook: WebhookConfig {
            url: format!("{}/webhook", server.uri()),
            method: "POST".to_string(),
            headers,
            timeout_ms: 5000,
        },
        retry: RetryPolicy {
            max_retries: 0,
            backoff_base_ms: 100,
        },
    };

    let dispatcher = WebhookDispatcher::new();
    let event = test_event("SubmitOrder");

    let result = dispatcher.dispatch(&config, &event).await;
    assert!(matches!(result.status, IntegrationStatus::Success));
}

// -----------------------------------------------------------------------
// Engine process_event (synchronous test helper)
// -----------------------------------------------------------------------

#[tokio::test]
async fn engine_process_event_dispatches_matching() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/fulfillment"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let configs = vec![IntegrationConfig {
        name: "fulfillment".to_string(),
        trigger: "SubmitOrder".to_string(),
        webhook: WebhookConfig {
            url: format!("{}/fulfillment", server.uri()),
            method: "POST".to_string(),
            headers: BTreeMap::new(),
            timeout_ms: 5000,
        },
        retry: RetryPolicy {
            max_retries: 0,
            backoff_base_ms: 100,
        },
    }];

    let registry = IntegrationRegistry::from_configs(configs);
    let dispatcher = WebhookDispatcher::new();
    let event = test_event("SubmitOrder");

    let results = IntegrationEngine::process_event(&registry, &dispatcher, &event).await;
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].status, IntegrationStatus::Success));
}

#[tokio::test]
async fn engine_process_event_skips_unmatched() {
    let registry = IntegrationRegistry::from_configs(vec![IntegrationConfig {
        name: "fulfillment".to_string(),
        trigger: "SubmitOrder".to_string(),
        webhook: WebhookConfig {
            url: "https://example.com/fulfillment".to_string(),
            method: "POST".to_string(),
            headers: BTreeMap::new(),
            timeout_ms: 5000,
        },
        retry: RetryPolicy::default(),
    }]);

    let dispatcher = WebhookDispatcher::new();
    let event = test_event("CancelOrder"); // No integration registered

    let results = IntegrationEngine::process_event(&registry, &dispatcher, &event).await;
    assert!(results.is_empty());
}

// -----------------------------------------------------------------------
// Verification cascade: specs with [[integration]] still pass
// -----------------------------------------------------------------------

#[test]
fn verification_cascade_passes_with_integrations() {
    let cascade = VerificationCascade::from_ioa(ORDER_IOA_WITH_INTEGRATIONS)
        .with_max_items(2)
        .with_sim_seeds(3)
        .with_prop_test_cases(50);

    let result = cascade.run();
    assert!(
        result.all_passed,
        "cascade should pass for spec with integrations: {:#?}",
        result.levels
    );
}
