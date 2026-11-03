//! A record whose append hangs says so while it hangs, naming the await, and
//! says when it ends; the event is written once.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::registry::SpecRegistry;
use crate::{ServerState, StorageStack};
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use crate::storage::{BackendLabel, BoxedEventStore};

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

/// Every log line written while the guard lives, as text.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn with(&self, words: &[&str]) -> usize {
        self.text()
            .lines()
            .filter(|line| words.iter().all(|w| line.contains(w)))
            .count()
    }
}

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn add_item() -> EntityMsg {
    EntityMsg::Action {
        caller: None,
        name: "AddItem".into(),
        params: serde_json::json!({ "ProductId": "p-1" }),
        related: BTreeMap::new(),
        idempotency_key: None,
        expected_authorization_precondition: None,
    }
}

#[tokio::test(start_paused = true)]
async fn dst_a_hung_append_is_reported_until_it_ends_and_is_written_once() {
    for (seed, deadline) in
        (0..8).flat_map(|seed| [5, 45].map(move |secs| (seed, Duration::from_secs(secs))))
    {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _logs = tracing::subscriber::set_default(subscriber);

        let store = Arc::new(SimEventStore::no_faults(seed));
        let mut server = server((*store).clone());
        server.action_dispatch_timeout = deadline;
        let actor = server
            .get_or_spawn_tenant_actor(&TenantId::default(), "Order", "hung")
            .unwrap();
        let _: EntityResponse = actor
            .ask(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .unwrap();
        let written_before = store.dump_journal("default:Order:hung").len();

        store.hang_next_append("default:Order:hung");
        let asked = actor.clone();
        let reply = tokio::spawn(async move {
            asked
                .ask::<EntityResponse>(add_item(), Duration::from_secs(600))
                .await
        });

        // Quiet until the ask deadline; then once, and again a minute later.
        tokio::time::sleep(deadline - Duration::from_secs(1)).await;
        assert_eq!(lines.with(&["handler still running"]), 0, "seed {seed}");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            lines.with(&["handler still running"]),
            1,
            "seed {seed}, deadline {deadline:?}: {}",
            lines.text()
        );
        tokio::time::sleep(Duration::from_secs(60)).await;
        let reports = [
            "handler still running",
            "entity_type=Order",
            "entity_id=hung",
            "action=\"AddItem\"",
            "awaiting=\"appending to the journal\"",
        ];
        assert_eq!(lines.with(&reports), 2, "seed {seed}: {}", lines.text());

        assert!(
            store.let_hung_appends_go(),
            "seed {seed}: the hang met the append"
        );
        let reply = reply
            .await
            .unwrap()
            .expect("the ask is answered once the append ends");
        assert!(reply.success, "seed {seed}: {:?}", reply.error);
        assert_eq!(
            lines.with(&["handler finished", "action=\"AddItem\""]),
            1,
            "seed {seed}"
        );
        assert_eq!(
            store.dump_journal("default:Order:hung").len(),
            written_before + 1,
            "seed {seed}: the hung append is written once"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_quick_message_is_never_reported() {
    let (_guard, _clock, _ids) = install_deterministic_context(3);
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _logs = tracing::subscriber::set_default(subscriber);

    let store = Arc::new(SimEventStore::no_faults(3));
    let actor = EntityActor::with_persistence(
        "Order",
        "quick",
        Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
        serde_json::json!({}),
        BoxedEventStore::from_arc(store),
        BackendLabel::Sim,
    );
    let actor = ActorSystem::new("watchdog").spawn(actor, "quick");
    let reply: EntityResponse = actor.ask(add_item(), Duration::from_secs(5)).await.unwrap();
    assert!(reply.success, "{:?}", reply.error);
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(lines.with(&["handler still running"]), 0);
    assert_eq!(lines.with(&["handler finished"]), 0);
}

fn server(store: SimEventStore) -> ServerState {
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(csdl).unwrap(),
        csdl.to_owned(),
        &[("Order", ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("watchdog"), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

#[path = "watchdog_faults_test.rs"]
mod faults;
