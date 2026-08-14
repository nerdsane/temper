use axum::extract::{Extension, Json as ExtractJson, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{require_authenticated_context, require_observe_auth, require_tenant_match};
use crate::request_context::extract_agent_context;
use crate::state::{ObserveRefreshHint, ServerState};

use self::model::{AgentAnalysisPayload, EvolutionAnalyzeRequest, EvolutionMaterializeRequest};
use self::records::materialize_finding;
use super::support::{emit_refresh_hints, spawn_intent_discovery};

mod issue;
mod model;
mod records;

#[cfg(test)]
use self::model::{AgentFinding, finding_intent_title, finding_issue_title, finding_symptom_title};

#[cfg(test)]
#[path = "materialize_test.rs"]
mod tests;

/// POST /api/evolution/analyze -- create and run one IntentDiscovery cycle.
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/analyze"))]
pub(crate) async fn handle_evolution_analyze(
    State(state): State<ServerState>,
    headers: HeaderMap,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "run_sentinel", "Evolution")?;
    let tenant = authenticated.tenant().clone();
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
    let mut agent_ctx = extract_agent_context(&headers);
    agent_ctx.security_ctx = Some(authenticated.security_context().clone());
    agent_ctx.agent_id = Some(authenticated.security_context().principal.id.clone());
    agent_ctx.agent_type = authenticated
        .security_context()
        .principal
        .agent_type
        .clone();
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
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    ExtractJson(payload_json): ExtractJson<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "run_sentinel", "Evolution")?;
    let payload = serde_json::from_value::<EvolutionMaterializeRequest>(payload_json)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if let Some(requested_tenant) = payload.tenant.as_deref() {
        require_tenant_match(authenticated, requested_tenant)?;
    }
    let tenant = authenticated.tenant().clone();
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
        "tenant": tenant.as_str(),
        "records_created_count": record_ids.len(),
        "issues_created_count": issue_ids.len(),
        "record_ids": record_ids,
        "issue_ids": issue_ids,
        "findings": findings_report,
    })))
}
