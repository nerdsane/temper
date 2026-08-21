use serde::{Deserialize, Serialize};
use temper_evolution::{
    Complexity, InsightCategory, ObservationClass, Severity, SolutionRisk, Trend,
};

#[derive(Debug, Deserialize)]
pub(super) struct EvolutionAnalyzeRequest {
    pub(super) reason: Option<String>,
    pub(super) source: Option<String>,
    pub(super) trigger_context: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct EvolutionMaterializeRequest {
    pub(super) intent_discovery_id: String,
    pub(super) analysis_json: String,
    pub(super) signal_summary_json: String,
    pub(super) tenant: Option<String>,
    pub(super) reason: Option<String>,
    pub(super) source: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct AgentAnalysisPayload {
    #[serde(default)]
    pub(super) summary: String,
    #[serde(default)]
    pub(super) findings: Vec<AgentFinding>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(super) struct AgentFinding {
    #[serde(default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) symptom_title: String,
    #[serde(default)]
    pub(super) intent_title: String,
    #[serde(default)]
    pub(super) recommended_issue_title: String,
    #[serde(default)]
    pub(super) intent: String,
    #[serde(default)]
    pub(super) recommendation: String,
    #[serde(default)]
    pub(super) priority_score: f64,
    #[serde(default)]
    pub(super) volume: u64,
    #[serde(default)]
    pub(super) success_rate: f64,
    #[serde(default)]
    pub(super) trend: String,
    #[serde(default)]
    pub(super) requires_spec_change: bool,
    #[serde(default)]
    pub(super) problem_statement: String,
    #[serde(default)]
    pub(super) root_cause: String,
    #[serde(default)]
    pub(super) spec_diff: String,
    #[serde(default)]
    pub(super) acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub(super) dedupe_key: String,
    #[serde(default)]
    pub(super) evidence: serde_json::Value,
}

pub(super) fn trend_from_str(value: &str) -> Trend {
    match value.trim().to_ascii_lowercase().as_str() {
        "declining" => Trend::Declining,
        "stable" => Trend::Stable,
        _ => Trend::Growing,
    }
}

pub(super) fn severity_from_score(score: f64) -> Severity {
    if score >= 0.85 {
        Severity::Critical
    } else if score >= 0.65 {
        Severity::High
    } else if score >= 0.40 {
        Severity::Medium
    } else {
        Severity::Low
    }
}

pub(super) fn solution_risk_from_score(score: f64) -> SolutionRisk {
    if score >= 0.85 {
        SolutionRisk::High
    } else if score >= 0.65 {
        SolutionRisk::Medium
    } else if score >= 0.35 {
        SolutionRisk::Low
    } else {
        SolutionRisk::None
    }
}

pub(super) fn complexity_from_finding(finding: &AgentFinding) -> Complexity {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "friction" | "governance_gap" => Complexity::Low,
        "workaround" => Complexity::Medium,
        _ => Complexity::Medium,
    }
}

pub(super) fn observation_class_for_finding(finding: &AgentFinding) -> ObservationClass {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "governance_gap" => ObservationClass::AuthzDenied,
        _ => ObservationClass::Trajectory,
    }
}

pub(super) fn insight_category_for_finding(finding: &AgentFinding) -> InsightCategory {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "friction" => InsightCategory::Friction,
        "workaround" => InsightCategory::Workaround,
        "governance_gap" => InsightCategory::PlatformGap,
        _ => InsightCategory::UnmetIntent,
    }
}

pub(super) fn issue_priority_level(score: f64) -> i64 {
    if score >= 0.85 {
        1
    } else if score >= 0.65 {
        2
    } else if score >= 0.40 {
        3
    } else {
        4
    }
}

fn preferred_title(candidates: &[&str], fallback: &str) -> String {
    candidates
        .iter()
        .find_map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .unwrap_or_else(|| fallback.to_string())
}

pub(super) fn finding_symptom_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[
            &finding.symptom_title,
            &finding.title,
            &finding.problem_statement,
        ],
        "Observed workflow symptom",
    )
}

pub(super) fn finding_intent_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[&finding.intent_title, &finding.intent, &finding.title],
        "Enable unmet intent",
    )
}

pub(super) fn finding_issue_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[
            &finding.recommended_issue_title,
            &finding.intent_title,
            &finding.title,
            &finding.intent,
            &finding.symptom_title,
        ],
        "Investigate unmet intent",
    )
}

pub(super) fn default_acceptance_criteria(finding: &AgentFinding) -> Vec<String> {
    if !finding.acceptance_criteria.is_empty() {
        return finding.acceptance_criteria.clone();
    }
    let issue_title = finding_issue_title(finding);
    vec![
        format!(
            "Agents can complete '{}' without the current failure mode.",
            issue_title
        ),
        "Observe metrics show improved completion for the affected workflow.".to_string(),
    ]
}

pub(super) fn build_issue_description(
    summary: &str,
    finding: &AgentFinding,
    record_ids: &[String],
) -> String {
    let acceptance_criteria = default_acceptance_criteria(finding)
        .into_iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Summary:\n{summary}\n\nIntent Title:\n{}\n\nObserved Symptom:\n{}\n\nIntent:\n{}\n\nRecommendation:\n{}\n\nProblem Statement:\n{}\n\nRoot Cause:\n{}\n\nSpec Diff:\n{}\n\nAcceptance Criteria:\n{}\n\nEvidence:\n{}\n\nEvolution Records:\n{}",
        finding_intent_title(finding),
        finding_symptom_title(finding),
        if finding.intent.is_empty() {
            "No explicit intent supplied."
        } else {
            finding.intent.as_str()
        },
        finding.recommendation,
        if finding.problem_statement.is_empty() {
            "No formal problem statement supplied."
        } else {
            finding.problem_statement.as_str()
        },
        if finding.root_cause.is_empty() {
            "No root cause supplied."
        } else {
            finding.root_cause.as_str()
        },
        if finding.spec_diff.is_empty() {
            "No spec diff supplied."
        } else {
            finding.spec_diff.as_str()
        },
        acceptance_criteria,
        serde_json::to_string_pretty(&finding.evidence).unwrap_or_else(|_| "{}".to_string()),
        record_ids.join(", ")
    )
}
