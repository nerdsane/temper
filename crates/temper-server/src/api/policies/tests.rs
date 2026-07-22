//! Policy publication regression tests.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use super::*;
use crate::storage::{BackendLabel, BoxedEventStore, PolicyStore, PolicyStoreRow, StorageStack};

const CSDL_XML: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
const OLD_POLICY: &str = "permit(principal, action, resource) when { resource.old == true };";
const NEW_POLICY: &str = "permit(principal, action, resource) when { resource.new == true };";
const EXTRA_POLICY: &str = "permit(principal, action, resource) when { resource.extra == true };";

#[derive(Default)]
struct FaultPolicyStore {
    rows: Mutex<Vec<PolicyStoreRow>>,
    compatibility_text: Mutex<Option<String>>,
    // 0 = no fault, 1 = fail before write, 2 = fail after write.
    next_save_fault: AtomicU8,
}

impl FaultPolicyStore {
    fn with_primary(cedar_text: &str) -> Self {
        Self {
            rows: Mutex::new(vec![PolicyStoreRow {
                tenant: "default".to_string(),
                policy_id: "primary".to_string(),
                cedar_text: cedar_text.to_string(),
                policy_hash: "test-hash".to_string(),
                created_at: "1970-01-01T00:00:00Z".to_string(),
                created_by: "test".to_string(),
                enabled: true,
            }]),
            compatibility_text: Mutex::new(Some(cedar_text.to_string())),
            next_save_fault: AtomicU8::new(0),
        }
    }

    fn fail_next_save_after_commit(&self) {
        self.next_save_fault.store(2, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl PolicyStore for FaultPolicyStore {
    async fn replace_policy_generation(
        &self,
        tenant: &str,
        entries: &[crate::storage::PolicyGenerationWrite],
        compatibility_text: &str,
    ) -> Result<(), String> {
        let fault = self.next_save_fault.swap(0, Ordering::SeqCst);
        if fault == 1 {
            return Err("injected pre-commit policy fault".to_string());
        }
        let replacement = entries
            .iter()
            .map(|entry| PolicyStoreRow {
                tenant: tenant.to_string(),
                policy_id: entry.policy_id.clone(),
                cedar_text: entry.cedar_text.clone(),
                policy_hash: "test-hash".to_string(),
                created_at: "1970-01-01T00:00:00Z".to_string(),
                created_by: entry.created_by.clone(),
                enabled: entry.enabled,
            })
            .collect::<Vec<_>>();
        *self.rows.lock().expect("policy rows lock poisoned") = replacement;
        *self
            .compatibility_text
            .lock()
            .expect("policy compatibility lock poisoned") = Some(compatibility_text.to_string());
        if fault == 2 {
            return Err("injected ambiguous post-commit policy fault".to_string());
        }
        Ok(())
    }

    async fn load_policy_compatibility_text(
        &self,
        _tenant: &str,
    ) -> Result<Option<String>, String> {
        Ok(self
            .compatibility_text
            .lock()
            .expect("policy compatibility lock poisoned")
            .clone())
    }

    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let fault = self.next_save_fault.swap(0, Ordering::SeqCst);
        if fault == 1 {
            return Err("injected pre-commit policy fault".to_string());
        }
        let mut rows = self.rows.lock().expect("policy rows lock poisoned");
        if let Some(row) = rows
            .iter_mut()
            .find(|row| row.tenant == tenant && row.policy_id == policy_id)
        {
            row.cedar_text = cedar_text.to_string();
            row.created_by = created_by.to_string();
            row.enabled = true;
        } else {
            rows.push(PolicyStoreRow {
                tenant: tenant.to_string(),
                policy_id: policy_id.to_string(),
                cedar_text: cedar_text.to_string(),
                policy_hash: "test-hash".to_string(),
                created_at: "1970-01-01T00:00:00Z".to_string(),
                created_by: created_by.to_string(),
                enabled: true,
            });
        }
        if fault == 2 {
            return Err("injected ambiguous post-commit policy fault".to_string());
        }
        Ok(true)
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        Ok(self
            .rows
            .lock()
            .expect("policy rows lock poisoned")
            .iter()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect())
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        Ok(self.rows.lock().expect("policy rows lock poisoned").clone())
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("policy rows lock poisoned");
        let Some(row) = rows
            .iter_mut()
            .find(|row| row.tenant == tenant && row.policy_id == policy_id)
        else {
            return Ok(false);
        };
        row.enabled = enabled;
        Ok(true)
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("policy rows lock poisoned");
        let Some(row) = rows
            .iter_mut()
            .find(|row| row.tenant == tenant && row.policy_id == policy_id)
        else {
            return Ok(false);
        };
        row.cedar_text = cedar_text.to_string();
        row.created_by = created_by.to_string();
        Ok(true)
    }

    async fn replace_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        enabled: bool,
        created_by: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("policy rows lock poisoned");
        let Some(row) = rows
            .iter_mut()
            .find(|row| row.tenant == tenant && row.policy_id == policy_id)
        else {
            return Ok(false);
        };
        row.cedar_text = cedar_text.to_string();
        row.enabled = enabled;
        row.created_by = created_by.to_string();
        Ok(true)
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.rows
            .lock()
            .expect("policy rows lock poisoned")
            .retain(|row| row.tenant != tenant || row.policy_id != policy_id);
        Ok(())
    }
}

fn policy_test_state(store: Arc<FaultPolicyStore>) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("test CSDL should parse");
    let mut state = ServerState::new(
        ActorSystem::new("policy-generation-test"),
        csdl,
        CSDL_XML.to_string(),
    );
    let events = Arc::new(SimEventStore::no_faults(238));
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::from_arc(events),
        None,
        None,
        None,
        Some(store),
        None,
        None,
        None,
        None,
    ));
    state
        .authz
        .reload_tenant_policies_named(
            "default",
            &[("primary".to_string(), OLD_POLICY.to_string())],
        )
        .expect("activate initial policy");
    state
}

#[tokio::test]
async fn ambiguous_policy_commit_stays_gated_until_exact_retry_activates_it() {
    let store = Arc::new(FaultPolicyStore::with_primary(OLD_POLICY));
    let state = policy_test_state(Arc::clone(&store));
    let tenant = TenantId::default();
    store.fail_next_save_after_commit();

    assert!(
        publish_policy_upsert(&state, "default", "primary", NEW_POLICY, "test", None, None,)
            .await
            .is_err()
    );
    assert_eq!(
        state.authz.get_tenant_policy_text("default").as_deref(),
        Some(OLD_POLICY)
    );
    assert_eq!(
        store
            .load_policies_for_tenant("default")
            .await
            .expect("load committed policy")[0]
            .cedar_text,
        NEW_POLICY
    );
    assert!(state.spec_publication_gated(&tenant));

    assert!(
        publish_policy_upsert(
            &state,
            "default",
            "primary",
            "permit(principal, action, resource);",
            "test",
            None,
            None,
        )
        .await
        .is_err()
    );
    assert!(state.spec_publication_gated(&tenant));

    publish_policy_upsert(&state, "default", "primary", NEW_POLICY, "test", None, None)
        .await
        .expect("exact retry should finish the durable generation");
    assert_eq!(
        state.authz.get_tenant_policy_text("default").as_deref(),
        Some(NEW_POLICY)
    );
    assert!(!state.spec_publication_gated(&tenant));
}

#[tokio::test]
async fn stable_tenant_reader_rejects_policy_generation_without_mutation() {
    let store = Arc::new(FaultPolicyStore::with_primary(OLD_POLICY));
    let state = policy_test_state(Arc::clone(&store));
    let tenant = TenantId::default();
    let stable_reader = state.begin_tenant_request(&tenant).await;

    assert!(
        publish_policy_upsert(&state, "default", "primary", NEW_POLICY, "test", None, None,)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .load_policies_for_tenant("default")
            .await
            .expect("load unchanged policy")[0]
            .cedar_text,
        OLD_POLICY
    );
    assert_eq!(
        state.authz.get_tenant_policy_text("default").as_deref(),
        Some(OLD_POLICY)
    );

    drop(stable_reader);
    publish_policy_upsert(&state, "default", "primary", NEW_POLICY, "test", None, None)
        .await
        .expect("policy generation should publish after reader exits");
}

#[tokio::test]
async fn first_granular_policy_write_seeds_the_complete_legacy_generation() {
    let store = Arc::new(FaultPolicyStore::default());
    let state = policy_test_state(Arc::clone(&store));
    let added = "permit(principal, action, resource) when { resource.added == true };";

    publish_policy_upsert(&state, "default", "added-policy", added, "test", None, None)
        .await
        .expect("first granular mutation must preserve legacy policy authority");

    let rows = store
        .load_policies_for_tenant("default")
        .await
        .expect("load migrated generation");
    assert!(
        rows.iter()
            .any(|row| row.policy_id == "primary" && row.cedar_text.contains("resource.old")),
        "legacy grants must be durably represented by the primary migration row"
    );
    assert!(
        rows.iter()
            .any(|row| row.policy_id == "added-policy" && row.cedar_text == added)
    );
    let active = state
        .authz
        .get_tenant_policy_text("default")
        .expect("active combined generation");
    assert!(active.contains("resource.old"));
    assert!(active.contains("resource.added"));
}

#[tokio::test]
async fn upserting_a_disabled_policy_reenables_the_same_live_generation() {
    let store = Arc::new(FaultPolicyStore::with_primary(OLD_POLICY));
    store
        .toggle_policy_enabled("default", "primary", false)
        .await
        .expect("disable durable primary policy");
    let state = policy_test_state(Arc::clone(&store));
    state
        .authz
        .reload_tenant_policies_named("default", &[])
        .expect("start with no live policies");

    publish_policy_upsert(&state, "default", "primary", NEW_POLICY, "test", None, None)
        .await
        .expect("upsert should durably and live re-enable the policy");

    let rows = store
        .load_policies_for_tenant("default")
        .await
        .expect("load re-enabled policy");
    assert!(rows[0].enabled, "the durable row must be enabled");
    assert_eq!(rows[0].cedar_text, NEW_POLICY);
    assert_eq!(
        state.authz.get_tenant_policy_text("default").as_deref(),
        Some(NEW_POLICY),
        "the generation activated by the successful request must include the row"
    );
}

#[tokio::test]
async fn durable_replace_all_removes_every_non_primary_policy_row() {
    let store = Arc::new(FaultPolicyStore::with_primary(OLD_POLICY));
    store
        .save_policy("default", "extra", EXTRA_POLICY, "test")
        .await
        .expect("seed extra durable policy");
    let state = policy_test_state(Arc::clone(&store));

    publish_policy_replace_all(&state, "default", NEW_POLICY, "test", None, None)
        .await
        .expect("replace the durable policy generation");

    let rows = store
        .load_policies_for_tenant("default")
        .await
        .expect("load replaced generation");
    assert_eq!(rows.len(), 1, "replace-all must remove extra durable rows");
    assert_eq!(rows[0].policy_id, "primary");
    assert_eq!(rows[0].cedar_text, NEW_POLICY);
    assert_eq!(
        state.authz.get_tenant_policy_text("default").as_deref(),
        Some(NEW_POLICY),
        "the live generation must match the exact durable replacement"
    );
}

#[tokio::test]
async fn deleting_the_last_policy_row_cannot_resurrect_the_legacy_generation_on_restart() {
    let store = Arc::new(FaultPolicyStore::with_primary(OLD_POLICY));
    let state = policy_test_state(Arc::clone(&store));
    crate::authz::policy_persistence::persist_complete_policy_generation(
        &state,
        "default",
        &[],
        "primary",
        "test",
    )
    .await
    .expect("publish explicitly empty generation");

    let restarted = policy_test_state(Arc::clone(&store));
    crate::authz::load_and_activate_tenant_policies(&restarted, "default").await;

    assert!(
        store
            .load_policies_for_tenant("default")
            .await
            .expect("load restarted generation")
            .is_empty(),
        "an explicitly empty granular generation must remain empty after restart"
    );
    assert!(
        restarted
            .authz
            .get_tenant_policy_text("default")
            .is_none_or(|text| text.is_empty()),
        "stale compatibility text must not be promoted after deleting the last row"
    );
}
