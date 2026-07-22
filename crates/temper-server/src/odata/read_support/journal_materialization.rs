use super::*;
use crate::entity_actor::{
    EntityRecoveryContext, EntityState, recover_entity_state_from_stable_sources,
};
use crate::state::source_fenced_projection::repair_projection_from_stable_source;

const MAX_STABLE_JOURNAL_READ_ATTEMPTS: usize = 3;

enum EntityMaterializationSource {
    Absent,
    Actor {
        repair_projection: bool,
    },
    State {
        state: Box<EntityState>,
        repair_projection: bool,
    },
}

/// Result of closing a direct entity read against its durable sources.
pub(in crate::odata) enum ExactEntityMaterialization {
    /// A stable journal, snapshot, or actor generation supplied the body.
    Present(serde_json::Value),
    /// A stable durable generation proves that the entity is terminal.
    NotFound,
    /// Neither journal nor snapshot exists, so ADR-0077 catalog-only migration
    /// compatibility may supply the body.
    CatalogCompatible,
}

/// Prove that neither durable source exists before permitting ADR-0077 catalog
/// compatibility. Journal and snapshot are both read twice so a source transition
/// cannot be flattened into catalog authority.
pub(in crate::odata) async fn durable_source_absent_for_catalog_materialization(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Result<bool, AuthoritativeMaterializationError> {
    let Some((store, _)) = state.event_journal() else {
        return Ok(false);
    };
    let persistence_id = format!("{}:{entity_type}:{entity_id}", tenant.as_str());
    for attempt in 1..=MAX_STABLE_JOURNAL_READ_ATTEMPTS {
        let before_journal = store.journal_boundary(&persistence_id).await;
        let before_snapshot = store.load_snapshot(&persistence_id).await;
        let after_journal = store.journal_boundary(&persistence_id).await;
        let after_snapshot = store.load_snapshot(&persistence_id).await;
        match (
            before_journal,
            before_snapshot,
            after_journal,
            after_snapshot,
        ) {
            (Ok(before_journal), Ok(before_snapshot), Ok(after_journal), Ok(after_snapshot))
                if before_journal == after_journal && before_snapshot == after_snapshot =>
            {
                return Ok(before_journal.latest_sequence == 0 && before_snapshot.is_none());
            }
            (Err(error), _, _, _)
            | (_, Err(error), _, _)
            | (_, _, Err(error), _)
            | (_, _, _, Err(error)) => {
                tracing::warn!(
                    error = %error,
                    attempt,
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "failed to fence catalog compatibility against durable entity sources"
                );
            }
            _ => {
                tracing::debug!(
                    attempt,
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "durable entity sources changed while proving catalog compatibility"
                );
            }
        }
    }
    Err(AuthoritativeMaterializationError::JournalUnstable)
}

/// Materialize one direct entity read from its authoritative source before the
/// asynchronous catalog is considered. A terminal durable generation is kept
/// distinct from true source absence so a stale catalog row cannot resurrect it.
pub(in crate::odata) async fn materialize_exact_entity(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    entity_id: &str,
) -> Result<ExactEntityMaterialization, AuthoritativeMaterializationError> {
    let source = stable_journal_state(state, tenant, entity_type, entity_id).await?;
    let (entity_state, repair_projection) = match source {
        EntityMaterializationSource::Absent => {
            return Ok(ExactEntityMaterialization::CatalogCompatible);
        }
        EntityMaterializationSource::Actor { repair_projection } => {
            let response = state
                .get_tenant_entity_state(tenant, entity_type, entity_id)
                .await
                .map_err(|error| {
                    tracing::debug!(
                        error = %error,
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        "failed authoritative actor materialization for direct OData read"
                    );
                    AuthoritativeMaterializationError::JournalUnstable
                })?;
            (response.state, repair_projection)
        }
        EntityMaterializationSource::State {
            state,
            repair_projection,
        } => (*state, repair_projection),
    };

    Ok(
        match materialize_state(
            state,
            tenant,
            entity_type,
            entity_set_name,
            entity_id,
            entity_state,
            repair_projection,
        )
        .await
        {
            Some(body) => ExactEntityMaterialization::Present(body),
            None => ExactEntityMaterialization::NotFound,
        },
    )
}

/// Materialize one collection candidate from the source allowed by `catalog_policy`.
pub(super) async fn materialize_entity(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    entity_id: &str,
    catalog_policy: CatalogMaterializationPolicy,
) -> Result<Option<serde_json::Value>, AuthoritativeMaterializationError> {
    let source = match catalog_policy {
        CatalogMaterializationPolicy::Any => EntityMaterializationSource::Actor {
            repair_projection: true,
        },
        CatalogMaterializationPolicy::JournalAbsentOnly => {
            stable_journal_state(state, tenant, entity_type, entity_id).await?
        }
    };
    let (entity_state, repair_projection) = match source {
        EntityMaterializationSource::Absent => return Ok(None),
        EntityMaterializationSource::Actor { repair_projection } => match state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            Ok(response) => (response.state, repair_projection),
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "failed to materialize entity for OData collection"
                );
                return match catalog_policy {
                    CatalogMaterializationPolicy::Any => Ok(None),
                    CatalogMaterializationPolicy::JournalAbsentOnly => {
                        Err(AuthoritativeMaterializationError::JournalUnstable)
                    }
                };
            }
        },
        EntityMaterializationSource::State {
            state,
            repair_projection,
        } => (*state, repair_projection),
    };

    Ok(materialize_state(
        state,
        tenant,
        entity_type,
        entity_set_name,
        entity_id,
        entity_state,
        repair_projection,
    )
    .await)
}

async fn stable_journal_state(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Result<EntityMaterializationSource, AuthoritativeMaterializationError> {
    let Some((store, backend)) = state.event_journal() else {
        return Ok(EntityMaterializationSource::Actor {
            repair_projection: true,
        });
    };
    let persistence_id = format!("{}:{entity_type}:{entity_id}", tenant.as_str());
    let table = {
        let registry = state.registry.read().expect("registry lock poisoned");
        registry
            .get_table_live(tenant, entity_type)
            .map(|table| table.read().expect("table lock poisoned").clone())
    }
    .or_else(|| {
        state
            .transition_tables
            .get(entity_type)
            .map(|table| (**table).clone())
    });
    let Some(table) = table else {
        tracing::warn!(
            tenant = %tenant,
            entity_type,
            entity_id,
            "missing transition table for exact-key journal materialization"
        );
        return Err(AuthoritativeMaterializationError::JournalUnstable);
    };
    let blob_store = state.blob_store_for_tenant(tenant).ok();

    for attempt in 1..=MAX_STABLE_JOURNAL_READ_ATTEMPTS {
        let source = match recover_entity_state_from_stable_sources(EntityRecoveryContext {
            tenant: tenant.as_str(),
            entity_type,
            entity_id,
            table: &table,
            store: &store,
            backend,
            initial_fields: &serde_json::json!({}),
            blob_store: blob_store.as_ref(),
        })
        .await
        {
            Ok(recovered) => recovered,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    attempt,
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "failed strict replay for exact-key fallback materialization"
                );
                continue;
            }
        };
        if source.state.is_none() {
            return Ok(EntityMaterializationSource::Absent);
        }
        if source.journal_sequence == 0 {
            return Ok(EntityMaterializationSource::State {
                state: Box::new(
                    source
                        .state
                        .expect("snapshot-only source was validated as present"),
                ),
                repair_projection: false,
            });
        }
        match repair_projection_from_stable_source(
            state,
            tenant,
            entity_type,
            entity_id,
            &store,
            &persistence_id,
            &source,
        )
        .await
        {
            Ok(true) => {
                return Ok(EntityMaterializationSource::State {
                    state: Box::new(
                        source
                            .state
                            .expect("journal source was validated as present"),
                    ),
                    // Repair and its closing source fence completed together.
                    // Repeating it after this return would reopen the race.
                    repair_projection: false,
                });
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    attempt,
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    "source-fenced exact-key projection repair failed"
                );
                continue;
            }
        }
        tracing::debug!(
            attempt,
            tenant = %tenant,
            entity_type,
            entity_id,
            "durable source advanced during exact-key fallback materialization; retrying"
        );
    }

    tracing::warn!(
        attempts = MAX_STABLE_JOURNAL_READ_ATTEMPTS,
        tenant = %tenant,
        entity_type,
        entity_id,
        "exact-key fallback materialization could not obtain a stable journal generation"
    );
    Err(AuthoritativeMaterializationError::JournalUnstable)
}

async fn materialize_state(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_set_name: &str,
    entity_id: &str,
    entity_state: EntityState,
    repair_projection: bool,
) -> Option<serde_json::Value> {
    if repair_projection {
        repair_projection_for_state(state, tenant, entity_type, entity_id, &entity_state).await;
    }
    if entity_state.status == "Deleted" {
        return None;
    }
    let mut entity = serde_json::to_value(entity_state).unwrap_or_default();
    hydrate_blob_refs_for_tenant(state, tenant, &mut entity).await;
    if let Some(object) = entity.as_object_mut() {
        object.insert(
            "@odata.id".into(),
            serde_json::json!(format!("{entity_set_name}('{entity_id}')")),
        );
    }
    Some(entity)
}

async fn repair_projection_for_state(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    entity_state: &EntityState,
) {
    if entity_state.status == "Deleted" {
        remove_deleted_projection(state, tenant, entity_type, entity_id).await;
        return;
    }
    if let Some(query_plane) = state.query_plane_store() {
        let fields = state.query_projection_fields(tenant, entity_type, &entity_state.fields);
        let projected_state = state.query_projection_state(entity_state);
        if let Err(error) = query_plane
            .upsert_projection(
                tenant.as_str(),
                entity_type,
                entity_id,
                &entity_state.status,
                &fields,
                &projected_state,
                entity_state.sequence_nr,
            )
            .await
        {
            tracing::debug!(
                error = %error,
                tenant = %tenant,
                entity_type,
                entity_id,
                "failed to repair query projection after authoritative materialization fallback"
            );
        }
    }
}
