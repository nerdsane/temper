//! Eventual invariant tracker and background convergence task.
//!
//! When a cross-entity invariant has `kind = "Eventual"` with a `window_ms`,
//! writes are allowed to proceed immediately. The invariant is recorded as
//! "pending" and re-checked periodically by a background task until it
//! converges or the recheck budget is exhausted.

use std::collections::BTreeMap;

use tracing::{Instrument, instrument};

use temper_observe::wide_event;
use temper_runtime::scheduler::sim_now;

/// Maximum number of pending invariants tracked simultaneously (TigerStyle budget).
const MAX_PENDING: usize = 10_000;
/// Maximum number of recheck attempts before giving up.
const MAX_RECHECK_ATTEMPTS: u8 = 5;

/// A pending eventual invariant awaiting convergence.
#[derive(Debug, Clone)]
pub struct PendingInvariant {
    /// Invariant name from the spec.
    pub name: String,
    /// Tenant ID.
    pub tenant: String,
    /// The entity type that triggered the invariant.
    pub entity_type: String,
    /// The entity ID that triggered the invariant.
    pub entity_id: String,
    /// When the invariant was first recorded.
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    /// Deadline by which the invariant must converge.
    pub deadline: chrono::DateTime<chrono::Utc>,
    /// Number of recheck attempts so far.
    pub check_count: u8,
}

/// Tracker for eventual invariants pending convergence.
#[derive(Debug, Clone, Default)]
pub struct EventualInvariantTracker {
    /// Pending invariants keyed by "{tenant}:{invariant_name}:{entity_type}:{entity_id}".
    pending: BTreeMap<String, PendingInvariant>,
}

impl EventualInvariantTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
        }
    }

    /// Record a new pending eventual invariant.
    ///
    /// Returns `false` if the budget is exhausted.
    pub fn record(
        &mut self,
        name: &str,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        window_ms: u64,
    ) -> bool {
        if self.pending.len() >= MAX_PENDING {
            return false;
        }
        let now = sim_now();
        let deadline = now + chrono::Duration::milliseconds(window_ms as i64);
        let key = format!("{tenant}:{name}:{entity_type}:{entity_id}");
        self.pending.insert(
            key,
            PendingInvariant {
                name: name.to_string(),
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                recorded_at: now,
                deadline,
                check_count: 0,
            },
        );
        true
    }

    /// Return all invariants that are due for recheck (deadline passed or check needed).
    pub fn due_for_recheck(&self) -> Vec<(String, PendingInvariant)> {
        let now = sim_now();
        self.pending
            .iter()
            .filter(|(_, inv)| now >= inv.deadline && inv.check_count < MAX_RECHECK_ATTEMPTS)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Resolve (remove) a pending invariant by key.
    pub fn resolve(&mut self, key: &str) -> Option<PendingInvariant> {
        self.pending.remove(key)
    }

    /// Increment the check count for a pending invariant.
    pub fn increment_check(&mut self, key: &str) {
        if let Some(inv) = self.pending.get_mut(key) {
            inv.check_count += 1;
        }
    }

    /// Return all invariants that have exhausted their recheck budget.
    pub fn exhausted(&self) -> Vec<(String, PendingInvariant)> {
        self.pending
            .iter()
            .filter(|(_, inv)| inv.check_count >= MAX_RECHECK_ATTEMPTS)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Number of pending invariants.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether there are no pending invariants.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Spawn a background task that periodically re-checks due eventual invariants.
///
/// The task polls every `interval` and re-evaluates invariants whose deadline
/// has passed. On convergence, the invariant is resolved; on budget exhaustion,
/// it is removed and an error is logged.
#[instrument(skip_all, fields(otel.name = "eventual.spawn_recheck"))]
pub fn spawn_eventual_recheck(
    state: crate::state::ServerState,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(// determinism-ok: background convergence task
        async move {
            let mut ticker = tokio::time::interval(interval); // determinism-ok: convergence polling
            loop {
                ticker.tick().await;

                let due_items = {
                    let tracker = state.eventual_tracker.read().unwrap();
                    tracker.due_for_recheck()
                };

                for (key, inv) in due_items {
                    let recheck_span = tracing::info_span!(
                        "eventual.recheck_item",
                        invariant = %inv.name,
                        tenant = %inv.tenant,
                        entity_type = %inv.entity_type,
                        entity_id = %inv.entity_id,
                        check_count = inv.check_count,
                    );
                    let tenant = temper_runtime::tenant::TenantId::new(&inv.tenant);

                    // Re-read the related entity and check if the invariant now holds
                    let converged = check_invariant_convergence(&state, &tenant, &inv)
                        .instrument(recheck_span)
                        .await;

                    if converged {
                        if let Ok(mut tracker) = state.eventual_tracker.write() {
                            tracker.resolve(&key);
                        }
                        state.metrics.record_cross_invariant_check(
                            &inv.tenant,
                            &inv.entity_type,
                            "eventual_converged",
                        );
                        // Observability: emit WideEvent for invariant convergence
                        let wide = wide_event::from_invariant_check(wide_event::InvariantCheckInput {
                            invariant_name: &inv.name,
                            entity_type: &inv.entity_type,
                            entity_id: &inv.entity_id,
                            tenant: &inv.tenant,
                            check_count: inv.check_count as u32,
                            outcome: "converged",
                            duration_ns: 0,
                        });
                        wide_event::emit_span(&wide);
                        wide_event::emit_metrics(&wide);
                        tracing::info!(
                            invariant = %inv.name,
                            tenant = %inv.tenant,
                            entity_type = %inv.entity_type,
                            entity_id = %inv.entity_id,
                            "eventual invariant converged"
                        );
                    } else {
                        if let Ok(mut tracker) = state.eventual_tracker.write() {
                            tracker.increment_check(&key);
                        }
                    }
                }

                // Clean up exhausted invariants
                let exhausted = {
                    let tracker = state.eventual_tracker.read().unwrap();
                    tracker.exhausted()
                };
                for (key, inv) in exhausted {
                    if let Ok(mut tracker) = state.eventual_tracker.write() {
                        tracker.resolve(&key);
                    }
                    state.metrics.record_cross_invariant_violation(
                        &inv.tenant,
                        &inv.name,
                        "eventual_convergence_failed",
                    );
                    // Observability: emit WideEvent for invariant convergence failure
                    let wide = wide_event::from_invariant_check(wide_event::InvariantCheckInput {
                        invariant_name: &inv.name,
                        entity_type: &inv.entity_type,
                        entity_id: &inv.entity_id,
                        tenant: &inv.tenant,
                        check_count: inv.check_count as u32,
                        outcome: "failed",
                        duration_ns: 0,
                    });
                    wide_event::emit_span(&wide);
                    wide_event::emit_metrics(&wide);
                    tracing::error!(
                        invariant = %inv.name,
                        tenant = %inv.tenant,
                        entity_type = %inv.entity_type,
                        entity_id = %inv.entity_id,
                        checks = inv.check_count,
                        "eventual invariant failed to converge within budget"
                    );
                }
            }
        }
        .instrument(tracing::info_span!("eventual.recheck")),
    )
}

/// Check if a pending eventual invariant has converged.
async fn check_invariant_convergence(
    state: &crate::state::ServerState,
    tenant: &temper_runtime::tenant::TenantId,
    inv: &PendingInvariant,
) -> bool {
    // Re-read the invariant assertion from the registry
    let assertion_info = {
        let registry = state.registry.read().unwrap();
        let tc = registry.get_tenant(tenant);
        tc.and_then(|tc| {
            tc.cross_invariants.as_ref().and_then(|ci| {
                ci.invariants
                    .iter()
                    .find(|i| i.name == inv.name)
                    .map(|i| i.assertion.clone())
            })
        })
    };

    let Some(assertion_str) = assertion_info else {
        // Invariant no longer exists in spec — treat as converged
        return true;
    };

    let Some(assertion) =
        temper_spec::cross_invariant::parse_related_status_in_assert(&assertion_str)
    else {
        return true; // Invalid assertion — skip
    };

    // Read the entity that triggered the invariant
    let fields = match state
        .get_tenant_entity_state(tenant, &inv.entity_type, &inv.entity_id)
        .await
    {
        Ok(resp) => serde_json::to_value(&resp.state.fields).unwrap_or_default(),
        Err(_) => return false,
    };

    // Extract the target ID from the entity fields
    let Some(target_id) = fields
        .get(&assertion.source_field)
        .or_else(|| {
            fields
                .get("fields")
                .and_then(|f| f.get(&assertion.source_field))
        })
        .and_then(|v| v.as_str())
    else {
        return false;
    };

    // Read the target entity status
    let target_status = match state
        .get_tenant_entity_state(tenant, &assertion.target_entity, target_id)
        .await
    {
        Ok(resp) => resp.state.status,
        Err(_) => return false,
    };

    // Check if the assertion now holds
    assertion.statuses.iter().any(|s| s == &target_status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_record_and_resolve() {
        let mut tracker = EventualInvariantTracker::new();
        assert!(tracker.record("inv1", "t1", "Order", "o1", 5000));
        assert_eq!(tracker.len(), 1);

        let key = "t1:inv1:Order:o1";
        assert!(tracker.resolve(key).is_some());
        assert!(tracker.is_empty());
    }

    #[test]
    fn tracker_increment_and_exhaust() {
        let mut tracker = EventualInvariantTracker::new();
        tracker.record("inv1", "t1", "Order", "o1", 0); // deadline = now
        let key = "t1:inv1:Order:o1";

        for _ in 0..5 {
            tracker.increment_check(key);
        }

        let exhausted = tracker.exhausted();
        assert_eq!(exhausted.len(), 1);
        assert_eq!(exhausted[0].1.check_count, 5);
    }

    #[test]
    fn tracker_due_for_recheck() {
        let mut tracker = EventualInvariantTracker::new();
        // window_ms = 0 means deadline = now, so it's immediately due
        tracker.record("inv1", "t1", "Order", "o1", 0);

        let due = tracker.due_for_recheck();
        assert_eq!(due.len(), 1);
    }
}
