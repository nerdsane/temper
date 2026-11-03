//! DST: replay restores what each event's effects wrote.
//!
//! A `spawn` effect mints its child's id when the action runs. When the parent
//! is rebuilt from its journal (a restart, or the ADR-0046 retry that replays
//! after a concurrency conflict), its field must still name the child that was
//! really created: replay restores the id from the journal and never mints a
//! new one. The same holds for any value an effect computed when it ran, such
//! as a counter whose effect a later spec changed.
//!
//! Real `ServerState` dispatch (spawn included) over a `SimEventStore`; the
//! child is real when its own journal exists.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityResponse;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const NUM_SEEDS: u64 = 50;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReplayOutcomes" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Doc">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="notice" Type="Edm.String"/>
        <Property Name="asks" Type="Edm.Int64"/>
      </EntityType>
      <EntityType Name="Note">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Docs" EntityType="Temper.ReplayOutcomes.Doc"/>
        <EntitySet Name="Notes" EntityType="Temper.ReplayOutcomes.Note"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const NOTE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Draft", "Raised"]
initial = "Draft"

[[action]]
name = "Raise"
from = ["Draft"]
to = "Raised"
"#;

/// `Ask` spawns a Note, stores the minted id in `notice`, and adds
/// `asks_step` to `asks`. `Touch` only counts itself: a move that changes
/// nothing would write no event, and the conflict below needs an append.
fn doc_ioa(asks_step: u32) -> String {
    format!(
        r#"
[automaton]
name = "Doc"
states = ["Open", "Asked"]
initial = "Open"

[[state]]
name = "notice"
type = "string"
initial = ""

[[state]]
name = "asks"
type = "counter"
initial = 0

[[action]]
name = "Ask"
from = ["Open", "Asked"]
to = "Asked"
effect = ["spawn('Note', 'Raise', notice)", "asks += {asks_step}"]

[[state]]
name = "touches"
type = "counter"
initial = 0

[[action]]
name = "Touch"
from = ["Asked"]
to = "Asked"
effect = ["touches += 1"]
"#
    )
}

/// A server over `store` whose Doc spec adds `asks_step` per `Ask`.
fn server(store: &SimEventStore, name: &str, asks_step: u32) -> ServerState {
    server_with_doc(store, name, &doc_ioa(asks_step))
}

fn server_with_doc(store: &SimEventStore, name: &str, doc: &str) -> ServerState {
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL.to_string(),
        &[("Doc", doc), ("Note", NOTE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
}

async fn dispatch(state: &ServerState, doc_id: &str, action: &str) -> EntityResponse {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Doc",
            doc_id,
            action,
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("{action} on {doc_id}: {e}"));
    assert!(response.success, "{action}: {:?}", response.error);
    response
}

async fn read_doc(state: &ServerState, doc_id: &str) -> EntityResponse {
    state
        .get_tenant_entity_state(&TenantId::default(), "Doc", doc_id)
        .await
        .unwrap_or_else(|e| panic!("read {doc_id}: {e}"))
}

fn notice(response: &EntityResponse) -> String {
    response
        .state
        .fields
        .get("notice")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn asks(response: &EntityResponse) -> usize {
    response.state.counters.get("asks").copied().unwrap_or(0)
}

/// Whether a Note with this id has a journal: the child really exists.
fn note_exists(store: &SimEventStore, id: &str) -> bool {
    !store.dump_journal(&format!("default:Note:{id}")).is_empty()
}

/// Wait until the spawned Note's journal holds its `Raise`.
async fn wait_for_raised_note(store: &SimEventStore, id: &str) {
    for _ in 0..200 {
        if store
            .dump_journal(&format!("default:Note:{id}"))
            .iter()
            .any(|env| env.event_type == "Raise")
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("Note {id} was not spawned and raised");
}

/// Spawn twice so the field's value comes from the later spawn; return the
/// second child's id.
async fn ask_twice(state: &ServerState, store: &SimEventStore, doc_id: &str) -> String {
    let first = notice(&dispatch(state, doc_id, "Ask").await);
    assert!(!first.is_empty(), "the spawn stored its child id");
    wait_for_raised_note(store, &first).await;
    let second = notice(&dispatch(state, doc_id, "Ask").await);
    assert!(!second.is_empty() && second != first, "a second child");
    wait_for_raised_note(store, &second).await;
    second
}

// =========================================================================
// A restart rebuilds the parent from its journal: the stored child id stays.
// =========================================================================

#[tokio::test]
async fn dst_restart_keeps_the_spawned_child_id() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let doc_id = format!("doc-restart-{seed}");

        let child = {
            let state = server(&store, "replay-outcomes-live", 1);
            ask_twice(&state, &store, &doc_id).await
        };

        // A fresh process over the same journal, with its own id sequence.
        let (_guard2, _clock2, _ids2) = install_deterministic_context(seed + 10_000);
        let state = server(&store, "replay-outcomes-restart", 1);
        let rebuilt = read_doc(&state, &doc_id).await;
        let stored = notice(&rebuilt);
        assert_eq!(
            stored, child,
            "seed {seed}: after the restart the parent names a different child"
        );
        assert!(
            note_exists(&store, &stored),
            "seed {seed}: the parent's notice {stored} has no record"
        );
        assert_eq!(asks(&rebuilt), 2, "seed {seed}: asks after the restart");
    }
}

// =========================================================================
// The ADR-0046 retry replays the journal inside a live actor: same rule.
// =========================================================================

#[tokio::test]
async fn dst_concurrency_retry_keeps_the_spawned_child_id() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let doc_id = format!("doc-retry-{seed}");
        let state = server(&store, "replay-outcomes-retry", 1);

        let child = ask_twice(&state, &store, &doc_id).await;

        // The next append conflicts: the actor replays its journal, then
        // retries `Touch` on the rebuilt state.
        store.inject_concurrency_violations(&format!("default:Doc:{doc_id}"), 1);
        let touched = dispatch(&state, &doc_id, "Touch").await;
        assert_eq!(
            store.pending_concurrency_violations(&format!("default:Doc:{doc_id}")),
            0,
            "seed {seed}: the conflict was consumed, so the retry replayed"
        );
        assert_eq!(
            notice(&touched),
            child,
            "seed {seed}: after the retry's replay the parent names a different child"
        );
        let read_back = read_doc(&state, &doc_id).await;
        assert_eq!(notice(&read_back), child, "seed {seed}: read back");
        assert!(
            note_exists(&store, &notice(&read_back)),
            "seed {seed}: the parent's notice has no record"
        );
        assert_eq!(asks(&read_back), 2, "seed {seed}: asks after the retry");
    }
}

// =========================================================================
// A value an effect computed is restored as stored, even after the spec
// changed what that effect computes.
// =========================================================================

#[tokio::test]
async fn dst_restart_under_a_changed_effect_keeps_the_stored_counter() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let doc_id = format!("doc-respec-{seed}");

        let child = {
            let state = server(&store, "replay-outcomes-v1", 1);
            ask_twice(&state, &store, &doc_id).await
        };

        // v2 adds 5 per Ask. The two Asks already taken added 1 each.
        let (_guard2, _clock2, _ids2) = install_deterministic_context(seed + 20_000);
        let state = server(&store, "replay-outcomes-v2", 5);
        let rebuilt = read_doc(&state, &doc_id).await;
        assert_eq!(
            asks(&rebuilt),
            2,
            "seed {seed}: replay re-ran the v2 effect instead of restoring the stored count"
        );
        assert_eq!(
            rebuilt.state.fields.get("asks"),
            Some(&serde_json::json!(2)),
            "seed {seed}: the asks field follows the counter"
        );
        assert_eq!(notice(&rebuilt), child, "seed {seed}: notice under v2");

        // A new Ask runs the v2 effect.
        let after = dispatch(&state, &doc_id, "Ask").await;
        assert_eq!(asks(&after), 7, "seed {seed}: a new Ask under v2");
        wait_for_raised_note(&store, &notice(&after)).await;
    }
}

// =========================================================================
// Events journaled before outcomes were recorded: a minted child id was never
// journaled, so replay leaves the field unknown instead of minting one.
// =========================================================================

#[tokio::test]
async fn dst_legacy_events_never_mint_a_child_id() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let doc_id = format!("doc-legacy-{seed}");
        let doc_journal = format!("default:Doc:{doc_id}");

        {
            let state = server(&store, "replay-outcomes-legacy-live", 1);
            ask_twice(&state, &store, &doc_id).await;
        }

        // The same journal as an older build wrote it: no recorded outcomes.
        let legacy = SimEventStore::no_faults(seed);
        for (index, mut envelope) in store.dump_journal(&doc_journal).into_iter().enumerate() {
            if let Some(payload) = envelope.payload.as_object_mut() {
                payload.remove("outcome");
            }
            legacy
                .append(&doc_journal, index as u64, &[envelope])
                .await
                .expect("copy the journal");
        }
        for persistence_id in store.list_all_persistence_ids() {
            if persistence_id.starts_with("default:Note:") {
                for (index, envelope) in store.dump_journal(&persistence_id).into_iter().enumerate()
                {
                    legacy
                        .append(&persistence_id, index as u64, &[envelope])
                        .await
                        .expect("copy the child journal");
                }
            }
        }
        assert!(
            legacy
                .dump_journal(&doc_journal)
                .iter()
                .all(|env| env.payload.get("outcome").is_none()),
            "seed {seed}: the copied journal records no outcomes"
        );

        let (_guard2, _clock2, _ids2) = install_deterministic_context(seed + 30_000);
        let state = server(&legacy, "replay-outcomes-legacy-restart", 1);
        let rebuilt = read_doc(&state, &doc_id).await;
        let stored = notice(&rebuilt);
        assert!(
            stored.is_empty(),
            "seed {seed}: replay of a legacy spawn wrote notice {stored} (record exists: {}); \
             the minted id was never journaled, so it must stay unknown",
            note_exists(&legacy, &stored)
        );
        assert_eq!(asks(&rebuilt), 2, "seed {seed}: legacy counters re-derive");
        assert_eq!(rebuilt.state.status, "Asked", "seed {seed}: status");
    }
}

/// An unchanged counter can still synchronize the legacy item_count value.
#[tokio::test]
async fn dst_replay_keeps_item_count_after_zero_increment() {
    let doc = r#"
[automaton]
strict_action_params = true
name = "Doc"
states = ["Open", "Asked"]
initial = "Open"

[[state]]
name = "items"
type = "counter"
initial = 5

[[action]]
name = "Ask"
from = ["Open"]
to = "Asked"
effect = ["items += 0"]

[[state]]
name = "touches"
type = "counter"
initial = 0

[[action]]
name = "Touch"
from = ["Asked"]
to = "Asked"
effect = ["touches += 1"]
"#;
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let doc_id = format!("doc-item-count-{seed}");
        let journal = format!("default:Doc:{doc_id}");
        {
            let state = server_with_doc(&store, "item-count-live", doc);
            let initial = read_doc(&state, &doc_id).await;
            assert_eq!(initial.state.counters.get("items"), Some(&5));
            assert_eq!(initial.state.item_count, 0);
            let live = dispatch(&state, &doc_id, "Ask").await;
            assert_eq!(live.state.item_count, 5, "seed {seed}: live effect");

            store.inject_concurrency_violations(&journal, 1);
            let retried = dispatch(&state, &doc_id, "Touch").await;
            assert_eq!(store.pending_concurrency_violations(&journal), 0);
            assert_eq!(retried.state.item_count, 5, "seed {seed}: conflict replay");
        }
        // Copy only events so the restart must rebuild without a snapshot.
        let replay_store = SimEventStore::no_faults(seed);
        for (index, envelope) in store.dump_journal(&journal).into_iter().enumerate() {
            replay_store
                .append(&journal, index as u64, &[envelope])
                .await
                .unwrap();
        }
        let state = server_with_doc(&replay_store, "item-count-restart", doc);
        let rebuilt = read_doc(&state, &doc_id).await;
        assert_eq!(rebuilt.state.counters.get("items"), Some(&5));
        assert_eq!(rebuilt.state.item_count, 5, "seed {seed}: journal restart");
    }
}
