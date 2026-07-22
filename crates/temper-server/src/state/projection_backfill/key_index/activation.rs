use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use crate::ServerState;

use super::prepare_key_index_type;

/// A fully replayed contract whose coverage can be published after the live
/// registry swaps to the corresponding activation epoch.
#[derive(Clone, Debug)]
pub(in crate::state) struct PreparedKeyIndexCoverage {
    pub entity_type: String,
    pub key_set: String,
    pub revision: u64,
    pub(super) total: usize,
    pub(super) newly_keyed: usize,
    pub(super) skipped: usize,
}

/// Replay candidate tables without publishing their coverage watermark. Hot
/// activation uses this while old-epoch writers are fenced, then publishes the
/// registry and the prepared watermark in that order.
pub(super) async fn prepare_key_index_coverage_for_tables(
    state: &ServerState,
    tenant: &TenantId,
    tables: &[(String, temper_jit::TransitionTable)],
) -> Result<Vec<PreparedKeyIndexCoverage>, String> {
    prepare_key_index_coverage(state, tenant, tables, false).await
}

/// Prepare activation coverage while reusing a retained durable proof when the
/// key signature and semantic spec fingerprint were unchanged. The activation
/// transaction preserves that watermark but advances the writer epoch; a
/// revision CAS in `publish_prepared_key_index_coverage` still fences a plain
/// append that races after this capture.
pub(in crate::state) async fn prepare_key_index_coverage_for_activation(
    state: &ServerState,
    tenant: &TenantId,
    tables: &[(String, temper_jit::TransitionTable)],
) -> Result<Vec<PreparedKeyIndexCoverage>, String> {
    prepare_key_index_coverage(state, tenant, tables, true).await
}

async fn prepare_key_index_coverage(
    state: &ServerState,
    tenant: &TenantId,
    tables: &[(String, temper_jit::TransitionTable)],
    reuse_durable_coverage: bool,
) -> Result<Vec<PreparedKeyIndexCoverage>, String> {
    let Some((store, backend)) = state.event_journal() else {
        return Ok(Vec::new());
    };
    if !store.supports_authoritative_key_index() {
        return Ok(Vec::new());
    }

    // Capture revisions before reading watermarks. If a plain writer lands
    // between the two reads it removes the watermark, so we replay. If it lands
    // after both reads, publication's revision CAS loses and the activation
    // retry replays. Reading in the opposite order could pair a removed
    // watermark with the writer's newer revision and incorrectly republish it.
    let retained_coverage = if reuse_durable_coverage {
        let mut revisions = BTreeMap::new();
        for (entity_type, _) in tables {
            match store
                .key_index_reconciliation_revision(tenant.as_str(), entity_type)
                .await
            {
                Ok(revision) => {
                    revisions.insert(entity_type.clone(), revision);
                }
                Err(error) => tracing::warn!(
                    tenant = %tenant,
                    entity_type,
                    error = %error,
                    "could not capture retained key coverage revision; replaying type"
                ),
            }
        }
        let watermarks = match store.key_index_backfilled_types(tenant.as_str()).await {
            Ok(watermarks) => watermarks.into_iter().collect::<BTreeMap<_, _>>(),
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    error = %error,
                    "could not read retained key coverage; replaying activation"
                );
                BTreeMap::new()
            }
        };
        Some((revisions, watermarks))
    } else {
        None
    };

    let mut prepared = Vec::new();
    for (entity_type, table) in tables {
        let current_key_set = crate::key_index::declared_key_set_signature(&table.keys);
        let retained_revision = retained_coverage
            .as_ref()
            .and_then(|(revisions, watermarks)| {
                (watermarks.get(entity_type) == Some(&current_key_set))
                    .then(|| revisions.get(entity_type).copied())
                    .flatten()
            });
        if let Some(contract) = prepare_key_index_type(
            state,
            tenant,
            &store,
            backend,
            entity_type,
            table,
            reuse_durable_coverage,
            retained_revision,
        )
        .await?
        {
            prepared.push(contract);
        }
    }
    Ok(prepared)
}

/// Publish prepared coverage with a revision CAS. Returning `false` as an error
/// keeps activation not-ready so no new-epoch live writer can enter on stale rows.
pub(in crate::state) async fn publish_prepared_key_index_coverage(
    state: &ServerState,
    tenant: &TenantId,
    prepared: &[PreparedKeyIndexCoverage],
) -> Result<(), String> {
    for contract in prepared {
        match state
            .mark_key_index_backfilled_if_revision(
                tenant,
                &contract.entity_type,
                &contract.key_set,
                contract.revision,
            )
            .await
        {
            Ok(true) => tracing::info!(
                tenant = %tenant,
                entity_type = %contract.entity_type,
                key_set = %contract.key_set,
                total = contract.total,
                newly_keyed = contract.newly_keyed,
                skipped = contract.skipped,
                repair_revision = contract.revision,
                "entity_key_index activation repair complete; contract ready"
            ),
            Ok(false) => {
                return Err(format!(
                    "key contract changed after candidate repair for {tenant}:{}",
                    contract.entity_type
                ));
            }
            Err(error) => {
                return Err(format!(
                    "failed to publish key coverage for {tenant}:{}: {error}",
                    contract.entity_type
                ));
            }
        }
    }
    Ok(())
}
