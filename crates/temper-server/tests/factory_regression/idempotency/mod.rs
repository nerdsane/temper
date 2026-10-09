//! nerdsane/temper#519 V6: an `Idempotency-Key` must be bound to the action and
//! canonical request body it was first used with.
//!
//! Every scenario runs on the warm path (same process, HTTP + actor cache) and
//! on the cold path (fresh server over the same journal: durable replay path).
//! Expected contract (ADR-0182):
//! - same key + same logical request -> 200 with the ORIGINAL logical response,
//!   no new journal event;
//! - same key + different action/body -> 422 `IdempotencyKeyMismatch`, no event;
//! - key known but its binding cannot be verified -> 409
//!   `IdempotencyKeyUnverifiable`, no event.

use axum::http::StatusCode;
use serde_json::Value;
use temper_runtime::scheduler::install_deterministic_context;

use super::harness::{
    Harness, INTRUDER, Mode, Reply, TENANT_A, TENANT_B, action_request, items, logical, status,
};
use super::legacy::{legacy_copy, save_legacy_snapshot};

const MARK: &str = "FACTORY_REGRESSION_ASSERTION";

const ORDER: &str = "order-v6";
const K1: &str = "idem-k1";
const K2: &str = "idem-k2";

/// First request bound to K1.
const ORIGINAL: &str =
    r#"{"ProductId":"p-1","Quantity":1,"Meta":{"gift":{"wrap":true,"note":"n"},"tags":["a","b"]}}"#;
/// Same logical request: nested keys reordered and a top-level server-derived
/// `Id` that dispatch strips. Array order is unchanged.
const EQUIVALENT: &str = r#"{"Meta":{"tags":["a","b"],"gift":{"note":"n","wrap":true}},"Quantity":1,"ProductId":"p-1","Id":"order-v6"}"#;
/// Intervening successful action bound to K2.
const SECOND: &str = r#"{"ProductId":"p-2","Quantity":1}"#;

const OTHER_QUANTITY: &str =
    r#"{"ProductId":"p-1","Quantity":2,"Meta":{"gift":{"wrap":true,"note":"n"},"tags":["a","b"]}}"#;

/// Transport-only metadata that must not influence the binding.
const TRANSPORT_HEADERS: &[(&str, &str)] = &[
    (
        "traceparent",
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    ),
    ("x-session-id", "retry-session"),
    ("x-intent", "retry after timeout"),
    ("x-temper-observe-meta-producer.attempt", "2"),
];

/// Boot, create the order, run K1 (original) and K2 (intervening). Returns the
/// harness, the original K1 response and the journal length after K2.
async fn setup(seed: u64) -> (Harness, Reply, usize) {
    let h = Harness::new(seed);
    h.create_order(TENANT_A, ORDER).await;
    let original = h
        .action(
            TENANT_A,
            ORDER,
            "Temper.Example.AddItem",
            Some(K1),
            ORIGINAL,
        )
        .await;
    assert_eq!(
        original.status,
        StatusCode::OK,
        "harness setup: original K1 AddItem failed: {}",
        original.body
    );
    let second = h
        .action(TENANT_A, ORDER, "Temper.Example.AddItem", Some(K2), SECOND)
        .await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "harness setup: intervening K2 AddItem failed: {}",
        second.body
    );
    let len = h.journal_len(TENANT_A, ORDER);
    assert!(
        len >= 3,
        "harness setup: expected Created + 2 AddItem events, journal has {len}"
    );
    (h, original, len)
}

/// Assert a mismatch/unverifiable rejection that appended nothing.
fn assert_rejected(
    reply: &Reply,
    expected_status: StatusCode,
    expected_code: &str,
    journal_before: usize,
    journal_after: usize,
    scenario: &str,
) {
    assert!(
        reply.status == expected_status && reply.error_code() == Some(expected_code),
        "{MARK}: {scenario}: expected {expected_status} {expected_code}, got {} with body {}",
        reply.status,
        reply.body
    );
    assert_eq!(
        journal_after,
        journal_before,
        "{MARK}: {scenario}: rejected request appended {} journal event(s)",
        journal_after.saturating_sub(journal_before)
    );
}

use super::harness;

mod concurrency;
mod history;
mod requests;
