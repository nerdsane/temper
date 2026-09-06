use serde_json::json;
use sha2::{Digest as _, Sha256};

use super::BlobReadSource;
use super::hydration::hydrate_blob_refs_with_source;
use super::{
    BlobHydrationBudget, FIELD_OVERFLOW_REF_KEY, blob_ref_value, field_overflow_descriptor,
};
use crate::blob_store::BlobStore;

async fn store_json(store: &BlobStore, value: &serde_json::Value) -> (String, usize) {
    let bytes = serde_json::to_vec(value).expect("serialize overflow JSON");
    let key = format!("field-overflow/sha256/{:x}.json", Sha256::digest(&bytes));
    store
        .put_if_absent(&key, &bytes, None)
        .await
        .expect("store overflow JSON");
    (key, bytes.len())
}

#[test]
fn descriptor_requires_the_canonical_json_encoding() {
    let key = format!("field-overflow/sha256/{}.json", "0".repeat(64));
    let missing_encoding = json!({
        "__temper_blob_ref": key,
        "__temper_blob_size": 4,
    });
    let wrong_encoding = json!({
        "__temper_blob_ref": key,
        "__temper_blob_size": 4,
        "__temper_blob_encoding": "raw",
    });

    assert!(field_overflow_descriptor(&missing_encoding).is_none());
    assert!(field_overflow_descriptor(&wrong_encoding).is_none());
}

#[tokio::test]
async fn aggregate_inline_budget_is_shared_across_all_refs() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (first_key, first_len) = store_json(&store, &json!("a".repeat(400))).await;
    let (second_key, second_len) = store_json(&store, &json!("b".repeat(400))).await;
    assert_eq!(first_len, second_len);
    let mut value = json!({
        "a": blob_ref_value(&first_key, first_len),
        "b": blob_ref_value(&second_key, second_len),
    });
    let budget = BlobHydrationBudget::new(700, 500, 0, 0);

    let deferred =
        hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value["a"].as_str().map(str::len), Some(400));
    assert!(value["b"].get(FIELD_OVERFLOW_REF_KEY).is_some());
    assert!(deferred.is_empty());
    assert_eq!(budget.remaining(), (700 - first_len, 0));
}

#[tokio::test]
async fn lying_small_descriptor_cannot_trigger_oversized_buffered_read() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (key, _) = store_json(&store, &json!("x".repeat(64 * 1024))).await;
    let mut value = blob_ref_value(&key, 4);
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(
        budget.remaining(),
        (1024, 0),
        "failed bounded reads refund admission"
    );
}

#[tokio::test]
async fn noncanonical_overflow_key_is_never_read() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let key = "wasm-modules/not-a-field-overflow-object";
    store
        .put_if_absent(key, br#"{"secret":true}"#, None)
        .await
        .expect("store noncanonical object");
    let mut value = blob_ref_value(key, 15);
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(budget.remaining(), (1024, 0));
}

#[tokio::test]
async fn descriptor_length_must_match_the_stored_object() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (key, actual) = store_json(&store, &json!("short")).await;
    let mut value = blob_ref_value(&key, actual + 10);
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(budget.remaining(), (1024, 0));
}

#[tokio::test]
async fn missing_blob_reads_have_an_aggregate_attempt_budget() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let mut values = serde_json::Map::new();
    for index in 0..100u64 {
        let key = format!("field-overflow/sha256/{index:064x}.json");
        values.insert(index.to_string(), blob_ref_value(&key, 4));
    }
    let mut value = serde_json::Value::Object(values);
    let budget = BlobHydrationBudget::new(1024, 16, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(budget.read_attempts_remaining(), 0);
    assert!(
        value
            .as_object()
            .expect("object")
            .values()
            .all(|value| value.get(FIELD_OVERFLOW_REF_KEY).is_some())
    );
}

#[tokio::test]
async fn a_repeated_missing_key_is_read_only_once() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let key = format!("field-overflow/sha256/{}.json", "5".repeat(64));
    let mut value = serde_json::Value::Array((0..10).map(|_| blob_ref_value(&key, 4)).collect());
    let budget = BlobHydrationBudget::new(1024, 16, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(budget.read_attempts_remaining(), 63);
}

#[tokio::test]
async fn object_bytes_must_match_the_content_addressed_key() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let bytes = serde_json::to_vec(&json!("tampered")).expect("serialize value");
    let key = format!("field-overflow/sha256/{}.json", "6".repeat(64));
    store
        .put_if_absent(&key, &bytes, None)
        .await
        .expect("store corrupted content-addressed object");
    let mut value = blob_ref_value(&key, bytes.len());
    let budget = BlobHydrationBudget::new(1024, 1024, 0, 0);

    hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(value[FIELD_OVERFLOW_REF_KEY], key);
    assert_eq!(budget.remaining(), (1024, 0));
}

#[tokio::test]
async fn wasm_deferred_cache_has_a_hard_aggregate_budget() {
    let dir = tempfile::tempdir().expect("blob dir");
    let store = BlobStore::local_fs(dir.path());
    let (first_key, first_len) = store_json(&store, &json!("a".repeat(300))).await;
    let (second_key, second_len) = store_json(&store, &json!("b".repeat(300))).await;
    let mut value = json!({
        "a": blob_ref_value(&first_key, first_len),
        "b": blob_ref_value(&second_key, second_len),
    });
    let budget = BlobHydrationBudget::new(0, 0, 500, 400);

    let deferred =
        hydrate_blob_refs_with_source(&BlobReadSource::Store(&store), &mut value, &budget).await;

    assert_eq!(deferred.len(), 1);
    assert!(deferred.contains_key(&first_key));
    assert!(value["a"].get(FIELD_OVERFLOW_REF_KEY).is_some());
    assert!(value["b"].get(FIELD_OVERFLOW_REF_KEY).is_some());
    assert_eq!(budget.remaining(), (0, 500 - first_len));
}
