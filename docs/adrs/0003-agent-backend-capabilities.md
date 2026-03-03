# ADR-0003: Agent Backend Capabilities

## Status

Accepted

## Date

2026-02-24

## Deciders

Seshendra Nallakrishnan

## Related

- ADR-0002: Multi-Tenant Spec Registry
- IOA Spec Integration Model
- WASM Integration Pipeline

## Context

Temper is evolving toward being a backend for personal assistant agents (e.g. OpenClaw). Multiple agents belonging to the same user need to coordinate state against Temper and execute actions through Temper's integration layer (WASM, webhooks, external APIs).

Four gaps exist in the current dispatch pipeline:

1. **Agent Identity** — No way to know which agent performed an action. Trajectory entries and SSE events lack agent attribution, making it impossible to audit or debug multi-agent scenarios.

2. **Blocking Integrations** — WASM and webhook integrations are fire-and-forget (`tokio::spawn`). Agents cannot do "check then decide" in a single request — they must poll for the callback result.

3. **Credentials Vault** — API keys for external services are hardcoded via environment variables. No per-tenant encrypted secret management exists, and secrets cannot be injected into WASM host functions at runtime.

4. **Idempotency** — Agents retry failed requests. Without deduplication, the same action can be dispatched multiple times, causing duplicate state transitions and side effects.

## Decision

We add all four capabilities as backward-compatible extensions to the existing dispatch pipeline.

### Sub-decision 1: Agent Identity via HTTP Headers

Introduce `X-Agent-Id` and `X-Session-Id` request headers. An `AgentContext` struct is extracted alongside the existing `X-Tenant-Id` header and threaded through the entire dispatch chain.

- Both headers are optional — omitting them preserves full backward compatibility.
- Agent context is recorded in `TrajectoryEntry`, `EntityStateChange`, `WasmInvocationContext`, and OTEL span attributes.
- The Postgres `trajectories` table gains `agent_id` and `session_id` columns.

**Rationale**: HTTP headers are the simplest mechanism that works with all clients (curl, SDKs, agent frameworks). No authentication is added at this stage — agent identity is informational/audit only.

### Sub-decision 2: Credentials Vault with AES-256-GCM

Introduce a `SecretsVault` that encrypts secrets at rest using AES-256-GCM with a master key (`TEMPER_VAULT_KEY` env var, read once at startup).

- Secrets are per-tenant, stored in a `tenant_secrets` Postgres table as `(ciphertext, nonce)`.
- A management API (`PUT/DELETE/GET /api/tenants/{tenant}/secrets/{key_name}`) handles CRUD.
- At WASM invocation time, all tenant secrets are decrypted and injected into the `ProductionWasmHost` environment map.
- TigerStyle budgets: max 100 secrets per tenant, max 8 KB per secret value.

**Rationale**: AES-256-GCM is the industry standard for authenticated encryption. A single master key simplifies key management for self-hosted deployments. The in-memory cache avoids decrypting on every request.

### Sub-decision 3: Idempotency via Per-Actor LRU Cache

Introduce an in-memory `IdempotencyCache` keyed by `(actor_key, idempotency_key)`.

- The `Idempotency-Key` HTTP header triggers deduplication when present.
- On cache hit, the cached `EntityResponse` is returned immediately (no dispatch).
- On cache miss, the response is cached after successful dispatch.
- TigerStyle budgets: max 1,000 entries per actor, 1-hour TTL.
- No persistence — the cache is ephemeral and survives only within a server process lifetime.

**Rationale**: In-memory LRU is sufficient for agent retry storms (typically seconds, not hours). Persistent idempotency would require distributed locking, which is out of scope for single-node deployments.

### Sub-decision 4: Blocking Integrations via Query Parameter

The `?await_integration=true` query parameter on POST requests changes WASM integration dispatch from fire-and-forget to inline-await.

- A new `dispatch_wasm_integrations_blocking` method awaits the WASM invocation and callback dispatch inline.
- The final `EntityResponse` (post-callback state) is returned to the caller.
- If no matching integrations exist, the original response is returned unchanged.
- Existing fire-and-forget behavior is preserved when the parameter is absent.

**Rationale**: A query parameter is the least invasive extension point. It preserves backward compatibility and lets agents opt in per-request.

## Consequences

### Positive

- Agents can now be identified and audited per-action.
- Secrets are encrypted at rest with per-tenant isolation.
- Agent retries are safe with idempotency keys.
- Agents can perform synchronous "action + integration" flows.
- All four features are backward-compatible — existing clients are unaffected.

### Negative

- `IdempotencyCache` consumes memory proportional to active actors × entry count.
- `SecretsVault` requires a master key environment variable — missing key prevents secret operations.
- Blocking integrations increase request latency (bounded by `WasmResourceLimits.max_duration`).
- Agent identity is unauthenticated — any client can claim any agent ID.

## Alternatives Considered

1. **JWT-based agent identity** — Rejected. Adds authentication complexity; not needed for the informational audit use case.
2. **Redis-backed idempotency** — Rejected. Adds a Redis dependency for a feature that only needs in-process dedup.
3. **Envelope encryption (per-tenant keys)** — Rejected for now. Single master key is simpler; envelope encryption can be added later.
4. **WebSocket-based blocking** — Rejected. Query parameter is simpler and works with standard HTTP clients.
