# ADR-0046: WASM Host Function for Blob-Ref Field Reads

> **Implementation note:** the shipped host function is `host_read_field(field_name_ptr, field_name_len, buf_ptr, buf_len) -> i32`, a direct memory-buffer write matching the `host_get_context` pattern. The ADR text below references an earlier stream-based draft (`host_read_field_stream`) — that shape was dropped during implementation because Temper's host has no stream-read-back primitive (streams are one-way: host → HTTP / hash / cache, never into WASM memory). The behavioral contract (plain vs. blob-ref resolution, return codes `-1`/`-2`/`-3`, inline-ceiling split, pre-fetched `blob_cache`) is identical; only the byte transport changes. Return shape matches `host_get_context`: if `needed > buf_len` the caller resizes and retries.

- Status: Accepted
- Date: 2026-04-16
- Deciders: Temper core maintainers
- Supersedes: —
- Related:
  - ADR-0040: Blob-Backed Overflow for Large Entity Field Values
  - ADR-0045: Field-Overflow Inline Ceiling
  - `crates/temper-wasm/src/engine/host_functions.rs`
  - `crates/temper-wasm/src/stream.rs`
  - `crates/temper-server/src/state/dispatch/wasm.rs`
  - `crates/temper-server/src/blobs.rs`
  - `crates/temper-wasm-sdk/src/context.rs`
  - `crates/temper-wasm-sdk/src/host.rs`

## Context

ADR-0040 introduced blob-backed field overflow for oversize entity values. OData reads hydrate blob refs transparently via `hydrate_blob_refs_in_value` (`temper-server/src/blobs.rs:131`). ADR-0045 raised the inline ceiling to 128KB so that the common-case oversize field (Session.user_message et al.) stays inline and is directly readable by WASM guests.

That still leaves the > ceiling case: any field whose serialized value exceeds `DEFAULT_FIELD_INLINE_MAX` (128KB) lives in `fields` as a `{"__temper_blob_ref": "...", "__temper_blob_size": N, "__temper_blob_encoding": "json"}` reference object. The OData path resolves these automatically; the WASM invocation context path does not. `crates/temper-server/src/state/dispatch/wasm.rs` serializes `entity_state` straight into `WasmInvocationContext` with no hydration pass, so a WASM module that reads `fields["big_output"].as_str()` sees the ref envelope, not the bytes.

Two options exist to close this gap. Transparent hydration at the WASM handoff (walk the JSON, fetch all refs, inline them) — safe for correctness but dangerous for memory: a single 200MB field becomes a 200MB invocation context, which blows past any reasonable `CTX_BUF_LEN` and consumes the WASM module's entire heap budget in one go. Or explicit opt-in hydration via a host function plus a pre-populated blob cache — bounded memory, one extra call per field the guest actually needs, and aligned with the existing stream-based host-function pattern (`host_cache_to_stream`, `host_http_call_stream`).

## Decision

Add a new host function `host_read_field_stream` that resolves a field name to bytes written into a pre-allocated stream. Pair it with two SDK helpers (`Context::read_field_string`, `Context::read_field_bytes`) that wrap the host call and abstract plain-vs-ref detection from module authors. Pre-fetch oversize blob-ref fields at dispatch time into a per-invocation `blob_cache` on `HostState` so the host function stays synchronous.

### Sub-Decision 1: Host function signature

```text
host_read_field_stream(
    field_name_ptr: i32, field_name_len: i32,
    stream_id_ptr:  i32, stream_id_len:  i32,
) -> i32
```

Return codes:

- `> 0`  — bytes written to stream. For plain values, the bytes are the UTF-8 JSON serialization (`serde_json::to_vec(value)`). For blob-ref fields, the bytes are the decoded blob payload (i.e., the original oversize JSON value), not the envelope.
- `0`    — field exists and its value is `null`, `""`, or `[]`. Stream is written with zero bytes so the caller can distinguish "no value" from "missing".
- `-1`   — field is not in `entity_state.fields`.
- `-2`   — field is a blob ref that the pre-fetch failed to resolve. Paired with a `tracing::warn!` on the host.
- `-3`   — generic host error (memory read failure, stream store failure). Reserved for consistency with other host functions; not expected in practice.

**Why synchronous**: the host function runs inside `wasmtime::Linker::func_wrap`, which is sync. The existing `host_cache_to_stream` and `host_http_call_stream` are sync in the same way — async work happens before invocation (or outside `spawn_blocking`), and results land in `HostState`. Following the same pattern keeps the wasmtime call path uniform and avoids introducing a new async executor inside the sandbox.

### Sub-Decision 2: Pre-fetched `blob_cache` on `HostState`

Add a new field to `HostState`:

```rust
pub blob_cache: BTreeMap<String, Vec<u8>>, // blob_key -> decoded bytes
```

At dispatch time (`state/dispatch/wasm.rs`), before entering `tokio::task::spawn_blocking`:

1. Walk `entity_state.fields` for blob refs (using the existing `collect_blob_ref_pointers` from `blobs.rs`).
2. For each ref whose `__temper_blob_size` exceeds the inline ceiling, collect its `__temper_blob_ref` key. Sort keys lexicographically (DST determinism).
3. `join_all` a batch of `blobs::get_blob_bytes` calls.
4. Populate `blob_cache` with successful fetches. Log failures.

The host function reads from this cache synchronously. Because the pre-fetch is gated on "exceeds the inline ceiling", fields ≤ ceiling still take the existing inline-hydration path (they're already plain values in `fields`); only the genuinely oversize case triggers the extra fetch.

**Why BTreeMap not HashMap**: `temper-wasm` is simulation-visible; `HostState` participates in `Store::new()` setup and any iteration order must be deterministic.

**Invalidation**: `blob_cache` is per-invocation. The host state is constructed fresh for each `invoke_blocking` call — no cross-invocation mutation, no stale-cache bugs.

### Sub-Decision 3: Inline-hydrate below the ceiling, leave refs above

`hydrate_blob_refs_in_value` becomes `hydrate_blob_refs_in_value_with_ceiling(value, store, max_inline_bytes)`. The existing no-ceiling version (used by OData) remains; internally it calls the new one with `max_inline_bytes = usize::MAX`. The new dispatch code path passes `max_inline_bytes = DEFAULT_FIELD_INLINE_MAX`.

The dispatcher always runs the new helper before building the invocation context. Result:

- Field ≤ ceiling with a blob ref: hydrated in place. Module reads it as a plain value directly.
- Field > ceiling with a blob ref: ref stays in the context. Module must call `ctx.read_field_string(name)` (or `read_field_bytes`) to get the value.

The hydration pass exists even when all fields are already plain — it's a no-op walk. DST and runtime impact are negligible on entities without refs.

### Sub-Decision 4: SDK helpers

`crates/temper-wasm-sdk/src/context.rs`:

```rust
impl Context {
    /// Read a field as a String. Works for plain string values and for
    /// blob-ref fields; hydrates via host function when needed.
    pub fn read_field_string(&self, name: &str) -> Result<String, String> { ... }

    /// Read a field as bytes (UTF-8 JSON serialization for plain values,
    /// raw bytes for blob refs).
    pub fn read_field_bytes(&self, name: &str) -> Result<Vec<u8>, String> { ... }
}
```

These detect the `__temper_blob_ref` envelope. If present, call `host_read_field_stream` with a unique stream id (e.g. `fmt!("__field_read:{name}")`), then read the stream back via the existing SDK stream-read helper. Otherwise parse the plain value.

Module authors don't branch: one call site, correct behavior either way. This is the contract we want — the old behavior ("read `fields["x"]` as string") silently corrupts on blob refs; the new helper is the safe replacement.

### Sub-Decision 5: FFI declaration

`crates/temper-wasm-sdk/src/host.rs` gains the `extern "C"` declaration for `host_read_field_stream` alongside the existing imports. No new Cargo dependency.

## Rollout Plan

1. **Phase 1 (landed — ADR-0045)** — inline ceiling raised; paw-agent consumers unaware of blob refs.
2. **Phase 2 (this ADR)** — host function + SDK helpers + dispatcher prefetch. Infrastructure only; no consumer migrated yet. Ships behind the ADR with tests only.
3. **Phase 3 (separate, OpenPaw)** — `workspace_provisioner` and `llm_caller` migrated to `ctx.read_field_string`. Unblocks openpaw#58 for the > 128KB tail.

## Consequences

### Positive

- WASM modules get a safe, uniform way to read fields regardless of size. The plain-vs-ref branching is pushed into the SDK, not each module.
- Pre-fetch at dispatch keeps the wasmtime call path synchronous (no new async runtime inside the sandbox). Latency is paid once per invocation per oversize field, not once per guest read.
- Memory is bounded by the fields the guest actually asks for (via stream writes), not by the total size of all ref fields in `entity_state`.
- Reuses existing primitives: `hydrate_blob_refs_in_value`, `get_blob_bytes`, `StreamRegistry::store_stream`. No new storage, no new protocol.

### Negative

- Every invocation pays the cost of a JSON-pointer walk over `entity_state.fields` to detect oversize refs. For entities with no refs, this is O(n) over the fields map with no I/O. Acceptable.
- Per-invocation `blob_cache` duplicates bytes between the store and the host state. For a single 10MB field used once, that's a 10MB allocation that immediately goes away when the invocation ends. Acceptable.
- `HostState` grows a new field. Serialized size unaffected; runtime memory grows only when the cache is populated.

### Risks

- A malicious or buggy guest could call `host_read_field_stream` for the same field many times, re-writing the stream each time. Existing `StreamRegistry` behavior (stream-id-keyed; stored bytes replace on same id) makes this idempotent. No storage amplification.
- A field declared as ref but missing from the blob store (orphan ref) returns `-2` instead of silently reading `""`. Guests that fail to check the return code will see empty bytes, which is the same failure mode as the status quo — but the host-side `tracing::warn!` surfaces it to operators.

### DST Compliance

- `blob_cache: BTreeMap<String, Vec<u8>>` — deterministic iteration order.
- `join_all` of blob fetches is run *before* `spawn_blocking`, i.e., on the async runtime thread. Order of fetches is deterministic because keys are sorted. The resulting `BTreeMap` is identical across runs.
- `tracing::warn!` is deterministic (pure side effect).
- No new wall-clock reads, no new RNG.
- Host function is a `Linker::func_wrap` — runs on the single-threaded wasmtime store. No concurrency inside the sandbox.

## Non-Goals

- Per-field ceiling override in spec (deferred to Phase 4 with `overflow_ttl_seconds`).
- Streaming very large blobs in chunks. Current approach reads the full blob into `Vec<u8>` then writes it to the stream. If multi-hundred-MB fields become common, chunked reads can be added without changing the host-function contract.
- Cedar authorization on field reads. `host_read_field_stream` is implicitly authorized — if the module can run, it can read any field in `entity_state`. Matches current semantics.
- Cross-invocation caching. `blob_cache` is per-call.

## Alternatives Considered

1. **Transparent hydration at handoff.** Walk and inline every blob ref before the WASM call. Simplest possible API (module code unchanged). Rejected because an unbounded field size becomes an unbounded context size — exactly the pathology the ceiling was designed to prevent. Leaves no path for > 128KB fields except the ceiling itself.
2. **Pure stream host function with no ceiling-gated prefetch.** Every oversize field read requires an explicit host call, including ones that would have fit under a reasonable ceiling. More consistent, but 9 existing WASM modules would need to branch on ref-vs-plain for every field read. ADR-0045 + this hybrid keeps the churn at two modules (migrated in Phase 3).
3. **Make the host function async via `block_on`.** Avoids the prefetch step. Rejected because `spawn_blocking` runs the wasmtime task on a thread without a tokio runtime handle, and installing one inside the sandbox violates DST's single-threaded-simulation rule.
4. **Chunked read API (`host_read_field_chunk(field, offset, len)`).** Useful for very-large payloads. Deferred — the current API is strictly simpler and can compose with chunking later by layering on the SDK side without a new host function.

## Rollback Policy

Two-step revert. (1) Remove the `host_read_field_stream` linker entry in `host_functions.rs` — SDK calls start returning a link error, alerting any caller that adopted it. (2) Remove the `blob_cache` field from `HostState` and the prefetch in the dispatcher. The `hydrate_*_with_ceiling` refactor stays — it's strictly additive and benefits OData too. No data migration.
