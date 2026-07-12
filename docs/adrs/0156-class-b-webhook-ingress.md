# ADR-0156: Authenticated webhook ingress (Class B)

- Status: Proposed
- Date: 2026-07-06
- Deciders: Temper core maintainers
- Related:
  - ARN-171 (kernel Class B), ARN-165 (remediation epic), ARN-168 (TemperPaw counterpart)
  - `crates/temper-server/src/webhooks/receiver.rs` (the inbound handler)
  - `crates/temper-server/src/odata/authz.rs`, `crates/temper-server/src/odata/bindings.rs` (the OData write path this mirrors)
  - `crates/temper-spec/src/automaton/types.rs` (`Webhook` spec: `hmac_secret`, `hmac_header`)

## Context

The inbound webhook route `GET|POST /webhooks/{tenant}/{*path}`
(`webhooks/receiver.rs`) dispatches a spec-declared entity action via
`dispatch_tenant_action(...)` with `security_ctx: None`. Two independent
protections that guard every other write path are absent here:

1. **No authorization.** `dispatch_tenant_action_core` has no Cedar gate;
   Cedar is enforced only in the OData binding/extractor layers, which this
   route never touches. A principal Cedar would deny on the OData path
   succeeds through the webhook path.
2. **No authenticity.** There is no signature check. The `Webhook` spec has
   carried `hmac_secret` and `hmac_header` fields the whole time, but
   `receiver.rs` never read them — the fields were dead.

Tenant is taken from the URL path and `entity_id` from a client query
param — both attacker-chosen. So `POST /webhooks/<tenant>/pay/callback?entity_id=INV-123`
drives an arbitrary declared action on an arbitrary entity, unauthenticated.

This is the kernel instance of the systemic **Class B** finding in ARN-165.
The TemperPaw side (ARN-168, a no-op HMAC that only checks header presence)
mirrors this design once landed.

## Decision

Make the webhook route a single authenticated ingress that applies **both**
protections before any dispatch, reusing the exact primitives the OData
write path already uses. No new dispatch path, no bespoke auth.

### Sub-Decision 1: Cedar gate on every webhook, always

Before dispatch, build a real `SecurityContext` for the webhook caller and
call `state.authorize_with_context(&ctx, action, entity_type, &attrs, tenant)`
— the same function the OData bound-action path calls. Resource attributes
come from `load_authz_resource_snapshot` (the same loader the OData and
trigger paths use), falling back to a minimal `{id, status}` view when the
target entity does not yet exist.

The webhook principal is **restricted, not `System`**:

- `kind = Agent`, `id = "webhook:{name}"`, `role = "webhook"`,
  `agent_type = "webhook"`,
- attribute `authenticated: bool` = whether an HMAC signature was verified,
- `action_context = "webhook:{name}"` (ADR-0040 provenance).

**Why this approach**: Cedar is already default-deny per tenant in
production (a tenant with a loaded policy set denies anything not explicitly
permitted — see `AuthzEngine::authorize_for_tenant`). Attaching a real,
narrow principal means the webhook route is governed by the same policies as
every other write, and is **fail-closed by default**: a webhook only
dispatches if the tenant's policy explicitly permits the `webhook:*`
principal for that action. Policies can additionally require
`principal.authenticated == true` to insist on a verified signature.

Denials for an **authenticated** caller (one that proved it holds the signing
secret) are recorded via `record_authz_denial` exactly like the OData path, so
a denied webhook intent surfaces as a pending decision rather than failing
silently. Denials for an unauthenticated caller (a webhook with no declared
secret) return a plain `403` and are **not** recorded — otherwise anyone could
amplify pending-decision/governance records by spamming the public route.

### Sub-Decision 2: HMAC-SHA256 authenticity when a secret is declared

When the webhook declares `hmac_secret`, the request must carry a valid
signature or it is rejected with `401` before dispatch:

- Resolve the signing secret from the **tenant secret store** via the
  existing `{secret:KEY}` template resolver (`resolve_secret_templates` over
  `state.secrets_vault`). A literal secret is also accepted.
- Read the signature from `hmac_header` (default `X-Temper-Signature`). A
  missing header is a `401`.
- Compute `HMAC-SHA256(secret, raw_request_body)`, hex-encode, and compare
  against the provided value (optional `sha256=` prefix, case-insensitive)
  using a **constant-time comparison** (`subtle::ConstantTimeEq`) — never
  `==` on the digest.
- **Fail closed on misconfiguration**: if the secret is declared but cannot
  be resolved (no vault, or an unresolved `{secret:KEY}`), reject with `401`
  and log — an unverifiable webhook must not dispatch.

The raw body is read as `axum::body::Bytes`, which is bounded by axum's
default `DefaultBodyLimit` (2 MiB) so a webhook body cannot buffer unbounded.

**Why this approach**: HMAC over the raw body is the de-facto standard for
signed webhooks (GitHub `X-Hub-Signature-256`, Stripe, Datadog). Reusing the
spec's existing `hmac_secret`/`hmac_header` fields and the existing secret
vault means no new configuration surface. Constant-time comparison prevents a
timing side channel on the digest.

### Sub-Decision 3: Order of checks

`lookup (404)` → `method (405)` → `HMAC verify (401, only if a secret is
declared)` → `entity-id extraction (400)` → `Cedar gate (403)` → `dispatch`.
HMAC runs before entity resolution so an unauthenticated caller learns
nothing about entity state.

### Sub-Decision 4: Exact replay uses durable dispatch idempotency

Every accepted webhook delivery receives a deterministic dispatch
idempotency key derived from the resolved tenant, HTTP method, webhook route,
webhook declaration, target entity, ordered query parameters, and raw body.
The key is threaded through `AgentContext.idempotency_key`, so the existing
entity actor/store idempotency path records it with the committed event and
deduplicates an exact replay of the same callback.

This deliberately avoids a process-local replay cache: a local cache would be
lost on restart and would not prove anything about the committed state. It
also avoids inventing a timestamp header that external providers do not send.
Signed providers that need stricter provider-native windows (for example
Stripe-style timestamped signatures) should use a follow-up signature scheme
selector.

### Tenant/principal are authenticated, not merely claimed

The tenant still appears in the URL, but it is no longer *trusted* from the
URL: the signing secret is read from **that tenant's** vault, so only a
caller holding the tenant's secret can produce a valid signature. The
principal is derived server-side (`webhook:{name}`), never from an
attacker-supplied header.

## Rollout Plan — backward compatibility (read before deploy)

This change makes the webhook route Cedar-authorized **always**. For a tenant
with a policy set loaded (default-deny), a webhook that works today
unauthenticated will start returning `403` unless a policy permits its
`webhook:{name}` principal for the dispatched action. The webhook principal is
`Agent`-kind with `role == "webhook"` and `agent_type == "webhook"`.

**Required permit shape** (per webhook, added to the tenant's Cedar policy set):

```cedar
permit(
  principal is Agent,
  action == Action::"<WebhookAction>",
  resource is <EntityType>
) when { principal.role == "webhook" };
```

Tighten further with `&& principal.authenticated == true` to require a verified
HMAC signature, or match a specific webhook via
`principal.action_context == "webhook:<name>"`.

**Inbound webhooks that exist in THIS repo** (grep `[[webhook]]` in entity
specs — only entity-spec webhooks hit `/webhooks/{tenant}/{*path}`; the
`url`-based `[[webhook]]` blocks in `reference-apps/*/integration.toml` are the
**outbound** dispatcher and are unaffected):

- `test-fixtures/specs/gmail_oauth.ioa.toml` — `oauth_callback` → action
  `OAuthCallback` on the OAuth entity. Test fixture only; its DST test drives
  `sim.step("OAuthCallback", …)` directly, not the HTTP route, so it does not
  regress. A real deploy of this spec would need:
  `permit(principal is Agent, action == Action::"OAuthCallback", resource is GmailConnection) when { principal.role == "webhook" };`

**Out-of-repo production webhooks** (kernel cannot see these; flagged for the
deploy owner): any app deployed on Temper that uses an inbound kernel webhook —
notably **TemperPaw OAuth callbacks** and any **Katagami** integration
callbacks — must ship the matching `webhook:*` permit **in the same deploy** as
this kernel change, or those callbacks will 403. The platform's per-spec Cedar
generator (`temper-platform/hooks/generate_cedar.rs`) does **not** currently
emit a webhook permit automatically — it generates `ThisAgent`-scoped policies
from GovernanceDecision fields — so the permit must be added explicitly (via
the app's policy set) until a generator rule is added (follow-up).

**Rollout order**: land the permit policies for live webhooks first (or in the
same change set), then deploy this kernel gate. A policy-less tenant is
unaffected (permissive fallback), but no production app tenant is policy-less.

## Consequences

### Positive
- The webhook route is governed by the same Cedar policies as every other
  write, and authenticated by HMAC when a secret is configured. No code path
  dispatches with `security_ctx: None`.
- The long-dead `hmac_secret`/`hmac_header` spec fields become live.
- Exact callback replays are idempotent through the same durable dispatch
  mechanism as agent retries; a replay does not attempt a second state
  transition after the first delivery commits.
- Establishes the kernel pattern the TemperPaw fix (ARN-168) mirrors.

**Precondition on the Cedar fail-closed guarantee.** `authorize_for_tenant`
is default-deny only for a tenant that has a **policy set loaded**; a tenant
with zero policies falls through to the engine's global fallback, which in the
default `ServerState` is `AuthzEngine::permissive()` (permit-all). The platform
generates a per-tenant Cedar policy set on spec registration
(`temper-platform` `hooks/generate_cedar.rs` → `reload_tenant_policies`), so a
deployed app tenant is default-deny; a policy-less tenant is permit-all. This
is exactly the OData write path's behavior — the webhook route is now at
parity, not stricter. For a **no-secret** webhook this means: it is fully
governed (fail-closed) once the tenant has a deny-by-default policy set, which
is a deployment requirement, not an automatic property of this route.

### Negative / Tradeoffs
- A webhook with **no** declared `hmac_secret` is authenticated only by its
  Cedar policy (plus any unguessable capability in its payload, e.g. an
  OAuth `state` nonce). This deliberately preserves the OAuth-callback
  capability, which providers cannot HMAC-sign. Any webhook whose caller
  *can* sign should declare a secret. This is the central tradeoff; the
  stricter alternative (below) was rejected to avoid breaking OAuth.
- Webhook authors must add a Cedar permit for the `webhook:*` principal for
  webhooks to fire in production — intentional, and auditable.
- Exact replay idempotency is not a substitute for authenticating an
  unsigned callback. It prevents a captured callback from applying the same
  transition twice; it does not prove that the first caller was the intended
  OAuth provider. First-class unsigned callback capabilities remain a
  follow-up spec surface.

### Risks
- Signature-format assumptions: hex digest with an optional `sha256=`
  prefix. Providers using base64 or a timestamped scheme (Stripe's `t=,v1=`)
  are out of scope here (see Non-Goals) and would need a follow-up encoding
  selector on the spec.

### DST Compliance
- `webhooks/receiver.rs` is an HTTP boundary, not part of the deterministic
  actor simulation. No wall-clock, RNG, or ambient I/O is introduced in the
  new code: HMAC/SHA are pure CPU; the `SecurityContext` correlation UUID is
  minted inside `temper-authz` as before; secrets come from the injected
  vault. `BTreeMap` is used throughout. No new `// determinism-ok` needed.

## Non-Goals
- Base64 or provider-specific signature schemes (Stripe timestamped, Shopify
  base64). Hex (optionally `sha256=`-prefixed) only.
- Provider-native timestamp/nonce windows — a possible follow-up. This ADR
  now covers exact replay idempotency only.
- Changing the outbound `WebhookDispatcher`.

## Alternatives Considered

1. **Hard-require HMAC on every webhook (reject any webhook with no
   declared secret).** Strongest authenticity, and closest to the epic's
   "fail closed when no credential source is configured" wording. Rejected
   because it breaks OAuth-style callbacks, which the provider redirects
   without a signature — a working capability we must preserve. Cedar
   default-deny already provides the fail-closed guarantee for the no-secret
   case.
2. **Cedar gate only, no HMAC.** Closes the authorization bypass but leaves
   signed webhooks (payments/events) spoofable by anyone a policy permits.
   Insufficient — the issue calls for authenticity too.
3. **A dedicated `Webhook` `PrincipalKind`.** Cleaner typing, but a
   cross-crate Cedar-schema change for little gain; `Agent` + `role/
   agent_type = "webhook"` + `action_context` expresses the same policy
   surface today.
