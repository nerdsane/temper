# RFC-0002: First-Class Authorization for Temper Apps

- Status: Draft
- Date: 2026-07-11
- Authors: Claude Code, with product direction from the human director
- Related:
  - ADR-0033: Platform-Assigned Agent Identity (cite by name; three ADRs share the 0033 number)
  - ADR-0004: Cedar Authorization for Agents
  - ADR-0032: Granular Cedar Policy Storage
  - Linear: ARN-255 (this effort), ARN-163 (platform MCP surface), ARN-170 (ingress auth edge), ARN-151 (contributor identity), ARN-145 (Member roles), ARN-248 (personal spaces), ARN-230 (permit-all fallback)
  - `crates/temper-authz/src/context.rs` (SecurityContext, Principal)
  - `crates/temper-server/src/identity/resolver.rs` (bearer credential resolution)
  - `crates/temper-authz/src/policy_gen.rs` (Cedar generation)
  - katagami `ui/src/lib/oauth-as.ts` (the app-level authorization server this RFC absorbs)

## Summary

A Temper app has three kinds of callers: people using its web front end,
agents connecting through MCP (the Model Context Protocol, the standard way
agents discover and call tools) or an API, and the app's own services. Today
the platform can tell none of them apart. Every caller arrives on one shared
API key, and each app re-implements its own access control in front-end code.

This RFC makes authorization a platform capability. The platform runs the
authorization server; apps handle sign-in and exchange the user's identity for
a short-lived platform token; the kernel verifies every token and resolves it
to a principal; policies written in Cedar (the policy language the kernel
evaluates on every request) — generated from the app's spec — decide what
that principal may do. An app built this way ships no authorization code of
its own.

The decision to build this, and to have the platform (rather than each app)
run the authorization server, was made by the human director on 2026-07-11
(ARN-255, "Option B") after an audit of Katagami, the first Temper app with
both a human front end and agent clients.

## Why This RFC Exists

Katagami is the proof case. It has Google sign-in for humans, an OAuth 2.1
front door for agents, a curation pipeline, and Cedar policies — every piece
an authorized system needs. The 2026-07-11 audit traced how those pieces
actually connect and found that the platform makes no per-caller access
decisions:

- **Humans.** katagami.ai authorizes users entirely inside its Next.js code:
  curator powers are gated on a `KATAGAMI_OWNER_SUBS` env allowlist
  (`ui/src/lib/owner.ts:22`), and "only the creator may edit" rules are
  JavaScript comparisons against a session cookie. Every backend call then
  uses one shared `TEMPER_API_KEY` (`ui/src/lib/odata.ts:30-34`). The kernel
  never learns which human is acting; identity reaches it only as writable
  entity data (`SetCreator(creator_sub, …)`), which the shared key could set
  to any value.
- **Agents.** The MCP front door mints real per-agent tokens — ES256, 15-minute
  TTL, rotating refresh, revocable grants. The kernel cannot verify them, so
  the adapter swaps to the same shared `TEMPER_API_KEY` and self-declares
  `x-temper-agent-type: contributor` in headers (`mcp/src/temper.ts:25-34`).
  The kernel trusts those headers verbatim on the non-credential path
  (`crates/temper-server/src/odata/bindings.rs:91-101`). The contributor
  boundary therefore depends on the adapter choosing to stamp the header.
- **Policies.** The app's Cedar set is wildcard permit statements plus forbid
  overlays that fire only when `principal.agent_type == "contributor"`. Human
  identity appears in zero policies. Any holder of the shared key can publish,
  archive, and manage grants by omitting one header.

None of this is a Katagami bug. Katagami built the strongest layer it could —
the audit's conclusion is that the missing pieces are platform pieces, and
every future Temper app with a front end or an agent surface will hit the same
wall. ADR-0033 already established the principle for agents (the platform
assigns identity; self-declared identity is rejected) and shipped the
`AgentCredential` registry and resolver. That ADR deliberately left OAuth/JWT
validation and delegation as non-goals. This RFC picks those up and extends
the same principle to humans.

## Goals

1. The kernel can tell callers apart: humans, agents, and services each
   resolve to a distinct, verified Cedar principal.
2. Access decisions live in Cedar, derived from the app's spec. Front-end
   checks remain only as UX (hiding buttons a caller can't use).
3. A new Temper app gets all of this by default, writing no authorization
   code.
4. The shared global API key stops being the ambient credential for
   everything; it shrinks to explicitly registered service credentials.

## Decision

**The platform runs the authorization server.** Apps do sign-in and token
exchange only. The kernel verifies every inbound token, resolves it to a
principal with attributes, and evaluates Cedar. This was chosen over the
lighter alternative (each app runs its own authorization server and the kernel
federates to it via published signing keys — "Option A") because Option A
still leaves every app implementing and operating an OAuth server, which is
exactly the code this RFC exists to delete. Option A remains a useful interim
integration path (see Rollout), and nothing in this design prevents a tenant
from federating an external issuer later.

## Design

### 1. Platform authorization server

A per-tenant authorization server in the platform, providing what
katagami.ai's hand-rolled one (`ui/src/lib/oauth-as.ts`) provides today:

- Dynamic client registration (RFC 7591) for agent clients.
- Authorization-code flow with PKCE for interactive consent; a human approves
  an agent's access, producing a revocable grant entity.
- Pre-authorized grants for headless agents (CI jobs, cron agents — callers
  with no browser to click a consent screen): a human mints the grant in the
  app UI and the refresh token is shown once. ARN-163 flagged this consent
  gap as a carry-over constraint; the platform AS is where it gets solved
  once for every app.
- Short-lived signed access tokens (minutes) carrying the owning
  human, the acting client, the grant id, and scopes; rotating refresh tokens
  with replay detection.
- Published signing keys (JWKS) and discovery metadata (RFC 8414), which the
  ARN-163 MCP resource server points at.

The `OAuthClient` and `AgentGrant` entities Katagami defined in
katagami-commons move into platform specs (alongside `AgentCredential` from
ADR-0033), so every tenant gets them. Katagami's copies migrate.

### 2. Verified principals for every caller

`identity/resolver.rs` today resolves one credential shape: a bearer API key
hashed and looked up in the `AgentCredential` registry. This RFC extends
resolution to three shapes, all producing a verified `SecurityContext`:

- **Platform-issued JWTs** (from the authorization server above): verified by
  signature, expiry, and grant liveness. The principal is the acting agent
  (`Agent::"<client>"`) with `acting_for` set to the owning human and
  attributes (`agent_type`, scopes) taken from the token the platform itself
  minted.
- **Human token exchange**: the app front end authenticates the human however
  it likes (Google OIDC in Katagami's case), then exchanges the proof at a
  kernel endpoint for a short-lived platform token whose principal is
  `Customer::"<sub>"`. The exchange endpoint verifies the upstream identity
  (issuer allowlisted per tenant, standard OIDC ID-token validation) and loads
  principal attributes from the tenant's Member entity (see 3).
- **Registered service credentials** (existing ADR-0033 path): the curation
  pipeline, SSR readers, and other app services each get their own
  `AgentCredential` instead of sharing the global key.

With these in place, the self-declared `x-temper-principal-*` /
`x-temper-agent-type` headers are removed from the protocol, completing what
ADR-0033 specified and ARN-170's ingress work began. A request with no
verified credential resolves to an anonymous principal that can reach only
whatever policies explicitly permit anonymously. This deliberately revises
ADR-0033's "no token = 401" posture: once headers are gone the kernel needs a
defined identity for credential-less requests, and apps with public read
surfaces (a catalog page, a published design language) need a way to serve
them without handing the front end a privileged key. An anonymous principal
governed by explicit Cedar permits is one way to provide that; whether
generated policies include anonymous read permits per entity, or apps instead
mint a public-reader service credential, is an open question below.

### 3. Member roles as principal attributes

Apps need durable, queryable roles for humans (ARN-145). Katagami already
defines a `Member` entity (Google `sub` as key; role owner / curator /
contributor) that nothing consults. Under this RFC the Member spec becomes a
platform spec, and the resolver injects the member's role into the principal's
attributes at token-exchange time — the same mechanism that injects
`agent_type` for agents today (`engine/mod.rs` principal-entity construction).
Policies can then say:

```cedar
permit(principal, action == Action::"Publish", resource is DesignLanguage)
  when { principal has role && ["owner", "curator"].contains(principal.role) };
```

Env-var allowlists like `KATAGAMI_OWNER_SUBS` retire.

### 4. Authorization declared in the spec, compiled to Cedar

Hand-written permit-all files are how Katagami ended up open by default. An
app's behavior is declared in its IOA spec (the TOML document that defines a
Temper app's entities, state machines, and actions), so that spec is also
where access requirements belong:

```toml
[[action]]
name = "Publish"
requires_role = ["owner", "curator"]

[[action]]
name = "Withdraw"
requires = "creator"        # resource.creator_sub == principal.sub
```

At install time the platform compiles these into Cedar policies. This
compiler is new work: the existing Cedar-generation machinery
(`crates/temper-authz/src/policy_gen.rs`) builds permits from
principal/action/resource scope matrices for the denial-remediation flow, and
would be extended — it has no resource-attribute or set-membership
conditions, and no spec-install wiring today. Per-policy storage already
exists (ADR-0032), and
resource-attribute injection already puts entity fields like `creator_sub` on
the Cedar resource, so creator rules evaluate without new engine surface.
Actions with no declaration get no permit — the app's posture flips from
permit-all with forbid overlays to default-deny with generated permits. That
flip is only trustworthy once ARN-230's permit-all fallback on failed policy
loads is fixed (see Risks). Hand-written `.cedar` files remain supported for
policies the sugar cannot express.

### 5. Front-end auth kit

A thin client package so app front ends write no auth plumbing: OIDC login →
Member upsert → token exchange → a fetch client that attaches the caller's
platform token and refreshes it. Katagami's Next.js app is the first consumer;
its session cookie remains purely a front-end concern (which also contains the
blast radius of its 30-day stateless cookie — the backend stops honoring
anything the cookie alone asserts).

### How a request flows after this RFC

Human path: sign in with Google on katagami.ai → front end exchanges the ID
token at the kernel → kernel verifies issuer, loads Member, mints a 15-minute
token for `Customer::"114585…"` with `role: "owner"` → the page's server code
calls the kernel's OData API (the HTTP protocol Temper serves entity data and
actions over) with that token → Cedar evaluates `Publish` against the
generated policy → allowed because `principal.role == "owner"`.

Agent path: agent completes consent (or holds a pre-authorized grant) → calls
the MCP surface with its platform-issued JWT → resolver verifies signature +
grant liveness → principal `Agent::"kc_abc…"`, `acting_for` the granting
human, `agent_type: "contributor"` from the platform's own registry → Cedar
denies `Publish`, permits `SubmitForReview`. The boundary holds because the
kernel resolved the identity itself; there are no headers left for an adapter
to stamp.

## Rollout

Ordered by dependency; step numbers match ARN-255. Steps 1 and 2 ship
together (see the coordination note); steps 3 and 4 each land value on their
own.

1. **Kernel verifies tokens.** Extend the resolver to validate JWTs from
   allowlisted per-tenant issuers (interim Option A: katagami.ai's existing
   authorization server is the first allowlisted issuer). Self-declared
   principal headers are dropped in the same change.
   *Coordination:* ARN-170 (PR #343) closes the header-trust path the
   Katagami MCP adapter currently depends on for its contributor stamp, so
   step 2 must land together with the header removal — otherwise contributor
   enforcement silently disappears.
2. **Katagami MCP adapter forwards tokens.** The adapter passes the caller's
   JWT through instead of swapping to the shared key. The contributor boundary
   becomes kernel-enforced.
3. **Human token exchange + Member roles.** Kernel exchange endpoint; Member
   spec promoted to the platform; katagami.ai server actions call with
   per-user tokens; `KATAGAMI_OWNER_SUBS` retires. Unblocks ARN-248 (personal
   spaces need the kernel to know whose space is whose).
4. **Platform authorization server + spec-compiled policies.** The AS moves
   into the platform and katagami.ai's retires; `requires_role` / `requires`
   sugar ships; new apps default to generated default-deny policies;
   Katagami's hand-written permit-all files are migrated to spec declarations
   in the same pass (they are the proof that the sugar covers a real app);
   ARN-163's MCP resource server points at the platform AS.

## Consequences

### Positive

- Authorization is enforced where the data lives. A compromised or buggy
  front end can no longer publish, delete, or reassign ownership.
- "Agents act, humans own" becomes checkable: every write carries a verified
  acting agent and owning human.
- New apps get login, roles, consent, grants, and policies without writing
  any of it.
- One audited implementation of OAuth machinery instead of one per app.

### Negative

- The kernel takes on OAuth surface (an AS, JWKS, token exchange) it did not
  have; this is real security-critical code the platform must own and test.
- Every app service needs a provisioned credential; the days of one
  environment variable unlocking everything end, which adds setup steps.
- Token exchange adds a network hop to first-request latency per session
  (mitigated by short-lived token caching in the auth kit).

### Risks

- **Migration breakage.** Katagami is live; each rollout step changes a live
  credential path. Every step needs the local end-to-end run against a seeded
  server before deploy, per the standing definition of done.
- **Spec sugar scope creep.** `requires_role` / `requires = "creator"` covers
  the audited needs; resisting a policy-language-in-TOML is deliberate.
  Anything more complex is a hand-written Cedar file.
- **Fail-open regressions.** ARN-230's finding (missing tenant policies fall
  back to permit-all) must be fixed before default-deny generation can be
  trusted; otherwise a failed policy load reopens everything.

## Non-Goals

- Replacing app-chosen human identity providers. Apps keep owning sign-in
  (Google today; anything OIDC later). The platform's job starts at verifying
  the resulting identity and exchanging it for a platform token.
- Agent-to-agent delegation chains beyond one `acting_for` hop (deferred, as
  in ADR-0033).
- SPIFFE/SPIRE integration (compatible, out of scope).
- Fine-grained per-tool consent scopes on the MCP surface (a `contribute`
  scope exists today; splitting it further is future work with ARN-163).

## Open Questions

1. **Where does consent UI live?** Today the consent screen is a katagami.ai
   page. When the AS moves to the platform, does consent stay app-hosted
   (branded, redirect-based) or become a platform-hosted page per tenant?
2. **Anonymous read.** Public catalog pages currently read through the shared
   key. Should anonymous principals get read permits in generated policies,
   or should apps mint a public-reader service credential?
3. **Customer credential revocation.** Human tokens are short-lived; is that
   sufficient, or does the platform need a per-human revocation list
   (sign-out-everywhere) from day one?
4. **Numbering collision hygiene.** Three ADRs share number 0033; policy work
   here will add ADRs — fix the numbering scheme before this effort's ADRs
   land.
