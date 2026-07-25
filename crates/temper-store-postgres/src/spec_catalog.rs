use std::collections::BTreeSet;

use temper_runtime::persistence::PersistenceError;

use crate::PostgresEventStore;

impl PostgresEventStore {
    /// Atomically publish a tenant catalog update under the shared replacement lock.
    ///
    /// When `replace` is true, omissions are discovered after the tenant-scoped
    /// transaction lock is acquired and tombstoned in the same transaction. Two
    /// replicas therefore serialize source-of-truth replacements instead of
    /// committing their union from stale pre-transaction snapshots. An omitted
    /// constraint source is preserved for merges and cleared for replacements.
    pub async fn persist_spec_catalog_update(
        &self,
        tenant: &str,
        specs: &[(&str, &str, &str)],
        csdl_xml: &str,
        additional_removed_entity_types: &[String],
        replace: bool,
        cross_invariants_toml: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock( \
                 hashtextextended('spec-catalog:' || $1, 0) \
             )",
        )
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        let incoming = specs
            .iter()
            .map(|(entity_type, _, _)| *entity_type)
            .collect::<BTreeSet<_>>();
        let mut removed_entity_types = if replace {
            sqlx::query_scalar::<_, String>(
                "SELECT entity_type FROM specs WHERE tenant = $1 \
                 UNION \
                 SELECT entity_type FROM staged_specs WHERE tenant = $1 \
                 UNION \
                 SELECT entity_type FROM spec_declaration_authority \
                 WHERE tenant = $1 AND present = true \
                 ORDER BY entity_type",
            )
            .bind(tenant)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?
            .into_iter()
            .filter(|entity_type| !incoming.contains(entity_type.as_str()))
            .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        removed_entity_types.extend(
            additional_removed_entity_types
                .iter()
                .filter(|entity_type| !incoming.contains(entity_type.as_str()))
                .cloned(),
        );
        let removed_entity_types = removed_entity_types.into_iter().collect::<Vec<_>>();

        for (entity_type, ioa_source, content_hash) in specs {
            sqlx::query("DELETE FROM staged_specs WHERE tenant = $1 AND entity_type = $2")
                .bind(tenant)
                .bind(entity_type)
                .execute(&mut *tx)
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            sqlx::query(
                "INSERT INTO specs \
                 (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, true, 1, false, 'pending', now()) \
                 ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                     ioa_source = EXCLUDED.ioa_source, csdl_xml = EXCLUDED.csdl_xml, \
                     content_hash = EXCLUDED.content_hash, committed = true, \
                     version = specs.version + 1, verified = false, \
                     verification_status = 'pending', levels_passed = NULL, \
                     levels_total = NULL, verification_result = NULL, updated_at = now()",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(ioa_source)
            .bind(csdl_xml)
            .bind(content_hash)
            .execute(&mut *tx)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        }
        for entity_type in &removed_entity_types {
            sqlx::query("DELETE FROM staged_specs WHERE tenant = $1 AND entity_type = $2")
                .bind(tenant)
                .bind(entity_type)
                .execute(&mut *tx)
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            sqlx::query("SELECT tombstone_spec_declaration_authority($1, $2)")
                .bind(tenant)
                .bind(entity_type)
                .execute(&mut *tx)
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        }
        if let Some(source) = cross_invariants_toml {
            sqlx::query(
                "INSERT INTO tenant_constraints \
                 (tenant, cross_invariants_toml, version, updated_at) \
                 VALUES ($1, $2, 1, now()) \
                 ON CONFLICT (tenant) DO UPDATE SET \
                     cross_invariants_toml = EXCLUDED.cross_invariants_toml, \
                     version = tenant_constraints.version + 1, updated_at = now()",
            )
            .bind(tenant)
            .bind(source)
            .execute(&mut *tx)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        } else if replace {
            sqlx::query("DELETE FROM tenant_constraints WHERE tenant = $1")
                .bind(tenant)
                .execute(&mut *tx)
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        Ok(removed_entity_types)
    }
}

#[cfg(test)]
#[path = "spec_catalog_test.rs"]
mod tests;
