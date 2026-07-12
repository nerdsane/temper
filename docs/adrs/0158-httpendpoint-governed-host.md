# ADR-0158: HttpEndpoint uses one governed WASM host (ARN-208)

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - ARN-208: HttpEndpoint WASM bypasses the governed host
  - ADR-0156: Native adapter sandbox boundary (parallel trust edge)
  - `crates/temper-server/src/router.rs` (HttpEndpoint dispatch)
  - `crates/temper-server/src/http_endpoint_host.rs`
  - `crates/temper-server/src/state/dispatch/wasm.rs` (entity dispatch path)
  - `crates/temper-wasm/src/authorized_host.rs`

## Context

Entity WASM dispatch builds `AuthorizedWasmHost` around `ProductionWasmHost`
with Cedar-gated HTTP and secret access, and only bootstrap secrets up front.

The inbound `HttpEndpoint` fallback in the router constructed a raw
`ProductionWasmHost` with the **full tenant secret map** and forwarded every
inbound header (including `Authorization` / principal headers) into the guest
context. That is a second production invocation path that skips the trust
boundary.

## Decision

### Sub-Decision 1: One governed factory for HttpEndpoint

HttpEndpoint invocations must use the same authorization envelope as entity
dispatch:

1. Bootstrap secrets only (`get_authorized_wasm_host_bootstrap_secrets`)
2. Lazy secret resolver gated by `WasmAuthzGate`
3. Outer `AuthorizedWasmHost` with `WasmAuthzContext` for the endpoint module

Raw `ProductionWasmHost::with_shared_streams(...)` is not called from the
router without that envelope.

### Sub-Decision 2: Strip inbound auth material before guest delivery

Before building `HttpDispatchContext.headers`, drop:

- `authorization` / `proxy-authorization`
- `cookie` / `set-cookie`
- `x-api-key` / `x-temper-api-key`
- any header whose name starts with `x-temper-principal` (id, kind, scopes, attrs)
- any header whose name starts with `x-temper-agent-` (type, role, …)

Guests never receive ambient platform credentials or identity carriers from
the inbound request. Kernel-side action-bridge auth still reads the raw
request headers.

### Sub-Decision 3: Principal for host ops

Host authorization context is bound to the **integration module** and tenant
(module as Cedar principal), not to guest-supplied principal headers. Action
bridge auth remains separate (existing `requires_auth` path).

## Consequences

### Positive

- Endpoint guests cannot read arbitrary tenant secrets or perform ungated
  outbound HTTP.
- Auth headers are not reflected into guest code.
- One consistent production host construction story.

### Negative

- Endpoint modules need Cedar permits for secrets/HTTP they legitimately use
  (same as entity integrations).

### Risks

- Policies that only covered entity dispatch may need endpoint module entries;
  fail-closed is intentional.

## Non-Goals

- Redesigning HttpEndpoint routing table semantics.
- Full Class-A auth rewrite (ARN-170) for the HTTP surface.

## Alternatives Considered

1. **Disable HttpEndpoint entirely** — too blunt for git/webhook surfaces.
2. **Only strip secrets, leave raw host** — still skips Cedar HTTP gate.
