use std::collections::BTreeMap;

use tracing::instrument;

use temper_evolution::records::{
    FeatureRequestDisposition, FeatureRequestRecord, PlatformGapCategory, RecordHeader, RecordType,
};

use crate::state::trajectory::TrajectorySource;

/// A grouped unmet intent from trajectory data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UnmetIntent {
    pub entity_type: String,
    pub action: String,
    pub error_pattern: String,
    pub failure_count: u64,
    pub first_seen: String,
    pub last_seen: String,
    pub status: String,
    pub resolved_by: Option<String>,
    pub recommendation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_intent: Option<String>,
}

struct UnmetIntentAccum {
    entity_type: String,
    action: String,
    error_pattern: String,
    count: u64,
    first_seen: String,
    last_seen: String,
    sample_body: Option<serde_json::Value>,
    sample_intent: Option<String>,
}

/// Generate unmet intent summaries from trajectory data.
///
/// This path is superseded in production by [`generate_unmet_intents_from_aggregated`]
/// (SQL GROUP BY). Retained for unit tests that exercise the aggregation logic
/// against in-memory trajectory slices.
#[cfg_attr(not(test), allow(dead_code))]
#[instrument(skip_all, fields(entry_count = entries.len(), intent_count = tracing::field::Empty))]
pub(crate) fn generate_unmet_intents(
    entries: &[crate::state::TrajectoryEntry],
) -> Vec<UnmetIntent> {
    let mut submitted_specs: BTreeMap<String, String> = BTreeMap::new();
    let mut failures: BTreeMap<(String, String), UnmetIntentAccum> = BTreeMap::new();

    for entry in entries {
        if entry.action == "SubmitSpec" && entry.success {
            submitted_specs.insert(entry.entity_type.clone(), entry.timestamp.clone());
            continue;
        }

        if !entry.success {
            let error_pattern = super::categorize_error(entry.error.as_deref());
            if error_pattern == "AuthzDenied" || entry.authz_denied == Some(true) {
                continue;
            }

            let key = (entry.entity_type.clone(), error_pattern.clone());
            let accum = failures.entry(key).or_insert_with(|| UnmetIntentAccum {
                entity_type: entry.entity_type.clone(),
                action: entry.action.clone(),
                error_pattern,
                count: 0,
                first_seen: entry.timestamp.clone(),
                last_seen: entry.timestamp.clone(),
                sample_body: None,
                sample_intent: None,
            });
            accum.count += 1;
            accum.last_seen = entry.timestamp.clone();
            if entry.request_body.is_some() {
                accum.sample_body = entry.request_body.clone();
            }
            if entry.intent.is_some() {
                accum.sample_intent = entry.intent.clone();
            }
        }
    }

    let intents: Vec<UnmetIntent> = failures
        .into_values()
        .map(|accum| {
            let resolved = submitted_specs.contains_key(&accum.entity_type);
            let resolved_by = submitted_specs.get(&accum.entity_type).cloned();
            let recommendation = if resolved {
                format!("Spec for '{}' has been submitted.", accum.entity_type)
            } else {
                match accum.error_pattern.as_str() {
                    "EntitySetNotFound" => {
                        format!("Consider creating '{}' entity type.", accum.entity_type)
                    }
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
                sample_body: accum.sample_body,
                sample_intent: accum.sample_intent,
            }
        })
        .collect();
    let open_count = intents
        .iter()
        .filter(|intent| intent.status == "open")
        .count();
    let resolved_count = intents
        .iter()
        .filter(|intent| intent.status == "resolved")
        .count();
    tracing::Span::current().record("intent_count", intents.len());
    if open_count > 0 {
        tracing::warn!(
            entry_count = entries.len(),
            intents_count = intents.len(),
            open_count,
            resolved_count,
            "unmet_intent"
        );
    } else {
        tracing::info!(
            entry_count = entries.len(),
            intents_count = intents.len(),
            open_count,
            resolved_count,
            "unmet_intent"
        );
    }
    intents
}

const FEATURE_REQUEST_THRESHOLD: u64 = 3;

pub(crate) fn generate_feature_requests(
    entries: &[crate::state::TrajectoryEntry],
) -> Vec<FeatureRequestRecord> {
    if entries.is_empty() {
        return Vec::new();
    }

    let mut groups: BTreeMap<(String, String), PlatformGapAccum> = BTreeMap::new();

    for entry in entries {
        if entry.source != Some(TrajectorySource::Platform) || entry.success {
            continue;
        }

        let error_pattern = super::categorize_error(entry.error.as_deref());
        if error_pattern == "AuthzDenied" || entry.authz_denied == Some(true) {
            continue;
        }

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
            "ActionNotFound" => PlatformGapCategory::MissingMethod,
            _ => PlatformGapCategory::MissingCapability,
        };

        feature_requests.push(FeatureRequestRecord {
            header: RecordHeader::new(RecordType::FeatureRequest, "insight-generator"),
            category,
            description: format!(
                "Agents tried '{}' {} times — {}",
                accum.action, accum.count, accum.description,
            ),
            frequency: accum.count,
            trajectory_refs: accum.timestamps,
            disposition: FeatureRequestDisposition::Open,
            developer_notes: None,
        });
    }

    feature_requests.sort_by_key(|record| std::cmp::Reverse(record.frequency));
    feature_requests
}

struct PlatformGapAccum {
    action: String,
    error_pattern: String,
    description: String,
    count: u64,
    timestamps: Vec<String>,
}

#[instrument(skip_all, fields(failure_row_count = failures.len(), intent_count = tracing::field::Empty))]
pub(crate) fn generate_unmet_intents_from_aggregated(
    failures: &[temper_store_turso::UnmetIntentAggRow],
    submitted_specs: &std::collections::BTreeMap<String, String>,
) -> Vec<UnmetIntent> {
    let mut groups: BTreeMap<(String, String), UnmetIntentAccum> = BTreeMap::new();

    for row in failures {
        let error_pattern = super::categorize_error(row.error.as_deref());
        if error_pattern == "AuthzDenied" {
            continue;
        }

        let key = (row.entity_type.clone(), error_pattern.clone());
        let accum = groups.entry(key).or_insert_with(|| UnmetIntentAccum {
            entity_type: row.entity_type.clone(),
            action: row.action.clone(),
            error_pattern,
            count: 0,
            first_seen: row.first_seen.clone(),
            last_seen: row.last_seen.clone(),
            sample_body: None,
            sample_intent: None,
        });
        accum.count += row.count;
        if row.first_seen < accum.first_seen {
            accum.first_seen = row.first_seen.clone();
        }
        if row.last_seen > accum.last_seen {
            accum.last_seen = row.last_seen.clone();
        }
    }

    let intents: Vec<UnmetIntent> = groups
        .into_values()
        .map(|accum| {
            let resolved = submitted_specs.contains_key(&accum.entity_type);
            let resolved_by = submitted_specs.get(&accum.entity_type).cloned();
            let recommendation = if resolved {
                format!("Spec for '{}' has been submitted.", accum.entity_type)
            } else {
                match accum.error_pattern.as_str() {
                    "EntitySetNotFound" => {
                        format!("Consider creating '{}' entity type.", accum.entity_type)
                    }
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
                sample_body: accum.sample_body,
                sample_intent: accum.sample_intent,
            }
        })
        .collect();
    tracing::Span::current().record("intent_count", intents.len());
    intents
}
