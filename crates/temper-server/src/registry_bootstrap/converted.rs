//! Stored specs in the old predicate syntax: converted at the first boot that
//! reads them and written back, so no later boot converts them again. The
//! text each was stored with is kept first, so the conversion can be undone.

use std::sync::Arc;

use crate::platform_store::{PlatformStore, SpecVerificationUpdate};

/// How a persisted spec reads under this kernel.
pub(super) enum StoredSpec {
    /// Parses as stored.
    Current,
    /// Was in the old predicate syntax; this is the converted source.
    Converted(String),
    /// Neither parses nor converts.
    Unreadable,
}

/// A stored spec converted to the current syntax, to be written back.
pub(super) struct ConvertedSpec {
    /// The spec's entity type.
    pub(super) entity_type: String,
    /// The converted source.
    pub(super) source: String,
    /// The source as stored, kept before the converted one replaces it.
    pub(super) original: String,
    /// The CSDL stored with the spec.
    pub(super) csdl_xml: String,
    /// Whether the spec was verified as stored.
    pub(super) verified: bool,
}

pub(super) fn read_stored_spec(tenant: &str, entity_type: &str, ioa_source: &str) -> StoredSpec {
    let Err(parse_error) = temper_spec::automaton::parse_automaton(ioa_source) else {
        return StoredSpec::Current;
    };
    match temper_spec::automaton::legacy::migrate_source(ioa_source) {
        Ok(migration) => {
            tracing::info!(
                tenant,
                entity_type,
                notes = migration.notes.len(),
                "converted stored spec to the current syntax"
            );
            StoredSpec::Converted(migration.source)
        }
        Err(convert_error) => {
            tracing::error!(
                tenant,
                entity_type,
                %parse_error,
                %convert_error,
                "stored spec does not parse and cannot be converted; its entity type stays unavailable until the spec is fixed"
            );
            StoredSpec::Unreadable
        }
    }
}

/// What registering one stored tenant converted from the old syntax, and the
/// store its rows came from, so the conversion is written back there.
#[must_use = "converted specs not written back are converted again at every boot"]
pub struct Converted {
    pub(super) store: Arc<dyn PlatformStore>,
    pub(super) tenant: String,
    pub(super) specs: Vec<ConvertedSpec>,
}

impl Converted {
    /// Write the converted specs back to the store they were read from. See
    /// [`write_back`].
    pub async fn write_back(self) {
        write_back(self.store.as_ref(), &self.tenant, &self.specs).await;
    }
}

/// Write converted specs back so the conversion happens once. A failed write
/// only means the next boot converts the stored source again.
///
/// Each spec's stored text is kept first, in the store's `spec_originals`, and
/// a spec whose original could not be kept is not written: no stored text is
/// lost. Undoing a conversion is an `upsert_spec` of the kept original.
///
/// Writing a new source resets the row's verification, and nothing re-verifies
/// a restored spec, so its entity type would refuse writes (423). The
/// conversion keeps the spec's meaning, so a spec that was verified as stored
/// stays verified as converted, as the kernel already trusts prebuilt bundles.
pub(super) async fn write_back(
    store: &dyn PlatformStore,
    tenant: &str,
    converted: &[ConvertedSpec],
) {
    if converted.is_empty() {
        return;
    }
    let mut carry_verification = Vec::new();
    let mut written: Vec<(&str, String, &str)> = Vec::new();
    for spec in converted {
        if let Err(error) = store
            .keep_spec_original(tenant, &spec.entity_type, &spec.original)
            .await
        {
            tracing::warn!(tenant, entity_type = %spec.entity_type, %error, "failed to keep a spec's stored text; its conversion is not written");
            continue;
        }
        let hash = temper_store_turso::spec_content_hash(&spec.source);
        if let Err(error) = store
            .upsert_spec(
                tenant,
                &spec.entity_type,
                &spec.source,
                &spec.csdl_xml,
                &hash,
            )
            .await
        {
            tracing::warn!(tenant, entity_type = %spec.entity_type, %error, "failed to store converted spec");
            continue;
        }
        written.push((spec.entity_type.as_str(), hash, spec.csdl_xml.as_str()));
        if spec.verified {
            carry_verification.push(spec.entity_type.as_str());
        }
    }
    for entity_type in carry_verification {
        if let Err(error) = store
            .persist_spec_verification(
                tenant,
                entity_type,
                SpecVerificationUpdate {
                    status: "completed",
                    verified: true,
                    levels_passed: None,
                    levels_total: None,
                    verification_result_json: None,
                },
            )
            .await
        {
            tracing::warn!(tenant, entity_type, %error, "failed to carry verification to converted spec");
        }
    }
    // upsert_spec marks a changed row uncommitted; commit exactly the
    // converted rows, or restore keeps loading the stored version.
    let versions: Vec<(&str, &str, &str)> = written
        .iter()
        .map(|(entity_type, hash, csdl_xml)| (*entity_type, hash.as_str(), *csdl_xml))
        .collect();
    match store.commit_spec_versions(tenant, &versions, &[]).await {
        Ok(true) => {}
        Ok(false) => tracing::warn!(tenant, "converted specs changed before commit"),
        Err(error) => tracing::warn!(tenant, %error, "failed to commit converted specs"),
    }
}
