# ADR-0164: Bounds-check guest reads before allocating

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-wasm/src/engine/host_functions.rs` (guest memory reads)
  - `crates/temper-wasm/src/engine/mod.rs` (post-invocation result read)
  - ARN-226 (security finding)

## Context

WASM host functions read caller-supplied `(ptr, len)` operands out of the guest's
linear memory. The read helpers allocate the destination buffer **before** the
bounds check runs (`host_functions.rs`):

```rust
fn read_guest_bytes(caller, memory, ptr, len, …) -> Result<Vec<u8>, ()> {
    if ptr < 0 || len < 0 { … return Err(()); }
    let mut buf = vec![0u8; len as usize];        // allocation happens first
    memory.read(caller, ptr as usize, &mut buf)?; // bounds check happens here
    …
}
```

`len` is an `i32` chosen by the guest, so a single call with `len = i32::MAX`
forces the host to allocate ~2 GiB **before** `memory.read` rejects the
out-of-bounds range. A guest can repeat this cheaply, so it is an unauthenticated
host-side memory-exhaustion / DoS: the allocation is driven entirely by an
attacker-controlled length that is never validated against the guest's actual
memory size. `read_guest_string` has the same shape.

## Decision

Validate that the `[ptr, ptr + len)` range lies within the guest's current linear
memory **before** allocating. `guest_read_bounds_ok(mem_size, ptr, len)` returns
true only when `ptr + len` does not overflow `usize` and is `<= mem_size`
(`memory.data_size(store)`). The read helpers call it first and return the ABI
error sentinel on failure, so no buffer is allocated for an out-of-bounds length.

Each allocation is therefore bounded by the guest's linear memory size, which is
itself capped by `WasmResourceLimits::max_memory`: an oversized `len` is rejected
before any allocation, so no single read can allocate more than the guest could
legitimately hold.

This is a **per-allocation** bound, not a per-invocation one. A host function that
performs several reads (`host_evaluate_spec` holds four) still peaks at a multiple
of the guest's memory, and the outbound stream channel is bounded in chunks rather
than bytes. An aggregate copy budget is the right next tightening and is tracked
separately (see Follow-ups); it is out of scope here because it is a functionality
cap, not a fix for the disclosed vector.

The guard is applied at every guest-length-driven allocation, not only the two
helpers:

- `read_guest_string` / `read_guest_bytes` check internally, and the ten previously
  inline `vec![0u8; len]` sites (the `host_emit_*` family, `host_read_field`,
  `host_evaluate_spec`, `host_http_stream_send_response_head`) are refactored onto
  them. Four of those had no negative-length check at all, so `len = -1` requested a
  `usize::MAX`-sized allocation.
- The **post-invocation result read** (`engine/mod.rs`) is a separate vector: the
  guest returns a pointer whose preceding four bytes are a `u32` length prefix, so a
  forged prefix could drive a ~4 GiB allocation. It calls `guest_read_bounds_ok`
  directly before allocating and fails with a distinct
  `"result length exceeds guest linear memory"`.

Tests cover the predicate *and* the wiring, because a returned error sentinel alone
cannot distinguish "rejected before allocating" from "allocated, then failed":

- a unit test pins the predicate's boundaries (`end == mem_size` allowed; overflow
  and out-of-range rejected);
- two end-to-end tests drive a guest calling a helper-backed host function with a
  64 MiB length against a 64 KiB page — one per helper (`host_emit_progress` for
  `read_guest_string`, `host_cache_contains` for `read_guest_bytes`) — and a counting
  allocator in the test binary asserts **no allocation at or above 32 MiB happened**.
  This is the only way to catch a deleted helper guard: the guest sees the same
  result either way (`-1`, and for the cache ABI a plain `0`), so only the
  allocation itself distinguishes "refused" from "allocated, then failed";
- a second end-to-end test returns a forged 64 MiB result-length prefix and asserts
  both the distinct error and the absence of a large allocation, so moving the
  allocation above the guard is caught as well;
- a third returns a length that is small on its own but whose `ptr + len` runs past
  the end of memory, pinning the check to the whole range rather than the length.

Each of those five mutations — deleting either helper guard, deleting the result
guard, moving the result allocation above its guard, and weakening the check to
`len` alone — was applied and confirmed to turn the suite red.

A sixth test guards the *class* rather than the instances: it asserts that guest
memory is read in exactly two places in `host_functions.rs` (inside the two
bounds-checked helpers). The original vulnerability existed because the raw
allocate-then-read shape had been copied to ten call sites; this fails if an
eleventh appears, instead of waiting for someone to notice.

## Consequences

### Positive
- A guest-supplied `len` can no longer force a host allocation larger than the
  guest's own (already bounded) memory. Host-function reads fail fast with the same
  ABI error sentinel the out-of-bounds `memory.read` would have produced, minus the
  allocation; the post-invocation result path deliberately reports a distinct
  `"result length exceeds guest linear memory"` so its guard is observable in tests.

### Behavior
- Legitimate in-bounds reads are unchanged (the extra check is a single
  comparison). Out-of-bounds reads already failed; they now fail before rather
  than after allocating.

### DST Compliance
- Pure integer arithmetic (`checked_add`, comparison); no wall clock, no threads,
  no `HashMap`, no ambient I/O. Deterministic.

## Non-Goals / Follow-ups
- **Aggregate host-copy budget — tracked as ARN-348.** This ADR bounds each
  allocation by the guest's own memory; it does not bound their *sum*. In-bounds
  paths still amplify: `host_evaluate_spec` holds four reads at once, the outbound
  stream channel is bounded in chunks rather than bytes (so large in-bounds writes
  can pin far more than the intended ~1 MiB), and `max_response_bytes` is still not
  enforced on the HTTP body path. Fuel and `max_duration` bound execution, not
  resident host bytes, so they are not substitutes. That work is a functionality
  cap as much as a security one and needs its own sign-off.

## Alternatives Considered
1. **Clamp `len` to a fixed constant.** Rejected: a fixed cap either breaks
   legitimate large reads or is still larger than needed; bounding by the guest's
   own memory size is exact and self-adjusting.
