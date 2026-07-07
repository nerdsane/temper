# ADR-0157: Class A Auth Edge — Never Trust a Client-Asserted Principal

- Status: Accepted
- Date: 2026-07-06
- Deciders: Temper core maintainers
- Related:
  - ADR-0033: Platform-Assigned Agent Identity (this ADR completes its Sub-Decision 1 for the privilege-escalation vector it left open)
  - ADR-0043: WASM Host-Injected Auth Headers for Internal API Calls (internal components inject identity *after* the edge)
  - ADR-0046: Unified Action Triggers (removed the hard-coded System bypass in favor of the `system-platform` Cedar policy)
  - Linear ARN-170 (kernel), ARN-167 (TemperPaw, same edge design), epic ARN-165 (systemic "Class A")
  - `crates/temper-authz/src/context.rs` (`SecurityContext::from_headers`)
  - `crates/temper-server/src/authz/edge.rs` (new — the strip layer)
  - `crates/temper-server/src/router.rs`, `crates/temper-platform/src/router.rs` (edge wiring)
  - `crates/temper-platform/src/bearer_auth.rs` (credential resolution)

## Context

Temper derives the Cedar principal for a request from `X-Temper-*` HTTP
headers. `SecurityContext::from_headers` maps `x-temper-principal-kind:
admin` straight to `PrincipalKind::Admin`, and several surfaces treat Admin
as a privilege bypass:

- `authz/helpers.rs::require_observe_auth` and `observe_tenant_scope` short-circuit
  `Admin | System` (observe read + cross-tenant view) *before* Cedar runs.
- `require_governed_mutation_auth` returns "allowed" for `Admin`.
- The `temper-system` tenant loads a baseline `permit(principal is Admin, action, resource)`
  policy (`temper-platform/src/state.rs`), so an Admin has full authority over
  platform entities (GovernanceDecision, Project, …).

Only `system` is refused from headers (`from_headers` maps unknown kinds to
Customer). `admin` was never blocked — the same escalation the `system`
safeguard exists to prevent is wide open one enum variant over. And nothing
strips inbound `X-Temper-*` headers, so a client sets its own principal
freely.

Two deployment shapes make this exploitable end-to-end:

1. **`TEMPER_API_KEY` unset.** `bearer_auth_check` fail-**opens**: with no
   `api_token` configured it runs every route with `Ok(next.run(req))`, no
   identity resolution. An unauthenticated client then sends
   `x-temper-principal-kind: admin` + `x-temper-principal-id: x` and reads
   every OData entity, policy-management, and observe endpoint across tenants.
2. **`build_router` embedded directly** (no platform auth layer at all) — same,
   with no key required.

Even with a key set, the ADR-0043 guest-override branch
(`bearer_auth.rs:81-83`) lets a *global-API-key holder* pass arbitrary
`x-temper-principal-*` headers through unmodified — impersonating any
principal, including Admin.

ADR-0033 already decided the correct model — "the platform assigns identity,
never the agent; identity is derived exclusively from bearer-token
resolution" — and even said the self-declared-identity constructor would be
removed. That removal was never completed: `from_headers` still mints
authority from raw headers. This ADR closes the gap for the privilege vector.

This is the kernel half of the systemic **Class A** finding (self-asserted /
missing-header trusted + fail-open auth). The TemperPaw half (ARN-167) applies
the same three-part edge.

## Decision

Establish a single principle and enforce it in three layers, generically —
not per call site, and with no entity/domain names hard-coded.

**Principle: a privileged principal (Admin or System) is only ever produced
from a resolved credential or a trusted in-process constructor — never from a
client-supplied header.**

### Sub-Decision 1: `from_headers` refuses to derive Admin (and System)

`SecurityContext::from_headers` maps both `admin` and `system` to
`Customer`, exactly as `system` was already handled. Raw headers can now
produce only the two unprivileged kinds, `Customer` and `Agent`.

**Why this approach**: This is the root, universal fix. Every consumer of
`from_headers` — OData bindings, observe helpers, the API surface, the
in-process WASM TData host, and `build_router` embedded standalone — inherits
it at once. It holds even on paths that never pass through an HTTP middleware
layer (in-process dispatch), which a strip layer alone cannot cover.

Privileged principals continue to exist, but only through trusted channels:
`SecurityContext::system()` (in-process platform code) and
`from_resolved_identity` (credential resolution). A credential whose registered
`AgentType` is the platform operator type resolves to a verified operator
identity through that path — never through a header.

### Sub-Decision 2: Strip inbound identity headers at the edge

A reusable Tower/axum layer, `strip_inbound_identity_headers`
(`temper-server/src/authz/edge.rs`), removes every inbound `x-temper-*`
authority header before any handler or auth middleware reads it. The only
`x-temper-*` headers allowed through are the non-authority observability
namespaces `x-temper-observe-*` and `x-temper-workflow-*` (session/intent/
trace correlation a client may legitimately supply; they never influence
authorization).

The layer is applied **outermost in `build_platform_router`** — it runs
*first*, before `bearer_auth_check`, `tenant_access_check`, and identity-cache
invalidation. A client can never present an `x-temper-*` authority header to
the credential-resolution edge or to any platform middleware that reads
identity; only the platform, *after* the edge, injects trusted identity
(ADR-0043 pattern). The platform binary is the sole credential-resolving
ingress, so this is the single place a trusted principal is ever produced from
an inbound request.

The strip lives in `temper-server` (not `temper-platform`) precisely so it is a
reusable layer: `build_router` on its own installs no authentication and never
materializes a trusted principal, so its handlers can only ever derive an
unprivileged `Customer`/`Agent` from headers (Sub-Decision 1). Any future
service that embeds `build_router` *and* resolves credentials must apply this
same strip layer at its ingress — see the direct-embed note under Risks.

**Why this approach**: Defense in depth beyond Sub-Decision 1. `from_headers`
also reads `x-temper-attr-*` (ABAC attributes), `x-temper-principal-scopes`,
`x-temper-agent-type`, `x-temper-action-context`, and `x-temper-ctx-*`. A
client could otherwise still spoof ABAC attributes or an action-context even
though it can no longer claim Admin. Stripping the whole authority namespace
at ingress closes that class in one place, and is prefix-based so a future
authority header is covered without being re-listed.

### Sub-Decision 3: Fail closed — no credential, no privilege

`bearer_auth_check` is hardened so that the absence of a credential can never
yield a trusted/privileged principal:

- **Drop the header-injection fallback.** The old path injected
  `x-temper-principal-kind: admin` on a global-key match; that is exactly the
  "assert Admin via a header" pattern. Replace it by inserting a trusted
  operator `EdgeAuthenticatedPrincipal` request extension, so the operator is the same
  verified identity whether or not its credential entity happens to be
  bootstrapped — and it is never Admin-by-header.
- **Drop the guest-override passthrough** (`matches_global_api_key &&
  has_explicit_principal`) that forwarded arbitrary client `x-temper-principal-*`
  headers. With the strip running first, those headers are already gone; the
  branch only ever served the impersonation vector the finding calls out.
- **No-key mode is unprivileged, not trusted.** When no `api_token` is
  configured, requests still pass (local dev, ungoverned reads) but as an
  anonymous Customer — the strip removed any asserted identity and
  `from_headers` cannot mint privilege. Every privileged surface (observe,
  cross-tenant, policy management, the `temper-system` Admin policy) therefore
  denies a no-credential request. Failing "closed" here means *closed to
  privilege*, which both fixes the fail-open and keeps unprivileged local dev
  working.

## Rollout Plan

1. **Phase 0 (this PR)** — All three sub-decisions land together behind the
   red-green exploit tests. Kernel-only; no schema or data migration.
2. **Phase 1** — TemperPaw (ARN-167) applies the same three-part edge to its
   own ingress and self-asserted `x-temper-principal` path.

## Consequences

### Positive
- A client can no longer assert its own principal — the confused-deputy /
  header-spoof escalation is closed at the root and in depth.
- Fail-open on unset `TEMPER_API_KEY` becomes fail-closed-to-privilege.
- Completes ADR-0033's "identity from credentials, never from headers" for the
  privilege vector.
- One edge, reused by the kernel router and the platform binary; TemperPaw
  reuses the same design.

### Negative
- Callers that reached privileged behavior by *spoofing* `x-temper-principal-kind:
  admin` (including internal convenience paths like `temper-transport` with no
  API key, and tests) must present a real credential or use a trusted
  in-process context. Their tests are updated to the supported mechanism.
- A WASM TData host call that *inherited* an Admin principal now inherits it as
  Customer — consistent with how System inheritance was already downgraded in
  `local_tdata_host::security_context_headers`. Platform/system paths use
  `SecurityContext::system()` or a service principal, which are unaffected.

### Risks
- If a production surface silently depended on header-derived Admin, it now
  denies. Mitigation: the operator/dashboard authenticate through credential
  resolution (verified operator identity), which this ADR does not change; the
  exploit tests assert both that spoofed admin is denied and that the
  legitimate authenticated paths still succeed.
- **Direct embed of `build_router` without the strip layer.** Sub-Decision 1
  makes the `admin`/`system` escalation unreachable from a plain
  `x-temper-principal-kind` header everywhere (including a directly-embedded
  kernel router and in-process dispatch). The one signal that *would* elevate a
  header to `Admin` — the internal `TRUSTED_PRINCIPAL_HEADER` marker — is only
  guaranteed-stripped by the platform ingress edge. There is no production
  topology that embeds `build_router` directly (the only non-test caller is
  `build_platform_router`, which applies the strip), and such a router carries
  no authentication at all, so this is not a reachable exploit today. The
  residual is recorded so any future embedder resolving credentials applies
  `strip_inbound_identity_headers` at its ingress rather than relying on the
  marker being absent.
- **`tenant_access_check` now reads only post-edge identity.** With the strip
  outermost, a client-asserted `x-temper-principal-id: github:*` no longer
  reaches `tenant_access_check` — which is the intended posture (a
  client-asserted principal was itself the Class A pattern). The per-tenant
  github access check therefore only constrains *resolved* identities;
  Cedar remains the primary authorization gate. Tightening that middleware to
  validate the materialized principal is tracked separately (it is a routed-mode
  Turso concern, and github principals originate in TemperPaw — ARN-167).

### DST Compliance
- The strip layer is a pure `HeaderMap` transform — no clock, no RNG, no
  collections with nondeterministic iteration. `from_headers` changes are pure.
- No `sim_now`/`sim_uuid` semantics change. No `// determinism-ok` needed.

## Non-Goals
- OAuth/JWT validation, SPIFFE, and delegation chains (`acting_for`) remain out
  of scope (ADR-0033 Non-Goals).
- Narrowing the broad `temper-system` `principal is Admin` policy or the
  observe Admin-bypass to least privilege is a separate follow-up; this ADR
  makes Admin unspoofable, which is the exploitable part.

## Alternatives Considered

1. **Strip only, keep `from_headers` mapping `admin` → Admin.** Rejected: an
   in-process path or a directly-embedded `build_router` that never crosses the
   strip layer would still mint Admin from a header. Sub-Decision 1 is the only
   fix that covers every call site.
2. **Block `admin` in `from_headers` but keep injecting a trusted `admin`
   header for the operator.** Rejected as self-contradictory: the injected
   header would be demoted by the same block. The operator is carried as an `EdgeAuthenticatedPrincipal` extension instead.
3. **Hard 401 for every request when `TEMPER_API_KEY` is unset.** Rejected: it
   breaks local development and the large body of no-key tests that legitimately
   exercise unprivileged routes. "Closed to privilege" is the correct posture;
   privileged surfaces already deny an anonymous principal.
