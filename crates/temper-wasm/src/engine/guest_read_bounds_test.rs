//! ARN-226: guest reads are bounds-checked before the host allocates.
//!
//! These live in their own file (rather than `engine/tests.rs`) so the readability
//! ratchet counts them as tests: its production-file exclusion matches
//! `*_test.rs`, which `tests.rs` does not.

use super::tests::{make_context, make_host, make_streams};
use super::*;

// ARN-226 (wiring): a guest returns a result pointer whose 4-byte length prefix is
// forged far larger than its linear memory. The host must reject it on the bounds
// check BEFORE allocating a buffer of that size. Memory here is 1 page (64 KiB) and
// the forged length is 64 MiB — above LARGE_ALLOC_THRESHOLD, so the allocation
// counter observes the *ordering*, not just the rejection.
const WAT_FORGED_RESULT_LENGTH: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        ;; length prefix at address 100 = 64 MiB, far beyond the 64 KiB memory
        i32.const 100
        i32.const 67108864
        i32.store
        ;; return 104 so the host reads the prefix at 104-4 = 100
        i32.const 104
      )
    )
"#;

// A result whose length is individually *smaller* than the guest's memory but whose
// `ptr + len` runs past the end. This pins the predicate to the full range: a weaker
// check that only compared `len` against the memory size would wrongly accept it.
const WAT_RESULT_LENGTH_OVERRUNS_END: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        ;; length 10000 stored at 59996, so the host reads it for ptr = 60000.
        ;; 10000 < 65536, but 60000 + 10000 = 70000 > 65536.
        i32.const 59996
        i32.const 10000
        i32.store
        i32.const 60000
      )
    )
"#;

#[tokio::test]
async fn forged_result_length_is_rejected_before_allocating() {
    // This test locks the WIRING, not just the predicate: it asserts the specific
    // error the bounds check produces. Deleting the `guest_read_bounds_ok` call on
    // the result-read path makes the host allocate the forged size and then fail
    // with "failed to read result" instead, which fails this assertion.
    use std::sync::atomic::Ordering;
    let _serialize = ALLOC_OBSERVER_LOCK.lock().await;

    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_FORGED_RESULT_LENGTH.as_bytes())
        .unwrap();

    let before = LARGE_ALLOCS.load(Ordering::SeqCst);
    let err = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await
        .expect_err("a forged result length must be rejected");
    let large_allocs = LARGE_ALLOCS.load(Ordering::SeqCst) - before;

    let message = format!("{err:?}");
    assert!(
        message.contains("result length exceeds guest linear memory"),
        "expected the bounds check to reject before allocating, got: {message}"
    );
    // Ordering, not just rejection: moving the allocation above the guard would
    // still produce the error above, but would trip the counter.
    assert_eq!(
        large_allocs, 0,
        "the forged length must be rejected BEFORE allocating; {large_allocs} \
         allocation(s) >= {LARGE_ALLOC_THRESHOLD} bytes were made"
    );
}

#[tokio::test]
async fn result_length_overrunning_memory_end_is_rejected() {
    // Pins `ptr + len`, not just `len`: the length here is well under the guest's
    // memory size, so only a check on the whole range rejects it.
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_RESULT_LENGTH_OVERRUNS_END.as_bytes())
        .unwrap();

    let err = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await
        .expect_err("a result range running past the end of memory must be rejected");

    let message = format!("{err:?}");
    assert!(
        message.contains("result length exceeds guest linear memory"),
        "expected the range check to reject ptr+len past the end, got: {message}"
    );
}

// ---------------------------------------------------------------------------
// ARN-226 (wiring, helper path): an allocation-observing guard.
//
// A guest calling a helper-backed host function with a huge `len` gets the same
// `-1` back whether the bounds check runs before the allocation or after it, so
// the return value cannot distinguish the two. This counting allocator makes the
// ordering observable: it records allocations at or above a threshold no
// legitimate path in this test should reach. With the guard in place the count
// stays zero; delete the guard in `read_guest_string` / `read_guest_bytes` and
// the host allocates the guest-chosen size first, which the assertion catches.
// ---------------------------------------------------------------------------

/// Allocation size at or above which we consider a host allocation "large".
/// The guest memory in these tests is one 64 KiB page, so nothing legitimate in
/// the guest-read path approaches this. It must stay **below**
/// `WasmResourceLimits::default().max_memory` (64 MiB) so an unguarded read of a
/// guest-chosen length is always counted — and above any legitimate allocation in
/// this test binary (the largest is `MAX_MODULE_SIZE + 1`, 10 MiB). A future test
/// that grows guest memory past this and reads it in-bounds would need a rethink.
const LARGE_ALLOC_THRESHOLD: usize = 32 * 1024 * 1024;

static LARGE_ALLOCS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The counter is process-wide, so allocation-observing tests take this lock to
/// avoid attributing each other's allocations. Nothing else in this crate's suite
/// allocates near the threshold — the largest is `MAX_MODULE_SIZE + 1` (10 MiB) in
/// `module_too_large_rejected` — so serializing these is sufficient.
static ALLOC_OBSERVER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct LargeAllocCounter;

unsafe impl std::alloc::GlobalAlloc for LargeAllocCounter {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if layout.size() >= LARGE_ALLOC_THRESHOLD {
            LARGE_ALLOCS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        if layout.size() >= LARGE_ALLOC_THRESHOLD {
            LARGE_ALLOCS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        unsafe { std::alloc::System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        if new_size >= LARGE_ALLOC_THRESHOLD {
            LARGE_ALLOCS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        unsafe { std::alloc::System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC_COUNTER: LargeAllocCounter = LargeAllocCounter;

// Calls host_emit_progress(0, 64 MiB) against a single 64 KiB page — an
// out-of-bounds read whose length is far beyond the guest's own memory. Traps if
// the host does NOT return the -1 error sentinel.
const WAT_OVERSIZED_HOST_READ: &str = r#"
    (module
      (import "env" "host_emit_progress" (func $emit (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
        i32.const 67108864
        call $emit
        i32.const -1
        i32.ne
        if
          unreachable
        end
        i32.const 0
      )
    )
"#;

// Same shape as WAT_OVERSIZED_HOST_READ but through `host_cache_contains`, which
// reads via `read_guest_lossy` -> `read_guest_bytes` — the other helper. Its ABI is
// boolean with no error sentinel (0 = not cached), so the guest cannot observe the
// refusal; only the allocation counter can.
const WAT_OVERSIZED_HOST_BYTES_READ: &str = r#"
    (module
      (import "env" "host_cache_contains" (func $contains (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
        i32.const 67108864
        call $contains
        drop
        i32.const 0
      )
    )
"#;

#[tokio::test]
async fn oversized_host_bytes_read_is_rejected_before_allocating() {
    use std::sync::atomic::Ordering;
    let _serialize = ALLOC_OBSERVER_LOCK.lock().await;

    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_OVERSIZED_HOST_BYTES_READ.as_bytes())
        .unwrap();

    let before = LARGE_ALLOCS.load(Ordering::SeqCst);
    let result = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;
    let large_allocs = LARGE_ALLOCS.load(Ordering::SeqCst) - before;

    assert!(
        result.is_ok(),
        "the guest itself must run cleanly: {result:?}"
    );
    assert_eq!(
        large_allocs, 0,
        "read_guest_bytes must refuse the 64 MiB length before allocating; \
         {large_allocs} allocation(s) >= {LARGE_ALLOC_THRESHOLD} bytes were made"
    );
}

#[tokio::test]
async fn oversized_host_read_is_rejected_before_allocating() {
    use std::sync::atomic::Ordering;
    let _serialize = ALLOC_OBSERVER_LOCK.lock().await;

    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_OVERSIZED_HOST_READ.as_bytes())
        .unwrap();

    let before = LARGE_ALLOCS.load(Ordering::SeqCst);
    let result = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;
    let large_allocs = LARGE_ALLOCS.load(Ordering::SeqCst) - before;

    // The guest asserts it got -1 back, so a successful invocation proves the
    // read was refused rather than served.
    assert!(
        result.is_ok(),
        "host must return the error sentinel for an out-of-bounds read: {result:?}"
    );
    // And it was refused *before* allocating the guest-chosen 64 MiB.
    assert_eq!(
        large_allocs, 0,
        "a guest-supplied length must not drive a large host allocation; \
         {large_allocs} allocation(s) >= {LARGE_ALLOC_THRESHOLD} bytes were made"
    );
}

#[test]
fn guest_memory_reads_stay_inside_the_bounds_checked_helpers() {
    // ARN-226 (class guard). The individual guards are locked by the mutation
    // tests above, but nothing stopped a future edit from reintroducing a raw
    // `memory.read` + `vec![0u8; len]` at a new call site — which is exactly how
    // the original vulnerability was spread across ten places. This asserts the
    // structural invariant instead: in this file, guest memory is only ever read
    // inside `read_guest_string` / `read_guest_bytes`, which bounds-check first.
    //
    // If you are adding a host function, call one of those helpers rather than
    // relaxing this test.
    const ALLOWED_READ_SITES: usize = 2;
    let source = include_str!("host_functions.rs");
    let reads = source
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            // A guest-memory read passes arguments (store/caller, offset, buffer).
            // `RwLock::read()` takes none, so the empty-paren form is excluded.
            !trimmed.starts_with("//") && trimmed.contains(".read(") && !trimmed.contains(".read()")
        })
        .count();
    assert_eq!(
        reads, ALLOWED_READ_SITES,
        "guest memory must only be read inside the bounds-checked helpers; \
         found {reads} `.read(` call sites in host_functions.rs (expected \
         {ALLOWED_READ_SITES}: one in read_guest_string, one in read_guest_bytes)"
    );
}
