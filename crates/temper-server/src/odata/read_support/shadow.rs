use std::sync::OnceLock;
use std::time::Instant;

use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use tokio::spawn as spawn_catalog_shadow_check; // determinism-ok: production-only projection drift sampling task

use crate::state::ServerState;
use crate::storage::EntityCatalogRow;

fn catalog_shadow_read_sample_every() -> usize {
    static SAMPLE_EVERY: OnceLock<usize> = OnceLock::new();
    *SAMPLE_EVERY.get_or_init(|| super::env_usize("TEMPER_ODATA_CATALOG_SHADOW_READ_EVERY", 0))
}

fn stable_shadow_sample_hash(tenant: &TenantId, entity_type: &str, entity_id: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(tenant.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(entity_type.as_bytes());
    hasher.update(b"\0");
    hasher.update(entity_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

fn should_shadow_check_catalog_row(tenant: &TenantId, entity_type: &str, entity_id: &str) -> bool {
    let sample_every = catalog_shadow_read_sample_every();
    sample_every > 0
        && stable_shadow_sample_hash(tenant, entity_type, entity_id)
            .is_multiple_of(sample_every as u64)
}

fn shadow_sequence_gap(catalog_sequence: u64, actor_sequence: u64) -> (&'static str, u64) {
    match catalog_sequence.cmp(&actor_sequence) {
        std::cmp::Ordering::Less => ("catalog_behind", actor_sequence - catalog_sequence),
        std::cmp::Ordering::Equal => ("equal", 0),
        std::cmp::Ordering::Greater => ("catalog_ahead", catalog_sequence - actor_sequence),
    }
}

fn projection_drift_kind(
    catalog: &EntityCatalogRow,
    actor_status: &str,
    actor_fields: &serde_json::Value,
    actor_sequence: u64,
) -> &'static str {
    let status_drift = catalog.status != actor_status;
    let fields_drift = catalog.fields != *actor_fields;
    let sequence_drift = catalog.sequence_nr != actor_sequence;
    match (status_drift, fields_drift, sequence_drift) {
        (false, false, false) => "none",
        (true, false, false) => "status",
        (false, true, false) => "fields",
        (false, false, true) => "sequence",
        _ => "multiple",
    }
}

pub(super) fn maybe_spawn_catalog_shadow_check(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    row: &EntityCatalogRow,
) {
    if !should_shadow_check_catalog_row(tenant, entity_type, &row.entity_id) {
        return;
    }

    let state = state.clone();
    let tenant = tenant.clone();
    let entity_type = entity_type.to_string();
    let catalog = row.clone();
    spawn_catalog_shadow_check(async move {
        let started_at = Instant::now(); // determinism-ok: production-only sampled projection parity metric
        let result = state
            .get_tenant_entity_state(&tenant, &entity_type, &catalog.entity_id)
            .await;
        match result {
            Ok(response) => {
                let actor_fields =
                    state.query_projection_fields(&tenant, &entity_type, &response.state.fields);
                let drift_kind = projection_drift_kind(
                    &catalog,
                    &response.state.status,
                    &actor_fields,
                    response.state.sequence_nr,
                );
                let (sequence_direction, sequence_gap) =
                    shadow_sequence_gap(catalog.sequence_nr, response.state.sequence_nr);
                let result = if drift_kind == "none" {
                    "match"
                } else {
                    "drift"
                };
                crate::query_projection_metrics::record_shadow_check(
                    tenant.as_str(),
                    &entity_type,
                    result,
                    drift_kind,
                    sequence_direction,
                    sequence_gap,
                    started_at.elapsed(),
                );
                if drift_kind != "none" {
                    tracing::warn!(
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %catalog.entity_id,
                        drift_kind,
                        sequence_direction,
                        sequence_gap,
                        catalog_sequence = catalog.sequence_nr,
                        actor_sequence = response.state.sequence_nr,
                        "catalog fast-read shadow check detected projection drift"
                    );
                }
            }
            Err(error) => {
                crate::query_projection_metrics::record_shadow_check(
                    tenant.as_str(),
                    &entity_type,
                    "error",
                    "actor_error",
                    "unknown",
                    0,
                    started_at.elapsed(),
                );
                tracing::debug!(
                    error = %error,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %catalog.entity_id,
                    "catalog fast-read shadow check could not load authoritative actor state"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{projection_drift_kind, shadow_sequence_gap, stable_shadow_sample_hash};
    use crate::storage::EntityCatalogRow;
    use temper_runtime::tenant::TenantId;

    #[test]
    fn catalog_shadow_sample_hash_is_stable() {
        let tenant = TenantId::from("tenant-a");
        let first = stable_shadow_sample_hash(&tenant, "File", "file-1");
        let second = stable_shadow_sample_hash(&tenant, "File", "file-1");
        let different = stable_shadow_sample_hash(&tenant, "File", "file-2");

        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn projection_drift_kind_identifies_status_fields_and_sequence() {
        let catalog = EntityCatalogRow {
            entity_id: "file-1".to_string(),
            status: "Ready".to_string(),
            fields: serde_json::json!({"Name": "alpha"}),
            sequence_nr: 7,
        };

        assert_eq!(
            projection_drift_kind(&catalog, "Ready", &serde_json::json!({"Name": "alpha"}), 7),
            "none"
        );
        assert_eq!(
            projection_drift_kind(
                &catalog,
                "Archived",
                &serde_json::json!({"Name": "alpha"}),
                7,
            ),
            "status"
        );
        assert_eq!(
            projection_drift_kind(&catalog, "Ready", &serde_json::json!({"Name": "beta"}), 7),
            "fields"
        );
        assert_eq!(
            projection_drift_kind(&catalog, "Ready", &serde_json::json!({"Name": "alpha"}), 8),
            "sequence"
        );
        assert_eq!(
            projection_drift_kind(
                &catalog,
                "Archived",
                &serde_json::json!({"Name": "beta"}),
                8,
            ),
            "multiple"
        );
    }

    #[test]
    fn shadow_sequence_gap_reports_direction_and_distance() {
        assert_eq!(shadow_sequence_gap(4, 9), ("catalog_behind", 5));
        assert_eq!(shadow_sequence_gap(9, 4), ("catalog_ahead", 5));
        assert_eq!(shadow_sequence_gap(7, 7), ("equal", 0));
    }
}
