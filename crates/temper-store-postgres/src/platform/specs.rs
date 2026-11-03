//! Spec rows and their commit protocol.
//!
//! A changed spec is written uncommitted and committed once its install
//! verifies. While a committed version is being replaced it is kept in
//! `spec_committed_versions`, and restore loads that committed version: a
//! failed, interrupted or still-running install never costs a type its
//! working version.

use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::PostgresEventStore;

/// An uncommitted row younger than this may belong to an install still
/// verifying in another replica, so startup cleanup leaves it alone. Restore
/// already loads the committed version, so waiting costs only the cleanup.
const ABANDONED_INSTALL_AGE: &str = "1 hour";

impl PostgresEventStore {
    /// Lock a spec row until the transaction ends.
    ///
    /// At READ COMMITTED each statement takes its own snapshot. Without the
    /// lock, an upsert could read the row while another transaction is
    /// committing a new version (and dropping the saved copy), save nothing,
    /// then overwrite that version once its lock frees: no committed version
    /// would remain. Taking the lock first makes every later statement of the
    /// transaction see the row as last committed.
    async fn lock_spec_row(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        tenant: &str,
        entity_type: &str,
    ) -> Result<(), PersistenceError> {
        crate::dbm::postgres_query!(
            &self.schema,
            "SELECT 1 FROM {schema}specs WHERE tenant = $1 AND entity_type = $2 FOR UPDATE",
        )
        .bind(tenant)
        .bind(entity_type)
        .execute(&mut **transaction)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Upsert a spec source with content-hash gating: an unchanged spec keeps
    /// its row as it is; a changed one is written uncommitted and pending.
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        self.lock_spec_row(&mut transaction, tenant, entity_type)
            .await?;
        crate::dbm::postgres_query!(&self.schema,
            "INSERT INTO {schema}spec_committed_versions \
             (tenant, entity_type, ioa_source, csdl_xml, content_hash, version, verified, \
              verification_status, levels_passed, levels_total, verification_result, updated_at) \
             SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, version, verified, \
                    verification_status, levels_passed, levels_total, verification_result, updated_at \
             FROM {schema}specs \
             WHERE tenant = $1 AND entity_type = $2 AND committed = true \
               AND (content_hash IS DISTINCT FROM $3 OR csdl_xml IS DISTINCT FROM $4) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = EXCLUDED.ioa_source, csdl_xml = EXCLUDED.csdl_xml, \
                 content_hash = EXCLUDED.content_hash, version = EXCLUDED.version, \
                 verified = EXCLUDED.verified, verification_status = EXCLUDED.verification_status, \
                 levels_passed = EXCLUDED.levels_passed, levels_total = EXCLUDED.levels_total, \
                 verification_result = EXCLUDED.verification_result, updated_at = EXCLUDED.updated_at",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(content_hash)
        .bind(csdl_xml)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        crate::dbm::postgres_query!(&self.schema,
            "INSERT INTO {schema}specs \
             (tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version, verified, verification_status, updated_at) \
             VALUES ($1, $2, $3, $4, $5, false, 1, false, 'pending', now()) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.ioa_source ELSE specs.ioa_source END, \
                 csdl_xml = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.csdl_xml ELSE specs.csdl_xml END, \
                 content_hash = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN EXCLUDED.content_hash ELSE specs.content_hash END, \
                 committed = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN false ELSE specs.committed END, \
                 version = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN specs.version + 1 ELSE specs.version END, \
                 verified = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN false ELSE specs.verified END, \
                 verification_status = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN 'pending' ELSE specs.verification_status END, \
                 levels_passed = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_passed END, \
                 levels_total = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.levels_total END, \
                 verification_result = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN NULL ELSE specs.verification_result END, \
                 updated_at = CASE WHEN specs.content_hash IS DISTINCT FROM EXCLUDED.content_hash OR specs.csdl_xml IS DISTINCT FROM EXCLUDED.csdl_xml THEN now() ELSE specs.updated_at END",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(ioa_source)
        .bind(csdl_xml)
        .bind(content_hash)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)
    }

    /// Delete a spec and any committed version saved for it.
    pub async fn delete_spec(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<(), PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        for sql in [
            "DELETE FROM {schema}specs WHERE tenant = $1 AND entity_type = $2",
            "DELETE FROM {schema}spec_committed_versions WHERE tenant = $1 AND entity_type = $2",
        ] {
            crate::dbm::postgres_query!(&self.schema, sql)
                .bind(tenant)
                .bind(entity_type)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)
    }

    /// Commit every spec row of a tenant.
    pub async fn commit_specs(&self, tenant: &str) -> Result<(), PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        for sql in [
            "UPDATE {schema}specs SET committed = true, updated_at = now() WHERE tenant = $1",
            "DELETE FROM {schema}spec_committed_versions WHERE tenant = $1",
        ] {
            crate::dbm::postgres_query!(&self.schema, sql)
                .bind(tenant)
                .execute(&mut *transaction)
                .await
                .map_err(storage_error)?;
        }
        transaction.commit().await.map_err(storage_error)
    }

    /// Commit one install in one transaction: exactly these spec versions,
    /// `(entity_type, content_hash, csdl_xml)`, and the deletion of `removed`
    /// types (a replace-mode load that dropped them).
    ///
    /// Returns true when every version is committed afterwards. When a row no
    /// longer holds its version (another install replaced it), the transaction
    /// rolls back and returns false, so an install commits whole or not at all.
    pub async fn commit_spec_versions(
        &self,
        tenant: &str,
        versions: &[(&str, &str, &str)],
        removed: &[&str],
    ) -> Result<bool, PersistenceError> {
        if versions.is_empty() && removed.is_empty() {
            return Ok(true);
        }
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        for (entity_type, content_hash, csdl_xml) in versions {
            self.lock_spec_row(&mut transaction, tenant, entity_type)
                .await?;
            let committed: bool = crate::dbm::postgres_query_scalar!(
                &self.schema,
                "WITH promoted AS ( \
                     UPDATE {schema}specs SET committed = true, updated_at = now() \
                     WHERE tenant = $1 AND entity_type = $2 AND content_hash = $3 \
                       AND csdl_xml IS NOT DISTINCT FROM $4 AND committed = false \
                     RETURNING 1) \
                 SELECT EXISTS (SELECT 1 FROM promoted) OR EXISTS ( \
                     SELECT 1 FROM {schema}specs WHERE tenant = $1 AND entity_type = $2 \
                       AND content_hash = $3 AND csdl_xml IS NOT DISTINCT FROM $4 AND committed = true)",
            )
            .bind(tenant)
            .bind(*entity_type)
            .bind(*content_hash)
            .bind(*csdl_xml)
            .fetch_one(&mut *transaction)
            .await
            .map_err(storage_error)?;
            if !committed {
                transaction.rollback().await.map_err(storage_error)?;
                return Ok(false);
            }
            crate::dbm::postgres_query!(
                &self.schema,
                "DELETE FROM {schema}spec_committed_versions WHERE tenant = $1 AND entity_type = $2",
            )
            .bind(tenant)
            .bind(*entity_type)
            .execute(&mut *transaction)
            .await
            .map_err(storage_error)?;
        }
        for entity_type in removed {
            for sql in [
                "DELETE FROM {schema}specs WHERE tenant = $1 AND entity_type = $2",
                "DELETE FROM {schema}spec_committed_versions WHERE tenant = $1 AND entity_type = $2",
            ] {
                crate::dbm::postgres_query!(&self.schema, sql)
                    .bind(tenant)
                    .bind(*entity_type)
                    .execute(&mut *transaction)
                    .await
                    .map_err(storage_error)?;
            }
        }
        transaction.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    /// Discard abandoned installs (startup recovery) and return how many
    /// uncommitted versions were discarded. An uncommitted row older than
    /// [`ABANDONED_INSTALL_AGE`] returns to its saved committed version, or is removed when its
    /// type never committed. Younger rows may be another replica's install in
    /// flight and are left alone; restore already loads the committed version.
    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let mut transaction = self.pool().begin().await.map_err(storage_error)?;
        let restored = crate::dbm::postgres_query!(&self.schema,
            "UPDATE {schema}specs AS s SET \
                 ioa_source = b.ioa_source, csdl_xml = b.csdl_xml, content_hash = b.content_hash, \
                 version = b.version, verified = b.verified, verification_status = b.verification_status, \
                 levels_passed = b.levels_passed, levels_total = b.levels_total, \
                 verification_result = b.verification_result, updated_at = b.updated_at, committed = true \
             FROM {schema}spec_committed_versions AS b \
             WHERE s.tenant = b.tenant AND s.entity_type = b.entity_type \
               AND s.committed = false AND s.updated_at < now() - $1::interval",
        )
        .bind(ABANDONED_INSTALL_AGE)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        let deleted = crate::dbm::postgres_query!(
            &self.schema,
            "DELETE FROM {schema}specs WHERE committed = false AND updated_at < now() - $1::interval"
        )
        .bind(ABANDONED_INSTALL_AGE)
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        crate::dbm::postgres_query!(&self.schema,
            "DELETE FROM {schema}spec_committed_versions AS b WHERE NOT EXISTS ( \
                 SELECT 1 FROM {schema}specs AS s \
                 WHERE s.tenant = b.tenant AND s.entity_type = b.entity_type AND s.committed = false)",
        )
        .execute(&mut *transaction)
        .await
        .map_err(storage_error)?;
        transaction.commit().await.map_err(storage_error)?;
        Ok((restored.rows_affected() + deleted.rows_affected()) as usize)
    }
}
