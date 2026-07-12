# ADR-0160: Guest memory validate-before-allocate (ARN-226)

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ARN-226: WASM guest lengths allocate before validation
  - `crates/temper-wasm/src/engine/guest_memory.rs`
  - `crates/temper-wasm/src/engine/host_functions.rs`
  - `crates/temper-wasm/src/engine/mod.rs` (HostState budget)

## Context

Host functions converted guest `i32` lengths to `usize` and allocated host
buffers before proving the guest memory range was valid or that an aggregate
per-invocation copy budget remained. A malicious module could force multi-GB
host allocations before wasmtime bounds checks ran.

## Decision

### Sub-Decision 1: Single guest-memory API

Guest→host copies use helpers that:

1. Reject negative `ptr` / `len`
2. Reject `ptr + len` overflow
3. Prove `ptr..ptr+len` is within current linear memory **before** allocation
4. Consume a per-invocation **guest copy budget** (default: `max_response_bytes`
   clamped, with a floor for small calls)

All host-function guest length parameters and the engine result-buffer path
must use this checked path (no direct `vec![0u8; guest_len]` from ABI lengths).

### Sub-Decision 2: Budget on HostState

`HostState` carries `guest_copy_budget` / `guest_copy_consumed`. Exhaustion
returns an error to the guest (fail-closed) without allocating.

### Sub-Decision 3: HTTP response body (follow-up)

`WasmResourceLimits.max_response_bytes` is reused as the guest-copy budget
floor input. Enforcing the same cap on **host HTTP response accumulation**
(`host_trait` buffering paths) is required for full ARN-226 closure but is
a separate land — not claimed done by this ADR's first ship.

## Consequences

### Positive

- Guest-controlled lengths cannot force unbounded host allocations on the copy
  path.
- Aggregate copy pressure is budgeted per invocation.

### Negative

- Legitimate large transfers must use stream handles / raise limits explicitly.

## Non-Goals

- Full table-growth / module-cache concurrency redesign (follow-up).
- HTTP response/request body accumulation caps on every `host_trait` path
  (follow-up; see Sub-Decision 3).
- Fuzz/property suite for adversarial guest lengths (follow-up).
