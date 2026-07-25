//! Atomic publication of staged Turso specs.

use libsql::{TransactionBehavior, params};
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, write_gate::WritePriority};
use crate::TursoSpecVerificationUpdate;
use crate::metrics::TursoQueryTimer;

impl TursoEventStore {
    /// Mark all uncommitted specs for a tenant as committed.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.commit_specs"))]
    pub async fn commit_specs(&self, tenant: &str) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.commit_specs");
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        tx.execute(
            "INSERT INTO specs (
                 tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version,
                 verified, verification_status, updated_at
             )
             SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, 1, version,
                    0, 'pending', datetime('now')
             FROM staged_specs WHERE tenant = ?1
             ON CONFLICT (tenant, entity_type) DO UPDATE SET
                 ioa_source = excluded.ioa_source,
                 csdl_xml = excluded.csdl_xml,
                 content_hash = excluded.content_hash,
                 committed = 1,
                 version = specs.version + 1,
                 verified = 0,
                 verification_status = 'pending',
                 levels_passed = NULL,
                 levels_total = NULL,
                 verification_result = NULL,
                 updated_at = datetime('now')",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM staged_specs WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Atomically promote only staged specs matching one operation's exact bytes.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.commit_spec_batch"))]
    pub async fn commit_spec_batch(
        &self,
        tenant: &str,
        expected: &[(&str, &str, &str)],
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.commit_spec_batch");
        let mut expected = expected.to_vec();
        expected.sort_unstable_by(|left, right| left.0.cmp(right.0));
        if expected.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(PersistenceError::Storage(format!(
                "duplicate spec batch entity type for tenant {tenant}"
            )));
        }
        let _write_permit = self
            .acquire_write_permit("turso.commit_spec_batch", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        for (entity_type, content_hash, csdl_xml) in expected {
            let promoted = tx
                .execute(
                    "INSERT INTO specs (
                         tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version,
                         verified, verification_status, updated_at
                     )
                     SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, 1, version,
                            0, 'pending', datetime('now')
                     FROM staged_specs
                     WHERE tenant = ?1 AND entity_type = ?2 AND content_hash = ?3
                       AND csdl_xml IS ?4
                     ON CONFLICT (tenant, entity_type) DO UPDATE SET
                         ioa_source = excluded.ioa_source,
                         csdl_xml = excluded.csdl_xml,
                         content_hash = excluded.content_hash,
                         committed = 1,
                         version = specs.version + 1,
                         verified = 0,
                         verification_status = 'pending',
                         levels_passed = NULL,
                         levels_total = NULL,
                         verification_result = NULL,
                         updated_at = datetime('now')",
                    params![tenant, entity_type, content_hash, csdl_xml],
                )
                .await
                .map_err(storage_error)?;
            if promoted != 1 {
                return Err(PersistenceError::Storage(format!(
                    "staged spec fingerprint changed for {tenant}/{entity_type}"
                )));
            }
            tx.execute(
                "DELETE FROM staged_specs
                 WHERE tenant = ?1 AND entity_type = ?2 AND content_hash = ?3
                   AND csdl_xml IS ?4",
                params![tenant, entity_type, content_hash, csdl_xml],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Atomically persist verification and commit only the expected spec bytes.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.commit_verified_spec"))]
    pub async fn commit_verified_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        expected_content_hash: &str,
        expected_csdl_xml: &str,
        update: TursoSpecVerificationUpdate<'_>,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.commit_verified_spec");
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let staged = {
            let mut rows = tx
                .query(
                    "SELECT ioa_source, csdl_xml, content_hash, version \
                     FROM staged_specs \
                     WHERE tenant = ?1 AND entity_type = ?2 AND content_hash = ?3 \
                       AND csdl_xml IS ?4",
                    params![
                        tenant,
                        entity_type,
                        expected_content_hash,
                        expected_csdl_xml
                    ],
                )
                .await
                .map_err(storage_error)?;
            rows.next()
                .await
                .map_err(storage_error)?
                .map(|row| {
                    Ok::<_, PersistenceError>((
                        row.get::<String>(0).map_err(storage_error)?,
                        row.get::<Option<String>>(1).map_err(storage_error)?,
                        row.get::<String>(2).map_err(storage_error)?,
                        row.get::<i64>(3).map_err(storage_error)?,
                    ))
                })
                .transpose()?
        };
        let Some((ioa_source, csdl_xml, content_hash, version)) = staged else {
            return Err(PersistenceError::Storage(format!(
                "staged spec fingerprint changed for {tenant}/{entity_type}"
            )));
        };
        tx.execute(
            "INSERT INTO specs (
                 tenant, entity_type, ioa_source, csdl_xml, content_hash, committed, version,
                 verified, verification_status, levels_passed, levels_total,
                 verification_result, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))
             ON CONFLICT (tenant, entity_type) DO UPDATE SET
                 ioa_source = excluded.ioa_source,
                 csdl_xml = excluded.csdl_xml,
                 content_hash = excluded.content_hash,
                 committed = 1,
                 version = specs.version + 1,
                 verified = excluded.verified,
                 verification_status = excluded.verification_status,
                 levels_passed = excluded.levels_passed,
                 levels_total = excluded.levels_total,
                 verification_result = excluded.verification_result,
                 updated_at = datetime('now')",
            params![
                tenant,
                entity_type,
                ioa_source,
                csdl_xml,
                content_hash,
                version,
                update.verified as i64,
                update.status,
                update.levels_passed,
                update.levels_total,
                update.verification_result_json
            ],
        )
        .await
        .map_err(storage_error)?;
        let affected = tx
            .execute(
                "DELETE FROM staged_specs \
                 WHERE tenant = ?1 AND entity_type = ?2 AND content_hash = ?3 \
                   AND csdl_xml IS ?4",
                params![
                    tenant,
                    entity_type,
                    expected_content_hash,
                    expected_csdl_xml
                ],
            )
            .await
            .map_err(storage_error)?;
        if affected != 1 {
            return Err(PersistenceError::Storage(format!(
                "staged spec fingerprint changed for {tenant}/{entity_type}"
            )));
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Delete all uncommitted specs across all tenants.
    #[instrument(skip_all, fields(otel.name = "turso.delete_uncommitted_specs"))]
    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_uncommitted_specs");
        let conn = self.configured_connection().await?;
        let staged = conn
            .execute("DELETE FROM staged_specs", ())
            .await
            .map_err(storage_error)?;
        let legacy = conn
            .execute("DELETE FROM specs WHERE committed = 0", ())
            .await
            .map_err(storage_error)?;
        Ok((staged + legacy) as usize)
    }
}
