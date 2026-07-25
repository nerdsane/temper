#[cfg(feature = "observe")]
use std::collections::{BTreeMap, BTreeSet};

use super::ServerState;
#[cfg(feature = "observe")]
use super::TenantMetadataBackend;

impl ServerState {
    /// Atomically promote the exact verified staged catalog and its omissions.
    #[cfg(feature = "observe")]
    pub(crate) async fn persist_verified_spec_catalog_update(
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
                    temper_store_turso::spec_content_hash(source),
                    csdl_xml,
                )
            })
            .collect::<Vec<_>>();
        let expected = fingerprints
            .iter()
            .map(|(entity_type, fingerprint, csdl)| (*entity_type, fingerprint.as_str(), *csdl))
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
                        .persist_verified_spec_catalog_update(
                            tenant,
                            &expected,
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
                        .persist_verified_spec_catalog_update(
                            tenant,
                            &expected,
                            additional_removed_entity_types,
                            replace,
                            cross_invariants_toml,
                        )
                        .await
                        .map_err(|error| error.to_string())?,
                );
            }
            Some(TenantMetadataBackend::Redis) => {
                return Err(Self::redis_ephemeral_error(
                    "Verified spec catalog publication",
                ));
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

        for (entity_type, fingerprint, _) in &fingerprints {
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
