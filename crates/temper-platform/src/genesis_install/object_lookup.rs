//! Resolving a git object to the entity Genesis stored it as.
//!
//! Genesis writes git objects into Temper keyed by a composite of repository
//! and sha. Reading one back means recomputing that key exactly as the writer
//! formed it, loading it, and confirming the row belongs to the repository the
//! caller asked about. Getting any part of that wrong makes an object that
//! cloned perfectly report as "not found", which is what broke every install
//! through Genesis (ARN-479).

use temper_runtime::tenant::TenantId;
use temper_server::state::ServerState;

use super::string_field;

/// Recompute the durable entity id Genesis assigns to a git object.
///
/// Git objects are persisted keyed by `{sanitized_repository_id}-{git_sha}`.
/// This MUST stay byte-identical to `object_entity_id`, the writer, in the
/// genesis app bundle at `wasm/scm_ingest_pack/src/lib.rs` (the genesis repository);
/// any divergence makes the keyed lookup miss and reintroduces the bundle 404.
/// The contract is exercised end-to-end by the genesis repo's
/// `scripts/live-genesis-install-e2e-smoke.sh` push→bundle round-trip.
pub(super) fn genesis_object_entity_id(repository_id: &str, git_sha: &str) -> String {
    let mut repo = String::with_capacity(repository_id.len());
    let mut last_dash = false;
    for ch in repository_id.chars() {
        if ch.is_ascii_alphanumeric() {
            repo.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            repo.push('-');
            last_dash = true;
        }
    }
    let repo = repo.trim_matches('-');
    if repo.is_empty() {
        format!("obj-{git_sha}")
    } else {
        format!("{repo}-{git_sha}")
    }
}

/// Resolve a git object (Commit/Tree/Blob) by its durable entity key.
///
/// Objects are content-addressed under `{repository_id}-{git_sha}`, so we load
/// that key directly (hydrating from the event store when the actor is cold).
/// A bare-sha fallback covers any legacy object stored before the composite-key
/// scheme. The previous implementation looked up the bare sha — which is never
/// the real key — and then scanned `list_entity_ids_lazy`, whose partially
/// populated in-memory index could omit durable objects; that made the Genesis
/// bundle export 404 with "blob not found" for objects that existed and cloned
/// cleanly. Keyed lookup is both correct and O(1) instead of O(objects).
pub(super) async fn load_genesis_object(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    repository_id: &str,
    git_sha: &str,
) -> Result<Option<temper_server::EntityResponse>, String> {
    debug_assert!(!git_sha.is_empty(), "git object sha must not be empty");

    let composite_id = genesis_object_entity_id(repository_id, git_sha);
    if let Some(found) = load_genesis_object_by_key(
        state,
        tenant,
        entity_type,
        repository_id,
        git_sha,
        &composite_id,
    )
    .await?
    {
        return Ok(Some(found));
    }

    // Legacy objects predating the composite-key scheme were keyed by bare sha.
    // `composite_id` is `{repo}-{sha}` or `obj-{sha}`, so it never equals a
    // non-empty bare sha; the guard only avoids a redundant duplicate lookup.
    if composite_id != git_sha
        && let Some(found) =
            load_genesis_object_by_key(state, tenant, entity_type, repository_id, git_sha, git_sha)
                .await?
    {
        return Ok(Some(found));
    }

    Ok(None)
}

/// Load one candidate entity id and confirm it is the requested git object.
async fn load_genesis_object_by_key(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    repository_id: &str,
    git_sha: &str,
    entity_id: &str,
) -> Result<Option<temper_server::EntityResponse>, String> {
    if !state
        .ensure_entity_loaded(tenant, entity_type, entity_id)
        .await
    {
        // The other silent path to "not found": the entity could not be
        // hydrated at all. Logged for the same reason as the mismatch below.
        tracing::warn!(
            %entity_type,
            %entity_id,
            tenant = %tenant,
            "Genesis object entity could not be loaded"
        );
        return Ok(None);
    }
    let found = state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .map_err(|e| format!("read Genesis {entity_type} {entity_id}: {e}"))?;
    let fields = &found.state.fields;
    let object_repo = string_field(fields, "RepositoryId").unwrap_or_default();

    // Deliberately not comparing a git sha against `fields["Id"]`.
    //
    // `Id` / `id` / `Status` / `status` are server-derived names
    // (`temper_spec::automaton::is_server_derived_field_name`): the actor
    // overwrites them with the entity id and the state-machine state on every
    // hydrate, so `fields["Id"]` here is always the *entity id*, never the git
    // sha the caller asked for. Comparing the two rejected every intact row and
    // made the bundle endpoint answer "commit not found" for commits that read
    // back perfectly, which blocked every install through Genesis.
    //
    // The sha needs no separate check. `entity_id` is derived from
    // (repository_id, git_sha) by `genesis_object_entity_id`, so having loaded
    // the row at that key, the only fact left to confirm is that the row really
    // belongs to the requested repository.
    if object_repo == repository_id {
        Ok(Some(found))
    } else {
        // Both ways this function returns "not found" used to be silent, which
        // is why a 404 here gave nothing to work from. Say what was compared.
        tracing::warn!(
            %entity_type,
            %entity_id,
            wanted_repository = %repository_id,
            found_repository = %object_repo,
            requested_sha = %git_sha,
            "Genesis object key resolved but the row belongs to another repository"
        );
        Ok(None)
    }
}
