use axum::http::StatusCode;
use temper_evolution::records::{ImpactAssessment, SolutionOption};
use temper_evolution::{
    AnalysisRecord, InsightRecord, InsightSignal, ObservationRecord, ProblemRecord, RecordHeader,
    RecordType,
};
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

use super::super::support::{
    create_system_entity_logged, dispatch_system_action_required, next_system_entity_id,
    persist_record,
};
use super::issue::create_issue_for_finding;
use super::model::{
    AgentFinding, EvolutionMaterializeRequest, complexity_from_finding,
    default_acceptance_criteria, finding_intent_title, finding_issue_title, finding_symptom_title,
    insight_category_for_finding, observation_class_for_finding, severity_from_score,
    solution_risk_from_score, trend_from_str,
};

#[derive(Default)]
struct SpecChangeArtifacts {
    record_ids: Vec<String>,
    observation_entity_id: String,
    derived_from_record_id: Option<String>,
}

pub(super) struct MaterializedFinding {
    pub(super) record_ids: Vec<String>,
    pub(super) issue_id: String,
    pub(super) report: serde_json::Value,
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
    persist_record(
        state,
        tenant.as_str(),
        "Observation",
        &observation.header,
        &observation,
    )
    .await?;

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
    persist_record(state, tenant.as_str(), "Problem", &problem.header, &problem).await?;

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
    persist_record(
        state,
        tenant.as_str(),
        "Analysis",
        &analysis.header,
        &analysis,
    )
    .await?;

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

pub(super) async fn materialize_finding(
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
    persist_record(state, tenant.as_str(), "Insight", &insight.header, &insight).await?;
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
