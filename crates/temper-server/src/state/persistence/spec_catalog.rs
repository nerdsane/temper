#[cfg(feature = "observe")]
use std::collections::{BTreeMap, BTreeSet};

use super::ServerState;
#[cfg(feature = "observe")]
use super::TenantMetadataBackend;

impl ServerState {
    /// Atomically persist one hot-loaded catalog update before registry publication.
    ///
    /// SQL backends discover replacement omissions only after taking their shared
    /// tenant-scoped write lock. The returned types are therefore the omissions
    /// from the durable catalog version that this update actually replaced.
    #[cfg(feature = "observe")]
    pub(crate) async fn persist_spec_catalog_update(
        &self,
        tenant: &str,
        ioa_sources: &BTreeMap<String, String>,
        csdl_xml: &str,
        additional_removed_entity_types: &[String],
        replace: bool,
        cross_invariants_toml: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let fingerprints = ioa_sources
            .iter()
            .map(|(entity_type, source)| {
                (
                    entity_type.as_str(),
                    source.as_str(),
                    temper_store_turso::spec_content_hash(source),
                )
            })
            .collect::<Vec<_>>();
        let specs = fingerprints
            .iter()
            .map(|(entity_type, source, fingerprint)| (*entity_type, *source, fingerprint.as_str()))
            .collect::<Vec<_>>();
        let incoming = ioa_sources
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let mut removed_entity_types = additional_removed_entity_types
            .iter()
            .filter(|entity_type| !incoming.contains(entity_type.as_str()))
            .cloned()
            .collect::<BTreeSet<_>>();

        match self.tenant_metadata_backend(tenant).await {
            Some(TenantMetadataBackend::Postgres(pool)) => {
                removed_entity_types.extend(
                    temper_store_postgres::PostgresEventStore::new(pool)
                        .persist_spec_catalog_update(
                            tenant,
                            &specs,
                            csdl_xml,
                            additional_removed_entity_types,
                            replace,
                            cross_invariants_toml,
                        )
                        .await
                        .map_err(|error| error.to_string())?,
                );
            }
            Some(TenantMetadataBackend::Turso(store)) => {
                removed_entity_types.extend(
                    store
                        .persist_spec_catalog_update(
                            tenant,
                            &specs,
                            csdl_xml,
                            additional_removed_entity_types,
                            replace,
                            cross_invariants_toml,
                        )
                        .await
                        .map_err(|error| error.to_string())?,
                );
            }
            Some(TenantMetadataBackend::Redis) => {
                return Err(Self::redis_ephemeral_error("Spec catalog replacement"));
            }
            None if replace => {
                if let Some((store, _)) = self.event_journal() {
                    removed_entity_types.extend(
                        store
                            .spec_declaration_entity_types(tenant)
                            .await
                            .map_err(|error| error.to_string())?
                            .into_iter()
                            .filter(|entity_type| !incoming.contains(entity_type.as_str())),
                    );
                }
            }
            None => {}
        }

        for (entity_type, _, fingerprint) in &fingerprints {
            self.persist_event_store_spec_declaration(tenant, entity_type, fingerprint)
                .await?;
        }
        for entity_type in &removed_entity_types {
            self.persist_event_store_spec_declaration(tenant, entity_type, "absent:v1")
                .await?;
        }
        Ok(removed_entity_types.into_iter().collect())
    }

    pub(super) async fn persist_event_store_spec_declaration(
        &self,
        tenant: &str,
        entity_type: &str,
        declaration_fingerprint: &str,
    ) -> Result<(), String> {
        if let Some((store, _)) = self.event_journal() {
            store
                .persist_spec_declaration(tenant, entity_type, declaration_fingerprint)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}
