# ADR-0176: Typed Outer Authentication Composition

- Status: Proposed
- Date: 2026-08-22
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Credential-Bound Class A Authentication Edge
  - `crates/temper-authz/src/context.rs`
  - `crates/temper-platform/src/bearer_auth.rs`

## Context

Temper can be embedded behind an application-owned authentication middleware.
TemperPaw verifies a local account session cookie before requests reach the
embedded platform router. ADR-0157 correctly removed the old
`PreAuthenticatedRequest` marker because it paired a forgeable marker with raw
principal headers. That removal also left no supported way for a trusted outer
middleware to pass the already-authenticated authority into the kernel.

Reconstructing identity from headers, minting a reusable deployment credential,
or weakening the no-credential failure path would reopen the Class A boundary.
The existing `AuthenticatedRequestContext` already carries an immutable
security context and tenant as one typed value, but the bearer edge currently
ignores a context installed by an in-process outer layer.

## Decision

### Typed context is the only composition primitive

The bearer edge accepts an existing `AuthenticatedRequestContext` request
extension as completed authentication. It does not inspect principal headers or
restore `PreAuthenticatedRequest`.

The existing context's tenant must exactly equal the validated requested tenant.
A mismatch returns `401 Unauthorized`. The edge removes any authorization
header before dispatch so an outer-authenticated request cannot forward a
second credential downstream.

**Why this approach**: an HTTP client cannot construct an axum extension. The
outer middleware is already part of the trusted in-process computing base, and
the typed context binds tenant and authority without splitting them across
headers or markers.

### Protocol routes retain normal admission

The platform still matches configured HTTP endpoint routes. When a matching
route exists, a typed outer context receives the same
`AdmittedHttpEndpoint` extension as a bearer-resolved context before dispatch.
Route admission therefore does not depend on which trusted authentication edge
produced the context.

### Legacy header-only internal calls stay rejected

Raw `x-temper-principal-*` headers never satisfy authentication. Internal WASM
HTTP fallthrough continues to use the single-use, tenant/method/path-bound
internal bearer defined by ADR-0157.

### WASM-local OData re-entry uses module authority

Triggered and HTTP-endpoint WASM calls that take the in-process local TData
optimization enter the OData handlers with the same immutable module principal
used by the WASM host gate: the exact module ID, `wasm_module` role, and
host-derived invocation context. They do not inherit the ambient action caller
or the generic `service:wasm-runtime` relay identity. Direct `blob_adapter`
invocations retain their caller-bound authority contract from ADR-0157.

**Why this approach**: network fallthrough and the in-process optimization are
transport variants of the same guest call. Cedar must observe the same
host-derived module authority on either path, and app policy must not need a
broad permit for a generic relay service.

### Scoped integration dispatch preserves the execution pin

When a scoped entity emits a WASM custom effect, integration metadata is
resolved from the exact immutable bundle named by its `SchemaExecutionPin`,
not from the tenant-global spec registry. The same pinned agent context is
then carried into the callback dispatch.

**Why this approach**: a scoped transition and its reaction are one governed
execution. Falling back to global integration metadata can silently strand a
durable intermediate state or invoke an unrelated module definition.

## Rollout Plan

1. Add focused platform tests and the typed-context acceptance path.
2. Update TemperPaw's verified cookie middleware to construct the typed admin
   context and remove its obsolete marker/header-only bypass.
3. Build and exercise the pinned TemperPaw development image before either
   dependency pin is eligible for production.

## Readiness Gates

- Matching-tenant typed contexts pass and authorization headers are stripped.
- Cross-tenant typed contexts return 401.
- Raw principal headers remain insufficient.
- TemperPaw cookie-session and internal-bearer flows pass end to end.
- Triggered and HTTP-endpoint local TData calls resolve to the exact module ID
  and `wasm_module` role; relay-service and caller authority do not leak across
  that boundary.
- A scoped action resolves its WASM integration from the exact pinned bundle
  and completes its callback without consulting tenant-global metadata.

## Consequences

### Positive

- Embedded applications can compose independent authentication with Temper's
  typed authorization boundary.
- No reusable kernel credential is needed for a verified application session.
- Tenant identity remains bound to authority at the edge.

### Negative

- Every outer authentication middleware becomes trusted code responsible for
  constructing the complete context correctly.
- Embedded hosts must migrate from the removed marker rather than receiving a
  compatibility shim.

### Risks

- Incorrect middleware ordering could install the context after bearer
  authentication. End-to-end embedding tests cover the production order.
- Accepting a mismatched context could cross tenants. The edge checks equality
  before route admission or dispatch.

### DST Compliance

The authentication changes affect HTTP request admission in `temper-platform`.
Scoped integration lookup changes only which immutable registry snapshot is
read; it introduces no time, randomness, storage, or simulation-visible
ordering. Existing background integration scheduling is unchanged.

## Non-Goals

- Deriving authority from HTTP headers.
- Treating loopback transport as authentication.
- Allowing System authority over an HTTP boundary.
- Replacing internal invocation capabilities.

## Alternatives Considered

1. **Restore `PreAuthenticatedRequest`** — rejected because the marker/header
   split was the vulnerability ADR-0157 removed.
2. **Mint a platform credential for every application cookie** — rejected
   because it creates unnecessary durable secrets and lifecycle coupling.
3. **Trust principal headers only on loopback** — rejected because transport
   location is not identity and WASM guests can reach loopback paths.

## Rollback Policy

Remove the typed-context branch from bearer authentication and require every
embedded host to present a normal credential. Do not restore marker or header
compatibility.
