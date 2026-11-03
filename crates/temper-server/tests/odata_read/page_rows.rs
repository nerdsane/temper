//! A list read loads the catalog rows of its page, not those of every record
//! of its type. On 30 Oct 2026 a page of one record, out of 1,800 records of
//! 120 KB, took 170 ms and about 450 MB: before it built the page, the read
//! loaded every record's catalog row whole to find any record without one.

use super::*;
use std::sync::{Arc, Mutex};
use tracing_subscriber::layer::SubscriberExt;

/// Records of the type: many more than any page below.
const RECORDS: usize = 40;
/// The most catalog rows a page below may load: `$top=1` asks for one more
/// row than it answers, to know whether a next page follows.
const PAGE_ROWS: u64 = 2;

/// The most catalog rows one `turso.load_entity_catalog_rows` call loaded,
/// read from its `entity_count`.
#[derive(Clone, Default)]
struct LargestRowLoad(Arc<Mutex<u64>>);

impl LargestRowLoad {
    fn take(&self) -> u64 {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[derive(Default)]
struct EntityCount(u64);

impl tracing::field::Visit for EntityCount {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "entity_count" {
            self.0 = value;
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LargestRowLoad {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if attrs.metadata().name() != "load_entity_catalog_rows" {
            return;
        }
        let mut count = EntityCount::default();
        attrs.record(&mut count);
        let mut largest = self.0.lock().unwrap();
        *largest = (*largest).max(count.0);
    }
}

#[tokio::test]
async fn a_list_read_loads_only_its_pages_catalog_rows() {
    let db_dir = tempfile::tempdir().expect("create isolated query-plane db dir");
    let db_url = format!("file:{}", db_dir.path().join("page-rows.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let state = build_turso_state("odata-read-page-rows", store);
    for index in 0..RECORDS {
        let (status, body) = post_json(
            &state,
            "/tdata/Orders",
            serde_json::json!({ "id": format!("ord-{index:02}"), "Currency": "USD" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create {index}: {body:?}");
    }

    let largest = LargestRowLoad::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(largest.clone()));
    for path in [
        "/tdata/Orders?$top=1&$select=Currency",
        "/tdata/Orders?$count=true&$top=1",
        "/tdata/Orders?$filter=Currency%20eq%20'USD'&$top=1&$select=Currency",
    ] {
        let (status, body) = get_json(&state, path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body:?}");
        assert_eq!(
            body["value"].as_array().map(Vec::len),
            Some(1),
            "{path}: {body:?}"
        );
        let loaded = largest.take();
        assert!(
            loaded <= PAGE_ROWS,
            "{path} loaded {loaded} catalog rows in one call for a page of one record \
             out of {RECORDS}"
        );
    }
}
