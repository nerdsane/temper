use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::entity_actor::types::recent_events_budget;
use crate::storage::{BackendLabel, BoxedEventStore};
use temper_runtime::persistence::{
    CommitRows, EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError,
};
use temper_store_sim::SimEventStore;

/// Apply `moves` AddItem actions to one Order.
async fn add_items(state: &ServerState, entity_id: &str, moves: usize) {
    for i in 0..moves {
        let result = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                entity_id,
                "AddItem",
                serde_json::json!({ "ProductId": format!("p{i}") }),
                &AgentContext::default(),
            )
            .await;
        assert!(result.is_ok(), "AddItem {i} failed: {result:?}");
    }
}

fn actor_is_loaded(state: &ServerState, entity_id: &str) -> bool {
    let key = format!("{}:Order:{entity_id}", TenantId::default());
    state.actor_registry.read().unwrap().contains_key(&key)
}

async fn history_response(state: ServerState, uri: &str) -> (StatusCode, serde_json::Value) {
    let app = Router::new()
        .nest("/observe", build_observe_router())
        .with_state(state);
    let response = app.oneshot(system_get(uri)).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// A record whose in-memory trail has been cut short still shows every move
/// it ever took, from sequence 1, while its actor stays loaded.
#[tokio::test]
async fn history_reads_whole_journal_while_actor_is_loaded() {
    let state = test_state_with_turso().await;
    let entity_id = "order-long-trail";
    let moves = recent_events_budget() + 10;
    add_items(&state, entity_id, moves).await;

    assert!(actor_is_loaded(&state, entity_id), "actor stays loaded");
    let memory = state
        .get_tenant_entity_state(&TenantId::default(), "Order", entity_id)
        .await
        .expect("entity state");
    let (store, _) = state.event_journal().expect("event journal");
    let persistence_id = format!("{}:Order:{entity_id}", TenantId::default());
    let journal = store
        .read_events(&persistence_id, 0)
        .await
        .expect("journal readable");
    assert!(journal.len() >= moves, "every move is journaled");
    assert!(
        memory.state.events.len() < journal.len(),
        "precondition: memory holds a partial trail ({} of {})",
        memory.state.events.len(),
        journal.len()
    );

    let (status, json) = history_response(
        state,
        &format!("/observe/entities/Order/{entity_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["current_state"], "Draft", "live state still included");
    let events = json["events"].as_array().expect("events array");
    let sequences: Vec<u64> = events
        .iter()
        .map(|event| event["sequence"].as_u64().expect("sequence"))
        .collect();
    let stored: Vec<u64> = journal.iter().map(|env| env.sequence_nr).collect();
    assert_eq!(
        sequences, stored,
        "history shows the stored sequence numbers"
    );
    assert_eq!(sequences.first(), Some(&1), "history starts at sequence 1");
    for (event, envelope) in events.iter().zip(&journal) {
        assert_eq!(event["action"], envelope.payload["action"]);
    }
}

/// When the journal cannot be read the route says so, instead of answering
/// the partial trail held in memory.
#[tokio::test]
async fn history_store_read_failure_answers_unavailable() {
    let sim = SimEventStore::no_faults(7);
    let state = state_with_journal(sim.clone());
    let entity_id = "order-unreadable";
    add_items(&state, entity_id, 3).await;
    assert!(actor_is_loaded(&state, entity_id), "actor stays loaded");

    let persistence_id = format!("{}:Order:{entity_id}", TenantId::default());
    sim.fail_next_reads(&persistence_id, 1);
    let (status, _) = history_response(
        state,
        &format!("/observe/entities/Order/{entity_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// A server whose only storage is `store` as its event journal.
fn state_with_journal(store: impl EventStore) -> ServerState {
    let mut state = test_state_with_registry();
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(store),
        None,
        None,
        None,
        None,
        None,
        None,
    ));
    state
}

async fn journal_sequences(state: &ServerState, entity_id: &str) -> Vec<u64> {
    let (store, _) = state.event_journal().expect("event journal");
    let persistence_id = format!("{}:Order:{entity_id}", TenantId::default());
    store
        .read_events(&persistence_id, 0)
        .await
        .expect("journal readable")
        .iter()
        .map(|envelope| envelope.sequence_nr)
        .collect()
}

/// The sequences of one history answer and its `next_before`.
fn page_of(json: &serde_json::Value) -> (Vec<u64>, Option<u64>) {
    let sequences = json["events"]
        .as_array()
        .expect("events array")
        .iter()
        .map(|event| event["sequence"].as_u64().expect("sequence"))
        .collect();
    (sequences, json["next_before"].as_u64())
}

/// `?limit=N` is the newest N moves, oldest first; `before` steps back.
#[tokio::test]
async fn history_limit_pages_newest_first() {
    let state = test_state_with_turso().await;
    let entity_id = "order-paged";
    add_items(&state, entity_id, 12).await;
    let stored = journal_sequences(&state, entity_id).await;
    let n = *stored.last().expect("journaled moves");
    assert_eq!(stored, (1..=n).collect::<Vec<_>>());
    let uri = |query: &str| format!("/observe/entities/Order/{entity_id}/history{query}");

    let (status, json) = history_response(state.clone(), &uri("?limit=5")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page_of(&json), ((n - 4..=n).collect(), Some(n - 4)));
    assert_eq!(json["current_state"], "Draft", "live state still included");

    let before = format!("?limit=5&before={}", n - 4);
    let (status, json) = history_response(state.clone(), &uri(&before)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page_of(&json), ((n - 9..n - 4).collect(), Some(n - 9)));

    let (status, json) = history_response(state, &uri("")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page_of(&json), ((1..=n).collect(), None));
    assert!(
        json.get("next_before")
            .is_some_and(serde_json::Value::is_null),
        "the whole journal says there is nothing older"
    );
}

/// Following `next_before` until it is null reads every move exactly once.
#[tokio::test]
async fn history_pages_follow_next_before_to_sequence_one() {
    let state = test_state_with_turso().await;
    let entity_id = "order-followed";
    add_items(&state, entity_id, 10).await;
    let stored = journal_sequences(&state, entity_id).await;
    let limit = 3;

    let mut seen: Vec<u64> = Vec::new();
    let mut pages = 0;
    let mut before: Option<u64> = None;
    loop {
        assert!(pages <= stored.len(), "paging ends");
        let query = match before {
            Some(before) => format!("?limit={limit}&before={before}"),
            None => format!("?limit={limit}"),
        };
        let uri = format!("/observe/entities/Order/{entity_id}/history{query}");
        let (status, json) = history_response(state.clone(), &uri).await;
        assert_eq!(status, StatusCode::OK);
        let (sequences, next_before) = page_of(&json);
        assert!(sequences.len() <= limit, "a page holds at most limit moves");
        seen = [sequences, seen].concat();
        pages += 1;
        match next_before {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, stored, "every move once, in order, from sequence 1");
    assert_eq!(pages, stored.len().div_ceil(limit));
}

/// `limit` must be a whole number of at least 1, and `before` a number.
#[tokio::test]
async fn history_rejects_a_bad_limit() {
    let state = test_state_with_turso().await;
    add_items(&state, "order-bad-limit", 1).await;
    for query in [
        "limit=0",
        "limit=-1",
        "limit=abc",
        "limit=",
        "limit=2.5",
        "limit=5&before=abc",
    ] {
        let uri = format!("/observe/entities/Order/order-bad-limit/history?{query}");
        let (status, _) = history_response(state.clone(), &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
}

/// A journal that counts the rows it hands back, to show what a read touched.
#[derive(Clone)]
struct RowCountingStore {
    inner: SimEventStore,
    rows_read: Arc<AtomicUsize>,
}

impl RowCountingStore {
    fn counted(&self, rows: Vec<PersistenceEnvelope>) -> Vec<PersistenceEnvelope> {
        self.rows_read.fetch_add(rows.len(), Ordering::SeqCst);
        rows
    }
}

impl EventStore for RowCountingStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.inner
            .append(persistence_id, expected_sequence, events)
            .await
    }

    async fn append_with_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        self.inner
            .append_with_rows(persistence_id, expected_sequence, events, rows)
            .await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.inner.append_batch(appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let rows = self
            .inner
            .read_events(persistence_id, from_sequence)
            .await?;
        Ok(self.counted(rows))
    }

    async fn read_events_page(
        &self,
        persistence_id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let rows = self
            .inner
            .read_events_page(persistence_id, before_sequence, limit)
            .await?;
        Ok(self.counted(rows))
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.inner
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.inner.load_snapshot(persistence_id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.inner.list_entity_ids(tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.inner
            .list_entity_ids_by_type(tenant, entity_type)
            .await
    }
}

/// A page reads only its own rows from the journal, plus the one row that
/// tells whether older moves exist; never the whole journal.
#[tokio::test]
async fn history_page_reads_only_its_rows() {
    let store = RowCountingStore {
        inner: SimEventStore::no_faults(11),
        rows_read: Arc::new(AtomicUsize::new(0)),
    };
    let state = state_with_journal(store.clone());
    let entity_id = "order-counted";
    add_items(&state, entity_id, 30).await;
    assert!(actor_is_loaded(&state, entity_id), "actor stays loaded");
    let limit = 5;

    for query in [
        format!("?limit={limit}"),
        format!("?limit={limit}&before=10"),
    ] {
        store.rows_read.store(0, Ordering::SeqCst);
        let uri = format!("/observe/entities/Order/{entity_id}/history{query}");
        let (status, json) = history_response(state.clone(), &uri).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(page_of(&json).0.len(), limit, "{query}");
        let rows_read = store.rows_read.load(Ordering::SeqCst);
        assert!(
            rows_read <= limit + 1,
            "{query}: read {rows_read} journal rows for a page of {limit}"
        );
    }
}
