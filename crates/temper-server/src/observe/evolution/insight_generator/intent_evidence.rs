use std::collections::{BTreeMap, BTreeSet};

use tracing::instrument;

/// Richer unmet-intent evidence derived from recent trajectories.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IntentEvidenceSummary {
    pub intent_candidates: Vec<IntentCandidate>,
    pub workaround_patterns: Vec<WorkaroundPattern>,
    pub abandonment_patterns: Vec<AbandonmentPattern>,
    pub trajectory_samples: Vec<TrajectorySample>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IntentCandidate {
    pub intent_key: String,
    pub intent_title: String,
    pub intent_statement: String,
    pub recommended_issue_title: String,
    pub symptom_title: String,
    pub suggested_kind: String,
    pub status: String,
    pub entity_types: Vec<String>,
    pub attempted_actions: Vec<String>,
    pub successful_actions: Vec<String>,
    pub failure_patterns: Vec<String>,
    pub total_count: u64,
    pub failure_count: u64,
    pub success_count: u64,
    pub authz_denials: u64,
    pub workaround_count: u64,
    pub abandonment_count: u64,
    pub success_after_failure_count: u64,
    pub success_rate: f64,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_body: Option<serde_json::Value>,
    pub sample_agents: Vec<String>,
    pub recommendation: String,
    pub problem_statement: String,
    pub logfire_query_hint: serde_json::Value,
    pub evidence_examples: Vec<TrajectorySample>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkaroundPattern {
    pub intent_key: String,
    pub intent_title: String,
    pub failed_actions: Vec<String>,
    pub successful_actions: Vec<String>,
    pub occurrences: u64,
    pub sample_agents: Vec<String>,
    pub last_seen: String,
    pub recommendation: String,
    pub logfire_query_hint: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AbandonmentPattern {
    pub intent_key: String,
    pub intent_title: String,
    pub failed_actions: Vec<String>,
    pub abandonment_count: u64,
    pub sample_agents: Vec<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub recommendation: String,
    pub logfire_query_hint: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TrajectorySample {
    pub timestamp: String,
    pub entity_type: String,
    pub action: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

struct IntentCandidateAccum {
    intent_key: String,
    intent_title: String,
    intent_statement: String,
    recommended_issue_title: String,
    symptom_title: String,
    entity_types: BTreeSet<String>,
    attempted_actions: BTreeSet<String>,
    successful_actions: BTreeSet<String>,
    failure_patterns: BTreeSet<String>,
    sample_intent: Option<String>,
    sample_body: Option<serde_json::Value>,
    sample_agents: BTreeSet<String>,
    total_count: u64,
    failure_count: u64,
    success_count: u64,
    authz_denials: u64,
    workaround_count: u64,
    abandonment_count: u64,
    success_after_failure_count: u64,
    first_seen: String,
    last_seen: String,
    evidence_examples: Vec<TrajectorySample>,
}

struct PendingFailure {
    intent_key: String,
    failed_actions: BTreeSet<String>,
    agent_id: Option<String>,
    first_seen: String,
    last_seen: String,
}

struct WorkaroundAccum {
    intent_key: String,
    intent_title: String,
    failed_actions: BTreeSet<String>,
    successful_actions: BTreeSet<String>,
    sample_agents: BTreeSet<String>,
    occurrences: u64,
    last_seen: String,
}

struct AbandonmentAccum {
    intent_key: String,
    intent_title: String,
    failed_actions: BTreeSet<String>,
    sample_agents: BTreeSet<String>,
    abandonment_count: u64,
    first_seen: String,
    last_seen: String,
}

/// Generate richer, intent-shaped evidence from recent trajectories.
///
/// Unlike `generate_unmet_intents_from_aggregated`, this path intentionally
/// loads bounded raw trajectories so the evolution analyst can reason about:
/// - explicit caller intents (`X-Intent`)
/// - repeated failures around the same intended outcome
/// - workaround sequences (failure followed by alternate success)
/// - abandonment candidates (failed attempts that never recover)
#[instrument(skip_all, fields(entry_count = entries.len(), candidate_count = tracing::field::Empty))]
pub(crate) fn generate_intent_evidence(
    entries: &[crate::state::TrajectoryEntry],
) -> IntentEvidenceSummary {
    if entries.is_empty() {
        return IntentEvidenceSummary {
            intent_candidates: Vec::new(),
            workaround_patterns: Vec::new(),
            abandonment_patterns: Vec::new(),
            trajectory_samples: Vec::new(),
        };
    }

    let mut sorted_entries = entries.to_vec();
    sorted_entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    let mut candidates = BTreeMap::<String, IntentCandidateAccum>::new();
    let mut pending_failures = BTreeMap::<(String, String), PendingFailure>::new();
    let mut workarounds = BTreeMap::<String, WorkaroundAccum>::new();
    let mut abandonments = BTreeMap::<String, AbandonmentAccum>::new();

    for entry in &sorted_entries {
        let intent_key = derive_intent_key(entry);
        let intent_title =
            derive_intent_title(entry.intent.as_deref(), &entry.entity_type, &entry.action);
        let intent_statement = entry
            .intent
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| derive_intent_statement(&entry.entity_type, &entry.action));
        let symptom_title = derive_symptom_title(entry);
        let issue_title = derive_issue_title(
            &intent_title,
            entry.intent.as_deref(),
            &entry.entity_type,
            &entry.action,
        );
        let sample = sample_from_entry(entry);
        let accum = candidates
            .entry(intent_key.clone())
            .or_insert_with(|| IntentCandidateAccum {
                intent_key: intent_key.clone(),
                intent_title: intent_title.clone(),
                intent_statement: intent_statement.clone(),
                recommended_issue_title: issue_title.clone(),
                symptom_title: symptom_title.clone(),
                entity_types: BTreeSet::new(),
                attempted_actions: BTreeSet::new(),
                successful_actions: BTreeSet::new(),
                failure_patterns: BTreeSet::new(),
                sample_intent: None,
                sample_body: None,
                sample_agents: BTreeSet::new(),
                total_count: 0,
                failure_count: 0,
                success_count: 0,
                authz_denials: 0,
                workaround_count: 0,
                abandonment_count: 0,
                success_after_failure_count: 0,
                first_seen: entry.timestamp.clone(),
                last_seen: entry.timestamp.clone(),
                evidence_examples: Vec::new(),
            });

        accum.total_count += 1;
        accum.entity_types.insert(entry.entity_type.clone());
        accum.attempted_actions.insert(entry.action.clone());
        accum.last_seen = entry.timestamp.clone();
        if entry.timestamp < accum.first_seen {
            accum.first_seen = entry.timestamp.clone();
        }
        if let Some(agent_id) = entry
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            accum.sample_agents.insert(agent_id.to_string());
        }
        if let Some(intent) = entry
            .intent
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            accum.sample_intent = Some(intent.to_string());
        }
        if entry.request_body.is_some() {
            accum.sample_body = entry.request_body.clone();
        }

        if accum.evidence_examples.len() < 4 || !entry.success {
            accum.evidence_examples.push(sample.clone());
            accum.evidence_examples.truncate(4);
        }

        if entry.success {
            accum.success_count += 1;
            accum.successful_actions.insert(entry.action.clone());
        } else {
            accum.failure_count += 1;
            let error_pattern = super::categorize_error(entry.error.as_deref());
            let is_authz_denied = error_pattern == "AuthzDenied";
            accum.failure_patterns.insert(error_pattern);
            if entry.authz_denied == Some(true) || is_authz_denied {
                accum.authz_denials += 1;
            }
        }

        let actor_key = actor_intent_key(entry);
        if entry.success {
            if let Some(pending) = pending_failures.remove(&(actor_key.clone(), intent_key.clone()))
            {
                if pending
                    .failed_actions
                    .iter()
                    .any(|action| action != &entry.action)
                {
                    accum.workaround_count += 1;
                    accum.success_after_failure_count += 1;
                    let workaround_key = format!(
                        "{}::{}",
                        intent_key,
                        normalize_for_key(&format!(
                            "{}->{}",
                            join_set(&pending.failed_actions),
                            entry.action
                        ))
                    );
                    let workaround =
                        workarounds
                            .entry(workaround_key)
                            .or_insert_with(|| WorkaroundAccum {
                                intent_key: intent_key.clone(),
                                intent_title: intent_title.clone(),
                                failed_actions: pending.failed_actions.clone(),
                                successful_actions: BTreeSet::new(),
                                sample_agents: BTreeSet::new(),
                                occurrences: 0,
                                last_seen: entry.timestamp.clone(),
                            });
                    workaround.occurrences += 1;
                    workaround.last_seen = entry.timestamp.clone();
                    workaround.successful_actions.insert(entry.action.clone());
                    if let Some(agent_id) = pending
                        .agent_id
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                    {
                        workaround.sample_agents.insert(agent_id.to_string());
                    }
                    if let Some(agent_id) = entry
                        .agent_id
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                    {
                        workaround.sample_agents.insert(agent_id.to_string());
                    }
                } else {
                    accum.success_after_failure_count += 1;
                }
            }
        } else {
            let pending = pending_failures
                .entry((actor_key, intent_key.clone()))
                .or_insert_with(|| PendingFailure {
                    intent_key: intent_key.clone(),
                    failed_actions: BTreeSet::new(),
                    agent_id: entry.agent_id.clone(),
                    first_seen: entry.timestamp.clone(),
                    last_seen: entry.timestamp.clone(),
                });
            pending.failed_actions.insert(entry.action.clone());
            pending.last_seen = entry.timestamp.clone();
            if entry.timestamp < pending.first_seen {
                pending.first_seen = entry.timestamp.clone();
            }
        }
    }

    for pending in pending_failures.into_values() {
        if let Some(candidate) = candidates.get_mut(&pending.intent_key) {
            candidate.abandonment_count += 1;
        }
        let abandonment = abandonments
            .entry(pending.intent_key.clone())
            .or_insert_with(|| AbandonmentAccum {
                intent_key: pending.intent_key.clone(),
                intent_title: candidates
                    .get(&pending.intent_key)
                    .map(|value| value.intent_title.clone())
                    .unwrap_or_else(|| "Investigate unmet intent".to_string()),
                failed_actions: BTreeSet::new(),
                sample_agents: BTreeSet::new(),
                abandonment_count: 0,
                first_seen: pending.first_seen.clone(),
                last_seen: pending.last_seen.clone(),
            });
        abandonment.abandonment_count += 1;
        abandonment
            .failed_actions
            .extend(pending.failed_actions.into_iter());
        abandonment.last_seen = pending.last_seen.clone();
        if pending.first_seen < abandonment.first_seen {
            abandonment.first_seen = pending.first_seen.clone();
        }
        if let Some(agent_id) = pending
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            abandonment.sample_agents.insert(agent_id.to_string());
        }
    }

    let mut intent_candidates = candidates
        .into_values()
        .filter(|candidate| {
            candidate.failure_count > 0
                || candidate.workaround_count > 0
                || candidate.abandonment_count > 0
        })
        .map(finalize_intent_candidate)
        .collect::<Vec<_>>();
    intent_candidates.sort_by(|a, b| {
        score_intent_candidate(b)
            .cmp(&score_intent_candidate(a))
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    intent_candidates.truncate(12);

    let mut workaround_patterns = workarounds
        .into_values()
        .map(finalize_workaround_pattern)
        .collect::<Vec<_>>();
    workaround_patterns.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    workaround_patterns.truncate(8);

    let mut abandonment_patterns = abandonments
        .into_values()
        .map(finalize_abandonment_pattern)
        .collect::<Vec<_>>();
    abandonment_patterns.sort_by(|a, b| {
        b.abandonment_count
            .cmp(&a.abandonment_count)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    abandonment_patterns.truncate(8);

    let trajectory_samples = sorted_entries
        .iter()
        .rev()
        .take(20)
        .map(sample_from_entry)
        .collect::<Vec<_>>();

    tracing::Span::current().record("candidate_count", intent_candidates.len());

    IntentEvidenceSummary {
        intent_candidates,
        workaround_patterns,
        abandonment_patterns,
        trajectory_samples,
    }
}

fn finalize_intent_candidate(candidate: IntentCandidateAccum) -> IntentCandidate {
    let success_rate = if candidate.total_count == 0 {
        0.0
    } else {
        candidate.success_count as f64 / candidate.total_count as f64
    };
    let suggested_kind = if candidate.authz_denials > 0
        && candidate.authz_denials
            >= candidate
                .failure_count
                .saturating_sub(candidate.success_count)
    {
        "governance_gap".to_string()
    } else if candidate.workaround_count > 0 {
        "workaround".to_string()
    } else if candidate
        .failure_patterns
        .iter()
        .any(|pattern| matches!(pattern.as_str(), "EntitySetNotFound" | "ActionNotFound"))
    {
        "missing_capability".to_string()
    } else {
        "friction".to_string()
    };
    let status = if candidate.failure_count == 0 {
        "resolved"
    } else if candidate.workaround_count > 0 {
        "workaround"
    } else if candidate.success_count > 0 {
        "mixed"
    } else {
        "open"
    }
    .to_string();
    let hint_entity_type = candidate.entity_types.iter().next().cloned();
    let hint_action = candidate.attempted_actions.iter().next().cloned();
    let hint_intent = candidate.sample_intent.clone();
    let recommendation = match suggested_kind.as_str() {
        "governance_gap" => format!(
            "Align policy with the intended '{}' workflow and keep the scope limited to the minimum required principals/resources.",
            candidate.intent_title
        ),
        "workaround" => format!(
            "Promote the successful workaround into a first-class capability for '{}', so users stop relying on alternate action chains.",
            candidate.intent_title
        ),
        "friction" => format!(
            "Collapse the repeated multi-step flow behind '{}' into a simpler supported path.",
            candidate.intent_title
        ),
        _ => format!(
            "Add direct product/spec support for '{}'.",
            candidate.intent_title
        ),
    };
    let problem_statement = match suggested_kind.as_str() {
        "governance_gap" => format!(
            "The intended outcome '{}' is blocked by repeated authorization denials across the current workflow.",
            candidate.intent_statement
        ),
        "workaround" => format!(
            "Users and agents are trying to achieve '{}' and are only succeeding through alternate action paths rather than a direct capability.",
            candidate.intent_statement
        ),
        "friction" => format!(
            "The intended outcome '{}' is possible, but only after repeated retries or unnecessary extra steps.",
            candidate.intent_statement
        ),
        _ => format!(
            "The intended outcome '{}' is not directly supported by the current product/spec surface.",
            candidate.intent_statement
        ),
    };

    IntentCandidate {
        intent_key: candidate.intent_key.clone(),
        intent_title: candidate.intent_title.clone(),
        intent_statement: candidate.intent_statement,
        recommended_issue_title: candidate.recommended_issue_title,
        symptom_title: candidate.symptom_title,
        suggested_kind: suggested_kind.clone(),
        status,
        entity_types: candidate.entity_types.into_iter().collect(),
        attempted_actions: candidate.attempted_actions.iter().cloned().collect(),
        successful_actions: candidate.successful_actions.iter().cloned().collect(),
        failure_patterns: candidate.failure_patterns.iter().cloned().collect(),
        total_count: candidate.total_count,
        failure_count: candidate.failure_count,
        success_count: candidate.success_count,
        authz_denials: candidate.authz_denials,
        workaround_count: candidate.workaround_count,
        abandonment_count: candidate.abandonment_count,
        success_after_failure_count: candidate.success_after_failure_count,
        success_rate,
        first_seen: candidate.first_seen,
        last_seen: candidate.last_seen,
        sample_intent: candidate.sample_intent,
        sample_body: candidate.sample_body,
        sample_agents: candidate.sample_agents.iter().cloned().collect(),
        recommendation,
        problem_statement,
        logfire_query_hint: build_logfire_query_hint(
            &suggested_kind,
            hint_entity_type.as_deref(),
            hint_action.as_deref(),
            hint_intent.as_deref(),
        ),
        evidence_examples: candidate.evidence_examples,
    }
}

fn finalize_workaround_pattern(pattern: WorkaroundAccum) -> WorkaroundPattern {
    WorkaroundPattern {
        intent_key: pattern.intent_key.clone(),
        intent_title: pattern.intent_title.clone(),
        failed_actions: pattern.failed_actions.iter().cloned().collect(),
        successful_actions: pattern.successful_actions.iter().cloned().collect(),
        occurrences: pattern.occurrences,
        sample_agents: pattern.sample_agents.iter().cloned().collect(),
        last_seen: pattern.last_seen,
        recommendation: format!(
            "Inspect '{}' and graduate the successful alternate path into a supported single-step workflow.",
            pattern.intent_title
        ),
        logfire_query_hint: build_logfire_query_hint(
            "alternate_success_paths",
            None,
            pattern.failed_actions.iter().next().map(String::as_str),
            Some(pattern.intent_title.as_str()),
        ),
    }
}

fn finalize_abandonment_pattern(pattern: AbandonmentAccum) -> AbandonmentPattern {
    AbandonmentPattern {
        intent_key: pattern.intent_key.clone(),
        intent_title: pattern.intent_title.clone(),
        failed_actions: pattern.failed_actions.iter().cloned().collect(),
        abandonment_count: pattern.abandonment_count,
        sample_agents: pattern.sample_agents.iter().cloned().collect(),
        first_seen: pattern.first_seen,
        last_seen: pattern.last_seen,
        recommendation: format!(
            "Investigate why '{}' never reaches a successful outcome after the observed failed attempts.",
            pattern.intent_title
        ),
        logfire_query_hint: build_logfire_query_hint(
            "intent_abandonment",
            None,
            pattern.failed_actions.iter().next().map(String::as_str),
            Some(pattern.intent_title.as_str()),
        ),
    }
}

fn sample_from_entry(entry: &crate::state::TrajectoryEntry) -> TrajectorySample {
    TrajectorySample {
        timestamp: entry.timestamp.clone(),
        entity_type: entry.entity_type.clone(),
        action: entry.action.clone(),
        success: entry.success,
        error_pattern: (!entry.success).then(|| super::categorize_error(entry.error.as_deref())),
        error: entry.error.clone(),
        intent: entry.intent.clone(),
        agent_id: entry.agent_id.clone(),
        session_id: entry.session_id.clone(),
    }
}

fn score_intent_candidate(candidate: &IntentCandidate) -> u64 {
    candidate.failure_count.saturating_mul(4)
        + candidate.workaround_count.saturating_mul(5)
        + candidate.abandonment_count.saturating_mul(4)
        + candidate.authz_denials.saturating_mul(3)
        + candidate.success_after_failure_count.saturating_mul(2)
}

fn actor_intent_key(entry: &crate::state::TrajectoryEntry) -> String {
    let actor = entry
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            entry
                .agent_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "anonymous".to_string());
    format!("{actor}::{}", derive_intent_key(entry))
}

fn derive_intent_key(entry: &crate::state::TrajectoryEntry) -> String {
    if let Some(intent) = entry
        .intent
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return normalize_for_key(intent);
    }

    if let Some(request_body) = entry.request_body.as_ref() {
        for key in ["intent", "goal", "objective", "Title", "title"] {
            if let Some(value) = request_body.get(key).and_then(serde_json::Value::as_str)
                && !value.trim().is_empty()
            {
                return normalize_for_key(value);
            }
        }
    }

    normalize_for_key(&derive_intent_statement(&entry.entity_type, &entry.action))
}

fn derive_intent_title(sample_intent: Option<&str>, entity_type: &str, action: &str) -> String {
    if let Some(intent) = sample_intent.filter(|value| !value.trim().is_empty()) {
        return title_case(intent);
    }

    let action_lower = action.to_ascii_lowercase();
    let entity = humanize_identifier(entity_type).to_ascii_lowercase();
    if action_lower.starts_with("generate") {
        return format!("Enable {entity} generation");
    }
    if action_lower.starts_with("create") {
        return format!("Enable {entity} creation");
    }
    if let Some(target) = action
        .strip_prefix("MoveTo")
        .or_else(|| action.strip_prefix("moveTo"))
    {
        return format!(
            "Allow {} to reach {}",
            humanize_identifier(entity_type).to_ascii_lowercase(),
            humanize_identifier(target).to_ascii_lowercase()
        );
    }

    format!(
        "Enable {} {} workflow",
        entity,
        humanize_identifier(action).to_ascii_lowercase()
    )
}

fn derive_issue_title(
    intent_title: &str,
    sample_intent: Option<&str>,
    entity_type: &str,
    action: &str,
) -> String {
    if !intent_title.trim().is_empty() {
        return title_case(intent_title);
    }
    if let Some(intent) = sample_intent.filter(|value| !value.trim().is_empty()) {
        return title_case(intent);
    }
    title_case(&derive_intent_statement(entity_type, action))
}

fn derive_intent_statement(entity_type: &str, action: &str) -> String {
    let action_lower = action.to_ascii_lowercase();
    let entity = humanize_identifier(entity_type).to_ascii_lowercase();
    if action_lower.starts_with("generate") {
        return format!("Generate {entity}");
    }
    if action_lower.starts_with("create") {
        return format!("Create {entity}");
    }
    if let Some(target) = action
        .strip_prefix("MoveTo")
        .or_else(|| action.strip_prefix("moveTo"))
    {
        return format!(
            "Move {} to {}",
            entity,
            humanize_identifier(target).to_ascii_lowercase()
        );
    }
    format!(
        "{} {}",
        humanize_identifier(action),
        humanize_identifier(entity_type).to_ascii_lowercase()
    )
}

fn derive_symptom_title(entry: &crate::state::TrajectoryEntry) -> String {
    if entry.success {
        return format!(
            "{} succeeded via {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        );
    }

    let error_pattern = super::categorize_error(entry.error.as_deref());
    match error_pattern.as_str() {
        "AuthzDenied" => format!(
            "{} is denied while attempting {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        ),
        "EntitySetNotFound" => format!(
            "{} is missing for {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        ),
        _ => format!(
            "{} fails during {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        ),
    }
}

fn build_logfire_query_hint(
    query_kind: &str,
    entity_type: Option<&str>,
    action: Option<&str>,
    intent_text: Option<&str>,
) -> serde_json::Value {
    let normalized_query_kind = match query_kind {
        "workaround" => "alternate_success_paths",
        "governance_gap" => "intent_failure_cluster",
        other => other,
    };
    let mut hint = serde_json::json!({
        "tool": "logfire_query",
        "query_kind": normalized_query_kind,
        "service_name": "temper-platform",
        "environment": "local",
        "limit": 25,
        "lookback_minutes": 240,
    });
    if let Some(entity_type) = entity_type.filter(|value| !value.trim().is_empty()) {
        hint["entity_type"] = serde_json::json!(entity_type);
    }
    if let Some(action) = action.filter(|value| !value.trim().is_empty()) {
        hint["action"] = serde_json::json!(action);
    }
    if let Some(intent_text) = intent_text.filter(|value| !value.trim().is_empty()) {
        hint["intent_text"] = serde_json::json!(intent_text);
    }
    hint
}

fn normalize_for_key(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn humanize_identifier(value: &str) -> String {
    let mut out = String::new();
    let mut previous_lowercase = false;
    for ch in value.chars() {
        if ch == '_' || ch == '-' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            previous_lowercase = false;
            continue;
        }
        if ch.is_ascii_uppercase() && previous_lowercase {
            out.push(' ');
        }
        out.push(ch.to_ascii_lowercase());
        previous_lowercase = ch.is_ascii_lowercase();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn join_set(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(",")
}
