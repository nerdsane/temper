use super::*;

pub(super) fn row_to_spec(row: sqlx::postgres::PgRow) -> PostgresSpecRow {
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    let verification_result: Option<serde_json::Value> = row.get("verification_result");
    PostgresSpecRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        ioa_source: row.get("ioa_source"),
        csdl_xml: row.get("csdl_xml"),
        verification_status: row.get("verification_status"),
        verified: row.get("verified"),
        levels_passed: row.get("levels_passed"),
        levels_total: row.get("levels_total"),
        verification_result: verification_result.map(|v| v.to_string()),
        content_hash: Some(row.get("content_hash")),
        updated_at: updated_at.to_rfc3339(),
        committed: row.get("committed"),
    }
}

pub(super) fn row_to_installed_app(row: sqlx::postgres::PgRow) -> PostgresInstalledAppRow {
    let installed_at: chrono::DateTime<chrono::Utc> = row.get("installed_at");
    let last_reconciled_at: Option<chrono::DateTime<chrono::Utc>> = row.get("last_reconciled_at");
    PostgresInstalledAppRow {
        tenant: row.get("tenant"),
        app_name: row.get("app_name"),
        source_kind: row.get("source_kind"),
        app_ref: row.get("app_ref"),
        version_hash: row.get("version_hash"),
        pinned_version_hash: row.get("pinned_version_hash"),
        current_version_hash: row.get("current_version_hash"),
        follow_policy: row.get("follow_policy"),
        closure_id: row.get("closure_id"),
        registry_url: row.get("registry_url"),
        registry_tenant: row.get("registry_tenant"),
        dependency_lock_digest: row.get("dependency_lock_digest"),
        app_version: row.get("app_version"),
        bundle_digest: row.get("bundle_digest"),
        spec_digest: row.get("spec_digest"),
        policy_digest: row.get("policy_digest"),
        wasm_digest: row.get("wasm_digest"),
        content_digest: row.get("content_digest"),
        seed_digest: row.get("seed_digest"),
        installed_at: installed_at.to_rfc3339(),
        last_reconciled_at: last_reconciled_at.map(|dt| dt.to_rfc3339()),
        status: row.get("status"),
    }
}

pub(super) fn row_to_wasm_module(row: sqlx::postgres::PgRow) -> PostgresWasmModuleRow {
    let source: Option<String> = row.try_get("source").ok();
    PostgresWasmModuleRow {
        tenant: row.get("tenant"),
        module_name: row.get("module_name"),
        wasm_bytes: row.get("wasm_bytes"),
        sha256_hash: row.get("sha256_hash"),
        source: source.unwrap_or_else(|| "bundled".to_string()),
    }
}

pub(super) fn row_to_wasm_module_metadata(
    row: sqlx::postgres::PgRow,
) -> PostgresWasmModuleMetadataRow {
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    PostgresWasmModuleMetadataRow {
        tenant: row.get("tenant"),
        module_name: row.get("module_name"),
        sha256_hash: row.get("sha256_hash"),
        size_bytes: row.get("size_bytes"),
        updated_at: updated_at.to_rfc3339(),
    }
}

pub(super) fn row_to_wasm_invocation(row: sqlx::postgres::PgRow) -> PostgresWasmInvocationRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let duration_ms: i64 = row.get("duration_ms");
    PostgresWasmInvocationRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        module_name: row.get("module_name"),
        trigger_action: row.get("trigger_action"),
        callback_action: row.get("callback_action"),
        success: row.get("success"),
        error: row.get("error"),
        duration_ms: duration_ms as u64,
        created_at: created_at.to_rfc3339(),
    }
}

pub(super) fn row_to_policy(row: sqlx::postgres::PgRow) -> PostgresPolicyRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    PostgresPolicyRow {
        tenant: row.get("tenant"),
        policy_id: row.get("policy_id"),
        cedar_text: row.get("cedar_text"),
        policy_hash: row.get("policy_hash"),
        created_at: created_at.to_rfc3339(),
        created_by: row.get("created_by"),
        enabled: row.get("enabled"),
    }
}

pub(super) fn row_to_policy_denial_pattern(
    row: sqlx::postgres::PgRow,
) -> PostgresPolicyDenialPatternRow {
    let first_seen: chrono::DateTime<chrono::Utc> = row.get("first_seen");
    let last_seen: chrono::DateTime<chrono::Utc> = row.get("last_seen");
    let agent_type_raw: String = row.get("agent_type");
    let distinct_resource_ids_json: serde_json::Value = row.get("distinct_resource_ids_json");
    PostgresPolicyDenialPatternRow {
        tenant: row.get("tenant"),
        agent_type: if agent_type_raw.is_empty() {
            None
        } else {
            Some(agent_type_raw)
        },
        action: row.get("action"),
        resource_type: row.get("resource_type"),
        count: row.get("count"),
        first_seen: first_seen.to_rfc3339(),
        last_seen: last_seen.to_rfc3339(),
        distinct_resource_ids_json: distinct_resource_ids_json.to_string(),
    }
}

pub(super) fn row_to_trajectory(row: sqlx::postgres::PgRow) -> PostgresTrajectoryRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let request_body: Option<serde_json::Value> = row.get("request_body");
    let matched_policy_ids: Option<serde_json::Value> = row.get("matched_policy_ids");
    PostgresTrajectoryRow {
        tenant: row.get("tenant"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        action: row.get("action"),
        success: row.get("success"),
        from_status: row.get("from_status"),
        to_status: row.get("to_status"),
        error: row.get("error"),
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        authz_denied: row.get("authz_denied"),
        denied_resource: row.get("denied_resource"),
        denied_module: row.get("denied_module"),
        source: row.get("source"),
        spec_governed: row.get("spec_governed"),
        created_at: created_at.to_rfc3339(),
        request_body: request_body.map(|value| value.to_string()),
        intent: row.get("intent"),
        matched_policy_ids: matched_policy_ids
            .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok()),
        capture_seq: row.try_get("capture_seq").ok().flatten(),
    }
}

pub(super) fn row_to_unmet_intent(row: sqlx::postgres::PgRow) -> PostgresUnmetIntentAggRow {
    let count: i64 = row.get("cnt");
    let first_seen: chrono::DateTime<chrono::Utc> = row.get("first_seen");
    let last_seen: chrono::DateTime<chrono::Utc> = row.get("last_seen");
    PostgresUnmetIntentAggRow {
        entity_type: row.get("entity_type"),
        action: row.get("action"),
        error: row.get("error"),
        count: count as u64,
        first_seen: first_seen.to_rfc3339(),
        last_seen: last_seen.to_rfc3339(),
    }
}

pub(super) fn row_to_agent_summary(row: sqlx::postgres::PgRow) -> PostgresAgentSummary {
    let total = row.get::<i64, _>("total_actions") as u64;
    let success = row.get::<i64, _>("success_count") as u64;
    let last_active_at: chrono::DateTime<chrono::Utc> = row.get("last_active_at");
    PostgresAgentSummary {
        agent_id: row.get("agent_id"),
        total_actions: total,
        success_count: success,
        error_count: row.get::<i64, _>("error_count") as u64,
        denial_count: row.get::<i64, _>("denial_count") as u64,
        success_rate: if total > 0 {
            success as f64 / total as f64
        } else {
            0.0
        },
        last_active_at: last_active_at.to_rfc3339(),
    }
}

pub(super) fn row_to_feature_request(row: sqlx::postgres::PgRow) -> PostgresFeatureRequestRow {
    let trajectory_refs: serde_json::Value = row.get("trajectory_refs");
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    PostgresFeatureRequestRow {
        id: row.get("id"),
        tenant: row.get("tenant"),
        category: row.get("category"),
        description: row.get("description"),
        frequency: row.get("frequency"),
        trajectory_refs: trajectory_refs.to_string(),
        disposition: row.get("disposition"),
        developer_notes: row.get("developer_notes"),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.to_rfc3339(),
    }
}

pub(super) fn row_to_evolution_record(row: sqlx::postgres::PgRow) -> PostgresEvolutionRecordRow {
    let payload: serde_json::Value = row.get("payload");
    let timestamp: chrono::DateTime<chrono::Utc> = row.get("timestamp");
    PostgresEvolutionRecordRow {
        id: row.get("id"),
        tenant: row.get("tenant"),
        record_type: row.get("record_type"),
        status: row.get("status"),
        created_by: row.get("created_by"),
        derived_from: row.get("derived_from"),
        data: payload.to_string(),
        timestamp: timestamp.to_rfc3339(),
    }
}

pub(super) fn row_to_design_time_event(row: sqlx::postgres::PgRow) -> PostgresDesignTimeEventRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let step_number: Option<i16> = row.get("step_number");
    let total_steps: Option<i16> = row.get("total_steps");
    PostgresDesignTimeEventRow {
        id: row.get("id"),
        kind: row.get("kind"),
        entity_type: row.get("entity_type"),
        tenant: row.get("tenant"),
        summary: row.get("summary"),
        level: row.get("level"),
        passed: row.get("passed"),
        step_number: step_number.map(i64::from),
        total_steps: total_steps.map(i64::from),
        created_at: created_at.to_rfc3339(),
    }
}

pub(super) fn row_to_ots_trajectory(row: sqlx::postgres::PgRow) -> PostgresOtsTrajectoryRow {
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    PostgresOtsTrajectoryRow {
        trajectory_id: row.get("trajectory_id"),
        tenant: row.get("tenant"),
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        outcome: row.get("outcome"),
        turn_count: row.get("turn_count"),
        persistence_status: row.get("persistence_status"),
        persist_attempts: row.get("persist_attempts"),
        last_error: row.get("last_error"),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.to_rfc3339(),
    }
}

pub(super) fn row_to_ots_document(
    row: sqlx::postgres::PgRow,
    tenant: String,
    trajectory_id: String,
) -> PostgresOtsTrajectoryDocument {
    let data: serde_json::Value = row.get("data");
    PostgresOtsTrajectoryDocument {
        trajectory_id,
        tenant,
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        outcome: row.get("outcome"),
        data: data.to_string(),
    }
}

pub(super) fn row_to_queued_ots_trajectory(
    row: sqlx::postgres::PgRow,
) -> PostgresQueuedOtsTrajectoryRow {
    let data: serde_json::Value = row.get("data");
    PostgresQueuedOtsTrajectoryRow {
        trajectory_id: row.get("trajectory_id"),
        tenant: row.get("tenant"),
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        outcome: row.get("outcome"),
        turn_count: row.get("turn_count"),
        data: data.to_string(),
        persist_attempts: row.get("persist_attempts"),
    }
}

pub(super) fn row_to_published_artifact(
    row: sqlx::postgres::PgRow,
) -> PostgresPublishedArtifactRow {
    PostgresPublishedArtifactRow {
        id: row.get("id"),
        tenant: row.get("tenant"),
        source_file_id: row.get("source_file_id"),
        source_file_version_id: row.get("source_file_version_id"),
        content_hash: row.get("content_hash"),
        label: row.get("label"),
        mime_type: row.get("mime_type"),
        byte_length: row.get("byte_length"),
        public_storage_key: row.get("public_storage_key"),
        public_url: row.get("public_url"),
        owner_ref_type: row.get("owner_ref_type"),
        owner_ref_id: row.get("owner_ref_id"),
        status: row.get("status"),
    }
}
