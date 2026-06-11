use axum::http::StatusCode;
use temper_evolution::InsightRecord;
use temper_evolution::RecordHeader;
use temper_evolution::RecordType;
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;

use crate::request_context::AgentContext;
use crate::sentinel;
use crate::state::{DispatchExtOptions, ObserveRefreshHint, ServerState};

/// Serialize a value to JSON, logging a warning and returning `"{}"` on failure.
///
/// Used for embedding sub-payloads in entity action params where a
/// serialization failure must not abort the surrounding persistence flow.
pub(super) fn serialize_or_empty<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|error| {
        tracing::warn!(error = %error, "evolution.serialize_or_empty");
        "{}".to_string()
    })
}

pub(super) async fn persist_evolution_record(
    state: &ServerState,
    record_id: &str,
    record_type: &str,
    status: &str,
    created_by: &str,
    derived_from: Option<&str>,
    data_json: &str,
) -> Result<(), String> {
    let Some(store) = state.platform_metadata_store() else {
        tracing::debug!(
            record_id,
            record_type,
            status,
            created_by,
            "evolution.store.unavailable"
        );
        return Ok(());
    };

    store
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
                backend = store.backend_name(),
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
            &AgentContext::for_service("evolution-engine"),
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
) -> bool {
    match dispatch_system_action(state, entity_type, entity_id, action, params).await {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(
                error = %error,
                entity_type,
                entity_id,
                action,
                "failed to create system entity"
            );
            false
        }
    }
}

/// How a system-entity create dispatch failure is treated by
/// [`persist_record_with_entity`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EntityDispatchMode {
    /// Dispatch failure fails the call with 500 — for chain entities that
    /// later steps depend on (Problem, Analysis).
    Required,
    /// Dispatch failure is logged and reported via `entity_synced` only.
    BestEffort,
}

/// Map a record type to its temper-system entity mirror:
/// (entity type, entity-id prefix, create action).
fn system_entity_parts(record_type: RecordType) -> (&'static str, &'static str, &'static str) {
    match record_type {
        RecordType::Observation => ("Observation", "OBS", "CreateObservation"),
        RecordType::Problem => ("Problem", "PRB", "CreateProblem"),
        RecordType::Analysis => ("Analysis", "ANL", "CreateAnalysis"),
        RecordType::Insight => ("Insight", "INS", "CreateInsight"),
        RecordType::Decision => ("EvolutionDecision", "ED", "CreateDecision"),
        RecordType::FeatureRequest => ("FeatureRequest", "FR", "CreateFeatureRequest"),
    }
}

/// Outcome of [`persist_record_with_entity`].
pub(super) struct PersistedRecordEntity {
    /// Minted system entity id (e.g. "OBS-<uuid>").
    pub entity_id: String,
    /// Whether the entity create action dispatched successfully.
    pub entity_synced: bool,
}

/// Persist an evolution record and mirror it as a temper-system entity.
///
/// This is the canonical `persist_record` → `next_system_entity_id` →
/// create-entity sequence shared by sentinel alert persistence, insight
/// persistence, and intent-discovery materialization. The entity type,
/// entity-id prefix, and create action are derived from the record type.
/// Returns `Err` when the durable record write fails, or — in
/// [`EntityDispatchMode::Required`] — when the entity create dispatch fails.
pub(super) async fn persist_record_with_entity<T: serde::Serialize>(
    state: &ServerState,
    header: &RecordHeader,
    record: &T,
    create_params: serde_json::Value,
    mode: EntityDispatchMode,
) -> Result<PersistedRecordEntity, StatusCode> {
    let (entity_type, prefix, create_action) = system_entity_parts(header.record_type);
    persist_record(state, entity_type, header, record).await?;

    let entity_id = next_system_entity_id(prefix);
    let entity_synced = match mode {
        EntityDispatchMode::Required => {
            dispatch_system_action_required(
                state,
                entity_type,
                &entity_id,
                create_action,
                create_params,
            )
            .await?;
            true
        }
        EntityDispatchMode::BestEffort => {
            create_system_entity_logged(
                state,
                entity_type,
                &entity_id,
                create_action,
                create_params,
            )
            .await
        }
    };

    Ok(PersistedRecordEntity {
        entity_id,
        entity_synced,
    })
}

/// Result of persisting a batch of evolution records.
///
/// Batch persistence is deliberately log-and-continue: one failed record
/// must not abort the remaining items. Failures are logged by the persist
/// path, counted here, and reported alongside the per-item payloads.
pub(super) struct BatchPersistReport {
    /// Per-item JSON payloads for successfully persisted records.
    pub items: Vec<serde_json::Value>,
    /// Number of records persisted successfully.
    pub persisted: usize,
    /// Number of records that failed to persist.
    pub failed: usize,
}

/// Persist sentinel alert O-Records and mirror them as system entities.
///
/// Log-and-continue: a failed record is counted in the report instead of
/// aborting the batch (previously the first failure aborted all remaining
/// alerts with a 500).
pub(super) async fn persist_alerts(
    state: &ServerState,
    alerts: &[sentinel::SentinelAlert],
) -> BatchPersistReport {
    let mut report = BatchPersistReport {
        items: Vec::new(),
        persisted: 0,
        failed: 0,
    };
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

        let outcome = persist_record_with_entity(
            state,
            &alert.record.header,
            &alert.record,
            serde_json::json!({
                "source": alert.record.source,
                "classification": format!("{:?}", alert.record.classification),
                "evidence_query": alert.record.evidence_query,
                "context": serialize_or_empty(&alert.record.context),
                "tenant": "temper-system",
                "legacy_record_id": alert.record.header.id,
            }),
            EntityDispatchMode::BestEffort,
        )
        .await;
        let Ok(entity) = outcome else {
            report.failed += 1;
            continue;
        };

        report.persisted += 1;
        report.items.push(serde_json::json!({
            "rule": alert.rule_name,
            "record_id": alert.record.header.id,
            "entity_id": entity.entity_id,
            "entity_synced": entity.entity_synced,
            "source": alert.record.source,
            "classification": alert.record.classification,
            "threshold": alert.record.threshold_value,
            "observed": alert.record.observed_value,
        }));
    }
    report
}

/// Persist generated I-Records and mirror them as system entities.
///
/// Log-and-continue: a failed record is counted in the report instead of
/// being silently reported as a success item (previously a failed persist
/// still produced an entity and a result row).
pub(super) async fn persist_insights(
    state: &ServerState,
    insights: &[InsightRecord],
) -> BatchPersistReport {
    let mut report = BatchPersistReport {
        items: Vec::new(),
        persisted: 0,
        failed: 0,
    };
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

        let outcome = persist_record_with_entity(
            state,
            &insight.header,
            insight,
            serde_json::json!({
                "observation_id": "",
                "category": format!("{:?}", insight.category),
                "signal": insight.signal.intent,
                "recommendation": insight.recommendation,
                "priority_score": format!("{:.4}", insight.priority_score),
                "legacy_record_id": insight.header.id,
            }),
            EntityDispatchMode::BestEffort,
        )
        .await;
        let Ok(entity) = outcome else {
            report.failed += 1;
            continue;
        };

        report.persisted += 1;
        report.items.push(serde_json::json!({
            "record_id": insight.header.id,
            "entity_id": entity.entity_id,
            "entity_synced": entity.entity_synced,
            "category": format!("{:?}", insight.category),
            "intent": insight.signal.intent,
            "priority_score": insight.priority_score,
            "recommendation": insight.recommendation,
        }));
    }
    report
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
                await_reactions: true,
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
