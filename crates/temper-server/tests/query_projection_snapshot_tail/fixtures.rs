use std::future::Future;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::EntityState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::ServerState;
use temper_server::storage::{BoxedEventStore, StorageStack};
use temper_spec::csdl::parse_csdl;

pub(super) const ENTITY_ID: &str = "same-order-id";
const CSDL: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

pub(super) fn run_isolated<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("isolated runtime");
    let result = runtime.block_on(future);
    // Dropping ServerState alone leaves detached actors/projection/snapshot tasks alive.
    // Destroy their entire executor before seeding drift or opening the cold reader.
    drop(runtime);
    result
}

pub(super) fn build_state(storage: StorageStack) -> ServerState {
    let mut registry = SpecRegistry::new();
    for tenant in ["tenant-a", "tenant-b"] {
        registry.register_tenant(
            tenant,
            parse_csdl(CSDL).expect("fixture CSDL"),
            CSDL.to_string(),
            &[("Order", ORDER)],
        );
    }
    let mut state = ServerState::from_registry(ActorSystem::new("snapshot-tail"), registry);
    state.set_storage_stack(storage);
    state
}

pub(super) struct Checkpoint {
    pub tenant: TenantId,
    pub snapshot: EntityState,
    pub current: EntityState,
}

impl Checkpoint {
    pub fn persistence_id(&self) -> String {
        format!("{}:Order:{ENTITY_ID}", self.tenant)
    }

    pub async fn assert_durable_tail(&self, store: &BoxedEventStore) {
        let pid = self.persistence_id();
        let (seq, bytes) = store.load_snapshot(&pid).await.unwrap().unwrap();
        assert_eq!(seq, self.snapshot.sequence_nr);
        let snapshot: EntityState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snapshot.fields, self.snapshot.fields);
        assert_eq!(snapshot.counters, self.snapshot.counters);
        let tail = store.read_events(&pid, seq).await.unwrap();
        assert!(!tail.is_empty(), "fixture must contain committed live tail");
        assert_eq!(tail.last().unwrap().sequence_nr, self.current.sequence_nr);
        for (offset, event) in tail.iter().enumerate() {
            assert_eq!(event.sequence_nr, seq + offset as u64 + 1);
            assert_eq!(event.event_type, "AddItem");
        }
    }
}

pub(super) async fn write_snapshot_and_tail(
    state: &ServerState,
    store: &BoxedEventStore,
    tenant_name: &str,
    tail_len: usize,
) -> Checkpoint {
    let tenant = TenantId::new(tenant_name);
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            ENTITY_ID,
            serde_json::json!({"Title": tenant_name}),
        )
        .await
        .expect("create live order");
    let snapshot = add_item(state, &tenant, "snapshot-product", 1).await;
    assert_eq!(snapshot.counters["items"], 1);
    store
        .save_snapshot(
            &format!("{tenant}:Order:{ENTITY_ID}"),
            snapshot.sequence_nr,
            &serde_json::to_vec(&snapshot).unwrap(),
        )
        .await
        .expect("persist explicit checkpoint S");
    let mut current = snapshot.clone();
    for offset in 0..tail_len {
        current = add_item(
            state,
            &tenant,
            &format!("{tenant}-tail-{offset}"),
            offset + 2,
        )
        .await;
    }
    assert!(current.sequence_nr > snapshot.sequence_nr);
    assert_eq!(current.counters["items"], 1 + tail_len);
    assert_ne!(current.fields["ProductId"], snapshot.fields["ProductId"]);
    let checkpoint = Checkpoint {
        tenant,
        snapshot,
        current,
    };
    checkpoint.assert_durable_tail(store).await;
    checkpoint
}

async fn add_item(
    state: &ServerState,
    tenant: &TenantId,
    product: &str,
    quantity: usize,
) -> EntityState {
    let response = state
        .dispatch_tenant_action(
            tenant,
            "Order",
            ENTITY_ID,
            "AddItem",
            serde_json::json!({"ProductId": product, "Quantity": quantity}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch live AddItem");
    assert!(
        response.success,
        "live tail action must commit: {response:?}"
    );
    response.state
}

pub(super) fn projected_state(state: &EntityState) -> serde_json::Value {
    let mut value = serde_json::to_value(state).unwrap();
    value["events"] = serde_json::json!([]);
    value
}
