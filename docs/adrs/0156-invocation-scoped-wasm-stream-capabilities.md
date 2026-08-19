# ADR-0156: Invocation-scoped WASM stream capabilities

- Status: Accepted
- Date: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ADR-0057: `http_call_streaming` surface (created the `HttpStreamRegistry`)
  - ADR-0069: HttpEndpoint dispatch (shares the registry between dispatcher and guest)
  - `crates/temper-wasm/src/http_stream.rs` (registry + handles)
  - `crates/temper-wasm/src/host_trait.rs` (`ProductionWasmHost` stream methods)
  - `crates/temper-server/src/router.rs` (inbound HttpEndpoint dispatch)
  - ARN-207 (security finding)

## Context

ADR-0057 gave WASM guests a bidirectional HTTP streaming surface backed by an
`HttpStreamRegistry` of bounded channels. Each open channel end is addressed by a
`StreamHandle(u32)`. ADR-0069 made the HttpEndpoint dispatcher and the per-request
guest host **share one registry instance** so the kernel can mint an inbound
exchange before invoking the guest and hand the guest the handle IDs it should
read/write.

Two facts combine into a cross-tenant vulnerability (ARN-207):

1. **The shared registry is process-global.** `ServerState.http_stream_registry`
   is one instance cloned into every per-request `ProductionWasmHost`. Every
   tenant's active handles live in the same `BTreeMap<u32, _>`.
2. **The handle is the only authority, and it is guessable.** IDs are allocated
   from a single monotonic `next_id` counter, so they are small sequential
   integers. `http_stream_read`/`try_write`/`close` and the response-head
   operations validate only that the raw `u32` exists in the map. `AuthorizedWasmHost`
   delegates them straight through with no tenant, invocation, endpoint, or
   direction ownership check.

So a malicious guest can pass a `u32` it never received — a nearby integer
belonging to another tenant's in-flight request — and read that request's body,
inject or truncate its response, or close its stream. A process-global integer
is being used as an authority-bearing capability. Registry entries also
accumulate: an invocation that ends without explicitly closing its handles
leaves them resident in the global map indefinitely.

## Decision

Bind every handle to the invocation that owns it, and make that binding the
authority. The guessable integer becomes a mere index that is only meaningful
inside its owning scope.

### Sub-Decision 1: `StreamScope` — an ambient, per-invocation ownership token

Introduce `StreamScope(u64)`, minted per WASM invocation. Every handle created
for that invocation is tagged with its scope. The scope is carried **ambiently
on the host** (`ProductionWasmHost.stream_scope`) and **never crosses the guest
FFI boundary** — the guest FFI still passes only a raw `u32`. When the guest
calls `host_http_stream_read(handle)`, the host presents *its own* scope to the
registry, which rejects the operation unless the handle's owning scope matches.

**Why this approach**: the scope is unforgeable by the guest precisely because
the guest never names it — it is host-side ambient state, not a value in the
guest's address space. This is strictly stronger than making the integer
unguessable: even if a guest learns another invocation's exact handle ID, it
cannot present that invocation's scope, so the read is denied. It also needs no
randomness, which keeps the design deterministic and simulation-friendly (no
`OsRng`/`getrandom`). The finding's "non-addressable invocation-local indices"
requirement is met: an index is only addressable from within its own scope.

Handle IDs are allocated from a monotonic `u32` counter and **not reused** until
it wraps at 2^32 allocations — unreachable in any real process lifetime (a single
FFI `u32` handle bounds this by design), so a freed ID is not reallocated to a
different scope. This makes the ownership check robust against
time-of-check/time-of-use races: the worst case for a racing close is a benign
`InvalidHandle`, never a cross-scope leak. (The scope counter is a `u64` for the
same reason with far more headroom.)

### Sub-Decision 2: Guest-facing vs kernel/bridge handles

An inbound exchange creates four handles; an outbound exchange creates four plus
a head channel. Only two of each are ever handed to the guest. Each handle
records a `guest_facing` bit. Guest operations require `guest_facing == true`
**and** a scope match; the kernel pump/drain tasks and the outbound bridge task
use the existing privileged, non-scope-checked registry methods
(`read`/`write`/`close`/`await_*`) because they are trusted kernel code that
already holds the exact handle. A guest therefore cannot touch the
kernel-facing side of its own exchange (e.g. read the kernel's copy of its
response), which closes the residual within-invocation direction-confusion
surface in addition to the cross-tenant one.

Direction (read vs write) is already enforced structurally by the
`Sender`/`Receiver` typing of a handle and is unchanged.

### Sub-Decision 3: Scope-bound lifecycle cleanup

`HttpStreamRegistry::close_scope(scope)` removes every handle, pending read, and
head channel owned by a scope in one call; dropping the channels signals EOF/abort
to any peer and unblocks the pump/bridge tasks. The inbound dispatcher creates a
`ScopeCleanupGuard` **the moment it opens the exchange** and holds it for the
whole request, moving it into the response body stream on the success path. So
the scope is reclaimed on every exit: the error/timeout paths call `close_scope`
explicitly (guaranteed, awaited), the success path drops the guard when the body
finishes draining, and — critically — if the handler future is dropped
mid-request (client disconnect while the guest runs or while we await its head)
the guard's `Drop` still reclaims the scope. `Drop` first tries a non-blocking
lock; if the registry is momentarily contended it hands the reclaim to the async
runtime rather than skipping it, so cleanup is guaranteed, not best-effort.
Cleanup is bounded to the invocation rather than leaking into the process-global
map.

### Sub-Decision 4: Per-scope concurrent-stream bound

A guest can open outbound streaming exchanges in a loop. Each *live* exchange is
counted against a per-scope budget (`MAX_OUTBOUND_STREAMS_PER_SCOPE`); once the
limit is reached, `open_outbound_exchange` returns `StreamError::Aborted` and the
guest's `http_stream_begin_outbound` surfaces it as an error.

A slot is occupied from `open_outbound_exchange` until the exchange is **fully
done**: its bridge task has completed (socket closed, request buffer gone) **and**
its guest response handle has been closed/drained (response buffer gone). It is
freed by whichever of those two happens last — the bridge's
`release_outbound_exchange`, or the guest's `close` of the response handle.

Two adversarial rounds shaped this. Keying the slot on the *guest close* alone let
a guest loop `begin_outbound → close(response)` and free slots while the bridge,
its socket, and a channel of buffered request bytes stayed live. Keying it on
*bridge completion* alone (the first fix) then let a guest open exchanges, let
their bridges finish, and accumulate undrained response handles and their buffers
without limit — uncounted, because the slots were freed. Requiring **both**
conditions bounds both dimensions: at most `MAX_OUTBOUND_STREAMS_PER_SCOPE`
exchanges are ever live, counting live sockets *and* undrained response buffers.
A guest that makes many calls *sequentially* — each fully done (bridge complete
AND response **closed**) before the next — is never throttled. A guest that drains
to EOF without closing is throttled at the cap until request end; see the
`close`-on-EOF residual (ARN-359) below.

Scope of this bound, stated precisely so it is not read as more than it is:
- It bounds the **number** of live exchanges per invocation, hence the number of
  concurrent sockets, request channels, and undrained response channels.
- It does **not** bound bytes. Each channel is bounded in *chunks* (64), not
  bytes, so a guest writing large in-bounds chunks can hold far more than the
  nominal 1 MiB per channel. That aggregate per-invocation byte budget is
  **ARN-348**, not closed here.
- The slot bounds *concurrent* live exchanges within a scope, but two edges keep
  it from being an exact "socket lifetime" bound, both tracked rather than closed
  here (adversarial review, ARN-207):
  - `close_scope` cancels the scope's live reqwest bridge tasks (their
    `AbortHandle`s are retained per scope, keyed by exchange, and dropped on
    bridge completion so the map tracks only *live* bridges — not every call
    made), so sockets die promptly at request end rather than lingering until the
    outbound client's timeout (`WasmResourceLimits::max_duration`).
  - A closed scope is **terminal**: the registry tracks the set of *live* scopes
    (inserted at `mint_scope`, removed at `close_scope`), and
    `open_outbound_exchange` and `register_outbound_bridge` refuse a scope that is
    not live (a late registration aborts its bridge on the spot). A guest whose
    host call outlives `close_scope` — block-in-place surviving the invocation
    abort, or a disconnect racing the response drain — therefore cannot reopen the
    dead scope or slip a bridge in between spawn and registration. The live set is
    bounded by the number of *concurrent* invocations, not total requests, so it
    needs no eviction and has no eviction hazard (a closed-scope tombstone ring
    would eventually roll over and let a straggler reopen a forgotten scope).
  - A guest that reads a response to EOF but never calls `close` (the SDK does not
    close on EOF) holds the slot until request end, so such calls are throttled at
    `MAX_OUTBOUND_STREAMS_PER_SCOPE` rather than "never throttled". The honest fix
    is to close on EOF-drain, in the SDK or by auto-releasing a fully-drained
    bridge-complete slot — **ARN-359**.
- Inbound exchanges are kernel-minted one-per-request, so a guest cannot multiply
  them — but a guest that submits its response head and then never closes its
  response body can hold its own scope (and the request) open until the client
  disconnects. Bounding that, and a global/per-tenant HttpEndpoint request
  semaphore, is admission control — tracked as **ARN-356 / ARN-180**, not closed
  here.

## Consequences

### Positive
- Cross-tenant stream read/write/close/inject is denied at the authority layer,
  independent of handle guessability or whether the registry is shared.
- Registry entries no longer leak across requests; each invocation's streams are
  reclaimed when its dispatch ends.
- Concurrent-stream growth is bounded per invocation.
- No new randomness, so the fix is deterministic and testable in simulation.

### Negative
- `open_inbound_exchange`/`open_outbound_exchange` and `ProductionWasmHost`
  construction now require a scope. The ripple is small: only the inbound
  dispatcher and the host's own outbound path call these.

### Risks
- A misconstructed host that reuses another host's scope on the shared registry
  would defeat isolation. Mitigation: `with_shared_streams` takes the scope as a
  required parameter minted from the registry's monotonic counter, so a unique
  scope is guaranteed by construction; private-registry hosts use a fixed default
  scope safely because their registry is not shared.
- Handle ids are a monotonic `u32` and are not checked for wrap. After 2^32
  cumulative allocations the counter wraps and could collide with a live handle,
  weakening the "ids are never reused" premise the authorize-then-operate split
  relies on. Negligible in practice (billions of allocations per process
  lifetime) but not zero; tracked as **ARN-357**.

### DST Compliance
- The scope counter lives in `RegistryState` behind the existing async `Mutex`
  and is minted via `mint_scope().await`; it is a monotonic `u64`, deterministic
  under the single-threaded actor model. No wall clock, no `HashMap`, no new
  threads. `close_scope` iterates `BTreeMap`/`BTreeSet` in deterministic order.
- The router change threads a value and adds a cleanup call; it introduces no new
  `tokio::spawn`, wall-clock, or ambient I/O.

## Non-Goals

- Opaque random tokens. Ambient scopes make guessability irrelevant, so
  randomness is unnecessary. (`StreamScope` could later be randomized if a handle
  ever needs to be safely externalized, but nothing externalizes it today.)
- Bounding inbound exchanges per tenant across the whole process. Inbound
  exchanges are kernel-minted one-per-request, but there is no global or
  per-tenant HttpEndpoint request semaphore today, and a guest can hold its own
  request open by not closing its response body, so total live scopes are bounded
  only by client behaviour. This is admission control, tracked as ARN-356 /
  ARN-180.
- An aggregate per-invocation byte budget across all guest→host copies (streams
  and reads). Tracked as ARN-348.

## Alternatives Considered

1. **Per-dispatch registry (fresh `HttpStreamRegistry` per request).** Removes
   process-global sharing so handles are structurally isolated. Rejected as the
   *sole* fix because it does not give the authority layer a caller identity —
   the registry would still let any holder read any handle — so the exploit
   regression cannot be expressed or defended as a fast deterministic unit test,
   and a future re-introduction of sharing would silently reopen the hole. Scopes
   fix the authority model directly and compose with either sharing choice.
2. **Unguessable random handle IDs.** Raises the cost of guessing but keeps the
   integer as the sole authority and pulls randomness into a simulation-visible
   path. Rejected: ambient scopes are strictly stronger and deterministic.
