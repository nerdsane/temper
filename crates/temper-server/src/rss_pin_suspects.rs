//! Retained-memory check for the suspects in the temper pin
//! `37b343746843..40cf14fdf756`.
//!
//! Each wave is a realistic burst: classify a mixed Cedar list policy a few
//! thousand times, materialize one OData page out of a catalog-sized id list,
//! and admit several max-size HTTP bodies. A second and third wave must reuse
//! that working set. Growth across waves is a retainer.

use std::collections::HashMap; // determinism-ok: SecurityContext fields are HashMap

use axum::body::{Body, to_bytes};
use temper_authz::{AuthzEngine, ListReadUniformity, Principal, PrincipalKind, SecurityContext};

fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status") // determinism-ok: test RSS probe reads /proc
        .expect("status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .expect("VmRSS kilobytes")
                .parse()
                .expect("VmRSS is a number");
            return kb * 1024;
        }
    }
    panic!("VmRSS missing");
}

fn customer() -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: "reader".to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: HashMap::new(), // determinism-ok: SecurityContext field type
        },
        context_attrs: HashMap::new(), // determinism-ok: SecurityContext field type
        correlation_id: "rss-pin".to_string(),
        cause: None,
        credential: None,
    }
}

fn authz_wave(engine: &AuthzEngine, ctx: &SecurityContext) {
    for _ in 0..2_000 {
        let decision = engine.list_read_uniformity("default", ctx, "read", "File");
        std::hint::black_box(&decision);
        assert!(
            matches!(
                decision,
                ListReadUniformity::PerEntity {
                    attribute_keys: Some(_)
                }
            ),
            "status policy must stay per-entity, not allocate a new shape"
        );
    }
}

/// The shape `sparse_allowed_ids` keeps after the batching fix: one 128-row
/// batch of documents, then only the ids that survived the decision.
fn catalog_page_wave() -> Vec<serde_json::Value> {
    let mut page = Vec::new();
    let mut batch = Vec::with_capacity(128);
    for id in 0..10_000 {
        let document = serde_json::json!({
            "entity_id": format!("fl-{id:05}"),
            "status": "published",
            "fields": { "name": format!("file-{id}"), "bytes": "x".repeat(1024) }
        });
        batch.push(document);
        if batch.len() == 128 {
            let kept = batch
                .iter()
                .filter(|row| {
                    row.get("status").and_then(|value| value.as_str()) == Some("published")
                })
                .map(|row| {
                    row.get("entity_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect::<Vec<_>>();
            if page.len() < 100 {
                page.extend(batch.drain(..).take(100 - page.len()));
            } else {
                batch.clear();
            }
            std::hint::black_box(kept);
        }
    }
    page
}

async fn admission_wave() {
    for _ in 0..4 {
        let body = Body::from(vec![7u8; 8 * 1024 * 1024]);
        let bytes = to_bytes(body, 8 * 1024 * 1024)
            .await
            .expect("8MiB body is inside the admission cap");
        assert_eq!(bytes.len(), 8 * 1024 * 1024);
        drop(bytes);
    }
}

#[tokio::test]
async fn pin_suspects_plateau_under_repeated_list_and_admission_load() {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies(
            "default",
            r#"
            permit(principal, action == Action::"read", resource is File)
            when { resource.status == "published" };
            "#,
        )
        .expect("policy");
    let ctx = customer();

    authz_wave(&engine, &ctx);
    let _ = catalog_page_wave();
    admission_wave().await;
    let first = rss_bytes();

    authz_wave(&engine, &ctx);
    let page = catalog_page_wave();
    assert_eq!(
        page.len(),
        100,
        "a list wave returns one page, not the catalog"
    );
    admission_wave().await;
    let second = rss_bytes();

    authz_wave(&engine, &ctx);
    let _ = catalog_page_wave();
    admission_wave().await;
    let third = rss_bytes();

    let grew = third.saturating_sub(first);
    assert!(
        grew < 32 * 1024 * 1024,
        "repeated list/admission load grew RSS by {grew} bytes (first={first} second={second} third={third})"
    );
}
