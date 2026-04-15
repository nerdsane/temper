# ADR-0033: Platform-Assigned Agent Identity

- Status: Proposed
- Date: 2026-03-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0031: Agent Orchestration OS App
  - ADR-0004: Cedar Authorization for Agents
  - `crates/temper-authz/src/context.rs` (SecurityContext, Principal)
  - `crates/temper-mcp/src/runtime.rs` (MCP identity derivation)
  - `crates/temper-platform/src/bearer_auth.rs` (bearer auth middleware)
  - `crates/temper-platform/src/specs/agent_type.ioa.toml` (AgentType registry)

## Context

Agents in Temper currently self-declare their identity. MCP agents report `clientInfo.name` as their type via the initialize handshake, which is stored directly as `agent_type`. HTTP API agents set `X-Temper-Agent-Type` and `X-Temper-Principal-Id` headers freely. Any agent can claim to be any type and receive whatever Cedar policies grant to that type.

This is the **confused deputy problem**: an agent with delegated authority can claim an identity it does not possess, gaining unauthorized access to actions gated on `principal.agent_type`. The bearer auth middleware (`bearer_auth.rs`) validates that a token is present but does not map tokens to identities — it merely checks equality against a single global `TEMPER_API_KEY`.

The `agentTypeVerified` attribute is documented in the project guide but not implemented. Cedar policies referencing `principal.agent_type` (e.g., the orchestration app's `["supervisor", "human"].contains(principal.agent_type)`) have no way to distinguish verified claims from spoofed ones.

**Industry consensus**: IETF WIMSE (`draft-ni-wimse-ai-agent-identity-02`), Microsoft Entra Agent ID, SPIFFE/SPIRE, Google A2A, and the MCP authorization spec update (Nov 2025) all converge on one principle — the platform assigns identity, never the agent. Credentials are verified against a registry; self-declared identity is rejected.

## Decision

### Sub-Decision 1: Agents Never Declare Their Own Identity

All self-declared identity headers (`X-Temper-Agent-Type`, `X-Temper-Principal-Id`) are removed from the protocol. Identity is derived exclusively from bearer token resolution against the platform's credential registry.

**Why this approach**: Self-declared identity is fundamentally untrustworthy. Removing the headers entirely (rather than ignoring them) eliminates the attack surface and prevents any code path from accidentally trusting them.

### Sub-Decision 2: AgentCredential Entity as the Credential Store

A new `AgentCredential` IOA spec maps hashed API keys to `AgentType` entities:

- **States**: `Active` → `Rotated` → `Revoked` (and Active → Revoked)
- **Fields**: `agent_type_id` (reference to AgentType), `agent_instance_id` (platform-assigned UUIDv7), `key_hash` (SHA-256), `key_prefix` (first 8 chars for log identification), `description`, `created_by`, `expires_at`
- **Actions**: `Issue`, `Rotate`, `Revoke`

Each credential maps one API key to one AgentType and one platform-assigned instance ID. Multiple credentials can reference the same AgentType (multiple agents of the same type). Credential lifecycle (rotation, revocation) is independent of AgentType lifecycle (Draft/Active/Deprecated).

**Why this approach**: Keeping credentials as a separate entity (rather than adding `api_key_hash` to AgentType) supports multiple agents per type, independent credential rotation, and the standard IOA lifecycle with Cedar authorization on credential management actions.

### Sub-Decision 3: Identity Resolver with In-Memory Cache

A new `IdentityResolver` module in `temper-server` resolves bearer tokens to `ResolvedIdentity`:

1. Hash the bearer token (SHA-256)
2. Check in-memory cache (`BTreeMap` with TTL via `sim_now()`)
3. Query `AgentCredential` entities by `key_hash`
4. Verify the credential is `Active` and the linked `AgentType` is `Active`
5. Return `ResolvedIdentity { agent_instance_id, agent_type_id, agent_type_name, verified: true }`

The resolver is invoked from the bearer auth middleware. On successful resolution, `ResolvedIdentity` is set as an Axum request extension and consumed by `SecurityContext` construction.

**Why this approach**: The cache prevents entity lookups on every request. `BTreeMap` (not HashMap) ensures deterministic iteration for DST compliance. `sim_now()` for TTL enables deterministic cache expiry in simulation tests.

### Sub-Decision 4: Three Identity Paths Unified Under Credentials

**MCP Path**: `api_key` is required to start the MCP server. On the `initialize` handshake, the MCP server calls `POST /api/identity/resolve` to resolve its credential to a platform-assigned identity. All subsequent requests use the bearer token.

**HTTP Path**: Bearer token is required on all non-health-check requests. The middleware first attempts agent credential resolution, then falls back to the global `TEMPER_API_KEY` (for admin/operator access). No token = 401.

**Orchestrated Path**: When an Agent entity transitions to `Assigned`, the platform mints a new `AgentCredential` and passes the API key to the adapter (e.g., `TEMPER_API_KEY` env var for ClaudeCodeAdapter). The spawned process authenticates back to the platform using this credential, which resolves to the correct verified identity.

### Sub-Decision 5: `agentTypeVerified` in Cedar

`SecurityContext::from_resolved_identity()` sets `principal.attributes["agentTypeVerified"] = true` and `principal.agent_type` from the registry (not from any header). Cedar policies can gate on `context.agentTypeVerified == true` to require verified identity for sensitive actions.

The existing `with_agent_context()` method is removed — it allowed constructing identity from self-declared sources.

## Rollout Plan

1. **Phase 1** — Create `AgentCredential` IOA spec, `IdentityResolver` module, register in bootstrap.
2. **Phase 2** — Rewrite bearer auth middleware for credential resolution; replace `SecurityContext` identity construction; remove self-declared identity headers from protocol.
3. **Phase 3** — Add `/api/identity/resolve` endpoint; rewrite MCP identity initialization to use credential resolution; require `api_key` for MCP startup.
4. **Phase 4** — Mint credentials on Agent.Assign; pass to adapters.
5. **Phase 5** — Wire `agentTypeVerified` into Cedar evaluation and policy generation.

## Consequences

### Positive
- Agent identity is trustworthy — Cedar policies can rely on `principal.agent_type`.
- Spoofing is eliminated — headers are not read for identity.
- Credential lifecycle (rotation, revocation) is governed by the same IOA state machine as all other entities.
- Unified identity model across MCP, HTTP, and orchestrated paths.
- `agentTypeVerified` enables fine-grained Cedar policies distinguishing verified agents from operators.

### Negative
- All agents must have a credential — no more anonymous access. This requires creating credentials as part of agent setup.
- MCP server startup now requires `api_key` — agents can't connect without pre-provisioned credentials.

### Risks
- **Performance**: Entity lookup on every uncached request. Mitigated by in-memory cache with 60-second TTL.
- **Credential management overhead**: Agents need provisioned credentials before they can operate. For orchestrated agents, this is automatic (Phase 4). For MCP agents, a human must create the credential first.

### DST Compliance
- `IdentityResolver` cache uses `BTreeMap` (deterministic iteration) and `sim_now()` (deterministic TTL).
- `agent_instance_id` generation uses `sim_uuid()`.
- No wall-clock, no HashMap, no random in the resolver hot path.
- Annotation: `// determinism-ok` not needed — all primitives are sim-safe.

## Non-Goals

- **OAuth / JWT token validation**: This ADR introduces API-key-based credentials. OAuth integration (token exchange, JWT validation) is a future enhancement.
- **SPIFFE/SPIRE integration**: The credential model is compatible with SPIFFE (an SVID could be used as the bearer token) but direct integration is out of scope.
- **Agent-to-agent delegation chains**: The `acting_for` field on Principal exists but wiring it to token exchange is deferred.

## Alternatives Considered

1. **Add `api_key_hash` field to AgentType** — Rejected because multiple agents of the same type need distinct credentials, and credential rotation shouldn't modify the type definition.
2. **Keep self-declared headers as fallback** — Rejected because any fallback to self-declared identity undermines the security model. The confused deputy problem isn't solved by making it optional.
3. **JWT-based identity with signing keys** — Too complex for the current stage. API keys are simpler, and the `AgentCredential` entity can be extended to support JWT validation later.
