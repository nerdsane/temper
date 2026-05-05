//! Persistence methods for ServerState (Postgres, Turso, Redis backends).

use sqlx::PgPool;
use temper_runtime::tenant::TenantId;
use temper_store_turso::{TursoEventStore, TursoWasmInvocationInsert};

use super::ServerState;
use super::wasm_invocation_log::WasmInvocationEntry;
use crate::storage::BackendLabel;

/// Owned metadata backend for tenant-scoped operations.
///
/// `turso_for_tenant()` returns an owned `TursoEventStore` (Arc-based,
/// clone is cheap), so tenant-scoped operations use this owned variant.
pub(crate) enum TenantMetadataBackend {
    Postgres(PgPool),
    Turso(TursoEventStore),
    Redis,
}

mod logs_and_secrets;
mod spec_metadata;

const BUNDLED_REPLACE_UPLOAD_SOURCE: &str = "bundled-replace-upload";

impl ServerState {
    fn redis_ephemeral_error(operation: &str) -> String {
        format!(
            "{operation} is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
        )
    }

    /// Return a tenant-scoped metadata backend.
    ///
    /// In TenantRouted mode, routes to the per-tenant database.
    /// In single-DB Turso mode, returns the shared store.
    /// In Postgres mode, returns the shared pool (RLS handles isolation).
    pub(crate) async fn tenant_metadata_backend(
        &self,
        tenant: &str,
    ) -> Option<TenantMetadataBackend> {
        let stack = self.storage_stack.as_ref()?;
        if let Some(pool) = stack.postgres_pool.as_ref() {
            return Some(TenantMetadataBackend::Postgres(pool.clone()));
        }
        if let Some(provider) = stack.turso.as_ref()
            && let Some(turso) = provider.store_for_tenant(tenant).await
        {
            return Some(TenantMetadataBackend::Turso(turso));
        }
        if stack.backend == BackendLabel::Redis {
            Some(TenantMetadataBackend::Redis)
        } else {
            None
        }
    }

    /// Upsert a WASM module in the persistence backend (Postgres or Turso).
    ///
    /// `source` is `"bundled"` for the os-apps install pipeline and `"upload"`
    /// for the hot-upload API. The store is idempotent on hash and refuses to
    /// overwrite an existing `'upload'` row with a plain `'bundled'` row whose
    /// hash differs — that's how hot-uploaded modules survive same-bundle
    /// restarts.
    pub async fn upsert_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
        wasm_bytes: &[u8],
        sha256_hash: &str,
        source: &str,
    ) -> Result<(), String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(());
        };
        let replace_uploaded_wasm = source == BUNDLED_REPLACE_UPLOAD_SOURCE;
        let persisted_source = if replace_uploaded_wasm {
            "bundled"
        } else {
            source
        };

        let effective_hash = if sha256_hash.is_empty() {
            temper_wasm::WasmEngine::hash_module(wasm_bytes)
        } else {
            sha256_hash.to_string()
        };
        let tenant_id = TenantId::new(tenant);
        let artifact_key = crate::blob_store::wasm_artifact_key(&effective_hash);
        self.put_blob_object(&tenant_id, &artifact_key, wasm_bytes, None)
            .await
            .map_err(|e| {
                format!(
                    "failed to persist WASM artifact {tenant}/{module_name} to object store: {e}"
                )
            })?;

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO wasm_modules \
                     (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at, source) \
                     VALUES ($1, $2, $3, $4, 1, $5, now(), $6) \
                     ON CONFLICT (tenant, module_name) DO UPDATE SET \
                         wasm_bytes = EXCLUDED.wasm_bytes, \
                         sha256_hash = EXCLUDED.sha256_hash, \
                         version = wasm_modules.version + 1, \
                         size_bytes = EXCLUDED.size_bytes, \
                         updated_at = now(), \
                         source = EXCLUDED.source \
                     WHERE wasm_modules.sha256_hash IS DISTINCT FROM EXCLUDED.sha256_hash \
                        AND ($7 OR EXCLUDED.source = 'upload' OR wasm_modules.source = 'bundled')",
                )
                .bind(tenant)
                .bind(module_name)
                .bind(Vec::<u8>::new())
                .bind(&effective_hash)
                .bind(wasm_bytes.len() as i32)
                .bind(persisted_source)
                .bind(replace_uploaded_wasm)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| format!("failed to upsert WASM module {tenant}/{module_name}: {e}"))
            }
            TenantMetadataBackend::Turso(turso) => turso
                .upsert_wasm_module(tenant, module_name, wasm_bytes, &effective_hash, source)
                .await
                .map_err(|e| {
                    format!("failed to upsert WASM module {tenant}/{module_name} in turso: {e}")
                }),
            TenantMetadataBackend::Redis => Err(Self::redis_ephemeral_error("WASM module persistence")),
        }
    }

    /// Persist a bundled OS-app module while explicitly replacing an existing
    /// hot-upload row. Use this only when installed-app metadata proves the
    /// bundled WASM digest changed; normal restart/install paths should preserve
    /// hot uploads.
    pub async fn upsert_bundled_wasm_module_replacing_upload(
        &self,
        tenant: &str,
        module_name: &str,
        wasm_bytes: &[u8],
        sha256_hash: &str,
    ) -> Result<(), String> {
        self.upsert_wasm_module(
            tenant,
            module_name,
            wasm_bytes,
            sha256_hash,
            BUNDLED_REPLACE_UPLOAD_SOURCE,
        )
        .await
    }

    /// Return `(module_name -> (sha256_hash, source))` for every WASM module
    /// currently persisted for `tenant`. Used by the os-apps install pipeline
    /// to decide whether hot-uploaded modules should be preserved or replaced.
    pub async fn load_wasm_module_sources(
        &self,
        tenant: &str,
    ) -> Result<std::collections::BTreeMap<String, (String, String)>, String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(std::collections::BTreeMap::new());
        };

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                let rows = sqlx::query_as::<_, (String, String, String)>(
                    "SELECT module_name, sha256_hash, source FROM wasm_modules WHERE tenant = $1",
                )
                .bind(tenant)
                .fetch_all(&pool)
                .await
                .map_err(|e| format!("failed to load WASM module sources for {tenant}: {e}"))?;
                Ok(rows
                    .into_iter()
                    .map(|(name, hash, source)| (name, (hash, source)))
                    .collect())
            }
            TenantMetadataBackend::Turso(turso) => {
                let rows = turso
                    .load_all_wasm_modules(tenant)
                    .await
                    .map_err(|e| format!("failed to load WASM module sources for {tenant}: {e}"))?;
                Ok(rows
                    .into_iter()
                    .map(|r| (r.module_name, (r.sha256_hash, r.source)))
                    .collect())
            }
            TenantMetadataBackend::Redis => Ok(std::collections::BTreeMap::new()),
        }
    }

    /// Delete a WASM module from persistence.
    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(false);
        };

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                let result =
                    sqlx::query("DELETE FROM wasm_modules WHERE tenant = $1 AND module_name = $2")
                        .bind(tenant)
                        .bind(module_name)
                        .execute(&pool)
                        .await
                        .map_err(|e| {
                            format!("failed to delete WASM module {tenant}/{module_name}: {e}")
                        })?;
                Ok(result.rows_affected() > 0)
            }
            TenantMetadataBackend::Turso(turso) => turso
                .delete_wasm_module(tenant, module_name)
                .await
                .map_err(|e| {
                    format!("failed to delete WASM module {tenant}/{module_name} in turso: {e}")
                }),
            TenantMetadataBackend::Redis => {
                Err(Self::redis_ephemeral_error("WASM module deletion"))
            }
        }
    }

    /// Ensure a registered WASM module is compiled and cached in-memory.
    ///
    /// Startup recovery restores registry entries without eagerly compiling
    /// every persisted module. The first invocation compiles on demand.
    pub async fn ensure_wasm_module_cached(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        module_name: &str,
        expected_hash: &str,
    ) -> Result<(), String> {
        if self.wasm_engine.is_cached(expected_hash) {
            return Ok(());
        }

        let tenant_name = tenant.to_string();
        let Some(backend) = self.tenant_metadata_backend(&tenant_name).await else {
            return Err(format!(
                "cannot lazy-load WASM module '{module_name}' for tenant '{tenant_name}' without a metadata backend"
            ));
        };

        let (wasm_bytes, stored_hash) = match backend {
            TenantMetadataBackend::Postgres(pool) => {
                let row: Option<(Vec<u8>, String)> = sqlx::query_as(
                    "SELECT wasm_bytes, sha256_hash FROM wasm_modules WHERE tenant = $1 AND module_name = $2",
                )
                .bind(&tenant_name)
                .bind(module_name)
                .fetch_optional(&pool)
                .await
                .map_err(|e| {
                    format!(
                        "failed to lazy-load WASM module {tenant_name}/{module_name} from postgres: {e}"
                    )
                })?;
                let Some((wasm_bytes, sha256_hash)) = row else {
                    return Err(format!(
                        "WASM module '{module_name}' not found in persistence for tenant '{tenant_name}'"
                    ));
                };
                let wasm_bytes = self
                    .resolve_wasm_artifact_bytes(tenant, module_name, &sha256_hash, wasm_bytes)
                    .await?;
                (wasm_bytes, sha256_hash)
            }
            TenantMetadataBackend::Turso(turso) => {
                let row = turso
                    .load_wasm_module(&tenant_name, module_name)
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to lazy-load WASM module {tenant_name}/{module_name} from turso: {e}"
                        )
                    })?;
                let Some(row) = row else {
                    return Err(format!(
                        "WASM module '{module_name}' not found in persistence for tenant '{tenant_name}'"
                    ));
                };
                let wasm_bytes = self
                    .resolve_wasm_artifact_bytes(
                        tenant,
                        module_name,
                        &row.sha256_hash,
                        row.wasm_bytes,
                    )
                    .await?;
                (wasm_bytes, row.sha256_hash)
            }
            TenantMetadataBackend::Redis => {
                return Err(Self::redis_ephemeral_error("WASM lazy-load"));
            }
        };

        let resolved_hash = if stored_hash.is_empty() {
            temper_wasm::WasmEngine::hash_module(&wasm_bytes)
        } else {
            stored_hash
        };
        if resolved_hash != expected_hash {
            return Err(format!(
                "WASM module hash mismatch for {tenant_name}/{module_name}: registry={expected_hash} persisted={resolved_hash}"
            ));
        }

        self.wasm_engine
            .compile_and_cache(&wasm_bytes)
            .map(|_| {
                tracing::info!(
                    tenant = %tenant_name,
                    module = %module_name,
                    hash = %expected_hash,
                    "lazy-compiled persisted WASM module on first use"
                );
            })
            .map_err(|e| {
                format!(
                    "failed to compile lazy-loaded WASM module {tenant_name}/{module_name}: {e}"
                )
            })
    }

    async fn resolve_wasm_artifact_bytes(
        &self,
        tenant: &TenantId,
        module_name: &str,
        sha256_hash: &str,
        inline_bytes: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        if !inline_bytes.is_empty() {
            return Ok(inline_bytes);
        }
        if sha256_hash.is_empty() {
            return Err(format!(
                "WASM module '{module_name}' for tenant '{tenant}' has no inline bytes and no artifact hash"
            ));
        }

        let artifact_key = crate::blob_store::wasm_artifact_key(sha256_hash);
        self.get_blob_with_legacy_fallback(tenant, &artifact_key)
            .await?
            .ok_or_else(|| {
                format!(
                    "WASM artifact missing for tenant '{tenant}' module '{module_name}' at '{artifact_key}'"
                )
            })
    }

    /// Persist a WASM invocation log entry (Postgres or Turso).
    ///
    /// Fire-and-forget — callers should not block the dispatch path on this.
    pub async fn persist_wasm_invocation(&self, entry: &WasmInvocationEntry) -> Result<(), String> {
        let Some(store) = self.metadata_store_for_tenant(&entry.tenant).await else {
            return Ok(());
        };

        store
            .persist_wasm_invocation(&TursoWasmInvocationInsert {
                tenant: &entry.tenant,
                entity_type: &entry.entity_type,
                entity_id: &entry.entity_id,
                module_name: &entry.module_name,
                trigger_action: &entry.trigger_action,
                callback_action: entry.callback_action.as_deref(),
                success: entry.success,
                error: entry.error.as_deref(),
                duration_ms: entry.duration_ms,
                created_at: &entry.timestamp,
            })
            .await
            .map_err(|e| {
                format!(
                    "failed to persist WASM invocation for {}/{} via {} metadata store: {e}",
                    entry.tenant,
                    entry.module_name,
                    store.backend_name()
                )
            })
    }

    /// Load all WASM modules from the persistence backend and register them.
    ///
    /// Startup recovery now restores registry entries only; compilation is
    /// deferred until first invoke via [`ensure_wasm_module_cached`].
    pub async fn load_wasm_modules(&self) -> Result<usize, String> {
        let Some(stack) = self.storage_stack.as_ref() else {
            return Ok(0);
        };

        let mut recovered = 0usize;

        if let Some(turso_provider) = stack.turso.as_ref() {
            for turso in turso_provider.all_stores().await {
                let rows = turso
                    .load_wasm_modules_all_tenants()
                    .await
                    .map_err(|e| format!("failed to load WASM modules from turso: {e}"))?;
                for row in rows {
                    let hash = if row.sha256_hash.is_empty() {
                        temper_wasm::WasmEngine::hash_module(&row.wasm_bytes)
                    } else {
                        row.sha256_hash.clone()
                    };
                    let tenant_id = temper_runtime::tenant::TenantId::new(&row.tenant);
                    let mut wasm_reg = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                    wasm_reg.register(&tenant_id, &row.module_name, &hash);
                    recovered += 1;
                }
            }
            return Ok(recovered);
        }

        if let Some(platform) = stack.platform.as_ref() {
            let rows = platform.load_wasm_modules_all_tenants().await?;
            for row in rows {
                let hash = if row.sha256_hash.is_empty() {
                    temper_wasm::WasmEngine::hash_module(&row.wasm_bytes)
                } else {
                    row.sha256_hash
                };
                let tenant_id = temper_runtime::tenant::TenantId::new(&row.tenant);
                let mut wasm_reg = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                wasm_reg.register(&tenant_id, &row.module_name, &hash);
                recovered += 1;
            }
        }

        Ok(recovered)
    }
}
