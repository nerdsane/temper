//! Cold-start restoration of persisted WASM registry entries.

use super::*;

impl ServerState {
    /// Load all WASM modules from the persistence backend and register them.
    ///
    /// Startup recovery restores registry entries only; compilation is deferred
    /// until first invoke via [`ServerState::ensure_wasm_module_cached`].
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
                    .map_err(|error| format!("failed to load WASM modules from turso: {error}"))?;
                for row in rows {
                    let hash = if row.sha256_hash.is_empty() {
                        temper_wasm::WasmEngine::hash_module(&row.wasm_bytes)
                    } else {
                        row.sha256_hash.clone()
                    };
                    let tenant_id = TenantId::new(&row.tenant);
                    let mut registry = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                    registry.register(&tenant_id, &row.module_name, &hash);
                    recovered += 1;
                }
            }
            return Ok(recovered);
        }

        if let Some(platform) = stack.platform.as_ref() {
            for row in platform.load_wasm_modules_all_tenants().await? {
                let hash = if row.sha256_hash.is_empty() {
                    temper_wasm::WasmEngine::hash_module(&row.wasm_bytes)
                } else {
                    row.sha256_hash
                };
                let tenant_id = TenantId::new(&row.tenant);
                let mut registry = self.wasm_module_registry.write().unwrap(); // ci-ok: infallible lock
                registry.register(&tenant_id, &row.module_name, &hash);
                recovered += 1;
            }
        }

        Ok(recovered)
    }
}
