# ADR-0164: Bounds-check guest reads before allocating

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-wasm/src/engine/host_functions.rs` (guest memory reads)
  - ARN-226 (security finding)

> This is Fable's competing entry for ARN-226; compared head-to-head by the arena judge.

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

The allocation is therefore bounded by the guest's linear memory size, which is
itself capped by `WasmResourceLimits::max_memory` — an attacker can no longer make
the host allocate more than the guest could legitimately hold, and an oversized
`len` is rejected before any allocation.

## Consequences

### Positive
- A guest-supplied `len` can no longer force a host allocation larger than the
  guest's own (already bounded) memory; oversized reads fail fast with the same
  error the out-of-bounds `memory.read` would have returned, minus the allocation.

### Behavior
- Legitimate in-bounds reads are unchanged (the extra check is a single
  comparison). Out-of-bounds reads already failed; they now fail before rather
  than after allocating.

### DST Compliance
- Pure integer arithmetic (`checked_add`, comparison); no wall clock, no threads,
  no `HashMap`, no ambient I/O. Deterministic.

## Non-Goals / Follow-ups
- A per-read byte budget stricter than the guest memory size (so even an
  in-bounds but large read is capped) is a possible tightening, tracked as a
  follow-up; the disclosed vector (unvalidated `len` → pre-check allocation) is
  closed here.

## Alternatives Considered
1. **Clamp `len` to a fixed constant.** Rejected: a fixed cap either breaks
   legitimate large reads or is still larger than needed; bounding by the guest's
   own memory size is exact and self-adjusting.
