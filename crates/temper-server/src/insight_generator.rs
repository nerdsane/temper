//! Trajectory → InsightRecord pipeline.
//!
//! Aggregates trajectory log entries by (entity_type, action), computes
//! success rates and volumes, then generates `InsightRecord`s using the
//! classification and priority scoring from `temper-evolution`.

use std::collections::BTreeMap;

use temper_evolution::insight::{classify_insight, compute_priority_score};
use temper_evolution::records::{
    FeatureRequestDisposition, FeatureRequestRecord, InsightRecord, InsightSignal,
    PlatformGapCategory, RecordHeader, RecordType,
};

use crate::state::trajectory::TrajectorySource;

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
pub(crate) fn generate_insights(entries: &[crate::state::TrajectoryEntry]) -> Vec<InsightRecord> {
    if entries.is_empty() {
        return Vec::new();
    }

    // Phase 1: Aggregate by (entity_type, action).
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
        if entry.authz_denied == Some(true) {
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

    // Phase 2: Cross-reference — find entity types with SubmitSpec events.
    let submitted_types: std::collections::BTreeSet<String> = signals
        .values()
        .filter(|s| s.has_submit_spec)
        .map(|s| s.entity_type.clone())
        .collect();

    // Phase 3: Generate insights.
    let mut insights = Vec::new();

    for signal in signals.values() {
        // Skip very low-volume signals.
        if signal.total < 2 {
            continue;
        }

        let success_rate = if signal.total > 0 {
            signal.successes as f64 / signal.total as f64
        } else {
            0.0
        };

        // Special handling for EntitySetNotFound patterns.
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
                trend: "stable".to_string(),
                growth_rate: None,
            };

            let priority = if resolved {
                compute_priority_score(&insight_signal) * 0.5
            } else {
                compute_priority_score(&insight_signal).max(0.5)
            };
            let category = classify_insight(&insight_signal);

            let header = RecordHeader::new(RecordType::Insight, "insight-generator");
            insights.push(InsightRecord {
                header,
                category,
                signal: insight_signal,
                recommendation,
                priority_score: priority,
            });
            continue;
        }

        // Special handling for authz denial patterns.
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
                trend: "stable".to_string(),
                growth_rate: None,
            };

            let priority = compute_priority_score(&insight_signal);
            let category = classify_insight(&insight_signal);

            let header = RecordHeader::new(RecordType::Insight, "insight-generator");
            insights.push(InsightRecord {
                header,
                category,
                signal: insight_signal,
                recommendation,
                priority_score: priority,
            });
            continue;
        }

        // General pattern detection.
        let insight_signal = InsightSignal {
            intent: format!("{}.{}", signal.entity_type, signal.action),
            volume: signal.total,
            success_rate,
            trend: "stable".to_string(),
            growth_rate: None,
        };

        let priority = compute_priority_score(&insight_signal);
        // Only emit insights for meaningful signals.
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

        let header = RecordHeader::new(RecordType::Insight, "insight-generator");
        insights.push(InsightRecord {
            header,
            category,
            signal: insight_signal,
            recommendation,
            priority_score: priority,
        });
    }

    // Sort by priority (highest first).
    insights.sort_by(|a, b| {
        b.priority_score
            .partial_cmp(&a.priority_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    insights
}

/// Generate unmet intent summaries from trajectory data.
///
/// Groups failed trajectories by error pattern and cross-references with
/// SubmitSpec events to determine open vs resolved status.
pub(crate) fn generate_unmet_intents(
    entries: &[crate::state::TrajectoryEntry],
) -> Vec<UnmetIntent> {
    // Track entity types that have had specs submitted.
    let mut submitted_specs: BTreeMap<String, String> = BTreeMap::new();
    // Track failed patterns by (entity_type, error_pattern).
    let mut failures: BTreeMap<(String, String), UnmetIntentAccum> = BTreeMap::new();

    for entry in entries {
        if entry.action == "SubmitSpec" && entry.success {
            submitted_specs.insert(entry.entity_type.clone(), entry.timestamp.clone());
            continue;
        }

        if !entry.success {
            let error_pattern = categorize_error(entry.error.as_deref());
            let key = (entry.entity_type.clone(), error_pattern.clone());
            let accum = failures.entry(key).or_insert_with(|| UnmetIntentAccum {
                entity_type: entry.entity_type.clone(),
                action: entry.action.clone(),
                error_pattern,
                count: 0,
                first_seen: entry.timestamp.clone(),
                last_seen: entry.timestamp.clone(),
            });
            accum.count += 1;
            accum.last_seen = entry.timestamp.clone();
        }
    }

    failures
        .into_values()
        .map(|accum| {
            let resolved = submitted_specs.contains_key(&accum.entity_type);
            let resolved_by = submitted_specs.get(&accum.entity_type).cloned();
            let recommendation = if resolved {
                format!("Spec for '{}' has been submitted.", accum.entity_type)
            } else {
                match accum.error_pattern.as_str() {
                    "EntitySetNotFound" => {
                        format!("Consider creating '{}' entity type.", accum.entity_type,)
                    }
                    "AuthzDenied" => format!(
                        "Consider adding Cedar permit policy for actions on '{}'.",
                        accum.entity_type,
                    ),
                    _ => format!(
                        "Investigate failures for '{}' (pattern: {}).",
                        accum.entity_type, accum.error_pattern,
                    ),
                }
            };

            UnmetIntent {
                entity_type: accum.entity_type,
                action: accum.action,
                error_pattern: accum.error_pattern,
                failure_count: accum.count,
                first_seen: accum.first_seen,
                last_seen: accum.last_seen,
                status: if resolved {
                    "resolved".to_string()
                } else {
                    "open".to_string()
                },
                resolved_by,
                recommendation,
            }
        })
        .collect()
}

/// Minimum number of platform-source trajectory failures before generating a FR-Record.
const FEATURE_REQUEST_THRESHOLD: u64 = 3;

/// Stable dedup key for feature-request descriptions.
pub(crate) fn feature_request_dedup_key(description: &str) -> String {
    if let Some(rest) = description.strip_prefix("Agents tried '")
        && let Some((action, _)) = rest.split_once('\'')
    {
        return format!("action:{action}");
    }
    description
        .split(" — ")
        .next()
        .unwrap_or(description)
        .to_string()
}

/// Generate feature request records from platform-source trajectories.
///
/// Filters trajectory entries with `source == Some(Platform)`, groups by
/// `(action, error_pattern)`, and creates `FeatureRequestRecord`s for groups
/// that exceed the frequency threshold.
pub(crate) fn generate_feature_requests(
    entries: &[crate::state::TrajectoryEntry],
) -> Vec<FeatureRequestRecord> {
    if entries.is_empty() {
        return Vec::new();
    }

    // Group platform-source failures by (action, error_pattern).
    let mut groups: BTreeMap<(String, String), PlatformGapAccum> = BTreeMap::new();

    for entry in entries {
        // Only consider platform-source trajectories.
        if entry.source != Some(TrajectorySource::Platform) {
            continue;
        }
        if entry.success {
            continue;
        }

        let error_pattern = categorize_error(entry.error.as_deref());
        let key = (entry.action.clone(), error_pattern.clone());
        let accum = groups.entry(key).or_insert_with(|| PlatformGapAccum {
            action: entry.action.clone(),
            error_pattern,
            description: entry.error.clone().unwrap_or_default(),
            count: 0,
            timestamps: Vec::new(),
        });
        accum.count += 1;
        accum.timestamps.push(entry.timestamp.clone());
    }

    let mut feature_requests = Vec::new();

    for accum in groups.into_values() {
        if accum.count < FEATURE_REQUEST_THRESHOLD {
            continue;
        }

        let category = match accum.error_pattern.as_str() {
            "EntitySetNotFound" => PlatformGapCategory::MissingCapability,
            "AuthzDenied" => PlatformGapCategory::GovernanceBlocked,
            "ActionNotFound" => PlatformGapCategory::MissingMethod,
            _ => PlatformGapCategory::MissingCapability,
        };

        let description = format!(
            "Agents tried '{}' {} times — {}",
            accum.action, accum.count, accum.description,
        );

        let header = RecordHeader::new(RecordType::FeatureRequest, "insight-generator");
        feature_requests.push(FeatureRequestRecord {
            header,
            category,
            description,
            frequency: accum.count,
            trajectory_refs: accum.timestamps,
            disposition: FeatureRequestDisposition::Open,
            developer_notes: None,
        });
    }

    // Sort by frequency (highest first).
    feature_requests.sort_by_key(|b| std::cmp::Reverse(b.frequency));
    feature_requests
}

/// Accumulator for platform gap grouping.
struct PlatformGapAccum {
    action: String,
    error_pattern: String,
    description: String,
    count: u64,
    timestamps: Vec<String>,
}

/// Categorize an error string into a pattern name.
fn categorize_error(error: Option<&str>) -> String {
    match error {
        Some(e) if e.contains("EntitySetNotFound") || e.contains("entity set not found") => {
            "EntitySetNotFound".to_string()
        }
        Some(e) if e.contains("Authorization denied") || e.contains("authorization denied") => {
            "AuthzDenied".to_string()
        }
        Some(e) if e.contains("ActionNotFound") || e.contains("unknown action") => {
            "ActionNotFound".to_string()
        }
        Some(e) if e.contains("guard") => "GuardRejected".to_string(),
        Some(_) => "Other".to_string(),
        None => "Unknown".to_string(),
    }
}

struct UnmetIntentAccum {
    entity_type: String,
    action: String,
    error_pattern: String,
    count: u64,
    first_seen: String,
    last_seen: String,
}

/// A grouped unmet intent from trajectory data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UnmetIntent {
    /// Entity type involved.
    pub entity_type: String,
    /// Representative action.
    pub action: String,
    /// Error pattern category.
    pub error_pattern: String,
    /// Number of failures.
    pub failure_count: u64,
    /// First occurrence timestamp.
    pub first_seen: String,
    /// Most recent occurrence timestamp.
    pub last_seen: String,
    /// "open" or "resolved".
    pub status: String,
    /// What resolved it (e.g. spec submission timestamp).
    pub resolved_by: Option<String>,
    /// Recommendation text.
    pub recommendation: String,
}
