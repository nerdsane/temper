//! Platform invariant checkers (P1–P17) for deterministic simulation testing.
//!
//! Each function inspects the harness and returns `Ok(())` or `Err(message)`.
#![allow(dead_code)]
#![allow(clippy::await_holding_lock)]

use temper_server::storage::BoxedEventStore;

use crate::harness::SimPlatformHarness;

mod recovery;
mod store;

pub use recovery::*;
pub use store::*;

pub(crate) fn event_store(harness: &SimPlatformHarness) -> Option<BoxedEventStore> {
    harness
        .platform_state
        .server
        .storage_stack
        .as_ref()
        .map(|stack| stack.events.clone())
}

/// Check invariants that must hold even mid-operation under fault injection.
pub async fn assert_mid_operation_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p8_state_store_sequence(harness).await?;
    assert_p9_rollback_completeness(harness).await?;
    assert_p13_sequence_monotonicity(harness).await?;
    Ok(())
}

/// Check all boot-cycle invariants (P1, P2, P6, P7, P11, P17).
pub async fn assert_boot_invariants(harness: &SimPlatformHarness) -> Result<(), String> {
    assert_p1_registry_store_consistency(harness).await?;
    assert_p2_store_registry_consistency(harness).await?;
    assert_p6_cedar_spec_coherence(harness).await?;
    assert_p7_cedar_persistence(harness).await?;
    assert_p11_installed_apps_persistence(harness).await?;
    assert_p17_spec_roundtrip_equivalence(harness).await?;
    Ok(())
}

/// Check all data-plane invariants after dispatch.
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
