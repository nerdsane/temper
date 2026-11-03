//! Pause or fail only the writes relevant to review lifecycle regressions.
use super::*;
use temper_server::platform_store::{
    ModuleReviewRow, PolicyEntryRow, SpecRow, SpecVerificationUpdate, WasmModuleRow,
};
use tokio::sync::Notify;

pub(super) struct ReviewStore {
    inner: Arc<SimPlatformStore>,
    fail_install: bool,
    fail_save: bool,
    pub(super) fail_provenance: std::sync::atomic::AtomicBool,
    pub(super) spec_entered: Notify,
    pub(super) spec_release: Semaphore,
    pub(super) save_entered: Notify,
    pub(super) save_release: Semaphore,
}

impl ReviewStore {
    pub(super) fn attach(
        state: &mut PlatformState,
        inner: Arc<SimPlatformStore>,
        fail_install: bool,
        fail_save: bool,
    ) -> Arc<Self> {
        let store = Arc::new(Self {
            inner,
            fail_install,
            fail_save,
            fail_provenance: std::sync::atomic::AtomicBool::new(false),
            spec_entered: Notify::new(),
            spec_release: Semaphore::new(0),
            save_entered: Notify::new(),
            save_release: Semaphore::new(0),
        });
        let mut stack = temper_server::StorageStack::from_sim(
            temper_store_sim::SimEventStore::no_faults(0),
            None,
        );
        stack.platform = Some(store.clone());
        state.server.set_storage_stack(stack);
        store
    }
}

// Expand before async_trait so each delegated method gets the same lifetimes.
macro_rules! review_store {
    ($(async fn $method:ident(&self $(, $arg:ident: $ty:ty)* $(,)?) -> $result:ty;)*) => {
        #[async_trait::async_trait]
        impl PlatformStore for ReviewStore {
            async fn upsert_spec(&self, tenant: &str, entity_type: &str, ioa_source: &str, csdl_xml: &str, content_hash: &str) -> Result<(), String> {
                if self.fail_install {
                    self.spec_entered.notify_one();
                    self.spec_release.acquire().await.unwrap().forget();
                    return Err("injected spec write failure".into());
                }
                self.inner.upsert_spec(tenant, entity_type, ioa_source, csdl_xml, content_hash).await
            }
            async fn save_module_review(&self, tenant: &str, app_name: &str, version: &str, result: &str) -> Result<(), String> {
                self.save_entered.notify_one();
                // A failing install's review must be allowed to expose the old bug.
                if !self.fail_install {
                    self.save_release.acquire().await.unwrap().forget();
                }
                if self.fail_save {
                    return Err("injected review save failure".into());
                }
                self.inner.save_module_review(tenant, app_name, version, result).await
            }
            async fn record_installed_app_metadata(&self, record: &InstalledAppRecord) -> Result<(), String> {
                if record.source_kind == "pushed" && self.fail_provenance.load(std::sync::atomic::Ordering::SeqCst) {
                    self.spec_entered.notify_one();
                    self.spec_release.acquire().await.unwrap().forget();
                    return Err("injected provenance failure".into());
                }
                self.inner.record_installed_app_metadata(record).await
            }
            $(async fn $method(&self $(, $arg: $ty)*) -> $result {
                self.inner.$method($($arg),*).await
            })*
        }
    }
}

review_store! {
    async fn load_specs(&self) -> Result<Vec<SpecRow>, String>;
    async fn delete_spec(&self, tenant: &str, entity_type: &str) -> Result<(), String>;
    async fn commit_specs(&self, tenant: &str) -> Result<(), String>;
    async fn commit_spec_versions(&self, tenant: &str, versions: &[(&str, &str, &str)], removed: &[&str],) -> Result<bool, String>;
    async fn delete_uncommitted_specs(&self) -> Result<usize, String>;
    async fn load_verification_cache(&self, tenant: &str,) -> Result<BTreeMap<String, (String, bool)>, String>;
    async fn persist_spec_verification(&self, tenant: &str, entity_type: &str, update: SpecVerificationUpdate<'_>,) -> Result<(), String>;
    async fn upsert_tenant_policy(&self, tenant: &str, policy_text: &str) -> Result<(), String>;
    async fn upsert_tenant_constraints(&self, tenant: &str, cross_invariants_toml: &str,) -> Result<(), String>;
    async fn load_tenant_policies(&self) -> Result<Vec<(String, String)>, String>;
    async fn load_policy_entries(&self) -> Result<Vec<PolicyEntryRow>, String>;
    async fn is_app_installed(&self, tenant: &str, app_name: &str) -> Result<bool, String>;
    async fn record_installed_app(&self, tenant: &str, app_name: &str) -> Result<(), String>;
    async fn get_installed_app(&self, tenant: &str, app_name: &str,) -> Result<Option<InstalledAppRecord>, String>;
    async fn list_all_installed_apps(&self) -> Result<Vec<(String, String)>, String>;
    async fn upsert_pending_decision(&self, id: &str, tenant: &str, status: &str, data: &str,) -> Result<(), String>;
    async fn load_pending_decisions(&self, limit: usize) -> Result<Vec<String>, String>;
    async fn load_approved_session_decisions(&self, tenant: &str, session_id: &str,) -> Result<Vec<String>, String>;
    async fn load_all_wasm_modules(&self, tenant: &str) -> Result<Vec<WasmModuleRow>, String>;
    async fn load_wasm_modules_all_tenants(&self) -> Result<Vec<WasmModuleRow>, String>;
    async fn load_module_review(&self, tenant: &str, app_name: &str,) -> Result<Option<ModuleReviewRow>, String>;
    async fn upsert_wasm_module(&self, tenant: &str, name: &str, bytes: &[u8], hash: &str, source: &str,) -> Result<(), String>;
}
