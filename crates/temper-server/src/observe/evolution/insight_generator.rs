//! Trajectory -> InsightRecord pipeline.
//! Aggregates trajectory log entries by `(entity_type, action)` and generates
//! `InsightRecord`s using `temper-evolution` classification and priority scoring.

use std::collections::BTreeMap;

use tracing::instrument;

use temper_evolution::insight::{classify_insight, compute_priority_score};
use temper_evolution::records::{InsightRecord, InsightSignal, RecordHeader, RecordType};

mod gap_analysis;
mod intent_evidence;
#[cfg(test)]
#[path = "insight_generator/mod_test.rs"]
mod tests;

pub(crate) use gap_analysis::{generate_feature_requests, generate_unmet_intents_from_aggregated};
pub(crate) use intent_evidence::generate_intent_evidence;

/// Build an `InsightRecord` from a signal, recommendation, and optional priority override.
fn build_insight(
    signal: InsightSignal,
    recommendation: String,
    priority_override: Option<f64>,
) -> InsightRecord {
    let priority = priority_override.unwrap_or_else(|| compute_priority_score(&signal));
    let category = classify_insight(&signal);
    InsightRecord {
        header: RecordHeader::new(RecordType::Insight, "insight-generator"),
        category,
        signal,
        recommendation,
        priority_score: priority,
    }
}

/// Aggregated trajectory signal for a (entity_type, action) pair.
struct TrajectorySignal {
    entity_type: String,
    action: String,
    total: u64,
    successes: u64,
    failures: u64,
    authz_denials: u64,
    has_entity_not_found: bool,
    has_submit_spec: bool,
}

/// Generate insights from the current trajectory log.
///
/// Reads the trajectory log, aggregates by (entity_type, action), computes
/// classification and priority, and returns `InsightRecord`s. Also correlates
/// EntitySetNotFound 404 trajectories with SubmitSpec events to detect
/// resolved vs open unmet intents.
#[instrument(skip_all, fields(entry_count = entries.len(), insight_count = tracing::field::Empty))]
pub(crate) fn generate_insights(entries: &[crate::state::TrajectoryEntry]) -> Vec<InsightRecord> {
    if entries.is_empty() {
        tracing::debug!("evolution.insight");
        return Vec::new();
    }
    tracing::info!(entry_count = entries.len(), "evolution.insight");

    let mut signals: BTreeMap<(String, String), TrajectorySignal> = BTreeMap::new();

    for entry in entries {
        let key = (entry.entity_type.clone(), entry.action.clone());
        let signal = signals.entry(key).or_insert_with(|| TrajectorySignal {
            entity_type: entry.entity_type.clone(),
            action: entry.action.clone(),
            total: 0,
            successes: 0,
            failures: 0,
            authz_denials: 0,
            has_entity_not_found: false,
            has_submit_spec: false,
        });

        signal.total += 1;
        if entry.success {
            signal.successes += 1;
        } else {
            signal.failures += 1;
        }
        if entry.authz_denied == Some(true)
            || categorize_error(entry.error.as_deref()) == "AuthzDenied"
        {
            signal.authz_denials += 1;
        }
        if let Some(ref err) = entry.error
            && (err.contains("EntitySetNotFound") || err.contains("entity set not found"))
        {
            signal.has_entity_not_found = true;
        }
        if entry.action == "SubmitSpec" {
            signal.has_submit_spec = true;
        }
    }

    let submitted_types: std::collections::BTreeSet<String> = signals
        .values()
        .filter(|signal| signal.has_submit_spec)
        .map(|signal| signal.entity_type.clone())
        .collect();
    tracing::info!(
        signal_count = signals.len(),
        submitted_type_count = submitted_types.len(),
        "evolution.insight"
    );

    let mut insights = Vec::new();

    for signal in signals.values() {
        if signal.total < 2 {
            continue;
        }

        let success_rate = if signal.total > 0 {
            signal.successes as f64 / signal.total as f64
        } else {
            0.0
        };

        if signal.has_entity_not_found {
            let resolved = submitted_types.contains(&signal.entity_type);
            let intent_str = format!(
                "Entity type '{}' — {}",
                signal.entity_type,
                if resolved {
                    "spec submitted (resolved)"
                } else {
                    "entity type not found (open unmet intent)"
                }
            );
            let recommendation = if resolved {
                format!(
                    "Spec for '{}' has been submitted. Monitor for approval.",
                    signal.entity_type
                )
            } else {
                format!(
                    "Consider creating '{}' entity type. {} attempts, {:.0}% failure rate.",
                    signal.entity_type,
                    signal.total,
                    (1.0 - success_rate) * 100.0,
                )
            };

            let insight_signal = InsightSignal {
                intent: intent_str,
                volume: signal.total,
                success_rate,
                trend: temper_evolution::Trend::Stable,
                growth_rate: None,
            };
            let priority = if resolved {
                compute_priority_score(&insight_signal) * 0.5
            } else {
                compute_priority_score(&insight_signal).max(0.5)
            };
            let severity = if priority >= 0.7 {
                "high"
            } else if priority >= 0.4 {
                "medium"
            } else {
                "low"
            };
            let category = classify_insight(&insight_signal);

            if resolved {
                tracing::info!(
                    entity_type = %signal.entity_type,
                    action = %signal.action,
                    total = signal.total,
                    success_rate,
                    resolved,
                    priority_score = priority,
                    category = ?category,
                    severity,
                    recommendation = %recommendation,
                    "evolution.pattern"
                );
            } else {
                tracing::warn!(
                    entity_type = %signal.entity_type,
                    action = %signal.action,
                    total = signal.total,
                    success_rate,
                    resolved,
                    priority_score = priority,
                    category = ?category,
                    severity,
                    recommendation = %recommendation,
                    "evolution.pattern"
                );
            }

            insights.push(build_insight(
                insight_signal,
                recommendation,
                Some(priority),
            ));
            continue;
        }

        if signal.authz_denials > 0 && signal.authz_denials as f64 / signal.total as f64 > 0.3 {
            let intent_str = format!(
                "Action '{}' on '{}' denied {} times",
                signal.action, signal.entity_type, signal.authz_denials,
            );
            let recommendation = format!(
                "Consider adding Cedar permit policy for '{}' on '{}'.",
                signal.action, signal.entity_type,
            );

            let insight_signal = InsightSignal {
                intent: intent_str,
                volume: signal.total,
                success_rate,
                trend: temper_evolution::Trend::Stable,
                growth_rate: None,
            };

            let authz_priority = compute_priority_score(&insight_signal);
            let authz_category = classify_insight(&insight_signal);
            let authz_severity = if authz_priority >= 0.7 {
                "high"
            } else if authz_priority >= 0.4 {
                "medium"
            } else {
                "low"
            };
            tracing::warn!(
                entity_type = %signal.entity_type,
                action = %signal.action,
                total = signal.total,
                authz_denials = signal.authz_denials,
                success_rate,
                category = ?authz_category,
                severity = authz_severity,
                recommendation = %recommendation,
                "evolution.pattern"
            );
            insights.push(build_insight(insight_signal, recommendation, None));
            continue;
        }

        let insight_signal = InsightSignal {
            intent: format!("{}.{}", signal.entity_type, signal.action),
            volume: signal.total,
            success_rate,
            trend: temper_evolution::Trend::Stable,
            growth_rate: None,
        };

        let priority = compute_priority_score(&insight_signal);
        if priority < 0.1 {
            continue;
        }

        let category = classify_insight(&insight_signal);
        let recommendation = match category {
            temper_evolution::records::InsightCategory::UnmetIntent => {
                format!(
                    "Action '{}' on '{}' has {:.0}% failure rate ({} attempts). Possible missing feature.",
                    signal.action,
                    signal.entity_type,
                    (1.0 - success_rate) * 100.0,
                    signal.total,
                )
            }
            temper_evolution::records::InsightCategory::Friction => {
                format!(
                    "Action '{}' on '{}' has high volume ({} attempts). Consider simplifying.",
                    signal.action, signal.entity_type, signal.total,
                )
            }
            temper_evolution::records::InsightCategory::Workaround => {
                format!(
                    "Pattern detected on '{}.{}' — {} attempts with {:.0}% success. May be a workaround.",
                    signal.entity_type,
                    signal.action,
                    signal.total,
                    success_rate * 100.0,
                )
            }
            temper_evolution::records::InsightCategory::PlatformGap => {
                format!(
                    "Platform gap: '{}' on '{}' failed {} times. Consider adding this capability.",
                    signal.action, signal.entity_type, signal.total,
                )
            }
        };

        let severity = if priority >= 0.7 {
            "high"
        } else if priority >= 0.4 {
            "medium"
        } else {
            "low"
        };
        tracing::info!(
            entity_type = %signal.entity_type,
            action = %signal.action,
            total = signal.total,
            success_rate,
            priority_score = priority,
            category = ?category,
            severity,
            recommendation = %recommendation,
            "evolution.pattern"
        );

        insights.push(build_insight(
            insight_signal,
            recommendation,
            Some(priority),
        ));
    }

    insights.sort_by(|a, b| {
        b.priority_score
            .partial_cmp(&a.priority_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    tracing::Span::current().record("insight_count", insights.len());
    tracing::info!(insight_count = insights.len(), "evolution.insight");
    insights
}

pub(super) fn categorize_error(error: Option<&str>) -> String {
    match error {
        Some(err) if err.contains("EntitySetNotFound") || err.contains("entity set not found") => {
            "EntitySetNotFound".to_string()
        }
        Some(err)
            if err.contains("Authorization denied") || err.contains("authorization denied") =>
        {
            "AuthzDenied".to_string()
        }
        Some(err) if err.contains("ActionNotFound") || err.contains("unknown action") => {
            "ActionNotFound".to_string()
        }
        Some(err) if err.contains("guard") => "GuardRejected".to_string(),
        Some(_) => "Other".to_string(),
        None => "Unknown".to_string(),
    }
}
