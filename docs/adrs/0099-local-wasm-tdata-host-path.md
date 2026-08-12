# ADR-0099: Local WASM TData Host Path

- Status: Superseded
- Date: 2026-05-17
- Deciders: Temper core maintainers
- Superseded by: ADR-0157
- Related:
  - ADR-0095: Projection Transaction Fast Path
  - ADR-0098: Background WASM Trace Retention
  - `crates/temper-server/src/state/dispatch/wasm.rs`
  - `crates/temper-wasm/src/host_trait.rs`

## Context

TemperPaw's staged Session turn is now observable enough to distinguish cold
start effects from warm-path transport cost. The PERF-014 production proof on
TemperPaw version `2fe2d2178951e180c004539d2c3d39fc4e7750b2` showed that the
declared Session hot WASM modules no longer perform lazy blob fetches. The same
proof still spends meaningful user-visible time in same-service calls made by
WASM modules:

- `workspace_provisioner` spends about 150-170 ms creating and verifying the
  initial `SessionEntry` chain.
- `provider_response_applier` spends about 100-110 ms appending and verifying
  the assistant `SessionEntry`.
- routed replies still pay local TData action dispatch and Channel reply work.

The existing guest contract is intentionally simple: WASM modules call
`host_http_call` against the configured `temper_api_url`, and Temper's normal
OData/TData handlers apply verification gates, relation checks, projection
updates, state-change events, and action dispatch. In production TemperPaw,
`temper_api_url` is a loopback URL to the same process. This means hot WASM
modules often leave the process through `reqwest`, traverse the Axum router over
TCP, and then immediately re-enter the same server.

That loopback transport does not add mission value. The mission value is in the
governed write/read path, event audit, Cedar/WASM authorization boundary, tenant
isolation, and projection correctness. We can preserve those while removing the
same-process HTTP hop.

## Decision

Introduce a server-side WASM host wrapper for local TData calls.

When a WASM integration calls textual `host_http_call` with `GET` or `POST`
against a loopback URL whose path is under `/tdata`, the Temper server will
execute the existing OData handlers in-process and return the same status/body
shape to the guest. All other HTTP calls continue through the existing
`ProductionWasmHost`.

### Sub-Decision 1: Reuse Existing OData Handlers

The local path must call the same OData handler functions used by external HTTP
requests instead of writing directly to actors or stores.

**Why this approach**: this keeps verification gates, relation/invariant checks,
entity creation semantics, bound-action dispatch, response formatting, and
projection visibility on the same code path. The optimization is transport
shape, not data semantics. File `$value` paths are delegated in the first cut so
they continue using the existing native blob fast path.

### Sub-Decision 2: Keep WASM HTTP Authorization Outside The Wrapper

The wrapper sits inside the existing `AuthorizedWasmHost` chain. The current
WASM authorization gate still evaluates the module, tenant, trigger action,
target domain, method, and URL before the local OData handler runs.

**Why this approach**: local execution should not become a privilege bypass.
If a module is not allowed to call a TData URL today, the local path must deny it
the same way.

### Sub-Decision 3: Fallback To Network For Everything Else

The first implementation only intercepts textual `GET` and `POST`
`host_http_call` requests to loopback `/tdata` URLs. Non-loopback URLs,
non-TData URLs, `PUT`/`PATCH`/`DELETE`, File `$value` paths, streaming/binary
host calls, Connect calls, and malformed URLs delegate to the existing
production host.

**Why this approach**: this limits blast radius to the measured hot path while
keeping external provider calls, sandbox calls, blob streaming, and future API
surfaces unchanged.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add the local host wrapper, wire it into triggered
   and direct WASM invocations, and add focused unit/integration coverage.
2. **Phase 1 (TemperPaw Bump)** — Bump TemperPaw's Temper dependency to the
   merged Temper commit and run Session direct/routed proofs.
3. **Phase 2 (Production Proof)** — Deploy, verify `/paw/version`, direct and
   routed live runs, and compare Datadog traces for reduced OData loopback span
   cost in SessionEntry create/verify and local TData action calls.

## Readiness Gates

- Existing OData create, action, read, and `$value` tests still pass.
- WASM dispatch tests prove successful callback behavior still works.
- New tests prove local `GET`/`POST` `/tdata` calls return OData-compatible
  status/body without using network transport, and that delegated boundary paths
  still reach the production host.
- Datadog traces distinguish local TData host calls from delegated external HTTP
  calls.
- Live TemperPaw proofs preserve valid SessionEntry chains and routed replies.

## Consequences

### Positive

- Removes loopback TCP/client overhead from hot same-process WASM calls.
- Keeps guest modules unchanged.
- Preserves the OData/write path rather than adding app-specific backdoors.
- Makes future hot-path work easier because the next bottlenecks will be actor,
  projection, or module execution rather than artificial local transport.

### Negative

- The WASM host now depends on server routing behavior, so tests must protect
  parity with external OData responses.
- In-process calls will not naturally appear as outbound HTTP client spans.
  They need explicit tracing to remain visible.

### Risks

- A too-broad loopback detector could intercept a non-Temper local service.
  Mitigation: only intercept loopback URLs under `/tdata`; delegate all other
  URLs.
- Handler parity could drift if future OData handlers gain new extractor
  requirements. Mitigation: call the public handler functions directly and keep
  focused tests around create/read/action shapes.
- Binary `$value` traffic might still pay loopback cost. Mitigation: leave
  binary/streaming out of scope until a separate trace proves it is hot again.

### DST Compliance

- This touches `temper-server`, a simulation-visible crate.
- The wrapper does not spawn threads, introduce wall-clock sleeps, use random
  IDs, or alter actor scheduling.
- It reuses existing request handlers and delegates non-local HTTP to the
  existing production host.
- URL/query/header conversion uses deterministic iteration where maps are
  required.

## Non-Goals

- Do not create a TemperPaw-specific `SessionEntry` side channel.
- Do not collapse the Session state machine or skip SessionEntry read-back
  verification.
- Do not change Cedar policy semantics.
- Do not optimize external provider calls, sandbox calls, or binary blob
  streaming in this ADR.

## Alternatives Considered

1. **Native SessionEntry host function** — This would be faster for the current
   Session path, but it would expose an app-specific primitive from Temper core
   and risk bypassing generic OData correctness behavior.
2. **Composite provider-only Session executor** — This could remove more staged
   work, but it crosses a larger semantic boundary and needs a separate proof
   that event audit, recovery, trajectories, and approvals remain explainable.
3. **Remove read-after-write verification** — Rejected. The verification exists
   because production previously observed missing `SessionEntry` parents. Speed
   cannot come from making correctness probabilistic.

## Rollback Policy

The wrapper is a construction-time host choice. If it causes parity problems,
wire WASM invocation back to the plain `ProductionWasmHost` and keep all guest
WASM modules unchanged. If the feature needs a runtime kill switch, add it
before deployment rather than after a production incident.
