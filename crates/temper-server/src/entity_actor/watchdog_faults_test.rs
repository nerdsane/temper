//! Run real startup, snapshot and blob paths with individual I/O waits held.

use super::{Lines, ORDER_IOA, add_item};
use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use crate::storage::{BackendLabel, BoxedEventStore};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;
use tokio::sync::watch;

/// A snapshot load (startup) or save that cannot finish until released.
struct HeldSnapshots {
    store: SimEventStore,
    held: watch::Receiver<bool>,
    hold_load: bool,
}

impl HeldSnapshots {
    async fn wait(&self) {
        let _ = self.held.clone().wait_for(|held| !held).await;
    }
}

impl EventStore for HeldSnapshots {
    async fn append(
        &self,
        id: &str,
        seq: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.store.append(id, seq, events).await
    }
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.store.append_batch(appends).await
    }
    async fn read_events(
        &self,
        id: &str,
        from: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.store.read_events(id, from).await
    }
    async fn read_events_page(
        &self,
        id: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.store.read_events_page(id, before, limit).await
    }
    async fn save_snapshot(
        &self,
        id: &str,
        seq: u64,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        if !self.hold_load {
            self.wait().await;
        }
        self.store.save_snapshot(id, seq, bytes).await
    }
    async fn load_snapshot(&self, id: &str) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        if self.hold_load {
            self.wait().await;
        }
        self.store.load_snapshot(id).await
    }
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.store.list_entity_ids(tenant).await
    }
    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        kind: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.store.list_entity_ids_by_type(tenant, kind).await
    }
}

async fn reported(lines: &Lines, action: &str, awaiting: &str) {
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        lines.with(&["handler still running"]),
        0,
        "{}",
        lines.text()
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let action = format!("action=\"{action}\"");
    let awaiting = format!("awaiting=\"{awaiting}\"");
    let fields = [
        "handler still running",
        "tenant=default",
        "entity_id=held",
        &action,
        &awaiting,
    ];
    assert_eq!(lines.with(&fields), 1, "{}", lines.text());
    assert_eq!(
        lines.with(&["handler still running", "elapsed_secs=7"]),
        1,
        "{}",
        lines.text()
    );
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(lines.with(&fields), 2, "{}", lines.text());
    assert_eq!(
        lines.with(&["handler still running", "elapsed_secs=67"]),
        1,
        "{}",
        lines.text()
    );
    assert_eq!(lines.with(&["handler finished"]), 0, "{}", lines.text());
}

#[tokio::test(start_paused = true)]
async fn dst_hung_startup_and_snapshot_report_their_await_and_completion() {
    for (seed, startup) in (0..8).flat_map(|seed| [true, false].map(move |start| (seed, start))) {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let lines = Lines::default();
        let writer = lines.clone();
        let _logs = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let store = SimEventStore::no_faults(seed);
        let (held, receiver) = watch::channel(true);
        let actor = EntityActor::with_persistence(
            "Order",
            "held",
            Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
            serde_json::json!({}),
            BoxedEventStore::new(HeldSnapshots {
                store: store.clone(),
                held: receiver,
                hold_load: startup,
            }),
            BackendLabel::Sim,
        )
        .with_action_dispatch_timeout(Duration::from_secs(7));
        let actor = ActorSystem::new("watchdog").spawn(actor, "held");
        if !startup {
            // Created is sequence 1. Reach the real configured snapshot boundary,
            // leaving the final action blocked inside save_snapshot.
            for _ in 1..EntityActor::snapshot_interval() - 1 {
                let reply: EntityResponse =
                    actor.ask(add_item(), Duration::from_secs(5)).await.unwrap();
                assert!(reply.success, "{:?}", reply.error);
            }
        }
        let asked = actor.clone();
        let reply = tokio::spawn(async move {
            asked
                .ask::<EntityResponse>(
                    if startup {
                        EntityMsg::GetState
                    } else {
                        add_item()
                    },
                    Duration::from_secs(600),
                )
                .await
        });
        reported(
            &lines,
            if startup { "start" } else { "AddItem" },
            if startup {
                "replaying the journal"
            } else {
                "snapshotting"
            },
        )
        .await;
        held.send_replace(false);
        let reply = reply.await.unwrap().unwrap();
        assert!(reply.success, "seed {seed}: {:?}", reply.error);
        assert_eq!(
            lines.with(&["handler finished"]),
            1,
            "seed {seed}: {}",
            lines.text()
        );
        let reports = lines.with(&["handler still running"]);
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(lines.with(&["handler still running"]), reports);
        if !startup {
            assert!(
                store
                    .load_snapshot("default:Order:held")
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn dst_hung_blob_reads_and_writes_report_their_await_and_completion() {
    for (index, (seed, read)) in (0..8)
        .flat_map(|seed| [true, false].map(move |read| (seed, read)))
        .enumerate()
    {
        // Only the held portion uses paused time. File I/O after release runs
        // with the clock resumed, so Tokio cannot jump past a disk operation.
        if index > 0 {
            tokio::time::pause();
        }
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let lines = Lines::default();
        let writer = lines.clone();
        let _logs = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let dir = tempfile::tempdir().unwrap();
        let mut blobs = crate::blob_store::BlobStore::local_fs(dir.path());
        let (held, receiver) = watch::channel(!read);
        blobs.io_pause = Some(receiver);
        let store = SimEventStore::no_faults(seed);
        let actor = EntityActor::with_persistence(
            "Document",
            "held",
            Arc::new(RwLock::new(TransitionTable::from_ioa_source(
                crate::entity_actor::actor::contract_state_tests::OVERFLOW_CONTRACT,
            ))),
            serde_json::json!({}),
            BoxedEventStore::new(store.clone()),
            BackendLabel::Sim,
        )
        .with_blob_store(Some(blobs))
        .with_action_dispatch_timeout(Duration::from_secs(7));
        let actor = ActorSystem::new("watchdog").spawn(actor, "held");
        let large = "N".repeat(512 * 1024);
        let action = |name: &str, params| EntityMsg::Action {
            caller: None,
            name: name.into(),
            params,
            related: Default::default(),
            idempotency_key: None,
            expected_authorization_precondition: None,
        };
        let message = if read {
            tokio::time::resume();
            let written: EntityResponse = actor
                .ask(
                    action("Write", serde_json::json!({"Name":large})),
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
            assert!(written.success, "{:?}", written.error);
            assert!(
                crate::blobs::field_overflow_descriptor(&written.state.fields["Name"]).is_some()
            );
            held.send_replace(true);
            tokio::time::pause();
            action("Same", serde_json::json!({"expected":large}))
        } else {
            action("Write", serde_json::json!({"Name":large}))
        };
        let asked = actor.clone();
        let reply = tokio::spawn(async move {
            asked
                .ask::<EntityResponse>(message, Duration::from_secs(600))
                .await
        });
        reported(
            &lines,
            if read { "Same" } else { "Write" },
            if read {
                "reading blobs"
            } else {
                "writing blobs"
            },
        )
        .await;
        tokio::time::resume();
        held.send_replace(false);
        let reply = reply.await.unwrap().unwrap();
        assert!(reply.success, "seed {seed}: {:?}", reply.error);
        assert_eq!(
            lines.with(&["handler finished"]),
            1,
            "seed {seed}: {}",
            lines.text()
        );
        // Completion leaves the blob reference and both durable writes intact.
        assert!(crate::blobs::field_overflow_descriptor(&reply.state.fields["Name"]).is_some());
        assert_eq!(
            store.dump_journal("default:Document:held").len(),
            if read { 3 } else { 2 }
        );
    }
}
