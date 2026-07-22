//! PlatformStore implementation for the deterministic simulator.

use super::*;

#[async_trait::async_trait]
impl PlatformStore for SimPlatformStore {
    async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
        content_hash: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.spec_write_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected spec write failure".into());
        }

        let key = (tenant.to_string(), entity_type.to_string());
        inner.specs.insert(
            key,
            SpecRow {
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                ioa_source: ioa_source.to_string(),
                csdl_xml: Some(csdl_xml.to_string()),
                content_hash: content_hash.to_string(),
                committed: false,
            },
        );
        Ok(())
    }

    async fn publish_specs(
        &self,
        tenant: &str,
        specs: &[SpecPublication<'_>],
        mode: SpecPublicationMode,
        constraints: TenantConstraintsPublication<'_>,
        policy: TenantPolicyPublication<'_>,
        os_app: Option<OsAppPublication<'_>>,
        policy_generation: Option<PolicyGenerationPublication<'_>>,
        wasm_modules: &[WasmPublication<'_>],
    ) -> Result<Vec<String>, String> {
        self.publish_specs_inner(
            tenant,
            specs,
            mode,
            constraints,
            policy,
            os_app,
            policy_generation,
            wasm_modules,
        )
        .await
    }
    async fn load_specs(&self) -> Result<Vec<SpecRow>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.spec_read_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected spec read failure".into());
        }

        Ok(inner
            .specs
            .values()
            .filter(|s| s.committed)
            .cloned()
            .collect())
    }

    async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        let prob = inner.faults.cleanup_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected cleanup failure".into());
        }
        inner
            .specs
            .remove(&(tenant.to_string(), entity_type.to_string()));
        Ok(())
    }

    async fn commit_specs(&self, tenant: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        for spec in inner.specs.values_mut() {
            if spec.tenant == tenant {
                spec.committed = true;
            }
        }
        Ok(())
    }

    async fn delete_uncommitted_specs(&self) -> Result<usize, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock
        let before = inner.specs.len();
        inner.specs.retain(|_, s| s.committed);
        Ok(before - inner.specs.len())
    }

    async fn load_verification_cache(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, (String, bool)>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.spec_read_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected verification cache read failure".into());
        }

        let mut cache = BTreeMap::new();
        for ((t, et), (hash, verified)) in &inner.verification_cache {
            if t == tenant {
                cache.insert(et.clone(), (hash.clone(), *verified));
            }
        }
        Ok(cache)
    }

    async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        update: SpecVerificationUpdate<'_>,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.spec_write_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected verification write failure".into());
        }

        let key = (tenant.to_string(), entity_type.to_string());
        inner
            .verification_cache
            .insert(key, (update.status.to_string(), update.verified));
        Ok(())
    }

    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.policy_write_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected policy write failure".into());
        }

        inner
            .policies
            .insert(tenant.to_string(), policy_text.to_string());
        Ok(())
    }

    async fn upsert_tenant_constraints(
        &self,
        tenant: &str,
        cross_invariants_toml: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.policy_write_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected constraints write failure".into());
        }

        inner
            .constraints
            .insert(tenant.to_string(), cross_invariants_toml.to_string());
        Ok(())
    }

    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.policy_read_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected policy read failure".into());
        }

        Ok(inner
            .policies
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    async fn load_policy_entries(&self) -> Result<Vec<PolicyEntryRow>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.policy_read_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected granular policy read failure".into());
        }

        Ok(inner.policy_entries.values().cloned().collect())
    }

    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.app_list_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected app query failure".into());
        }

        Ok(inner
            .installed_apps
            .contains(&(tenant.to_string(), app_name.to_string())))
    }

    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.app_record_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected app record failure".into());
        }

        inner
            .installed_apps
            .insert((tenant.to_string(), app_name.to_string()));
        Ok(())
    }

    async fn record_installed_app_metadata(
        &self,
        record: &InstalledAppRecord,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.app_record_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected app metadata record failure".into());
        }

        let key = (record.tenant.clone(), record.app_name.clone());
        inner.installed_apps.insert(key.clone());
        inner.installed_app_records.insert(key, record.clone());
        Ok(())
    }

    async fn get_installed_app(
        &self,
        tenant: &str,
        app_name: &str,
    ) -> Result<Option<InstalledAppRecord>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.app_list_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected app metadata read failure".into());
        }

        Ok(inner
            .installed_app_records
            .get(&(tenant.to_string(), app_name.to_string()))
            .cloned())
    }

    async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.app_list_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected app list failure".into());
        }

        Ok(inner.installed_apps.iter().cloned().collect())
    }

    async fn upsert_pending_decision(
        &self,
        id: &str,
        tenant: &str,
        status: &str,
        data: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.decision_write_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected decision write failure".into());
        }

        inner.pending_decisions.insert(
            id.to_string(),
            (tenant.to_string(), status.to_string(), data.to_string()),
        );
        Ok(())
    }

    async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.decision_read_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected decision read failure".into());
        }

        Ok(inner
            .pending_decisions
            .values()
            .rev()
            .take(limit)
            .map(|(_, _, data)| data.clone())
            .collect())
    }

    async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.wasm_read_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected WASM read failure".into());
        }

        Ok(inner
            .wasm_modules
            .values()
            .filter(|m| m.tenant == tenant)
            .cloned()
            .collect())
    }

    async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.wasm_read_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected WASM read failure".into());
        }

        Ok(inner.wasm_modules.values().cloned().collect())
    }

    async fn upsert_wasm_module(
        &self,
        tenant: &str,
        name: &str,
        bytes: &[u8],
        hash: &str,
        source: &str,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("SimPlatformStore lock poisoned"); // ci-ok: infallible lock

        let prob = inner.faults.spec_write_failure_prob;
        if inner.rng.chance(prob) {
            return Err("SimPlatformStore: injected WASM write failure".into());
        }

        let key = (tenant.to_string(), name.to_string());
        let replace_uploaded_wasm = source == "bundled-replace-upload";
        let persisted_source = if replace_uploaded_wasm {
            "bundled"
        } else {
            source
        };
        let should_replace = inner.wasm_modules.get(&key).is_none_or(|existing| {
            existing.sha256_hash != hash
                && (replace_uploaded_wasm
                    || persisted_source == "upload"
                    || existing.source == "bundled")
        });
        if should_replace {
            inner.wasm_modules.insert(
                key,
                WasmModuleRow {
                    tenant: tenant.to_string(),
                    module_name: name.to_string(),
                    wasm_bytes: bytes.to_vec(),
                    sha256_hash: hash.to_string(),
                    source: persisted_source.to_string(),
                },
            );
        }
        Ok(())
    }
}
