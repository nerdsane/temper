use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

use super::build_observe_router;
use crate::registry::{
    RegistryQuarantineFailure, RegistryQuarantineReason, RegistryQuarantineSource,
    RegistryRestoreHealth, RegistryTenantQuarantine, SpecRegistry,
};
use crate::state::ServerState;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn health_app(restore_health: RegistryRestoreHealth) -> Router {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    registry.record_restore_health(&restore_health);
    let state = ServerState::from_registry(ActorSystem::new("registry-health-test"), registry);
    Router::new()
        .nest("/observe", build_observe_router())
        .with_state(state)
}

async fn get_health(app: Router) -> serde_json::Value {
    let response = app
        .oneshot(Request::get("/observe/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn get_metrics(app: Router) -> String {
    let response = app
        .oneshot(
            Request::get("/observe/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test]
async fn registry_restore_health_is_healthy_when_nothing_is_quarantined() {
    let health = get_health(health_app(RegistryRestoreHealth::default())).await;

    assert_eq!(health["status"], "healthy");
    assert_eq!(health["registry_restore"]["restored_specs"], 0);
    assert_eq!(
        health["registry_restore"]["quarantined_tenants"],
        serde_json::json!({})
    );
}

#[tokio::test]
async fn registry_restore_quarantine_degrades_health_with_exact_entity_reason() {
    let health = get_health(health_app(RegistryRestoreHealth {
        restored_specs: 1,
        quarantined_tenants: BTreeMap::from([(
            "broken-tenant".to_string(),
            RegistryTenantQuarantine {
                entity_failures: BTreeMap::from([(
                    "Order".to_string(),
                    RegistryQuarantineFailure {
                        spec_version: 3,
                        constraint_version: None,
                        reason: RegistryQuarantineReason::InvalidCsdl,
                        source_kind: RegistryQuarantineSource::Csdl,
                        source_line: Some(4),
                        source_column: Some(9),
                        acknowledged: true,
                        detail: "not exposed by health".to_string(),
                    },
                )]),
            },
        )]),
    }))
    .await;

    assert_eq!(health["status"], "degraded");
    assert_eq!(
        health["registry_restore"]["quarantined_tenants"]["broken-tenant"]["entity_failures"]["Order"]
            ["reason"],
        "invalid_csdl"
    );
    assert_eq!(health["registry_restore"]["quarantined_specs"], 1);
    assert_eq!(health["registry_restore"]["truncated"], false);
    assert_eq!(
        health["registry_restore"]["quarantined_tenants"]["broken-tenant"]["entity_failures"]["Order"]
            ["acknowledged"],
        true
    );
    assert!(
        health["registry_restore"]["quarantined_tenants"]["broken-tenant"]
            ["entity_failures"]["Order"]
            .get("detail")
            .is_none(),
        "unauthenticated health must not expose parser diagnostics"
    );
}

#[tokio::test]
async fn registry_restore_health_bounds_untrusted_quarantine_dimensions() {
    let quarantined_tenants = (0..(super::metrics::HEALTH_QUARANTINE_ENTRY_BUDGET + 7))
        .map(|index| {
            (
                format!("tenant-{index:03}"),
                RegistryTenantQuarantine {
                    entity_failures: BTreeMap::from([(
                        "Order".to_string(),
                        RegistryQuarantineFailure {
                            spec_version: 1,
                            constraint_version: None,
                            reason: RegistryQuarantineReason::InvalidCsdl,
                            source_kind: RegistryQuarantineSource::Csdl,
                            source_line: None,
                            source_column: None,
                            acknowledged: false,
                            detail: "private".to_string(),
                        },
                    )]),
                },
            )
        })
        .collect();
    let health = get_health(health_app(RegistryRestoreHealth {
        restored_specs: 0,
        quarantined_tenants,
    }))
    .await;

    assert_eq!(health["registry_restore"]["quarantined_specs"], 71);
    assert_eq!(health["registry_restore"]["visible_quarantines"], 64);
    assert_eq!(health["registry_restore"]["truncated"], true);
    assert_eq!(
        health["registry_restore"]["quarantined_tenants"]
            .as_object()
            .map(serde_json::Map::len),
        Some(64)
    );
}

#[tokio::test]
async fn registry_restore_metrics_are_exact_label_free_gauges() {
    let app = health_app(RegistryRestoreHealth {
        restored_specs: 0,
        quarantined_tenants: BTreeMap::from([(
            "private-tenant-name".to_string(),
            RegistryTenantQuarantine {
                entity_failures: BTreeMap::from([(
                    "PrivateEntity".to_string(),
                    RegistryQuarantineFailure {
                        spec_version: 1,
                        constraint_version: None,
                        reason: RegistryQuarantineReason::InvalidCsdl,
                        source_kind: RegistryQuarantineSource::Csdl,
                        source_line: None,
                        source_column: None,
                        acknowledged: false,
                        detail: "private".to_string(),
                    },
                )]),
            },
        )]),
    });
    let metrics = get_metrics(app).await;
    assert!(metrics.contains("temper_registry_restore_quarantined_specs 1"));
    assert!(metrics.contains("temper_registry_restore_quarantined_tenants 1"));
    assert!(!metrics.contains("private-tenant-name"));
    assert!(!metrics.contains("PrivateEntity"));
}
