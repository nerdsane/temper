use std::sync::OnceLock;
use std::time::Instant;

use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use tokio::spawn as spawn_catalog_shadow_check; // determinism-ok: production-only projection drift sampling task

use crate::state::ServerState;
use crate::storage::EntityCatalogRow;

fn catalog_shadow_read_sample_every() -> usize {
    static SAMPLE_EVERY: OnceLock<usize> = OnceLock::new();
    *SAMPLE_EVERY
        .get_or_init(|| super::config::env_usize("TEMPER_ODATA_CATALOG_SHADOW_READ_EVERY", 0))
}

fn catalog_shadow_read_max_per_response() -> usize {
    static MAX_PER_RESPONSE: OnceLock<usize> = OnceLock::new();
    *MAX_PER_RESPONSE.get_or_init(|| {
        super::config::env_usize("TEMPER_ODATA_CATALOG_SHADOW_READ_MAX_PER_RESPONSE", 2)
    })
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
    should_shadow_check_catalog_row_with_sample_every(tenant, entity_type, entity_id, sample_every)
}

fn should_shadow_check_catalog_row_with_sample_every(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    sample_every: usize,
) -> bool {
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
    actor_state: &serde_json::Value,
    actor_sequence: u64,
) -> &'static str {
    let status_drift = catalog.status != actor_status;
    let fields_drift = catalog.fields != *actor_fields;
    let state_drift = catalog
        .state
        .as_ref()
        .is_some_and(|catalog_state| catalog_state != actor_state);
    let sequence_drift = catalog.sequence_nr != actor_sequence;
    match (status_drift, fields_drift, state_drift, sequence_drift) {
        (false, false, false, false) => "none",
        (true, false, false, false) => "status",
        (false, true, false, false) => "fields",
        (false, false, true, false) => "state",
        (false, false, false, true) => "sequence",
        _ => "multiple",
    }
}

pub(super) struct CatalogShadowReadBudget {
    configured: usize,
    remaining: usize,
    scheduled: usize,
}

impl CatalogShadowReadBudget {
    pub(super) fn for_entity_set() -> Self {
        Self::new(catalog_shadow_read_max_per_response())
    }

    fn new(configured: usize) -> Self {
        Self {
            configured,
            remaining: configured,
            scheduled: 0,
        }
    }

    pub(super) fn configured(&self) -> usize {
        self.configured
    }

    pub(super) fn scheduled(&self) -> usize {
        self.scheduled
    }

    fn take_if_eligible_with_sample_every(
        &mut self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        sample_every: usize,
    ) -> bool {
        if self.remaining == 0
            || !should_shadow_check_catalog_row_with_sample_every(
                tenant,
                entity_type,
                entity_id,
                sample_every,
            )
        {
            return false;
        }

        self.remaining -= 1;
        self.scheduled += 1;
        true
    }

    fn take_if_eligible(&mut self, tenant: &TenantId, entity_type: &str, entity_id: &str) -> bool {
        self.take_if_eligible_with_sample_every(
            tenant,
            entity_type,
            entity_id,
            catalog_shadow_read_sample_every(),
        )
    }
}

pub(super) fn maybe_spawn_catalog_shadow_check(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    row: &EntityCatalogRow,
) -> bool {
    if !should_shadow_check_catalog_row(tenant, entity_type, &row.entity_id) {
        return false;
    }

    spawn_catalog_shadow_check_for_row(state, tenant, entity_type, row);
    true
}

pub(super) fn maybe_spawn_catalog_shadow_check_with_budget(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    row: &EntityCatalogRow,
    budget: &mut CatalogShadowReadBudget,
) -> bool {
    if !budget.take_if_eligible(tenant, entity_type, &row.entity_id) {
        return false;
    }

    spawn_catalog_shadow_check_for_row(state, tenant, entity_type, row);
    true
}

fn spawn_catalog_shadow_check_for_row(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    row: &EntityCatalogRow,
) {
    let state = state.clone();
    let tenant = tenant.clone();
    let entity_type = entity_type.to_string();
    let catalog = row.clone();
    spawn_catalog_shadow_check(async move {
        let started_at = Instant::now(); // determinism-ok: production-only sampled projection parity metric
        let result = crate::application_data::GovernedApplicationDataService::new(&state)
            .get(&tenant, &entity_type, &catalog.entity_id)
            .await;
        match result {
            Ok(response) => {
                let actor_fields =
                    state.query_projection_fields(&tenant, &entity_type, &response.state.fields);
                let actor_state = state.query_projection_state(&response.state);
                let drift_kind = projection_drift_kind(
                    &catalog,
                    &response.state.status,
                    &actor_fields,
                    &actor_state,
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
    use super::{
        CatalogShadowReadBudget, projection_drift_kind, shadow_sequence_gap,
        stable_shadow_sample_hash,
    };
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
            state: None,
            sequence_nr: 7,
        };
        let actor_state = serde_json::json!({
            "entity_type": "File",
            "entity_id": "file-1",
            "status": "Ready",
            "fields": {"Name": "alpha"},
            "events": [],
            "sequence_nr": 7
        });

        assert_eq!(
            projection_drift_kind(
                &catalog,
                "Ready",
                &serde_json::json!({"Name": "alpha"}),
                &actor_state,
                7,
            ),
            "none"
        );
        assert_eq!(
            projection_drift_kind(
                &catalog,
                "Archived",
                &serde_json::json!({"Name": "alpha"}),
                &actor_state,
                7,
            ),
            "status"
        );
        assert_eq!(
            projection_drift_kind(
                &catalog,
                "Ready",
                &serde_json::json!({"Name": "beta"}),
                &actor_state,
                7,
            ),
            "fields"
        );
        assert_eq!(
            projection_drift_kind(
                &catalog,
                "Ready",
                &serde_json::json!({"Name": "alpha"}),
                &actor_state,
                8,
            ),
            "sequence"
        );
        assert_eq!(
            projection_drift_kind(
                &catalog,
                "Archived",
                &serde_json::json!({"Name": "beta"}),
                &actor_state,
                8,
            ),
            "multiple"
        );

        let mut state_catalog = catalog.clone();
        state_catalog.state = Some(serde_json::json!({
            "entity_type": "File",
            "entity_id": "file-1",
            "status": "Ready",
            "fields": {"Name": "alpha"},
            "counters": {"Views": 41},
            "events": [],
            "sequence_nr": 7
        }));
        assert_eq!(
            projection_drift_kind(
                &state_catalog,
                "Ready",
                &serde_json::json!({"Name": "alpha"}),
                &serde_json::json!({
                    "entity_type": "File",
                    "entity_id": "file-1",
                    "status": "Ready",
                    "fields": {"Name": "alpha"},
                    "counters": {"Views": 42},
                    "events": [],
                    "sequence_nr": 7
                }),
                7,
            ),
            "state"
        );
    }

    #[test]
    fn shadow_sequence_gap_reports_direction_and_distance() {
        assert_eq!(shadow_sequence_gap(4, 9), ("catalog_behind", 5));
        assert_eq!(shadow_sequence_gap(9, 4), ("catalog_ahead", 5));
        assert_eq!(shadow_sequence_gap(7, 7), ("equal", 0));
    }

    #[test]
    fn catalog_shadow_budget_caps_eligible_rows_per_response() {
        let tenant = TenantId::from("tenant-a");
        let mut budget = CatalogShadowReadBudget::new(2);

        for index in 0..10 {
            let scheduled = budget.take_if_eligible_with_sample_every(
                &tenant,
                "Taxonomy",
                &format!("tax-{index}"),
                1,
            );
            assert_eq!(scheduled, index < 2);
        }

        assert_eq!(budget.configured(), 2);
        assert_eq!(budget.scheduled(), 2);
    }

    #[test]
    fn catalog_shadow_budget_preserves_disabled_sampling() {
        let tenant = TenantId::from("tenant-a");
        let mut budget = CatalogShadowReadBudget::new(2);

        assert!(!budget.take_if_eligible_with_sample_every(&tenant, "Taxonomy", "tax-1", 0));
        assert_eq!(budget.scheduled(), 0);
    }
}
