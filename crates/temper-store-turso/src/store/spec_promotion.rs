//! Atomic spec staging, generation reservation, and tenant promotion.

use std::collections::BTreeSet;

use libsql::{Transaction, TransactionBehavior, params};
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::{TursoEventStore, write_gate::WritePriority};
use crate::metrics::TursoQueryTimer;

const SPEC_BATCH_ENTITY_BUDGET: usize = 256;

#[derive(Debug, PartialEq, Eq)]
struct SourceFingerprint {
    ioa_source: String,
    csdl_xml: Option<String>,
    content_hash: String,
}

async fn load_fingerprint(
    tx: &Transaction,
    table: &str,
    tenant: &str,
    entity_type: &str,
) -> Result<Option<SourceFingerprint>, PersistenceError> {
    let sql = format!(
        "SELECT ioa_source, csdl_xml, COALESCE(content_hash, '') FROM {table} \
         WHERE tenant = ?1 AND entity_type = ?2 LIMIT 1"
    );
    let mut rows = tx
        .query(&sql, params![tenant, entity_type])
        .await
        .map_err(storage_error)?;
    let Some(row) = rows.next().await.map_err(storage_error)? else {
        return Ok(None);
    };
    Ok(Some(SourceFingerprint {
        ioa_source: row.get(0).map_err(storage_error)?,
        csdl_xml: row.get(1).map_err(storage_error)?,
        content_hash: row.get(2).map_err(storage_error)?,
    }))
}

async fn committed_sources_match(
    store: &TursoEventStore,
    tenant: &str,
    specs: &[(&str, &str, &str, &str)],
) -> Result<bool, PersistenceError> {
    let conn = store.configured_connection().await?;
    for (entity_type, ioa_source, csdl_xml, content_hash) in specs {
        let mut rows = conn
            .query(
                "SELECT ioa_source, csdl_xml, COALESCE(content_hash, '') FROM specs \
                 WHERE tenant = ?1 AND entity_type = ?2 AND committed = 1",
                params![tenant, *entity_type],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(false);
        };
        let fingerprint = SourceFingerprint {
            ioa_source: row.get(0).map_err(storage_error)?,
            csdl_xml: row.get(1).map_err(storage_error)?,
            content_hash: row.get(2).map_err(storage_error)?,
        };
        if fingerprint
            != (SourceFingerprint {
                ioa_source: (*ioa_source).to_string(),
                csdl_xml: Some((*csdl_xml).to_string()),
                content_hash: (*content_hash).to_string(),
            })
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn app_commit_is_identical(
    store: &TursoEventStore,
    tenant: &str,
    specs: &[(&str, &str, &str, &str)],
    policy: Option<&str>,
    app_name: &str,
) -> Result<bool, PersistenceError> {
    if !committed_sources_match(store, tenant, specs).await? {
        return Ok(false);
    }
    let conn = store.configured_connection().await?;
    if let Some(policy_text) = policy {
        let mut rows = conn
            .query(
                "SELECT policy_text FROM tenant_policies WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(false);
        };
        let persisted_policy: String = row.get(0).map_err(storage_error)?;
        if persisted_policy != policy_text {
            return Ok(false);
        }
    }
    let mut rows = conn
        .query(
            "SELECT 1 FROM tenant_installed_apps WHERE tenant_id = ?1 AND app_name = ?2 LIMIT 1",
            params![tenant, app_name],
        )
        .await
        .map_err(storage_error)?;
    Ok(rows.next().await.map_err(storage_error)?.is_some())
}

async fn reserve_spec_generation(
    tx: &Transaction,
    tenant: &str,
    entity_type: &str,
) -> Result<i64, PersistenceError> {
    tx.execute(
        "INSERT INTO spec_source_generations (tenant, entity_type, generation) \
         VALUES (?1, ?2, 1) \
         ON CONFLICT(tenant, entity_type) DO UPDATE SET generation = generation + 1",
        params![tenant, entity_type],
    )
    .await
    .map_err(storage_error)?;
    let mut rows = tx
        .query(
            "SELECT generation FROM spec_source_generations \
             WHERE tenant = ?1 AND entity_type = ?2",
            params![tenant, entity_type],
        )
        .await
        .map_err(storage_error)?;
    let row =
        rows.next().await.map_err(storage_error)?.ok_or_else(|| {
            PersistenceError::Storage("reserved spec generation disappeared".into())
        })?;
    row.get(0).map_err(storage_error)
}

async fn reserve_constraint_generation(
    tx: &Transaction,
    tenant: &str,
) -> Result<i64, PersistenceError> {
    tx.execute(
        "INSERT INTO tenant_constraint_generations (tenant, generation) VALUES (?1, 1) \
         ON CONFLICT(tenant) DO UPDATE SET generation = generation + 1",
        params![tenant],
    )
    .await
    .map_err(storage_error)?;
    let mut rows = tx
        .query(
            "SELECT generation FROM tenant_constraint_generations WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
    let row = rows.next().await.map_err(storage_error)?.ok_or_else(|| {
        PersistenceError::Storage("reserved constraint generation disappeared".into())
    })?;
    row.get(0).map_err(storage_error)
}

async fn upsert_committed_source(
    tx: &Transaction,
    tenant: &str,
    source: (&str, &str, &str, &str),
) -> Result<(), PersistenceError> {
    let (entity_type, ioa_source, csdl_xml, content_hash) = source;
    let fingerprint = SourceFingerprint {
        ioa_source: ioa_source.to_string(),
        csdl_xml: Some(csdl_xml.to_string()),
        content_hash: content_hash.to_string(),
    };
    if load_fingerprint(tx, "specs", tenant, entity_type).await? == Some(fingerprint) {
        return Ok(());
    }
    let generation = reserve_spec_generation(tx, tenant, entity_type).await?;
    tx.execute(
        "INSERT INTO specs (
             tenant, entity_type, ioa_source, csdl_xml, content_hash, committed,
             version, verified, verification_status, levels_passed, levels_total,
             verification_result, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 0, 'pending', NULL, NULL, NULL, datetime('now'))
         ON CONFLICT(tenant, entity_type) DO UPDATE SET
             ioa_source = excluded.ioa_source,
             csdl_xml = excluded.csdl_xml,
             content_hash = excluded.content_hash,
             committed = 1,
             version = excluded.version,
             verified = 0,
             verification_status = 'pending',
             levels_passed = NULL,
             levels_total = NULL,
             verification_result = NULL,
             updated_at = datetime('now')",
        params![
            tenant,
            entity_type,
            ioa_source,
            csdl_xml,
            content_hash,
            generation
        ],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
}

async fn promote_sources(
    tx: &Transaction,
    tenant: &str,
    specs: &[(&str, &str, &str, &str)],
    merge: bool,
) -> Result<(), PersistenceError> {
    if specs.len() > SPEC_BATCH_ENTITY_BUDGET {
        return Err(PersistenceError::Storage(format!(
            "spec batch exceeds {SPEC_BATCH_ENTITY_BUDGET}-entity budget"
        )));
    }
    let submitted = specs
        .iter()
        .map(|(entity_type, _, _, _)| *entity_type)
        .collect::<BTreeSet<_>>();
    if submitted.len() != specs.len() {
        return Err(PersistenceError::Storage(
            "spec batch contains duplicate entity types".into(),
        ));
    }

    tx.execute(
        "DELETE FROM spec_staging WHERE tenant = ?1",
        params![tenant],
    )
    .await
    .map_err(storage_error)?;
    for source in specs {
        upsert_committed_source(tx, tenant, *source).await?;
    }
    if !merge {
        let mut rows = tx
            .query(
                "SELECT entity_type FROM specs WHERE tenant = ?1 ORDER BY entity_type",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut existing = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            existing.push(row.get::<String>(0).map_err(storage_error)?);
        }
        for entity_type in existing {
            if !submitted.contains(entity_type.as_str()) {
                tx.execute(
                    "DELETE FROM specs WHERE tenant = ?1 AND entity_type = ?2",
                    params![tenant, entity_type],
                )
                .await
                .map_err(storage_error)?;
            }
        }
    }
    Ok(())
}

async fn promote_constraint(
    tx: &Transaction,
    tenant: &str,
    source: Option<&str>,
    merge: bool,
) -> Result<(), PersistenceError> {
    if let Some(source) = source {
        let mut rows = tx
            .query(
                "SELECT cross_invariants_toml FROM tenant_constraints WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let unchanged = match rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<String>(0).map_err(storage_error)? == source,
            None => false,
        };
        if !unchanged {
            let generation = reserve_constraint_generation(tx, tenant).await?;
            tx.execute(
                "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at) \
                 VALUES (?1, ?2, ?3, datetime('now')) \
                 ON CONFLICT(tenant) DO UPDATE SET cross_invariants_toml = excluded.cross_invariants_toml, \
                     version = excluded.version, updated_at = datetime('now')",
                params![tenant, source, generation],
            )
            .await
            .map_err(storage_error)?;
        }
    } else if !merge {
        tx.execute(
            "DELETE FROM tenant_constraints WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
    }
    Ok(())
}

impl TursoEventStore {
    /// Stage one candidate without overwriting the committed source.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.upsert_spec"))]
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_spec");
        let _permit = self
            .acquire_write_permit("turso.upsert_spec", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let candidate = SourceFingerprint {
            ioa_source: ioa_source.to_string(),
            csdl_xml: Some(csdl_xml.to_string()),
            content_hash: content_hash.to_string(),
        };
        if load_fingerprint(&tx, "specs", tenant, entity_type).await? == Some(candidate) {
            tx.execute(
                "DELETE FROM spec_staging WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
        let staged_matches = load_fingerprint(&tx, "spec_staging", tenant, entity_type).await?
            == Some(SourceFingerprint {
                ioa_source: ioa_source.to_string(),
                csdl_xml: Some(csdl_xml.to_string()),
                content_hash: content_hash.to_string(),
            });
        if !staged_matches {
            let generation = reserve_spec_generation(&tx, tenant, entity_type).await?;
            tx.execute(
                "INSERT INTO spec_staging (
                     tenant, entity_type, ioa_source, csdl_xml, content_hash, version,
                     verified, verification_status, levels_passed, levels_total,
                     verification_result, staged_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 'pending', NULL, NULL, NULL, datetime('now'))
                 ON CONFLICT(tenant, entity_type) DO UPDATE SET
                     ioa_source = excluded.ioa_source,
                     csdl_xml = excluded.csdl_xml,
                     content_hash = excluded.content_hash,
                     version = excluded.version,
                     verified = 0,
                     verification_status = 'pending',
                     levels_passed = NULL,
                     levels_total = NULL,
                     verification_result = NULL,
                     staged_at = datetime('now')",
                params![
                    tenant,
                    entity_type,
                    ioa_source,
                    csdl_xml,
                    content_hash,
                    generation
                ],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Atomically promote a complete or merge-mode tenant source batch.
    #[instrument(skip_all, fields(tenant, merge, otel.name = "turso.promote_spec_batch"))]
    pub async fn promote_spec_batch(
        &self,
        tenant: &str,
        specs: &[(&str, &str, &str, &str)],
        constraint_source: Option<&str>,
        merge: bool,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.promote_spec_batch");
        let _permit = self
            .acquire_write_permit("turso.promote_spec_batch", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        promote_sources(&tx, tenant, specs, merge).await?;
        promote_constraint(&tx, tenant, constraint_source, merge).await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Atomically promote app specs together with policy/install metadata.
    #[instrument(skip_all, fields(tenant, app_name, otel.name = "turso.upsert_specs_and_commit"))]
    pub async fn upsert_specs_and_commit(
        &self,
        tenant: &str,
        specs: &[(&str, &str, &str, &str)],
        policy: Option<&str>,
        app_name: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.upsert_specs_and_commit");
        if app_commit_is_identical(self, tenant, specs, policy, app_name).await? {
            return Ok(());
        }
        let _permit = self
            .acquire_write_permit("turso.upsert_specs_and_commit", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        promote_sources(&tx, tenant, specs, true).await?;
        if let Some(policy_text) = policy {
            tx.execute(
                "INSERT INTO tenant_policies (tenant, policy_text, updated_at) \
                 VALUES (?1, ?2, datetime('now')) \
                 ON CONFLICT(tenant) DO UPDATE SET policy_text = excluded.policy_text, updated_at = datetime('now')",
                params![tenant, policy_text],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO tenant_installed_apps (tenant_id, app_name) VALUES (?1, ?2)",
            params![tenant, app_name],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Delete a committed or staged source while retaining its generation high-water mark.
    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "turso.delete_spec"))]
    pub async fn delete_spec(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_spec");
        let _permit = self
            .acquire_write_permit("turso.delete_spec", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        for table in ["specs", "spec_staging"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE tenant = ?1 AND entity_type = ?2"),
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Promote every staged source for one tenant in one transaction.
    #[instrument(skip_all, fields(tenant, otel.name = "turso.commit_specs"))]
    pub async fn commit_specs(&self, tenant: &str) -> Result<(), PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.commit_specs");
        let _permit = self
            .acquire_write_permit("turso.commit_specs", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        tx.execute(
            "INSERT INTO specs (
                 tenant, entity_type, ioa_source, csdl_xml, content_hash, committed,
                 version, verified, verification_status, levels_passed, levels_total,
                 verification_result, updated_at
             )
             SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, 1,
                    version, verified, verification_status, levels_passed, levels_total,
                    verification_result, datetime('now')
             FROM spec_staging WHERE tenant = ?1 AND 1
             ON CONFLICT(tenant, entity_type) DO UPDATE SET
                 ioa_source = excluded.ioa_source,
                 csdl_xml = excluded.csdl_xml,
                 content_hash = excluded.content_hash,
                 committed = 1,
                 version = excluded.version,
                 verified = excluded.verified,
                 verification_status = excluded.verification_status,
                 levels_passed = excluded.levels_passed,
                 levels_total = excluded.levels_total,
                 verification_result = excluded.verification_result,
                 updated_at = datetime('now')",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM spec_staging WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        tx.execute(
            "UPDATE specs SET committed = 1, updated_at = datetime('now') \
             WHERE tenant = ?1 AND committed != 1",
            params![tenant],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    /// Delete abandoned staging while preserving committed last-known-good source.
    #[instrument(skip_all, fields(otel.name = "turso.delete_uncommitted_specs"))]
    pub async fn delete_uncommitted_specs(&self) -> Result<usize, PersistenceError> {
        let _query_timer = TursoQueryTimer::start("turso.delete_uncommitted_specs");
        let _permit = self
            .acquire_write_permit("turso.delete_uncommitted_specs", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let staged = tx
            .execute("DELETE FROM spec_staging", ())
            .await
            .map_err(storage_error)?;
        let legacy = tx
            .execute("DELETE FROM specs WHERE committed = 0", ())
            .await
            .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok((staged + legacy) as usize)
    }
}
