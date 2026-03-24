use axum::http::StatusCode;
use temper_evolution::InsightRecord;
use temper_evolution::RecordHeader;
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;

use crate::request_context::AgentContext;
use crate::sentinel;
use crate::state::{DispatchExtOptions, ObserveRefreshHint, ServerState};

pub(super) async fn persist_evolution_record(
    state: &ServerState,
    record_id: &str,
    record_type: &str,
    status: &str,
    created_by: &str,
    derived_from: Option<&str>,
    data_json: &str,
) -> Result<(), String> {
    let Some(turso) = state.platform_persistent_store() else {
        tracing::debug!(
            record_id,
            record_type,
            status,
            created_by,
            "evolution.store.unavailable"
        );
        return Ok(());
    };

    turso
        .insert_evolution_record(
            record_id,
            record_type,
            status,
            created_by,
            derived_from,
            data_json,
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                record_id,
                record_type,
                status,
                created_by,
                error = %error,
                "evolution.store.write"
            );
            error.to_string()
        })?;
    tracing::info!(
        record_id,
        record_type,
        status,
        created_by,
        derived_from,
        "evolution.store.write"
    );
    Ok(())
}

pub(super) async fn persist_record<T: serde::Serialize>(
    state: &ServerState,
    record_type: &str,
    header: &RecordHeader,
    record: &T,
) -> Result<(), StatusCode> {
    let data_json = serde_json::to_string(record).map_err(|error| {
        tracing::warn!(
            record_id = %header.id,
            record_type,
            error = %error,
            "evolution.store.serialize"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    persist_evolution_record(
        state,
        &header.id,
        record_type,
        &format!("{:?}", header.status),
        &header.created_by,
        header.derived_from.as_deref(),
        &data_json,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub(super) async fn dispatch_system_action(
    state: &ServerState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) -> Result<crate::entity_actor::EntityResponse, String> {
    let system_tenant = TenantId::new("temper-system");
    state
        .dispatch_tenant_action(
            &system_tenant,
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::system(),
        )
        .await
}

pub(super) async fn dispatch_system_action_required(
    state: &ServerState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) -> Result<crate::entity_actor::EntityResponse, StatusCode> {
    dispatch_system_action(state, entity_type, entity_id, action, params)
        .await
        .map_err(|error| {
            tracing::warn!(
                error = %error,
                entity_type,
                entity_id,
                action,
                "evolution.system_entity.dispatch"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub(super) async fn create_system_entity_logged(
    state: &ServerState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) {
    if let Err(error) = dispatch_system_action(state, entity_type, entity_id, action, params).await
    {
        tracing::warn!(
            error = %error,
            entity_type,
            entity_id,
            action,
            "failed to create system entity"
        );
    }
}

pub(super) async fn persist_alerts(
    state: &ServerState,
    alerts: &[sentinel::SentinelAlert],
) -> Result<Vec<serde_json::Value>, StatusCode> {
    let mut results = Vec::new();
    for alert in alerts {
        tracing::warn!(
            rule = %alert.rule_name,
            record_id = %alert.record.header.id,
            source = %alert.record.source,
            classification = ?alert.record.classification,
            observed_value = ?alert.record.observed_value,
            threshold = ?alert.record.threshold_value,
            "evolution.sentinel"
        );

        persist_record(state, "Observation", &alert.record.header, &alert.record).await?;

        let observation_id = next_system_entity_id("OBS");
        create_system_entity_logged(
            state,
            "Observation",
            &observation_id,
            "CreateObservation",
            serde_json::json!({
                "source": alert.record.source,
                "classification": format!("{:?}", alert.record.classification),
                "evidence_query": alert.record.evidence_query,
                "context": serde_json::to_string(&alert.record.context).unwrap_or_default(),
                "tenant": "temper-system",
                "legacy_record_id": alert.record.header.id,
            }),
        )
        .await;

        results.push(serde_json::json!({
            "rule": alert.rule_name,
            "record_id": alert.record.header.id,
            "entity_id": observation_id,
            "source": alert.record.source,
            "classification": alert.record.classification,
            "threshold": alert.record.threshold_value,
            "observed": alert.record.observed_value,
        }));
    }
    Ok(results)
}

pub(super) async fn persist_insights(
    state: &ServerState,
    insights: &[InsightRecord],
) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    for insight in insights {
        tracing::info!(
            record_id = %insight.header.id,
            category = ?insight.category,
            intent = %insight.signal.intent,
            volume = insight.signal.volume,
            success_rate = insight.signal.success_rate,
            priority_score = insight.priority_score,
            "evolution.insight"
        );
        let _ = persist_record(state, "Insight", &insight.header, insight).await;

        let insight_id = next_system_entity_id("INS");
        create_system_entity_logged(
            state,
            "Insight",
            &insight_id,
            "CreateInsight",
            serde_json::json!({
                "observation_id": "",
                "category": format!("{:?}", insight.category),
                "signal": insight.signal.intent,
                "recommendation": insight.recommendation,
                "priority_score": format!("{:.4}", insight.priority_score),
                "legacy_record_id": insight.header.id,
            }),
        )
        .await;

        results.push(serde_json::json!({
            "record_id": insight.header.id,
            "entity_id": insight_id,
            "category": format!("{:?}", insight.category),
            "intent": insight.signal.intent,
            "priority_score": insight.priority_score,
            "recommendation": insight.recommendation,
        }));
    }
    results
}

pub(super) async fn spawn_intent_discovery(
    state: &ServerState,
    tenant: &TenantId,
    reason: &str,
    source: &str,
    trigger_context: serde_json::Value,
    agent_ctx: &AgentContext,
    await_integration: bool,
) -> Result<(String, crate::entity_actor::EntityResponse), String> {
    let discovery_id = format!("intent-discovery-{}", sim_uuid());
    let response = state
        .dispatch_tenant_action_ext(
            tenant,
            "IntentDiscovery",
            &discovery_id,
            "Trigger",
            serde_json::json!({
                "reason": reason,
                "source": source,
                "trigger_context_json": trigger_context.to_string(),
            }),
            DispatchExtOptions {
                agent_ctx,
                await_integration,
            },
        )
        .await?;
    Ok((discovery_id, response))
}

pub(super) fn next_system_entity_id(prefix: &str) -> String {
    format!("{prefix}-{}", sim_uuid())
}

pub(super) fn emit_refresh_hints(state: &ServerState, hints: &[ObserveRefreshHint]) {
    for hint in hints {
        let _ = state.observe_refresh_tx.send(hint.clone());
    }
}
