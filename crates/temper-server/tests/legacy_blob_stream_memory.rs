//! Regression for the unlimited property's reachable legacy fallback: measure
//! allocations while opening and consuming a database-only value. Merely
//! splitting a whole buffered Vec into small output chunks must still fail.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures_util::StreamExt as _;
use sha2::{Digest as _, Sha256};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{ServerState, StorageStack, blob_store::BlobStreamRead};
use temper_spec::csdl::CsdlDocument;
use temper_store_turso::TursoEventStore;

struct MeasuringAllocator;
static MEASURE: AtomicBool = AtomicBool::new(false);
static LARGEST: AtomicUsize = AtomicUsize::new(0);

fn record(size: usize) {
    if MEASURE.load(Ordering::Relaxed) {
        LARGEST.fetch_max(size, Ordering::Relaxed);
    }
}

// SAFETY: every operation delegates unchanged to the system allocator; the
// measurement uses only atomics and does not allocate or retain any pointers.
unsafe impl GlobalAlloc for MeasuringAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: MeasuringAllocator = MeasuringAllocator;

#[tokio::test]
async fn legacy_stream_never_allocates_the_whole_blob() {
    let temp = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", temp.path().join("blobs.db").display()),
        None,
    )
    .await
    .unwrap();
    let bytes = vec![b'x'; 8 * 1024 * 1024];
    let expected_hash = Sha256::digest(&bytes);
    let length = bytes.len() as u64;
    store.put_blob("legacy", &bytes).await.unwrap();
    drop(bytes);
    let mut state = ServerState::new(
        ActorSystem::new("legacy-memory"),
        CsdlDocument {
            version: "4.0".into(),
            schemas: vec![],
        },
        String::new(),
    );
    state.data_dir = temp.path().join("objects");
    state.set_storage_stack(StorageStack::from_turso(store));
    MEASURE.store(true, Ordering::SeqCst);
    let opened = state
        .stream_blob_object(&TenantId::default(), "legacy", length)
        .await;
    let mut actual_hash = Sha256::new();
    let mut count = 0;
    if let Ok(BlobStreamRead::Found(object)) = &opened {
        assert_eq!(object.content_length(), length);
    }
    let result = match opened {
        Ok(BlobStreamRead::Found(object)) => {
            let mut stream = object.into_stream();
            let mut error = None;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => {
                        count += chunk.len() as u64;
                        actual_hash.update(&chunk);
                    }
                    Err(e) => {
                        error = Some(e.to_string());
                        break;
                    }
                }
            }
            error
        }
        Ok(_) => Some("legacy blob missing or refused".into()),
        Err(error) => Some(error),
    };
    MEASURE.store(false, Ordering::SeqCst);
    assert!(result.is_none(), "{result:?}");
    assert_eq!(count, length);
    assert_eq!(actual_hash.finalize(), expected_hash);
    let largest = LARGEST.load(Ordering::SeqCst);
    assert!(
        largest < 1024 * 1024,
        "legacy streaming allocated {largest} bytes for an 8 MiB blob"
    );
    assert!(matches!(
        state.stream_blob_object(&TenantId::default(), "legacy", length - 1).await.unwrap(),
        BlobStreamRead::TooLarge { actual_bytes: Some(n) } if n == length
    ));
    assert!(matches!(
        state
            .stream_blob_object(&TenantId::new("other-tenant"), "legacy", length)
            .await
            .unwrap(),
        BlobStreamRead::Missing
    ));
    let BlobStreamRead::Found(object) = state
        .stream_blob_object(&TenantId::default(), "legacy", length)
        .await
        .unwrap()
    else {
        panic!("legacy blob");
    };
    let mut bad_hash = object.verify_sha256(&"0".repeat(64)).into_stream();
    let mut rejected = false;
    while let Some(chunk) = bad_hash.next().await {
        if chunk.is_err() {
            rejected = true;
            break;
        }
    }
    assert!(
        rejected,
        "legacy bytes must still fail a false content address"
    );
}
