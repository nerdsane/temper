//! Production data-only creation over a Sim journal and an in-memory projection observer.
use super::*;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use temper_runtime::{persistence::PersistenceError, tenant::TenantId};
use temper_server::storage::{QueryPlaneStore, QueryProjectionFieldsRow, StorageStack};

#[derive(Default)]
struct Projection(Mutex<Option<Value>>);

#[async_trait::async_trait]
impl QueryPlaneStore for Projection {
    async fn upsert_projection(
        &self,
        tenant: &str,
        kind: &str,
        id: &str,
        status: &str,
        fields: &Value,
        state: &Value,
        sequence: u64,
    ) -> Result<(), PersistenceError> {
        assert_eq!(
            (tenant, kind, id, status, sequence),
            ("acme", "Order", "owned", "Draft", 1)
        );
        assert_eq!(fields["Notes"], "writable");
        assert!(state["fields"].get("units").is_none());
        *self.0.lock().unwrap() = Some(state.clone());
        Ok(())
    }
    async fn remove_projection(&self, _: &str, _: &str, _: &str) -> Result<(), PersistenceError> {
        panic!("unexpected delete")
    }
    async fn query_field_index(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        panic!("unexpected query")
    }
    async fn load_projection_fields_many(
        &self,
        _: &str,
        _: &str,
        _: &[String],
        _: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        panic!("unexpected read")
    }
    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

#[tokio::test]
async fn direct_data_only_create_does_not_journal_forged_owned_inputs() {
    let store = SimEventStore::no_faults(554);
    let mut live = state(store.clone(), false);
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let spec = SPEC.split("[[action]]").next().unwrap();
    live.registry.write().unwrap().register_tenant(
        "acme",
        temper_spec::csdl::parse_csdl(csdl).unwrap(),
        csdl.into(),
        &[("Order", spec)],
    );
    let projection = Arc::new(Projection::default());
    let mut stack = StorageStack::from_sim(store.clone(), None);
    stack.query_plane = Some(projection.clone());
    live.set_storage_stack(stack);
    let response = live.try_create_data_only_tenant_entity(&TenantId::new("acme"), "Order", "owned", json!({
        "units":900, "Units":901, "approved":true, "reviewers":["forged"], "Notes":"writable"
    })).await.unwrap().expect("must exercise the data-only fast path");
    assert!(response.success);
    assert_eq!(
        live.active_actor_count(),
        0,
        "fast path must not spawn an actor"
    );
    let journal = store.read_events("acme:Order:owned", 0).await.unwrap();
    assert_eq!(journal.len(), 1);
    let event = &journal[0].payload;
    for name in ["units", "Units", "approved", "reviewers"] {
        assert!(
            event["params"].get(name).is_none(),
            "direct create journal contains ignored {name}: {event}"
        );
        assert!(event["initial_values"]["fields"].get(name).is_none());
    }
    assert_eq!(event["params"], json!({"Notes":"writable"}));
    assert_eq!(
        projection.0.lock().unwrap().as_ref().unwrap()["fields"],
        response.state.fields
    );
}
