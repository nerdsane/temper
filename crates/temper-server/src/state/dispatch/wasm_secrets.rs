use temper_runtime::tenant::TenantId;
use temper_wasm::{WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};

impl crate::state::ServerState {
    /// Get secrets filtered through the WASM authorization gate.
    ///
    /// Phase 3 defense-in-depth: only inject secrets that the gate authorizes
    /// into the `ProductionWasmHost`. Even if the decorator is somehow
    /// bypassed, unauthorized secrets aren't in memory.
    pub(crate) fn get_authorized_wasm_secrets(
        &self,
        tenant: &TenantId,
        gate: &dyn WasmAuthzGate,
        authz_ctx: &WasmAuthzContext,
    ) -> std::collections::BTreeMap<String, String> {
        let all_secrets = self
            .secrets_vault
            .as_ref()
            .map(|v| v.get_tenant_secrets(&tenant.to_string()))
            .unwrap_or_default();

        all_secrets
            .into_iter()
            .filter(|(key, _)| {
                gate.authorize_secret_access(key, authz_ctx) == WasmAuthzDecision::Allow
            })
            .collect()
    }
}
