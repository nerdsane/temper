//! Sentinel anomaly detection for the Temper observability layer.
//!
//! The sentinel periodically (or on-demand) evaluates a set of rules against
//! the server's trajectory log and metrics, generating O-Records when
//! thresholds are crossed.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use temper_evolution::{
    ObservationClass, ObservationRecord, RecordHeader, RecordStatus, RecordType,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use crate::state::ServerState;

/// Check function type: given the server state, returns `Some(observed_value)` if triggered.
type SentinelCheckFn = Box<dyn Fn(&ServerState) -> Option<f64> + Send + Sync>;

/// A sentinel rule that can evaluate server state and detect anomalies.
pub struct SentinelRule {
    /// Human-readable rule name (e.g., "error_rate_spike").
    pub name: String,
    /// The observation source label (e.g., "sentinel:error_rate").
    pub source: String,
    /// Classification of observations produced by this rule.
    pub classification: ObservationClass,
    /// The threshold field name for the O-Record.
    pub threshold_field: String,
    /// The threshold value.
    pub threshold_value: f64,
    /// The check function: given the server state, returns `Some(observed_value)` if triggered.
    pub check: SentinelCheckFn,
}

/// Result of a sentinel check: the rule that triggered and the O-Record it produced.
pub struct SentinelAlert {
    /// The rule name that triggered.
    pub rule_name: String,
    /// The generated O-Record.
    pub record: ObservationRecord,
}

/// Build the default set of sentinel rules.
///
/// Rules included:
/// 1. **Error rate spike**: >10% failure rate across all transitions.
/// 2. **Guard rejection rate**: >20% rejection rate for any single action.
/// 3. **Low activity**: no transitions recorded at all (cold system).
pub fn default_rules() -> Vec<SentinelRule> {
    vec![
        SentinelRule {
            name: "error_rate_spike".to_string(),
            source: "sentinel:error_rate".to_string(),
            classification: ObservationClass::ErrorRate,
            threshold_field: "error_rate".to_string(),
            threshold_value: 0.10,
            check: Box::new(|state| {
                let total = state.metrics.transitions_total.load(Ordering::Relaxed);
                if total == 0 {
                    return None;
                }
                let errors = state.metrics.errors_total.load(Ordering::Relaxed);
                let error_rate = errors as f64 / total as f64;
                if error_rate > 0.10 {
                    Some(error_rate)
                } else {
                    None
                }
            }),
        },
        SentinelRule {
            name: "guard_rejection_rate".to_string(),
            source: "sentinel:guard_rejection".to_string(),
            classification: ObservationClass::StateMachine,
            threshold_field: "rejection_rate".to_string(),
            threshold_value: 0.20,
            check: Box::new(|state| {
                // Check per-action rejection rates from trajectory log.
                let log = match state.trajectory_log.read() {
                    Ok(l) => l,
                    Err(e) => e.into_inner(),
                };
                let entries = log.entries();
                if entries.is_empty() {
                    return None;
                }

                // Aggregate per-action stats.
                let mut per_action: BTreeMap<String, (u64, u64)> = BTreeMap::new();
                for entry in entries.iter() {
                    let counts = per_action.entry(entry.action.clone()).or_insert((0, 0));
                    counts.0 += 1; // total
                    if !entry.success {
                        counts.1 += 1; // failures
                    }
                }

                // Find the worst rejection rate.
                let mut worst_rate = 0.0_f64;
                for (total, failures) in per_action.values() {
                    if *total >= 5 {
                        // Need minimum sample size.
                        let rate = *failures as f64 / *total as f64;
                        if rate > worst_rate {
                            worst_rate = rate;
                        }
                    }
                }

                if worst_rate > 0.20 {
                    Some(worst_rate)
                } else {
                    None
                }
            }),
        },
        SentinelRule {
            name: "no_activity".to_string(),
            source: "sentinel:activity".to_string(),
            classification: ObservationClass::ResourceUsage,
            threshold_field: "transitions_total".to_string(),
            threshold_value: 1.0,
            check: Box::new(|state| {
                let total = state.metrics.transitions_total.load(Ordering::Relaxed);
                let active = {
                    let reg = match state.actor_registry.read() {
                        Ok(r) => r,
                        Err(e) => e.into_inner(),
                    };
                    reg.len() as u64
                };
                // If we have active entities but zero transitions, flag it.
                if active > 0 && total == 0 {
                    Some(0.0)
                } else {
                    None
                }
            }),
        },
    ]
}

/// Evaluate all sentinel rules against the current server state.
///
/// Returns a list of alerts for rules whose thresholds were crossed.
/// Each alert includes a fully-formed O-Record ready for insertion into the RecordStore.
pub fn check_rules(rules: &[SentinelRule], state: &ServerState) -> Vec<SentinelAlert> {
    let now = sim_now();
    let mut alerts = Vec::new();

    for rule in rules {
        if let Some(observed_value) = (rule.check)(state) {
            let id_suffix = &sim_uuid().to_string()[..8];
            let year = now.format("%Y");
            let record_id = format!("O-{year}-{id_suffix}");

            let record = ObservationRecord {
                header: RecordHeader {
                    id: record_id,
                    record_type: RecordType::Observation,
                    timestamp: now,
                    created_by: rule.source.clone(),
                    derived_from: None,
                    status: RecordStatus::Open,
                },
                source: rule.source.clone(),
                classification: rule.classification.clone(),
                evidence_query: format!(
                    "sentinel rule '{}': {} = {observed_value:.4} > threshold {:.4}",
                    rule.name, rule.threshold_field, rule.threshold_value,
                ),
                threshold_field: Some(rule.threshold_field.clone()),
                threshold_value: Some(rule.threshold_value),
                observed_value: Some(observed_value),
                context: serde_json::json!({
                    "rule_name": rule.name,
                }),
            };

            alerts.push(SentinelAlert {
                rule_name: rule.name.clone(),
                record,
            });
        }
    }

    alerts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::SpecRegistry;
    use crate::state::ServerState;
    use temper_runtime::tenant::TenantId;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;

    const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    fn test_state_with_registry() -> ServerState {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let mut registry = SpecRegistry::new();
        registry.register_tenant("default", csdl, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);
        let system = ActorSystem::new("test-sentinel");
        ServerState::from_registry(system, registry)
    }

    #[test]
    fn test_default_rules_count() {
        let rules = default_rules();
        assert_eq!(rules.len(), 3);
    }

    #[tokio::test]
    async fn test_error_rate_spike_triggers() {
        let state = test_state_with_registry();

        // Generate enough failures to trigger the 10% threshold.
        // SubmitOrder on a fresh entity fails (guard: no items).
        for i in 0..6 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("sentinel-err-{i}"),
                    "SubmitOrder",
                    serde_json::json!({}),
                )
                .await;
        }
        // Add a few successes.
        for i in 0..4 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("sentinel-ok-{i}"),
                    "AddItem",
                    serde_json::json!({"ProductId": "p1"}),
                )
                .await;
        }

        let rules = default_rules();
        let alerts = check_rules(&rules, &state);

        // Error rate is 6/10 = 60% which should trigger the error_rate_spike rule.
        let error_alert = alerts.iter().find(|a| a.rule_name == "error_rate_spike");
        assert!(error_alert.is_some(), "error_rate_spike should trigger");
        let record = &error_alert.expect("checked above").record;
        assert!(record.header.id.starts_with("O-"));
        assert!(record.observed_value.expect("should have value") > 0.10);
    }

    #[test]
    fn test_no_alerts_on_clean_state() {
        let state = test_state_with_registry();
        let rules = default_rules();
        let alerts = check_rules(&rules, &state);
        assert!(alerts.is_empty(), "no alerts on clean state with no activity and no actors");
    }
}
