use axum::extract::{Json as ExtractJson, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::{Deserialize, Serialize};
use temper_evolution::records::{ImpactAssessment, SolutionOption};
use temper_evolution::{
    AnalysisRecord, Complexity, InsightCategory, InsightRecord, InsightSignal, ObservationClass,
    ObservationRecord, ProblemRecord, RecordHeader, RecordType, Severity, SolutionRisk, Trend,
};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::authz::require_observe_auth;
use crate::odata::extract_tenant;
use crate::request_context::{AgentContext, extract_agent_context};
use crate::state::{ObserveRefreshHint, ServerState};

use super::support::{
    create_system_entity_logged, dispatch_system_action_required, emit_refresh_hints,
    next_system_entity_id, persist_record, spawn_intent_discovery,
};

#[cfg(test)]
#[path = "materialize_test.rs"]
mod tests;

#[derive(Debug, Deserialize)]
pub(crate) struct EvolutionAnalyzeRequest {
    pub reason: Option<String>,
    pub source: Option<String>,
    pub trigger_context: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvolutionMaterializeRequest {
    pub intent_discovery_id: String,
    pub analysis_json: String,
    pub signal_summary_json: String,
    pub tenant: Option<String>,
    pub reason: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentAnalysisPayload {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    findings: Vec<AgentFinding>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct AgentFinding {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    symptom_title: String,
    #[serde(default)]
    intent_title: String,
    #[serde(default)]
    recommended_issue_title: String,
    #[serde(default)]
    intent: String,
    #[serde(default)]
    recommendation: String,
    #[serde(default)]
    priority_score: f64,
    #[serde(default)]
    volume: u64,
    #[serde(default)]
    success_rate: f64,
    #[serde(default)]
    trend: String,
    #[serde(default)]
    requires_spec_change: bool,
    #[serde(default)]
    problem_statement: String,
    #[serde(default)]
    root_cause: String,
    #[serde(default)]
    spec_diff: String,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    dedupe_key: String,
    #[serde(default)]
    evidence: serde_json::Value,
}

#[derive(Default)]
struct SpecChangeArtifacts {
    record_ids: Vec<String>,
    observation_entity_id: String,
    derived_from_record_id: Option<String>,
}

struct MaterializedFinding {
    record_ids: Vec<String>,
    issue_id: String,
    report: serde_json::Value,
}

fn trend_from_str(value: &str) -> Trend {
    match value.trim().to_ascii_lowercase().as_str() {
        "declining" => Trend::Declining,
        "stable" => Trend::Stable,
        _ => Trend::Growing,
    }
}

fn severity_from_score(score: f64) -> Severity {
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

fn solution_risk_from_score(score: f64) -> SolutionRisk {
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

fn complexity_from_finding(finding: &AgentFinding) -> Complexity {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "friction" | "governance_gap" => Complexity::Low,
        "workaround" => Complexity::Medium,
        _ => Complexity::Medium,
    }
}

fn observation_class_for_finding(finding: &AgentFinding) -> ObservationClass {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "governance_gap" => ObservationClass::AuthzDenied,
        _ => ObservationClass::Trajectory,
    }
}

fn insight_category_for_finding(finding: &AgentFinding) -> InsightCategory {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "friction" => InsightCategory::Friction,
        "workaround" => InsightCategory::Workaround,
        "governance_gap" => InsightCategory::PlatformGap,
        _ => InsightCategory::UnmetIntent,
    }
}

fn issue_priority_level(score: f64) -> i64 {
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

fn finding_symptom_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[
            &finding.symptom_title,
            &finding.title,
            &finding.problem_statement,
        ],
        "Observed workflow symptom",
    )
}

fn finding_intent_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[&finding.intent_title, &finding.intent, &finding.title],
        "Enable unmet intent",
    )
}

fn finding_issue_title(finding: &AgentFinding) -> String {
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

fn default_acceptance_criteria(finding: &AgentFinding) -> Vec<String> {
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

fn build_issue_description(summary: &str, finding: &AgentFinding, record_ids: &[String]) -> String {
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

async fn create_issue_for_finding(
    state: &ServerState,
    tenant: &TenantId,
    summary: &str,
    finding: &AgentFinding,
    record_ids: &[String],
) -> Result<String, String> {
    let issue_id = temper_runtime::scheduler::sim_uuid().to_string();
    let now = sim_now().to_rfc3339();
    let issue_title = finding_issue_title(finding);
    let description = build_issue_description(summary, finding, record_ids);
    let acceptance_criteria = default_acceptance_criteria(finding).join("\n");

    state
        .get_or_create_tenant_entity(
            tenant,
            "Issue",
            &issue_id,
            serde_json::json!({
                "Id": issue_id.clone(),
                "Title": issue_title,
                "Description": description,
                "AcceptanceCriteria": acceptance_criteria,
                "Priority": issue_priority_level(finding.priority_score),
                "CreatedAt": now,
                "UpdatedAt": now,
            }),
        )
        .await?;

    let system_ctx = AgentContext::system();
    let _ = state
        .dispatch_tenant_action(
            tenant,
            "Issue",
            &issue_id,
            "SetPriority",
            serde_json::json!({ "level": issue_priority_level(finding.priority_score) }),
            &system_ctx,
        )
        .await;
    let _ = state
        .dispatch_tenant_action(
            tenant,
            "Issue",
            &issue_id,
            "MoveToTriage",
            serde_json::json!({}),
            &system_ctx,
        )
        .await;
    let _ = state
        .dispatch_tenant_action(
            tenant,
            "Issue",
            &issue_id,
            "MoveToTodo",
            serde_json::json!({}),
            &system_ctx,
        )
        .await;

    Ok(issue_id)
}

async fn materialize_spec_change_records(
    state: &ServerState,
    tenant: &TenantId,
    payload: &EvolutionMaterializeRequest,
    signal_summary: &serde_json::Value,
    finding: &AgentFinding,
) -> Result<SpecChangeArtifacts, StatusCode> {
    if !finding.requires_spec_change {
        return Ok(SpecChangeArtifacts::default());
    }

    let observation = ObservationRecord {
        header: RecordHeader::new(RecordType::Observation, "intent-discovery"),
        source: format!(
            "intent-discovery:{}",
            if finding.kind.is_empty() {
                "analysis"
            } else {
                finding.kind.as_str()
            }
        ),
        classification: observation_class_for_finding(finding),
        evidence_query: format!(
            "intent discovery {} -> symptom={} intent={}",
            payload.intent_discovery_id,
            finding_symptom_title(finding),
            finding_intent_title(finding)
        ),
        threshold_field: None,
        threshold_value: None,
        observed_value: Some(finding.volume as f64),
        context: serde_json::json!({
            "tenant": tenant.as_str(),
            "reason": payload.reason,
            "source": payload.source,
            "signal_summary": signal_summary.clone(),
            "finding": finding,
        }),
    };
    persist_record(state, "Observation", &observation.header, &observation).await?;

    let observation_entity_id = next_system_entity_id("OBS");
    create_system_entity_logged(
        state,
        "Observation",
        &observation_entity_id,
        "CreateObservation",
        serde_json::json!({
            "source": observation.source,
            "classification": format!("{:?}", observation.classification),
            "evidence_query": observation.evidence_query,
            "context": serde_json::to_string(&observation.context).unwrap_or_default(),
            "tenant": tenant.as_str(),
            "legacy_record_id": observation.header.id,
        }),
    )
    .await;

    let problem = ProblemRecord {
        header: RecordHeader::new(RecordType::Problem, "intent-discovery")
            .derived_from(&observation.header.id),
        problem_statement: if finding.problem_statement.is_empty() {
            format!(
                "{} blocks intended workflow completion.",
                finding_intent_title(finding)
            )
        } else {
            finding.problem_statement.clone()
        },
        invariants: default_acceptance_criteria(finding),
        constraints: if finding.dedupe_key.is_empty() {
            Vec::new()
        } else {
            vec![format!("dedupe_key={}", finding.dedupe_key)]
        },
        impact: ImpactAssessment {
            affected_users: Some(finding.volume),
            severity: severity_from_score(finding.priority_score),
            trend: trend_from_str(&finding.trend),
        },
    };
    persist_record(state, "Problem", &problem.header, &problem).await?;

    let problem_entity_id = next_system_entity_id("PRB");
    dispatch_system_action_required(
        state,
        "Problem",
        &problem_entity_id,
        "CreateProblem",
        serde_json::json!({
            "observation_id": observation_entity_id,
            "problem_statement": problem.problem_statement,
            "severity": problem.impact.severity.to_string(),
            "invariants": serde_json::to_string(&problem.invariants).unwrap_or_default(),
        }),
    )
    .await?;
    dispatch_system_action_required(
        state,
        "Problem",
        &problem_entity_id,
        "MarkReviewed",
        serde_json::json!({}),
    )
    .await?;

    let analysis = AnalysisRecord {
        header: RecordHeader::new(RecordType::Analysis, "intent-discovery")
            .derived_from(&problem.header.id),
        root_cause: if finding.root_cause.is_empty() {
            "IntentDiscovery inferred a missing platform capability.".to_string()
        } else {
            finding.root_cause.clone()
        },
        options: vec![SolutionOption {
            description: finding.recommendation.clone(),
            spec_diff: if finding.spec_diff.is_empty() {
                "No explicit spec diff supplied.".to_string()
            } else {
                finding.spec_diff.clone()
            },
            tla_impact: "NONE".to_string(),
            risk: solution_risk_from_score(finding.priority_score),
            complexity: complexity_from_finding(finding),
        }],
        recommendation: Some(0),
    };
    persist_record(state, "Analysis", &analysis.header, &analysis).await?;

    let analysis_entity_id = next_system_entity_id("ANL");
    dispatch_system_action_required(
        state,
        "Analysis",
        &analysis_entity_id,
        "CreateAnalysis",
        serde_json::json!({
            "problem_id": problem_entity_id,
            "root_cause": analysis.root_cause,
            "options": serde_json::to_string(&analysis.options).unwrap_or_default(),
            "recommendation": analysis.recommendation.unwrap_or_default().to_string(),
        }),
    )
    .await?;

    Ok(SpecChangeArtifacts {
        record_ids: vec![
            observation.header.id.clone(),
            problem.header.id.clone(),
            analysis.header.id.clone(),
        ],
        observation_entity_id,
        derived_from_record_id: Some(analysis.header.id.clone()),
    })
}

async fn materialize_finding(
    state: &ServerState,
    tenant: &TenantId,
    summary: &str,
    payload: &EvolutionMaterializeRequest,
    signal_summary: &serde_json::Value,
    finding: &AgentFinding,
) -> Result<MaterializedFinding, StatusCode> {
    let mut artifacts =
        materialize_spec_change_records(state, tenant, payload, signal_summary, finding).await?;

    let mut insight_header = RecordHeader::new(RecordType::Insight, "intent-discovery");
    if let Some(parent) = artifacts.derived_from_record_id.as_ref() {
        insight_header = insight_header.derived_from(parent.clone());
    }
    let insight = InsightRecord {
        header: insight_header,
        category: insight_category_for_finding(finding),
        signal: InsightSignal {
            intent: if finding.intent.is_empty() {
                finding_intent_title(finding)
            } else {
                finding.intent.clone()
            },
            volume: finding.volume,
            success_rate: finding.success_rate,
            trend: trend_from_str(&finding.trend),
            growth_rate: None,
        },
        recommendation: finding.recommendation.clone(),
        priority_score: finding.priority_score,
    };
    persist_record(state, "Insight", &insight.header, &insight).await?;
    artifacts.record_ids.push(insight.header.id.clone());

    create_system_entity_logged(
        state,
        "Insight",
        &next_system_entity_id("INS"),
        "CreateInsight",
        serde_json::json!({
            "observation_id": artifacts.observation_entity_id,
            "category": format!("{:?}", insight.category),
            "signal": insight.signal.intent,
            "recommendation": insight.recommendation,
            "priority_score": format!("{:.4}", insight.priority_score),
            "legacy_record_id": insight.header.id,
        }),
    )
    .await;

    let issue_id = create_issue_for_finding(state, tenant, summary, finding, &artifacts.record_ids)
        .await
        .map_err(|error| {
            tracing::warn!(
                error = %error,
                issue_title = %finding_issue_title(finding),
                "evolution.issue.create"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(MaterializedFinding {
        report: serde_json::json!({
            "title": finding_issue_title(finding),
            "intent_title": finding_intent_title(finding),
            "symptom_title": finding_symptom_title(finding),
            "kind": finding.kind.clone(),
            "record_ids": artifacts.record_ids,
            "issue_id": issue_id,
        }),
        record_ids: artifacts.record_ids,
        issue_id,
    })
}

/// POST /api/evolution/analyze -- create and run one IntentDiscovery cycle.
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/analyze"))]
pub(crate) async fn handle_evolution_analyze(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "run_sentinel", "Evolution")?;
    let tenant = extract_tenant(&headers, &state).map_err(|_| StatusCode::BAD_REQUEST)?;
    let payload = if body.is_empty() {
        EvolutionAnalyzeRequest {
            reason: None,
            source: None,
            trigger_context: None,
        }
    } else {
        serde_json::from_slice::<EvolutionAnalyzeRequest>(&body)
            .map_err(|_| StatusCode::BAD_REQUEST)?
    };
    let agent_ctx = extract_agent_context(&headers);
    let reason = payload.reason.unwrap_or_else(|| "manual".to_string());
    let source = payload.source.unwrap_or_else(|| "developer".to_string());
    let trigger_context = payload
        .trigger_context
        .unwrap_or_else(|| serde_json::json!({}));

    let (entity_id, response) = spawn_intent_discovery(
        &state,
        &tenant,
        &reason,
        &source,
        trigger_context,
        &agent_ctx,
        true,
    )
    .await
    .map_err(|error| {
        tracing::warn!(error = %error, tenant = %tenant, "failed to run IntentDiscovery");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({
        "tenant": tenant.as_str(),
        "entity_id": entity_id,
        "success": response.success,
        "status": response.state.status,
        "error": response.error,
        "fields": response.state.fields,
    })))
}

/// POST /api/evolution/materialize -- persist O/P/A/I records and PM issues.
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/materialize"))]
pub(crate) async fn handle_evolution_materialize(
    State(state): State<ServerState>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<EvolutionMaterializeRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "run_sentinel", "Evolution")?;
    let tenant = extract_tenant(&headers, &state).map_err(|_| StatusCode::BAD_REQUEST)?;
    let analysis = serde_json::from_str::<AgentAnalysisPayload>(&payload.analysis_json)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let signal_summary = serde_json::from_str::<serde_json::Value>(&payload.signal_summary_json)
        .unwrap_or_else(|_| serde_json::json!({}));
    let summary = if analysis.summary.is_empty() {
        "IntentDiscovery produced structured findings.".to_string()
    } else {
        analysis.summary.clone()
    };

    let mut record_ids = Vec::<String>::new();
    let mut issue_ids = Vec::<String>::new();
    let mut findings_report = Vec::<serde_json::Value>::new();

    for finding in &analysis.findings {
        let materialized = materialize_finding(
            &state,
            &tenant,
            &summary,
            &payload,
            &signal_summary,
            finding,
        )
        .await?;
        record_ids.extend(materialized.record_ids);
        issue_ids.push(materialized.issue_id);
        findings_report.push(materialized.report);
    }

    emit_refresh_hints(
        &state,
        &[
            ObserveRefreshHint::EvolutionRecords,
            ObserveRefreshHint::EvolutionInsights,
            ObserveRefreshHint::Entities,
        ],
    );

    Ok(Json(serde_json::json!({
        "intent_discovery_id": payload.intent_discovery_id,
        "tenant": payload.tenant.unwrap_or_else(|| tenant.as_str().to_string()),
        "records_created_count": record_ids.len(),
        "issues_created_count": issue_ids.len(),
        "record_ids": record_ids,
        "issue_ids": issue_ids,
        "findings": findings_report,
    })))
}
