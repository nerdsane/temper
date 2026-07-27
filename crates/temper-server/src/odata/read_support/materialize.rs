//! Journal-aligned OData entity materialization.

use std::collections::BTreeMap;

use futures_util::stream::{self, StreamExt};
use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;

use super::config::entity_set_materialization_concurrency;
use super::select_projection::catalog_row_to_selected_entity_body;
use super::shadow::{CatalogShadowReadBudget, maybe_spawn_catalog_shadow_check_with_budget};
use super::{
    catalog_row_to_entity_body, load_actor_state_at_least, should_read_catalog_for_materialization,
    try_load_catalog_rows, try_load_selected_catalog_rows,
};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::state::ServerState;
use crate::storage::EntityCatalogRow;

pub(in crate::odata) async fn materialize_entity_set_entities(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    entity_ids: &[String],
    prefer_catalog: bool,
    selected_catalog_fields: Option<&[String]>,
) -> Result<MaterializedEntitySet, PersistenceError> {
    let journal_sequences = state
        .live_journal_candidate_sequences(tenant, entity_type, entity_ids)
        .await?;
    let live_entity_ids = journal_sequences.as_ref().map_or_else(
        || entity_ids.to_vec(),
        |sequences| {
            entity_ids
                .iter()
                .filter(|entity_id| sequences.contains_key(*entity_id))
                .cloned()
                .collect()
        },
    );
    let selected_catalog_fields_owned = selected_catalog_fields.map(Vec::from);
    let mut catalog_hits: BTreeMap<String, EntityCatalogRow> =
        if should_read_catalog_for_materialization(prefer_catalog) {
            match selected_catalog_fields {
                Some(select) => {
                    try_load_selected_catalog_rows(
                        state,
                        tenant,
                        entity_type,
                        &live_entity_ids,
                        select,
                    )
                    .await
                }
                None => try_load_catalog_rows(state, tenant, entity_type, &live_entity_ids).await,
            }
        } else {
            BTreeMap::new()
        };
    if let Some(sequences) = journal_sequences.as_ref() {
        catalog_hits
            .retain(|entity_id, row| sequences.get(entity_id).copied() == Some(row.sequence_nr));
    }
    let candidates_have_journal_proof = journal_sequences.is_some();
    let mut shadow_budget = CatalogShadowReadBudget::for_entity_set();

    let concurrency = entity_set_materialization_concurrency();
    let entities = stream::iter(live_entity_ids)
        .map(|id| {
            let catalog_row = catalog_hits.remove(&id);
            let expected_sequence = journal_sequences
                .as_ref()
                .and_then(|sequences| sequences.get(&id))
                .copied();
            if selected_catalog_fields_owned.is_none()
                && let Some(row) = catalog_row.as_ref()
            {
                let _ = maybe_spawn_catalog_shadow_check_with_budget(
                    state,
                    tenant,
                    entity_type,
                    row,
                    &mut shadow_budget,
                );
            }
            let state = state.clone();
            let tenant = tenant.clone();
            let entity_type = entity_type.to_string();
            let entity_set_name = entity_set_name.to_string();
            let selected_catalog_fields = selected_catalog_fields_owned.clone();
            async move {
                if let Some(row) = catalog_row {
                    let mut entity = match selected_catalog_fields.as_deref() {
                        Some(select) => catalog_row_to_selected_entity_body(
                            &entity_type,
                            &entity_set_name,
                            row,
                            select,
                        ),
                        None => catalog_row_to_entity_body(&entity_type, &entity_set_name, row),
                    };
                    hydrate_blob_refs_for_tenant(&state, &tenant, &mut entity).await;
                    return Ok(Some(entity));
                }
                // A durable sequence map already proved this candidate live in
                // one bounded batch. Avoid a redundant per-entity tail probe;
                // the actor replay below is the authoritative fallback for a
                // stale/missing catalog row. Without a journal, retain the
                // in-memory existence gate.
                if !candidates_have_journal_proof
                    && !state.ensure_entity_loaded(&tenant, &entity_type, &id).await
                {
                    return Ok(None);
                }
                let response = load_actor_state_at_least(
                    &state,
                    &tenant,
                    &entity_type,
                    &id,
                    expected_sequence,
                )
                .await?;
                if response.state.status == "Deleted" {
                    return Ok(None);
                }
                if let Some(query_plane) = state.query_plane_store() {
                    let fields = state.query_projection_fields(
                        &tenant,
                        &entity_type,
                        &response.state.fields,
                    );
                    let projected_state = state.query_projection_state(&response.state);
                    if let Err(error) = query_plane
                        .upsert_projection(
                            tenant.as_str(),
                            &entity_type,
                            &id,
                            &response.state.status,
                            &fields,
                            &projected_state,
                            response.state.sequence_nr,
                        )
                        .await
                    {
                        tracing::debug!(
                            error = %error,
                            tenant = %tenant,
                            entity_type = %entity_type,
                            entity_id = %id,
                            "failed to repair query projection after actor materialization fallback"
                        );
                    }
                }
                let mut entity = serde_json::to_value(&response.state).unwrap_or_default();
                hydrate_blob_refs_for_tenant(&state, &tenant, &mut entity).await;
                if let Some(obj) = entity.as_object_mut() {
                    obj.insert(
                        "@odata.id".into(),
                        serde_json::json!(format!("{entity_set_name}('{id}')")),
                    );
                }
                Ok(Some(entity))
            }
        })
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, PersistenceError>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    Ok(MaterializedEntitySet {
        entities,
        catalog_shadow_check_budget: shadow_budget.configured(),
        catalog_shadow_check_scheduled: shadow_budget.scheduled(),
    })
}

pub(in crate::odata) struct MaterializedEntitySet {
    pub(in crate::odata) entities: Vec<serde_json::Value>,
    pub(in crate::odata) catalog_shadow_check_budget: usize,
    pub(in crate::odata) catalog_shadow_check_scheduled: usize,
}
