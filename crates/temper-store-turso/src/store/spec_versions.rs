//! The spec commit protocol.
//!
//! A changed spec is written uncommitted and committed by its own install once
//! that install verifies. While a committed version is being replaced it is
//! kept in `spec_committed_versions`, so the committed version is what restart
//! recovery loads: a failed, interrupted or still-running install never costs
//! a type its working version.

use std::collections::BTreeMap;

use crate::driver::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::instrumentation::InstrumentedConnection;
use super::{TursoEventStore, TursoSpecRow, write_gate::WritePriority};
use crate::metrics::TursoQueryTimer;

/// Columns a saved committed version carries, in `spec_committed_versions` order.
pub(super) const SPEC_VERSION_COLUMNS: &str = "tenant, entity_type, ioa_source, csdl_xml, content_hash, \
     version, verified, verification_status, levels_passed, levels_total, \
     verification_result, updated_at";

/// An uncommitted row younger than this may belong to an install still
/// verifying in another process sharing the database, so startup recovery
/// leaves it alone. Restore already loads the committed version, so waiting
/// costs nothing but the cleanup.
const ABANDONED_INSTALL_AGE: &str = "-3600 seconds";

/// A committed spec version saved while its row holds an uncommitted one.
pub(super) struct SavedSpecVersion {
    pub(super) tenant: String,
    pub(super) entity_type: String,
    ioa_source: String,
    csdl_xml: Option<String>,
    content_hash: Option<String>,
    version: i64,
    verified: i64,
    verification_status: String,
    levels_passed: Option<i64>,
    levels_total: Option<i64>,
    verification_result: Option<String>,
    updated_at: String,
}

impl SavedSpecVersion {
    /// The saved version as a committed spec row.
    pub(super) fn into_row(self) -> TursoSpecRow {
        TursoSpecRow {
            tenant: self.tenant,
            entity_type: self.entity_type,
            ioa_source: self.ioa_source,
            csdl_xml: self.csdl_xml,
            verification_status: self.verification_status,
            verified: self.verified != 0,
            levels_passed: self.levels_passed.map(|v| v as i32),
            levels_total: self.levels_total.map(|v| v as i32),
            verification_result: self.verification_result,
            content_hash: self.content_hash,
            updated_at: self.updated_at,
            committed: true,
        }
    }
}

impl TursoEventStore {
    pub(super) async fn load_saved_spec_versions(
        conn: &InstrumentedConnection,
    ) -> Result<Vec<SavedSpecVersion>, PersistenceError> {
        let mut rows = conn
            .query(
                &format!(
                    "SELECT {SPEC_VERSION_COLUMNS} FROM spec_committed_versions \
                     ORDER BY tenant, entity_type"
                ),
                (),
            )
            .await
            .map_err(storage_error)?;
        let mut saved = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            saved.push(SavedSpecVersion {
                tenant: row.get(0).map_err(storage_error)?,
                entity_type: row.get(1).map_err(storage_error)?,
                ioa_source: row.get(2).map_err(storage_error)?,
                csdl_xml: row.get(3).map_err(storage_error)?,
                content_hash: row.get(4).map_err(storage_error)?,
                version: row.get(5).map_err(storage_error)?,
                verified: row.get(6).map_err(storage_error)?,
                verification_status: row.get(7).map_err(storage_error)?,
                levels_passed: row.get(8).map_err(storage_error)?,
                levels_total: row.get(9).map_err(storage_error)?,
                verification_result: row.get(10).map_err(storage_error)?,
                updated_at: row.get(11).map_err(storage_error)?,
            });
        }
        Ok(saved)
    }

    /// Commit one install in one transaction: exactly these spec versions,
    /// `(entity_type, content_hash, csdl_xml)`, and the deletion of `removed`
    /// types (a replace-mode load that dropped them).
    ///
    /// Returns true when every version is committed afterwards. When a row no
    /// longer holds its version (another install replaced it), the transaction
    /// rolls back and returns false, so an install commits whole or not at all.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.commit_spec_versions"))]
    pub async fn commit_spec_versions(
        &self,
        tenant: &str,
        versions: &[(&str, &str, &str)],
        removed: &[&str],
    ) -> Result<bool, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.commit_spec_versions");
        if versions.is_empty() && removed.is_empty() {
            return Ok(true);
        }
        let conn = self
            .write_connection("turso.commit_spec_versions", WritePriority::High)
            .await?;
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        for (entity_type, content_hash, csdl_xml) in versions {
            let promoted = tx
                .execute(
                    "UPDATE specs SET committed = 1, updated_at = datetime('now') \
                     WHERE tenant = ?1 AND entity_type = ?2 AND content_hash = ?3 \
                       AND csdl_xml IS ?4 AND committed != 1",
                    params![tenant, *entity_type, *content_hash, *csdl_xml],
                )
                .await
                .map_err(storage_error)?;
            let committed = promoted > 0 || {
                let mut rows = tx
                    .query(
                        "SELECT 1 FROM specs WHERE tenant = ?1 AND entity_type = ?2 \
                         AND content_hash = ?3 AND csdl_xml IS ?4 AND committed = 1",
                        params![tenant, *entity_type, *content_hash, *csdl_xml],
                    )
                    .await
                    .map_err(storage_error)?;
                rows.next().await.map_err(storage_error)?.is_some()
            };
            if !committed {
                tx.rollback().await.map_err(storage_error)?;
                return Ok(false);
            }
            tx.execute(
                "DELETE FROM spec_committed_versions WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, *entity_type],
            )
            .await
            .map_err(storage_error)?;
        }
        for entity_type in removed {
            for sql in [
                "DELETE FROM specs WHERE tenant = ?1 AND entity_type = ?2",
                "DELETE FROM spec_committed_versions WHERE tenant = ?1 AND entity_type = ?2",
            ] {
                tx.execute(sql, params![tenant, *entity_type])
                    .await
                    .map_err(storage_error)?;
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(true)
    }

    /// Discard abandoned installs (startup recovery) and return how many
    /// uncommitted versions were discarded. An uncommitted row older than
    /// [`ABANDONED_INSTALL_AGE`] returns to its saved committed version, or is
    /// removed when its type never committed. Younger rows may be another
    /// process's install in flight and are left alone.
    #[instrument(skip_all, fields(otel.name = "turso.delete_uncommitted_specs"))]
    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_uncommitted_specs");
        let conn = self
            .write_connection("turso.delete_uncommitted_specs", WritePriority::High)
            .await?;
        let mut saved: BTreeMap<(String, String), SavedSpecVersion> =
            Self::load_saved_spec_versions(&conn)
                .await?
                .into_iter()
                .map(|v| ((v.tenant.clone(), v.entity_type.clone()), v))
                .collect();
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        let mut rows = tx
            .query(
                "SELECT tenant, entity_type FROM specs \
                 WHERE committed = 0 AND updated_at < datetime('now', ?1)",
                params![ABANDONED_INSTALL_AGE],
            )
            .await
            .map_err(storage_error)?;
        let mut abandoned = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            abandoned.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        drop(rows);
        for (tenant, entity_type) in &abandoned {
            match saved.remove(&(tenant.clone(), entity_type.clone())) {
                Some(v) => {
                    tx.execute(
                        "UPDATE specs SET ioa_source = ?3, csdl_xml = ?4, content_hash = ?5, \
                             version = ?6, verified = ?7, verification_status = ?8, \
                             levels_passed = ?9, levels_total = ?10, verification_result = ?11, \
                             updated_at = ?12, committed = 1 \
                         WHERE tenant = ?1 AND entity_type = ?2",
                        params![
                            v.tenant.as_str(),
                            v.entity_type.as_str(),
                            v.ioa_source.as_str(),
                            v.csdl_xml.clone(),
                            v.content_hash.clone(),
                            v.version,
                            v.verified,
                            v.verification_status.as_str(),
                            v.levels_passed,
                            v.levels_total,
                            v.verification_result.clone(),
                            v.updated_at.as_str()
                        ],
                    )
                    .await
                    .map_err(storage_error)?;
                    tx.execute(
                        "DELETE FROM spec_committed_versions WHERE tenant = ?1 AND entity_type = ?2",
                        params![tenant.as_str(), entity_type.as_str()],
                    )
                    .await
                    .map_err(storage_error)?;
                }
                None => {
                    tx.execute(
                        "DELETE FROM specs WHERE tenant = ?1 AND entity_type = ?2",
                        params![tenant.as_str(), entity_type.as_str()],
                    )
                    .await
                    .map_err(storage_error)?;
                }
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(abandoned.len())
    }
}
