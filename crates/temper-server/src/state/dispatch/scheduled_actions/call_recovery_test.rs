//! Call settlement under storage faults, without a real engine worker.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    CommitRows, EventMetadata, EventStore, PersistenceEnvelope, ScheduledActionRecord,
};
use temper_runtime::scheduler::{
    DeterministicIdGen, LogicalClock, install_sim_context, sim_now, sim_uuid,
};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

use crate::registry::SpecRegistry;
use crate::request_context::armed_by::encode_call_armed_by;
use crate::request_context::{AgentContext, ScheduledFire};
use crate::{ServerState, StorageStack};

fn server(store: &SimEventStore) -> ServerState {
    let mut server =
        ServerState::from_registry(ActorSystem::new("call-recovery"), SpecRegistry::new());
    server.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    server
}

async fn pass(wait: u64) {
    tokio::time::advance(Duration::from_secs(wait)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}

async fn arm_call(store: &SimEventStore, first: &ServerState) -> ScheduledActionRecord {
    let row = ScheduledActionRecord {
        tenant: "calls".into(),
        entity_type: "Lookup".into(),
        entity_id: "1".into(),
        arm_id: "1.call:lookup".into(),
        action: "GiveUp".into(),
        due_at: sim_now().to_rfc3339(),
        armed_by: encode_call_armed_by(&AgentContext::system(), first.call_process_id).unwrap(),
    };
    let event = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Ask".into(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "call-recovery".into(),
        },
    };
    store
        .append_with_rows(
            "calls:Lookup:1",
            0,
            &[event],
            CommitRows {
                scheduled_actions: std::slice::from_ref(&row),
                ..CommitRows::default()
            },
        )
        .await
        .unwrap();
    row
}

#[tokio::test(start_paused = true)]
async fn dst_call_cleanup_and_boot_recovery() {
    for seed in 0..32 {
        let _sim = install_sim_context(
            Arc::new(LogicalClock::new()),
            Arc::new(DeterministicIdGen::new(seed)),
        );
        let store = SimEventStore::no_faults(seed);
        let first = server(&store);
        let row = arm_call(&store, &first).await;
        let tenant = TenantId::new("calls");
        assert!(
            first.skip_call_recovery(&tenant, &row),
            "seed {seed}: a late boot read must skip this process's call"
        );
        assert!(first.armed_scheduled_actions().is_empty());
        let restarted = server(&store);
        assert!(
            !restarted.skip_call_recovery(&tenant, &row),
            "seed {seed}: another process must recover the lost call"
        );
        // A row written by a pre-marker binary still belongs to recovery.
        let mut legacy = row.clone();
        legacy.armed_by = "{}".into();
        assert!(!first.skip_call_recovery(&tenant, &legacy));

        let failures = 1 + seed as usize % 4;
        store.fail_next_scheduled_deletes(failures);
        let call = ScheduledFire {
            tenant: row.tenant.clone(),
            entity_type: row.entity_type.clone(),
            entity_id: row.entity_id.clone(),
            arm_id: row.arm_id.clone(),
        };
        let cleanup = tokio::spawn(async move { first.end_call_row(&call).await }); // determinism-ok: single-threaded simulated cleanup
        pass(0).await;
        assert_eq!(store.scheduled_delete_failures_pending(), failures - 1);
        let terminal = store
            .list_scheduled_actions("calls")
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            terminal.action.is_empty(),
            "seed {seed}: finality must survive failed deletion"
        );
        if seed % 2 == 0 {
            // Stop after deletion fails, losing the cleanup future. Recovery
            // must only collect the terminal row, never arm its former action.
            cleanup.abort();
            assert!(restarted.skip_call_recovery(&tenant, &terminal));
        }
        // With or without restart, failed cleanup remains pending until the
        // store accepts deletion. Backoff is 1, 2, 4, ... seconds.
        for wait in [0, 1, 2, 4, 8, 16] {
            pass(wait).await;
        }
        assert!(
            store
                .list_scheduled_actions("calls")
                .await
                .unwrap()
                .is_empty(),
            "seed {seed}: cleanup must retry"
        );
        assert!(
            restarted.armed_scheduled_actions().is_empty(),
            "seed {seed}: an ended call never arms"
        );
        if seed % 2 != 0 {
            cleanup.await.unwrap();
        }
    }
}
