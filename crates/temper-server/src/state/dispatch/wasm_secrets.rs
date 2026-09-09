use std::collections::BTreeMap;
use std::sync::Arc;

use temper_runtime::tenant::TenantId;
use temper_wasm::{SecretResolverFn, WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};

const WASM_BOOTSTRAP_SECRET_KEYS: [&str; 1] = ["blob_endpoint"];

fn is_wasm_bootstrap_secret(key: &str) -> bool {
    WASM_BOOTSTRAP_SECRET_KEYS.contains(&key) || key.starts_with("ca_cert:")
}

impl crate::state::ServerState {
    /// Get the small eager secret set needed to construct a production host.
    ///
    /// Most WASM invocations never call `host_get_secret`, so dispatch should
    /// not authorize and clone every tenant secret up front. This helper keeps
    /// eager materialization limited to host-bootstrap values required before
    /// guest code runs.
    pub(crate) fn get_authorized_wasm_host_bootstrap_secrets(
        &self,
        tenant: &TenantId,
        gate: &dyn WasmAuthzGate,
        authz_ctx: &WasmAuthzContext,
    ) -> BTreeMap<String, String> {
        let Some(vault) = self.secrets_vault.as_ref() else {
            return BTreeMap::new();
        };
        let tenant_str = tenant.to_string();

        vault
            .list_keys(&tenant_str)
            .into_iter()
            .filter(|key| is_wasm_bootstrap_secret(key))
            .filter_map(|key| {
                if gate.authorize_secret_access(&key, authz_ctx) != WasmAuthzDecision::Allow {
                    return None;
                }
                vault
                    .get_secret(&tenant_str, &key)
                    .map(|value| (key, value))
            })
            .collect()
    }

    /// Build an authorization-aware lazy resolver for guest secret lookups.
    pub(crate) fn authorized_wasm_secret_resolver(
        &self,
        tenant: &TenantId,
        gate: Arc<dyn WasmAuthzGate>,
        authz_ctx: WasmAuthzContext,
    ) -> Option<SecretResolverFn> {
        let vault = Arc::clone(self.secrets_vault.as_ref()?);
        let tenant_str = tenant.to_string();

        Some(Arc::new(move |key: &str| {
            if key.eq_ignore_ascii_case("temper_api_key") {
                return Err(
                    "secret 'temper_api_key' is reserved and unavailable to WASM guests"
                        .to_string(),
                );
            }
            match gate.authorize_secret_access(key, &authz_ctx) {
                WasmAuthzDecision::Allow => vault
                    .get_secret(&tenant_str, key)
                    .ok_or_else(|| format!("secret not found: {key}")),
                WasmAuthzDecision::Deny(reason) => {
                    Err(format!("authorization denied for secret '{key}': {reason}"))
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use crate::registry::SpecRegistry;
    use crate::secrets::SecretsVault;
    use crate::state::ServerState;
    use temper_runtime::ActorSystem;

    #[derive(Clone)]
    struct RecordingSecretGate {
        allowed: BTreeSet<String>,
        requested: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingSecretGate {
        fn allowing(keys: &[&str]) -> Self {
            Self {
                allowed: keys.iter().map(|key| (*key).to_string()).collect(),
                requested: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requested_keys(&self) -> Vec<String> {
            self.requested
                .lock()
                .expect("requested keys lock poisoned")
                .clone()
        }
    }

    impl WasmAuthzGate for RecordingSecretGate {
        fn authorize_http_call(
            &self,
            _domain: &str,
            _method: &str,
            _url: &str,
            _ctx: &WasmAuthzContext,
        ) -> WasmAuthzDecision {
            WasmAuthzDecision::Allow
        }

        fn authorize_secret_access(
            &self,
            secret_key: &str,
            _ctx: &WasmAuthzContext,
        ) -> WasmAuthzDecision {
            self.requested
                .lock()
                .expect("requested keys lock poisoned")
                .push(secret_key.to_string());
            if self.allowed.contains(secret_key) {
                WasmAuthzDecision::Allow
            } else {
                WasmAuthzDecision::Deny("denied by test gate".to_string())
            }
        }
    }

    fn test_authz_ctx() -> WasmAuthzContext {
        WasmAuthzContext {
            tenant: "tenant-a".to_string(),
            module_name: "provider_caller".to_string(),
            agent_id: Some("agent-1".to_string()),
            session_id: Some("ss-1".to_string()),
            entity_type: "Session".to_string(),
            trigger_action: "CallProvider".to_string(),
        }
    }

    fn state_with_vault(vault: SecretsVault) -> ServerState {
        ServerState::from_registry(ActorSystem::new("wasm-secret-tests"), SpecRegistry::new())
            .with_secrets_vault(vault)
    }

    #[test]
    fn host_bootstrap_secret_resolution_authorizes_only_bootstrap_keys() {
        let tenant = TenantId::new("tenant-a");
        let vault = SecretsVault::new(&[7u8; 32]);
        vault
            .cache_secret(
                tenant.as_str(),
                "blob_endpoint",
                "https://blob.example".into(),
            )
            .unwrap();
        vault
            .cache_secret(
                tenant.as_str(),
                "temper_api_url",
                "https://temper.example".into(),
            )
            .unwrap();
        vault
            .cache_secret(tenant.as_str(), "temper_api_key", "internal-token".into())
            .unwrap();
        vault
            .cache_secret(
                tenant.as_str(),
                "ca_cert:internal",
                "-----BEGIN CERT-----".into(),
            )
            .unwrap();
        vault
            .cache_secret(tenant.as_str(), "provider_api_key", "sk-provider".into())
            .unwrap();
        vault
            .cache_secret(tenant.as_str(), "unrelated_secret", "unused".into())
            .unwrap();
        let state = state_with_vault(vault);
        let gate = RecordingSecretGate::allowing(&[
            "blob_endpoint",
            "temper_api_key",
            "temper_api_url",
            "ca_cert:internal",
            "provider_api_key",
            "unrelated_secret",
        ]);

        let secrets =
            state.get_authorized_wasm_host_bootstrap_secrets(&tenant, &gate, &test_authz_ctx());

        assert_eq!(secrets.len(), 2);
        assert!(secrets.contains_key("blob_endpoint"));
        assert!(!secrets.contains_key("temper_api_key"));
        assert!(!secrets.contains_key("temper_api_url"));
        assert!(secrets.contains_key("ca_cert:internal"));
        assert!(!secrets.contains_key("provider_api_key"));
        assert!(!secrets.contains_key("unrelated_secret"));
        assert_eq!(
            gate.requested_keys(),
            vec!["blob_endpoint".to_string(), "ca_cert:internal".to_string()]
        );
    }

    #[test]
    fn lazy_secret_resolver_authorizes_requested_key_and_reads_current_value() {
        let tenant = TenantId::new("tenant-a");
        let vault = SecretsVault::new(&[8u8; 32]);
        vault
            .cache_secret(tenant.as_str(), "provider_api_key", "first".into())
            .unwrap();
        vault
            .cache_secret(tenant.as_str(), "blocked_key", "blocked".into())
            .unwrap();
        let state = state_with_vault(vault);
        let vault = Arc::clone(state.secrets_vault.as_ref().expect("vault installed"));
        let gate = RecordingSecretGate::allowing(&["provider_api_key"]);
        let requested = gate.requested.clone();
        let resolver = state
            .authorized_wasm_secret_resolver(&tenant, Arc::new(gate), test_authz_ctx())
            .expect("resolver should exist when vault is configured");

        assert_eq!(resolver("provider_api_key"), Ok("first".to_string()));
        vault
            .cache_secret(tenant.as_str(), "provider_api_key", "second".into())
            .unwrap();
        assert_eq!(resolver("provider_api_key"), Ok("second".to_string()));

        let denied = resolver("blocked_key").expect_err("blocked key should be denied");
        assert!(denied.contains("authorization denied for secret 'blocked_key'"));
        assert_eq!(
            *requested.lock().expect("requested keys lock poisoned"),
            vec![
                "provider_api_key".to_string(),
                "provider_api_key".to_string(),
                "blocked_key".to_string()
            ]
        );
    }

    #[test]
    fn lazy_secret_resolver_never_exposes_reserved_root_key() {
        let tenant = TenantId::new("tenant-a");
        let vault = SecretsVault::new(&[9_u8; 32]);
        vault
            .cache_secret(tenant.as_str(), "temper_api_key", "deployment-root".into())
            .unwrap();
        let state = state_with_vault(vault);
        let gate = RecordingSecretGate::allowing(&["temper_api_key"]);
        let requested = gate.requested.clone();
        let resolver = state
            .authorized_wasm_secret_resolver(&tenant, Arc::new(gate), test_authz_ctx())
            .expect("resolver should exist when vault is configured");

        let error = resolver("temper_api_key").expect_err("root key must stay unavailable");
        assert!(error.contains("reserved"));
        assert!(
            requested
                .lock()
                .expect("lock should not be poisoned")
                .is_empty()
        );
    }
}
