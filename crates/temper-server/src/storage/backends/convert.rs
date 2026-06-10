//! Row converters: Postgres result rows -> Turso-shaped row types.

use super::*;

pub(super) fn pg_trajectory_to_turso(
    row: temper_store_postgres::PostgresTrajectoryRow,
) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        tenant: row.tenant,
        entity_type: row.entity_type,
        entity_id: row.entity_id,
        action: row.action,
        success: row.success,
        from_status: row.from_status,
        to_status: row.to_status,
        error: row.error,
        agent_id: row.agent_id,
        session_id: row.session_id,
        authz_denied: row.authz_denied,
        denied_resource: row.denied_resource,
        denied_module: row.denied_module,
        source: row.source,
        spec_governed: row.spec_governed,
        created_at: row.created_at,
        request_body: row.request_body,
        intent: row.intent,
        matched_policy_ids: row.matched_policy_ids,
    }
}

pub(super) fn pg_unmet_to_turso(
    row: temper_store_postgres::PostgresUnmetIntentAggRow,
) -> UnmetIntentAggRow {
    UnmetIntentAggRow {
        entity_type: row.entity_type,
        action: row.action,
        error: row.error,
        count: row.count,
        first_seen: row.first_seen,
        last_seen: row.last_seen,
    }
}

pub(super) fn pg_stats_to_turso(
    stats: temper_store_postgres::PostgresTrajectoryStats,
) -> TrajectoryStats {
    TrajectoryStats {
        total: stats.total,
        success_count: stats.success_count,
        error_count: stats.error_count,
        success_rate: stats.success_rate,
        by_action: stats
            .by_action
            .into_iter()
            .map(|(name, action)| {
                (
                    name,
                    ActionStats {
                        total: action.total,
                        success: action.success,
                        error: action.error,
                    },
                )
            })
            .collect(),
        failed_intents: stats
            .failed_intents
            .into_iter()
            .map(pg_trajectory_to_turso)
            .collect(),
    }
}

pub(super) fn pg_agent_summary_to_turso(
    row: temper_store_postgres::PostgresAgentSummary,
) -> AgentSummary {
    AgentSummary {
        agent_id: row.agent_id,
        total_actions: row.total_actions,
        success_count: row.success_count,
        error_count: row.error_count,
        denial_count: row.denial_count,
        success_rate: row.success_rate,
        last_active_at: row.last_active_at,
    }
}

pub(super) fn pg_feature_request_to_turso(
    row: temper_store_postgres::PostgresFeatureRequestRow,
) -> FeatureRequestRow {
    FeatureRequestRow {
        id: row.id,
        category: row.category,
        description: row.description,
        frequency: row.frequency,
        trajectory_refs: row.trajectory_refs,
        disposition: row.disposition,
        developer_notes: row.developer_notes,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub(super) fn pg_evolution_record_to_turso(
    row: temper_store_postgres::PostgresEvolutionRecordRow,
) -> EvolutionRecordRow {
    EvolutionRecordRow {
        id: row.id,
        record_type: row.record_type,
        status: row.status,
        created_by: row.created_by,
        derived_from: row.derived_from,
        data: row.data,
        timestamp: row.timestamp,
    }
}

pub(super) fn pg_design_time_event_to_turso(
    row: temper_store_postgres::PostgresDesignTimeEventRow,
) -> DesignTimeEventRow {
    DesignTimeEventRow {
        id: row.id,
        kind: row.kind,
        entity_type: row.entity_type,
        tenant: row.tenant,
        summary: row.summary,
        level: row.level,
        passed: row.passed,
        step_number: row.step_number,
        total_steps: row.total_steps,
        created_at: row.created_at,
    }
}

pub(super) fn pg_ots_to_turso(
    row: temper_store_postgres::PostgresOtsTrajectoryRow,
) -> OtsTrajectoryRow {
    OtsTrajectoryRow {
        trajectory_id: row.trajectory_id,
        tenant: row.tenant,
        agent_id: row.agent_id,
        session_id: row.session_id,
        outcome: row.outcome,
        turn_count: row.turn_count,
        created_at: row.created_at,
    }
}

pub(super) fn pg_denial_pattern_to_turso(
    row: temper_store_postgres::PostgresPolicyDenialPatternRow,
) -> PolicyDenialPatternRow {
    PolicyDenialPatternRow {
        tenant: row.tenant,
        agent_type: row.agent_type,
        action: row.action,
        resource_type: row.resource_type,
        count: row.count,
        first_seen: row.first_seen,
        last_seen: row.last_seen,
        distinct_resource_ids_json: row.distinct_resource_ids_json,
    }
}

pub(super) fn pg_wasm_metadata_to_turso(
    row: temper_store_postgres::PostgresWasmModuleMetadataRow,
) -> TursoWasmModuleMetadataRow {
    TursoWasmModuleMetadataRow {
        tenant: row.tenant,
        module_name: row.module_name,
        sha256_hash: row.sha256_hash,
        size_bytes: row.size_bytes,
        updated_at: row.updated_at,
    }
}

pub(super) fn pg_wasm_invocation_to_turso(
    row: temper_store_postgres::PostgresWasmInvocationRow,
) -> TursoWasmInvocationRow {
    TursoWasmInvocationRow {
        tenant: row.tenant,
        entity_type: row.entity_type,
        entity_id: row.entity_id,
        module_name: row.module_name,
        trigger_action: row.trigger_action,
        callback_action: row.callback_action,
        success: row.success,
        error: row.error,
        duration_ms: row.duration_ms,
        created_at: row.created_at,
    }
}
