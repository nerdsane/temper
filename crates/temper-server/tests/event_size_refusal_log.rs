//! Each change refused for its size is logged once, where the size check refuses,
//! so a count of those lines (by app, through a log collector) is the count of
//! refused changes: a create, an action and a batch alike. Its own test binary, so
//! no test on another thread shares the log callsite while this one listens.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, PersistenceAppend, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::storage::{BackendLabel, BoxedEventStore, EventSizeLimits};
use temper_server::{EntityActor, EntityMsg, EntityResponse, ServerState, StorageStack};
use temper_store_sim::SimEventStore;
use tracing_subscriber::prelude::*;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const LIMITS: EventSizeLimits = EventSizeLimits {
    warn_bytes: Some(1_000),
    max_bytes: Some(4_000),
};

/// The size lines logged, as "<kind> <persistence id>": kind refused or large.
#[derive(Clone, Default)]
struct Refusals(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Refusals {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        #[derive(Default)]
        struct Line {
            message: String,
            persistence_id: String,
        }
        impl tracing::field::Visit for Line {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "persistence_id" {
                    self.persistence_id = value.to_string();
                }
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                }
            }
        }
        let mut line = Line::default();
        event.record(&mut line);
        let kind = if line.message.starts_with("event refused for its size") {
            "refused"
        } else if line.message.starts_with("large event") {
            "large"
        } else {
            return;
        };
        self.0
            .lock()
            .unwrap()
            .push(format!("{kind} {}", line.persistence_id));
    }
}

fn envelope(persistence_id: &str, product: String) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "AddItem".into(),
        payload: serde_json::json!({ "action": "AddItem", "params": { "ProductId": product } }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.into(),
        },
    }
}

#[tokio::test]
async fn each_refusal_is_logged_once_for_a_create_an_action_and_a_batch() {
    let refusals = Refusals::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(refusals.clone()));
    let logged = || refusals.0.lock().unwrap().clone();

    let csdl = temper_spec::csdl::parse_csdl(CSDL_XML).unwrap();
    let mut registry = SpecRegistry::new();
    let tenant = TenantId::default();
    registry.register_tenant(
        tenant.as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("event-size-log"), registry);
    let store = SimEventStore::no_faults(17);
    state.set_storage_stack(
        StorageStack::from_sim(store.clone(), None).with_event_size_limits(LIMITS),
    );

    let create = state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "c-big",
            serde_json::json!({ "Notes": "x".repeat(5_000) }),
        )
        .await;
    assert!(create.is_err());
    assert_eq!(
        logged(),
        ["refused default:Order:c-big"],
        "a refused create, one line"
    );

    let system = ActorSystem::new("event-size-log-actor");
    let actor = EntityActor::with_persistence(
        "Order",
        "a-big",
        Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
        serde_json::json!({}),
        BoxedEventStore::new(store.clone()).with_event_size_limits(LIMITS),
        BackendLabel::Sim,
    );
    let actor = system.spawn(actor, "a-big");
    let refused: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                caller: None,
                name: "AddItem".into(),
                params: serde_json::json!({ "ProductId": "x".repeat(5_000) }),
                related: std::collections::BTreeMap::new(),
                idempotency_key: None,
                expected_authorization_precondition: None,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(!refused.success);
    assert_eq!(
        logged(),
        ["refused default:Order:c-big", "refused default:Order:a-big"],
        "a refused action, one line"
    );

    let journal = BoxedEventStore::new(store).with_event_size_limits(LIMITS);
    // A batch with one change over the warning size and one over the limit writes
    // nothing, so it warns about nothing: one refusal line only.
    let batch = [
        PersistenceAppend {
            persistence_id: "default:Order:b-warn".into(),
            expected_sequence: 0,
            events: vec![envelope("default:Order:b-warn", "w".repeat(2_000))],
            state_timer: None,
            projection: None,
        },
        PersistenceAppend {
            persistence_id: "default:Order:b-big".into(),
            expected_sequence: 0,
            events: vec![envelope("default:Order:b-big", "x".repeat(5_000))],
            state_timer: None,
            projection: None,
        },
    ];
    assert!(journal.append_batch(&batch).await.is_err());
    assert_eq!(
        logged(),
        [
            "refused default:Order:c-big",
            "refused default:Order:a-big",
            "refused default:Order:b-big"
        ],
        "a refused batch, one line, and no warning for what it did not write"
    );

    // The same batch without the oversized change is written, with its one warning.
    assert!(journal.append_batch(&batch[..1]).await.is_ok());
    assert_eq!(
        logged().last().map(String::as_str),
        Some("large default:Order:b-warn")
    );
}
