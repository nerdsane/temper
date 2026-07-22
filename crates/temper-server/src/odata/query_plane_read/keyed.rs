//! Linearizable exact declared-key candidate reads.

use super::super::authz::{READ_ACTION, authorize_read};
use super::scan::{ScanCounters, TelemetryInput, base_telemetry, query_options_with_default_page};
use super::types::{
    QueryPlaneCoverageReport, QueryPlaneFallbackReason, QueryPlaneReadAuthorization,
    QueryPlaneReadError, QueryPlaneReadRequest, QueryPlaneReadResult, QueryPlaneReadStrategy,
};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::entity_actor::{EntityRecoveryContext, recover_entity_state_from_stable_sources};
use crate::odata::read_support::{
    catalog_row_to_entity_body, durable_source_absent_for_catalog_materialization,
};
use crate::query_eval::apply_query_options;
use crate::storage::{CatalogRowsLoad, load_catalog_rows_by_id};
use temper_runtime::persistence::EntityKeyLookup;

const MAX_OWNERSHIP_READ_ATTEMPTS: usize = 3;

/// How an exact declared-key filter constrains the authoritative read source.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum KeyedCandidateResolution {
    /// The query is not an exact declared-key query.
    NotApplicable,
    /// The declared-key index is not yet complete for the current key-set
    /// signature, so neither a hit nor a miss may bound the candidate set.
    NeedsAuthoritativeScan,
    /// The complete declared-key index supplied a fenced ownership proof.
    Authoritative(KeyOwnershipProof),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct KeyOwnershipProof {
    key_name: String,
    key_hash: String,
    owner: Option<EntityKeyLookup>,
    contract_revision: u64,
}

impl KeyOwnershipProof {
    pub(super) fn owner(&self) -> Option<&str> {
        self.owner.as_ref().map(|owner| owner.entity_id.as_str())
    }

    pub(super) fn into_owner(self) -> Option<String> {
        self.owner.map(|owner| owner.entity_id)
    }
}

pub(super) enum FencedKeyedRead {
    Complete(QueryPlaneReadResult),
    NeedsAuthoritativeScan,
}

async fn materialize_owner_from_durable_sources(
    request: &QueryPlaneReadRequest<'_>,
    owner: &EntityKeyLookup,
    key_name: &str,
    key_hash: &str,
) -> Result<Option<serde_json::Value>, QueryPlaneReadError> {
    let Some((store, backend)) = request.state.event_journal() else {
        return Err(QueryPlaneReadError::KeyOwnershipUnstable);
    };
    let table = {
        let registry = request
            .state
            .registry
            .read()
            .expect("registry lock poisoned");
        registry
            .get_table_live(request.tenant, request.entity_type)
            .map(|table| table.read().expect("table lock poisoned").clone())
    }
    .or_else(|| {
        request
            .state
            .transition_tables
            .get(request.entity_type)
            .map(|table| (**table).clone())
    })
    .ok_or(QueryPlaneReadError::KeyOwnershipUnstable)?;
    let blob_store = request.state.blob_store_for_tenant(request.tenant).ok();
    let source = recover_entity_state_from_stable_sources(EntityRecoveryContext {
        tenant: request.tenant.as_str(),
        entity_type: request.entity_type,
        entity_id: &owner.entity_id,
        table: &table,
        store: &store,
        backend,
        initial_fields: &serde_json::json!({}),
        blob_store: blob_store.as_ref(),
    })
    .await
    .map_err(|_| QueryPlaneReadError::KeyOwnershipUnstable)?;
    let durable_sequence = source.durable_sequence();
    let Some(recovered) = source.state else {
        return materialize_catalog_only_owner(request, owner, key_name, key_hash).await;
    };

    // The key row and journal sequence are co-committed. Requiring equality
    // prevents a stale resident actor/body at N-1 from being certified by a
    // stable owner row at N, and detects an index that failed to reconcile a
    // later journal append.
    if durable_sequence != owner.sequence_nr || recovered.status == "Deleted" {
        return materialize_catalog_only_owner(request, owner, key_name, key_hash).await;
    }

    // Sequence equality is insufficient for snapshot-backed generations: an
    // imported snapshot may replace bytes at the same aggregate sequence while
    // a stale key row retains that number. Close the ownership proof against the
    // recovered fields as well, so a same-sequence source rewrite cannot certify
    // the stale indexed owner.
    let owns_requested_key =
        crate::key_index::derive_entity_key_rows(&table.keys, &recovered.fields, true)
            .iter()
            .any(|row| row.key_name == key_name && row.key_hash == key_hash);
    if !owns_requested_key {
        return Ok(None);
    }

    let mut body =
        serde_json::to_value(recovered).map_err(|_| QueryPlaneReadError::KeyOwnershipUnstable)?;
    hydrate_blob_refs_for_tenant(request.state, request.tenant, &mut body).await;
    if let Some(object) = body.as_object_mut() {
        object.insert(
            "@odata.id".to_string(),
            serde_json::json!(format!(
                "{}('{}')",
                request.entity_set_name, owner.entity_id
            )),
        );
    }
    Ok(Some(body))
}

/// Materialize an ADR-0077 migration owner whose catalog is durable but whose
/// journal and snapshot are genuinely absent. The exact catalog generation must
/// match the key row, and both durable sources are checked before and after
/// materialization. The surrounding ownership/revision re-read is the final
/// linearization fence.
async fn materialize_catalog_only_owner(
    request: &QueryPlaneReadRequest<'_>,
    owner: &EntityKeyLookup,
    key_name: &str,
    key_hash: &str,
) -> Result<Option<serde_json::Value>, QueryPlaneReadError> {
    if !durable_source_absent_for_catalog_materialization(
        request.state,
        request.tenant,
        request.entity_type,
        &owner.entity_id,
    )
    .await
    .map_err(|_| QueryPlaneReadError::KeyOwnershipUnstable)?
    {
        return Ok(None);
    }
    let Some(query_plane) = request.state.query_plane_store() else {
        return Ok(None);
    };
    let ids = [owner.entity_id.clone()];
    let rows = load_catalog_rows_by_id(
        &query_plane,
        request.tenant.as_str(),
        request.entity_type,
        &ids,
    )
    .await
    .map_err(|_| QueryPlaneReadError::KeyOwnershipUnstable)?;
    let CatalogRowsLoad::Available(mut rows) = rows else {
        return Ok(None);
    };
    let Some(row) = rows.remove(&owner.entity_id) else {
        return Ok(None);
    };
    if row.sequence_nr != owner.sequence_nr || row.status == "Deleted" {
        return Ok(None);
    }
    let keys = request
        .state
        .declared_keys_for(request.tenant, request.entity_type);
    let row_owns_requested_key = crate::key_index::derive_entity_key_rows(&keys, &row.fields, true)
        .iter()
        .any(|row| row.key_name == key_name && row.key_hash == key_hash);
    if !row_owns_requested_key {
        return Ok(None);
    }
    let mut body = catalog_row_to_entity_body(request.entity_type, request.entity_set_name, row);
    let body_owns_requested_key = body
        .get("fields")
        .map(|fields| crate::key_index::derive_entity_key_rows(&keys, fields, true))
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.key_name == key_name && row.key_hash == key_hash)
        });
    if !body_owns_requested_key {
        return Ok(None);
    }
    hydrate_blob_refs_for_tenant(request.state, request.tenant, &mut body).await;
    if !durable_source_absent_for_catalog_materialization(
        request.state,
        request.tenant,
        request.entity_type,
        &owner.entity_id,
    )
    .await
    .map_err(|_| QueryPlaneReadError::KeyOwnershipUnstable)?
    {
        return Ok(None);
    }
    Ok(Some(body))
}

fn finish_keyed_result(
    request: &QueryPlaneReadRequest<'_>,
    owner: Option<&EntityKeyLookup>,
    body: Option<serde_json::Value>,
    authorization: QueryPlaneReadAuthorization,
) -> QueryPlaneReadResult {
    let mut authorized = Vec::new();
    if let (Some(owner), Some(body)) = (owner, body)
        && (!authorization.enforces_caller()
            || authorize_read(
                request.state,
                request.tenant,
                request.security_ctx,
                READ_ACTION,
                request.entity_type,
                &owner.entity_id,
                &body,
            )
            .is_ok())
    {
        authorized.push(body);
    }
    let options = query_options_with_default_page(request.query_options, request.budget);
    let (entities, count) = apply_query_options(authorized, &options);
    let returned_count = entities.len();
    let counters = ScanCounters {
        candidate_count: usize::from(owner.is_some()),
        materialized_count: usize::from(owner.is_some()),
        ..ScanCounters::empty()
    };
    let telemetry = base_telemetry(
        request,
        TelemetryInput {
            strategy: QueryPlaneReadStrategy::ReadSourceCursor,
            fallback_reason: if owner.is_none() {
                QueryPlaneFallbackReason::KeyedAbsence
            } else {
                QueryPlaneFallbackReason::NoFilterPushdown
            },
            filter_pushdown: false,
            catalog_materialization: false,
            coverage: QueryPlaneCoverageReport::default(),
            counters,
            returned_count,
        },
    );
    QueryPlaneReadResult {
        entities,
        count,
        telemetry,
        next_skiptoken: None,
    }
}

/// Resolve an exact declared-key filter through the co-committed ownership
/// index only after the current contract has complete coverage.
pub(super) async fn resolve_keyed_candidates(
    request: &QueryPlaneReadRequest<'_>,
) -> KeyedCandidateResolution {
    let Some(filter) = request.query_options.filter.as_ref() else {
        return KeyedCandidateResolution::NotApplicable;
    };
    let Some(pairs) = super::super::filter_sql::equality_field_predicates(filter) else {
        return KeyedCandidateResolution::NotApplicable;
    };
    let keys = request
        .state
        .declared_keys_for(request.tenant, request.entity_type);
    let Some((key_name, key_hash)) = crate::key_index::resolve_query_to_key(&keys, &pairs) else {
        return KeyedCandidateResolution::NotApplicable;
    };
    let Some((store, _)) = request.state.event_journal() else {
        return KeyedCandidateResolution::NeedsAuthoritativeScan;
    };
    if !store.supports_authoritative_key_index() {
        return KeyedCandidateResolution::NeedsAuthoritativeScan;
    }

    let current_key_set = crate::key_index::declared_key_set_signature(&keys);
    let revision_before = match store
        .key_index_reconciliation_revision(request.tenant.as_str(), request.entity_type)
        .await
    {
        Ok(revision) => revision,
        Err(_) => return KeyedCandidateResolution::NeedsAuthoritativeScan,
    };
    if !request
        .state
        .key_index_backfill_complete(request.tenant, request.entity_type, &current_key_set)
        .await
    {
        return KeyedCandidateResolution::NeedsAuthoritativeScan;
    }

    let lookup = store
        .lookup_by_key_with_sequence(
            request.tenant.as_str(),
            request.entity_type,
            &key_name,
            &key_hash,
        )
        .await;
    let revision_after = match store
        .key_index_reconciliation_revision(request.tenant.as_str(), request.entity_type)
        .await
    {
        Ok(revision) => revision,
        Err(_) => return KeyedCandidateResolution::NeedsAuthoritativeScan,
    };
    if revision_before != revision_after {
        return KeyedCandidateResolution::NeedsAuthoritativeScan;
    }

    match lookup {
        Ok(owner) => KeyedCandidateResolution::Authoritative(KeyOwnershipProof {
            key_name,
            key_hash,
            owner,
            contract_revision: revision_after,
        }),
        Err(_) => KeyedCandidateResolution::NeedsAuthoritativeScan,
    }
}

/// Materialize the indexed owner from journal-backed state, then re-read the
/// ownership row. A same-contract transfer changes the row without changing the
/// contract revision; retrying the new owner closes that lookup/materialization
/// gap. A compatible re-read is the linearization point for both hits and misses.
pub(super) async fn read_fenced_keyed_candidate(
    request: &QueryPlaneReadRequest<'_>,
    proof: KeyOwnershipProof,
    authorization: QueryPlaneReadAuthorization,
) -> Result<FencedKeyedRead, QueryPlaneReadError> {
    let Some((store, _)) = request.state.event_journal() else {
        return Ok(FencedKeyedRead::NeedsAuthoritativeScan);
    };
    let mut expected_owner = proof.owner;

    for _ in 0..MAX_OWNERSHIP_READ_ATTEMPTS {
        let body = match expected_owner.as_ref() {
            Some(owner) => {
                materialize_owner_from_durable_sources(
                    request,
                    owner,
                    &proof.key_name,
                    &proof.key_hash,
                )
                .await?
            }
            None => None,
        };
        let body_matches_generation = expected_owner.is_none() || body.is_some();
        let result = finish_keyed_result(request, expected_owner.as_ref(), body, authorization);

        let revision_before = match store
            .key_index_reconciliation_revision(request.tenant.as_str(), request.entity_type)
            .await
        {
            Ok(revision) if revision == proof.contract_revision => revision,
            _ => return Ok(FencedKeyedRead::NeedsAuthoritativeScan),
        };
        let observed_owner = match store
            .lookup_by_key_with_sequence(
                request.tenant.as_str(),
                request.entity_type,
                &proof.key_name,
                &proof.key_hash,
            )
            .await
        {
            Ok(owner) => owner,
            Err(_) => return Ok(FencedKeyedRead::NeedsAuthoritativeScan),
        };
        let revision_after = match store
            .key_index_reconciliation_revision(request.tenant.as_str(), request.entity_type)
            .await
        {
            Ok(revision) => revision,
            Err(_) => return Ok(FencedKeyedRead::NeedsAuthoritativeScan),
        };

        if revision_before != revision_after || revision_after != proof.contract_revision {
            return Ok(FencedKeyedRead::NeedsAuthoritativeScan);
        }
        if observed_owner == expected_owner {
            if !body_matches_generation {
                return Err(QueryPlaneReadError::KeyOwnershipUnstable);
            }
            return Ok(FencedKeyedRead::Complete(result));
        }
        expected_owner = observed_owner;
    }

    Err(QueryPlaneReadError::KeyOwnershipUnstable)
}
