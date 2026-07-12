# ADR-0156: Invocation-scoped HTTP stream capabilities (ARN-207)

- Status: Accepted
- Date: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ARN-207: Global enumerable WASM stream handles permit cross-tenant theft
  - ADR-0069: HttpEndpoint inbound path-prefix routing
  - `crates/temper-wasm/src/http_stream.rs`
  - `crates/temper-wasm/src/host_trait.rs`
  - `crates/temper-server/src/router.rs`

## Context

`ServerState` shares one `HttpStreamRegistry` across all tenants and requests.
Handles were sequential `u32` values. Guest-facing `ProductionWasmHost` stream
ops (`read` / `try_write` / `close` / head delivery) validated only that the
raw handle existed in the registry. A process-global integer was therefore an
authority-bearing capability: a malicious guest could enumerate another
tenant's active handle, read its request body, inject or close its response,
and leave leaked entries after timeouts.

## Decision

### Sub-Decision 1: Invocation-local capability table (fail closed)

Each `ProductionWasmHost` carries an invocation-scoped **grant set** of stream
handle IDs. Guest-facing stream operations require the handle to be present in
that set; otherwise they return `StreamError::InvalidHandle` with no further
registry interaction.

Grants are issued only by:

1. `http_stream_begin_outbound` — automatically grants the guest request/response
   handles of the newly opened exchange.
2. `grant_stream_handles` — used by the HttpEndpoint dispatcher when minting an
   inbound exchange so the guest may use the handles published in
   `HttpDispatchContext`.

A host with no grants cannot operate on any stream. Sharing a registry is no
longer sufficient to act on another invocation's handles.

**Why this approach**: The authority is the capability table, not the integer.
This matches least privilege and keeps the WASM ABI (`u32` handles) stable.

### Sub-Decision 2: Opaque non-enumerable handle IDs

Handle IDs are allocated from unguessable `u32` material (UUID-v4 bits), not
a global sequential counter. Enumeration of low IDs cannot discover live
handles. Ownership still fails closed even under collision (collision → next
ID).

### Sub-Decision 3: Direction stays structural

Each registry slot remains a sender or receiver end. Wrong-direction ops still
return `InvalidHandle`. Kernel-side ends of an exchange are never granted to
the guest, so the guest cannot close or write the kernel pump handles even
when it knows the ID from a leak.

### Sub-Decision 4: Budgets and cleanup

- Global registry handle budget and per-invocation grant budget bound resource
  use (concurrent streams / DoS).
- `close_granted_streams` / dispatcher cleanup closes granted guest ends on
  success, failure, timeout, and cancellation so entries do not retain forever.

### Sub-Decision 5: Kernel path stays privileged

Bridge and axum pump tasks continue to use registry `read` / `write` / `close`
directly with the exact handles they created. They never accept guest-supplied
handle IDs.

## Rollout Plan

1. **Phase 0 (this PR)** — grant table + opaque IDs + budgets + cleanup hooks +
   exploit regressions + live local E2E.
2. **Phase 1 (optional follow-up)** — richer endpoint/direction tags on grants
   for observability; per-tenant stream metrics in Observe.

## Consequences

### Positive

- Cross-tenant and cross-invocation stream theft/injection is denied by
  construction.
- Sequential handle guessing no longer yields foreign bodies.
- Working inbound/outbound streaming paths remain intact for granted handles.

### Negative

- Guests and hosts must obtain handles via begin/grant; tests that assumed
  global raw-handle authority need grants.

### Risks

- Missing a `grant_stream_handles` call on a new dispatcher path fails closed
  (guest sees InvalidHandle). Prefer that over open-by-default.

### DST Compliance

- Core registry lives in `temper-wasm` (I/O host path). `temper-server` router
  wiring is production-async; handle allocation uses UUID (annotated
  `// determinism-ok` where required). Concurrent isolation is covered by
  deterministic unit/integration tests, not wall-clock races.

## Non-Goals

- Changing the guest WASM ABI away from `u32` handles.
- Per-chunk encryption of stream payloads.
- Removing the shared registry (sharing is fine; authority is not shared).

## Alternatives Considered

1. **Per-tenant registries only** — blocks cross-tenant theft but not
   same-tenant cross-invocation; still leaves sequential guessability.
2. **Cedar policy on every stream op** — heavier and still needs a handle
   capability object; grants are the capability.
3. **Invocation-local indices remapped at the FFI boundary** — stronger
   non-addressability but larger ABI/engine change; deferred.

## Rollback Policy

Revert the PR: grants become unused, sequential IDs return. Do not partial-roll
grants without restoring deny-by-default — that would re-open the hole.
