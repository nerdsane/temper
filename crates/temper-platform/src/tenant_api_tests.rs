use std::sync::atomic::{AtomicUsize, Ordering};

use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::{BackendLabel, BoxedEventStore, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use temper_store_turso::{TenantUserRow, TursoEventStore};

use super::*;

const ITEM_IOA: &str = r#"
[automaton]
name = "Item"
states = ["New", "Ready"]
initial = "New"

[[action]]
name = "Create"
kind = "input"
from = ["New"]
to = "Ready"
params = ["Title"]

[[action]]
name = "Change"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["Title"]
"#;

const ITEM_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="Item">
    <Key><PropertyRef Name="Id"/></Key>
    <Property Name="Id" Type="Edm.String" Nullable="false"/>
    <Property Name="Title" Type="Edm.String"/>
    <Property Name="Status" Type="Edm.String"/>
  </EntityType>
  <EntityContainer Name="TestService">
    <EntitySet Name="Items" EntityType="Temper.Test.Item"/>
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

#[derive(Default)]
struct TestTenantAdminProvider {
    remove_calls: AtomicUsize,
}

fn unsupported_admin_call() -> PersistenceError {
    PersistenceError::Storage("unsupported test tenant-admin call".to_string())
}

#[async_trait::async_trait]
impl TursoStoreProvider for TestTenantAdminProvider {
    fn supports_tenant_admin(&self) -> bool {
        true
    }

    fn platform_store(&self) -> Option<TursoEventStore> {
        None
    }

    async fn store_for_tenant(&self, _tenant: &str) -> Option<TursoEventStore> {
        None
    }

    async fn all_stores(&self) -> Vec<TursoEventStore> {
        Vec::new()
    }

    async fn connected_tenants(&self) -> Vec<String> {
        Vec::new()
    }

    async fn tenants_for_user(
        &self,
        _user_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        Ok(Vec::new())
    }

    async fn register_tenant(&self, _tenant_id: &str) -> Result<TursoEventStore, PersistenceError> {
        Err(unsupported_admin_call())
    }

    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        Ok(Vec::new())
    }

    async fn remove_tenant(&self, _tenant_id: &str) -> Result<bool, PersistenceError> {
        self.remove_calls.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn add_tenant_user(
        &self,
        _tenant_id: &str,
        _user_id: &str,
        _role: &str,
    ) -> Result<(), PersistenceError> {
        Err(unsupported_admin_call())
    }

    async fn list_tenant_users(
        &self,
        _tenant_id: &str,
    ) -> Result<Vec<TenantUserRow>, PersistenceError> {
        Ok(Vec::new())
    }

    async fn remove_tenant_user(
        &self,
        _tenant_id: &str,
        _user_id: &str,
    ) -> Result<(), PersistenceError> {
        Err(unsupported_admin_call())
    }

    async fn ensure_tenant(&self, _tenant_id: &str) -> Result<bool, PersistenceError> {
        Err(unsupported_admin_call())
    }
}

#[tokio::test]
async fn tenant_delete_waits_for_an_in_flight_old_generation_append() {
    let tenant = TenantId::new("tenant-delete-fence");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.clone(),
        parse_csdl(ITEM_CSDL).expect("parse item CSDL"),
        ITEM_CSDL.to_string(),
        &[("Item", ITEM_IOA)],
    );
    let mut state = PlatformState::with_registry(registry, None);
    let store = SimEventStore::no_faults(18_911);
    let provider = Arc::new(TestTenantAdminProvider::default());
    state.server.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store.clone()),
        None,
        Some(provider.clone()),
        None,
        None,
        None,
        None,
        None,
        None,
    ));

    let created = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "in-flight",
            "Create",
            serde_json::json!({"Title": "before"}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch create");
    assert!(created.success, "create failed: {:?}", created.error);

    let gate = store.inject_append_gate(&format!("{tenant}:Item:in-flight"));
    let change_agent = AgentContext::default();
    let change = state.server.dispatch_tenant_action(
        &tenant,
        "Item",
        "in-flight",
        "Change",
        serde_json::json!({"Title": "after"}),
        &change_agent,
    );
    let delete_after_append_enters = async {
        gate.wait_until_blocked().await;
        let deletion = delete_tenant(
            State(state.clone()),
            axum::extract::Path(tenant.to_string()),
        );
        tokio::pin!(deletion);
        tokio::select! {
            biased;
            _ = &mut deletion => panic!("tenant deletion overtook the in-flight append"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(
            provider.remove_calls.load(Ordering::SeqCst),
            0,
            "persistence deletion must wait behind the generation writer"
        );
        assert!(
            state
                .registry
                .read()
                .unwrap()
                .get_table(&tenant, "Item")
                .is_some(),
            "registry removal overtook the in-flight append"
        );
        gate.release().await;
        let response = deletion.await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
    };

    let (changed, ()) = tokio::join!(change, delete_after_append_enters);
    let changed = changed.expect("dispatch in-flight change");
    assert!(changed.success, "change failed: {:?}", changed.error);
    assert_eq!(provider.remove_calls.load(Ordering::SeqCst), 1);
    assert!(
        state
            .registry
            .read()
            .unwrap()
            .get_table(&tenant, "Item")
            .is_none(),
        "tenant registry entry must be absent after deletion"
    );

    let rejected = state
        .server
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "in-flight",
            "Change",
            serde_json::json!({"Title": "must-not-append"}),
            &AgentContext::default(),
        )
        .await;
    assert!(rejected.is_err(), "deleted tenant must reject later writes");
}
