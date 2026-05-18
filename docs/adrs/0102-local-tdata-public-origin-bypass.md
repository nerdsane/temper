# ADR-0102: Local TData Public Origin Bypass

- Status: Proposed
- Date: 2026-05-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0099: Local WASM TData Host Path
  - ADR-0100: WASM Invocation Phase Observability
  - `crates/temper-server/src/state/dispatch/wasm/local_tdata_host.rs`
  - `crates/temper-server/src/state/mod.rs`

## Context

ADR-0099 added `LocalTDataWasmHost`, which keeps WASM guest `/tdata` calls
inside the process when the URL points at loopback. That preserved the Temper
mission: guests still traverse the same OData handlers, Cedar authorization,
spec verification gates, entity actors, event journal, and query projection
updates, but without a network hop back into the same service.

The current production trace evidence shows an uncovered shape. The
TemperPaw live proof on version
`f5ffa7d10192fca607943c384bfdeebd71360cf5` used public URLs such as
`openpaw-production.up.railway.app/tdata/SessionEntries` from WASM guest code.
The routed proof trace `9d0317bdbf78eeb763235a443d9fe34a` showed:

- `workspace_provisioner` issuing two `POST /tdata/SessionEntries` calls
  through the public Railway origin, with host-span durations around
  `127 ms` and `131 ms`;
- the child OData spans were still in the same `temperpaw` service and were
  dominated by the normal `entity.get_or_create_tenant_entity` create path;
- Datadog aggregation for the same version showed recent WASM hot-path calls
  grouped under `openpaw-production.up.railway.app/tdata/...`, not local
  in-process TData.

This means the platform already has the semantics-preserving local dispatch
architecture, but it only recognizes loopback hostnames. When an app, route,
or direct caller supplies the service's public origin as `temper_api_url`, the
guest leaves the process, enters the public HTTP edge, and returns to the same
service before running identical OData code.

That is not a fundamental Temper limitation. It is an overlooked origin
normalization gap at the host boundary.

## Decision

Extend the local TData host to recognize explicitly allowlisted public origins
for the current service, in addition to loopback hosts.

### Public Self-Host Allowlist

`ServerState` will carry a bounded set of local TData public hosts loaded once
at startup from:

- `TEMPER_LOCAL_TDATA_HOSTS`: comma-separated hostnames, with optional full
  URLs accepted for operator ergonomics;
- `TEMPER_PUBLIC_BASE_URL`: full base URL for generic Temper deployments;
- `PUBLIC_BASE_URL`: app-level public URL used by TemperPaw and webhook
  transports;
- `RAILWAY_PUBLIC_DOMAIN`: Railway-provided domain, interpreted as HTTPS host.

Only hostnames are stored. Schemes, paths, trailing slashes, and ports are
discarded. Empty and invalid values are ignored.

### Dispatch Rule

`LocalTDataWasmHost` will treat a guest URL as local when:

1. the scheme is `http` or `https`;
2. the path is under `/tdata`;
3. the method is `GET` or `POST`;
4. the host is either loopback or appears in `ServerState.local_tdata_hosts`;
5. the path is not a `Files('...')/$value` stream path, which remains on the
   existing binary/stream handling.

The intercepted request still uses the same in-process OData handlers as
ADR-0099. Headers, tenant identity, principal headers, authorization, entity
state transitions, projection writes, and response bodies remain unchanged.

### Observability

The existing `wasm.local_tdata_http_call` span remains the proof point. When
the bypass is active, traces should show:

- no `wasm.host.http_call` span to the public `/tdata` origin for that call;
- a `wasm.local_tdata_http_call` span with `local_tdata = true`;
- unchanged child OData, dispatch, projection, and database spans.

## Rollout Plan

1. **Phase 0 (Immediate)**: ship the core allowlist and parser tests, with no
   default behavior change unless one of the public-origin environment
   variables is set.
2. **Phase 1 (TemperPaw deploy)**: ensure production has `PUBLIC_BASE_URL` or
   `RAILWAY_PUBLIC_DOMAIN` available and run direct plus routed live proofs.
3. **Phase 2 (Broader platform)**: consider extending the same self-origin
   bypass to selected `/api/*` internal endpoints after separate evidence and
   ADR review.

## Readiness Gates

- Unit tests prove loopback behavior still works.
- Unit tests prove public hosts are intercepted only when allowlisted.
- Unit tests prove non-allowlisted external `/tdata` URLs still delegate.
- Local focused `temper-server` tests pass.
- Live Datadog traces prove public-origin `/tdata` spans are replaced by local
  TData spans, with SessionEntry correctness checks still passing.

## Consequences

### Positive

- Removes unnecessary public-edge round trips from WASM guest calls back into
  the same service.
- Keeps Temper's correctness model intact because the OData handlers and
  downstream actor/projection path are unchanged.
- Works even when a direct caller or legacy route config supplies a public
  `temper_api_url`.
- Gives operators an explicit rollout knob instead of guessing from every
  external hostname.

### Negative

- Adds one more runtime configuration surface to document and test.
- Cross-deployment calls to another Temper server with a matching hostname
  would be intercepted, so the allowlist must only contain origins owned by the
  current process.

### Risks

- Misconfigured allowlists could route intended remote calls locally. Mitigation:
  keep the default allowlist empty except for explicit environment-derived
  current-service origins, and preserve external delegation for all other hosts.
- The bypass removes public edge latency from traces, which may make before/after
  comparisons look like missing HTTP spans. Mitigation: rely on
  `wasm.local_tdata_http_call` and child OData spans for continuity.

### DST Compliance

- Environment is read only during `ServerState` initialization, matching
  existing startup-only configuration helpers and using `// determinism-ok`
  annotations.
- The per-call URL decision is deterministic from request URL plus immutable
  state carried by `ServerState`.
- No wall-clock, random, filesystem, or threaded behavior is introduced.

## Non-Goals

- Do not bypass OData, Cedar, entity actors, projection writes, or event
  journaling.
- Do not optimize the `entity.get_or_create_tenant_entity` internals in this
  slice.
- Do not intercept arbitrary external `/tdata` services.
- Do not intercept `/api/ots`, file `$value`, or other non-TData endpoints in
  this ADR.

## Alternatives Considered

1. **Normalize every guest `temper_api_url` to loopback in TemperPaw**.
   Rejected as incomplete: direct callers and stale route configs can still
   pass the public origin, and the semantic boundary belongs at the host.
2. **Intercept every `/tdata` URL regardless of host**. Rejected because it
   breaks legitimate cross-service Temper calls and violates explicit
   multi-tenant/multi-deployment boundaries.
3. **Create an app-specific SessionEntry fast path**. Deferred. It may still
   be useful, but the trace shows a transport-origin gap that can be removed
   without changing application semantics.

## Rollback Policy

Unset `TEMPER_LOCAL_TDATA_HOSTS`, `TEMPER_PUBLIC_BASE_URL`, `PUBLIC_BASE_URL`,
and `RAILWAY_PUBLIC_DOMAIN`, or revert the code change. Loopback local TData
dispatch from ADR-0099 remains unchanged.
