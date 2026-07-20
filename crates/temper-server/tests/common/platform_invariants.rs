//! Platform invariant checkers (P1–P17) for deterministic simulation testing.
//!
//! Each invariant is a standalone function that inspects the harness state
//! and returns `Ok(())` on success or `Err(message)` on violation.
//! All invariants run against PRODUCTION data structures — no test-only
//! reimplementations.
#![allow(dead_code)]
#![allow(clippy::await_holding_lock)]

use std::collections::BTreeSet;

use temper_jit::table::TransitionTable;
use temper_runtime::tenant::{TenantId, parse_persistence_id_parts};
use temper_server::platform_store::PlatformStore;
use temper_server::storage::BoxedEventStore;

use super::platform_harness::SimPlatformHarness;

fn event_store(harness: &SimPlatformHarness) -> Option<BoxedEventStore> {
    harness
        .platform_state
        .server
        .storage_stack
        .as_ref()
        .map(|stack| stack.events.clone())
}

mod persistence;
mod registry;
mod replay;

pub use persistence::*;
pub use registry::*;
pub use replay::*;

// ── Composite checks ────────────────────────────────────────────────────

/// Check invariants that must hold even mid-operation under fault injection.
///
/// P1/P2 (registry-store consistency) may be transiently violated when
/// `delete_spec` cleanup fails during a faulty `install_os_app`. These
/// orphans are reconciled on the next restart by
/// `restore_registry_from_platform_store`. So mid-operation, we only check
/// invariants that cannot be transiently violated by cleanup failures.
pub async fn assert_mid_operation_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p8_state_store_sequence(harness).await?;
    assert_p9_rollback_completeness(harness).await?;
    assert_p13_sequence_monotonicity(harness).await?;
    Ok(())
}

/// Check all boot-cycle invariants (P1, P2, P6, P7, P11, P17).
///
/// These invariants should hold after every restart: the in-memory state
/// is consistent with the durable stores.
pub async fn assert_boot_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p1_registry_store_consistency(harness).await?;
    assert_p2_store_registry_consistency(harness).await?;
    assert_p6_cedar_spec_coherence(harness).await?;
    assert_p7_cedar_persistence(harness).await?;
    assert_p11_installed_apps_persistence(harness).await?;
    assert_p17_spec_roundtrip_equivalence(harness).await?;
    Ok(())
}

/// Check all data-plane invariants (P3, P4, P5, P8, P9, P10, P13, P14, P15, P16).
///
/// These invariants should hold after dispatching actions.
pub async fn assert_data_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p3_index_store_agreement(harness).await?;
    assert_p4_store_index_completeness(harness).await?;
    assert_p5_tombstone_finality(harness).await?;
    assert_p8_state_store_sequence(harness).await?;
    assert_p9_rollback_completeness(harness).await?;
    assert_p10_field_replay_fidelity(harness).await?;
    assert_p13_sequence_monotonicity(harness).await?;
    assert_p14_tenant_isolation(harness).await?;
    assert_p15_initial_state_correctness(harness).await?;
    assert_p16_event_replay_fidelity(harness).await?;
    Ok(())
}
