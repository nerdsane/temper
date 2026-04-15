# ADR-0012: OAuth2 Enablement — Webhooks, Timers, and Secret Templates

- Status: Accepted
- Date: 2026-02-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0002: WASM integration for agent-generated API calls
  - ADR-0004: Cedar authorization for agents
  - `.vision/AGENT_OS.md` (governance model)
  - `crates/temper-server/src/state/dispatch.rs` (WASM integration dispatch)
  - `crates/temper-server/src/secrets_vault.rs` (encrypted secret storage)
  - `crates/temper-runtime/src/scheduler/mod.rs` (simulation scheduler)

## Context

Temper cannot currently support end-to-end OAuth2 flows (e.g., Gmail integration) purely through specs. After codebase analysis, the gap is smaller than initially assumed:

**Already exists:** Secrets vault (AES-256-GCM, Cedar-gated, multi-tenant), WASM host with `http_call(method, url, headers, body)`, integration engine with async callbacks, reaction system for cross-entity flows.

**Actually missing (3 gaps):**

1. **Secret template resolution** — `{secret:key}` patterns in integration config values are not resolved before WASM invocation. Secrets exist in the vault but cannot flow into HTTP headers/bodies.

2. **Inbound webhook receiver** — No HTTP endpoint for external systems to call back into Temper (e.g., OAuth redirect URI, Stripe webhook). The system can make outbound calls (ADR-0002) but cannot receive inbound callbacks.

3. **Scheduled actions** — No way to fire an action after a delay (e.g., refresh token every 45 minutes). The recurring timer pattern (each successful refresh schedules the next) requires a `schedule` effect type.

With these three features, a complete OAuth2 Gmail flow becomes a pure spec — no custom code. More broadly, any external integration requiring callbacks and periodic refresh becomes expressible.

## Decision

### Sub-Decision 1: Secret Template Resolution

Integration config values containing `{secret:KEY}` are resolved against the tenant's `SecretsVault` before being passed to the WASM module's `WasmInvocationContext`.

```toml
[[integration]]
name = "refresh_token"
type = "wasm"
module = "http_fetch"
headers = '{"Authorization": "Bearer {secret:gmail_refresh_token}"}'
```

A pure function `resolve_secret_templates(config, vault, tenant) -> BTreeMap<String, String>` scans each config value for `{secret:...}` patterns. Missing secrets leave the pattern as-is (no crash). The function uses a hand-rolled scanner (no regex dependency).

**Why this approach**: Minimal surface area. The resolution happens at the single point where integration configs are assembled into `WasmInvocationContext` — one line change in `dispatch_wasm_integrations()`. No new traits, no new config format.

### Sub-Decision 2: Inbound Webhook Receiver

A new `[[webhook]]` section in IOA specs declares inbound HTTP endpoints that trigger entity actions:

```toml
[[webhook]]
name = "oauth_callback"
path = "oauth/callback"
method = "GET"
action = "OAuthCallback"
entity_lookup = "query_param"
entity_param = "state"
extract = { code = "query.code" }
```

HTTP endpoint: `GET|POST /webhooks/{tenant}/{*path}`

Handler flow:
1. Look up webhook route from `SpecRegistry` by `(tenant, path)`
2. Validate HTTP method matches
3. Extract entity ID from configured source (`query_param`, `body_field`, `header`, `path_param`)
4. Extract action params via `extract` map (dotted paths: `query.code`, `body.token`)
5. Call `state.dispatch_tenant_action()` — same code path as all other action dispatch
6. Return 200 OK (or 400/404/405 for errors)

Cedar governance applies: the webhook dispatch goes through the same `dispatch_tenant_action()` path where Cedar policies are evaluated. The webhook is identified by `AgentContext { agent_id: Some("webhook:{name}") }` so Cedar policies can target it specifically. Default-deny means external webhook calls are blocked until a human approves a policy.

Optional HMAC validation (`hmac_secret` + `hmac_header` fields) provides transport-layer defense-in-depth for providers that support signed payloads (GitHub, Stripe).

**Why this approach**: Webhooks are metadata-only (like integrations) — they don't affect verification or state transitions. They dispatch through the existing action pipeline, inheriting all governance, telemetry, and event sourcing. No special security model needed.

### Sub-Decision 3: Scheduled Actions (Timer Effect)

A new `schedule` effect type allows a transition to schedule a delayed action on the same entity:

```toml
[[action]]
name = "ExchangeSucceeded"
from = ["Exchanging"]
to = "Authenticated"
effect = [
    { type = "schedule", action = "RefreshToken", delay_seconds = 2700 }
]
```

In production, `ScheduleAction` effects are executed via `tokio::spawn` + `tokio::time::sleep` (fire-and-forget). In simulation, a new `send_at()` method on `SimScheduler` delivers the message at `current_time + delay_ticks`.

Recurring timers are achieved through a self-scheduling spec pattern — each successful refresh schedules the next one. This is clean, explicit, and model-checkable.

**Why this approach**: No new infrastructure (no separate timer service, no cron, no database table). Timers are just delayed messages through the existing dispatch pipeline. The self-scheduling pattern eliminates the need for a recurring timer primitive while remaining verifiable.

### Sub-Decision 3b: Schedule-At (Absolute Timestamp Timer)

A `schedule_at` effect type reads an absolute ISO 8601 timestamp from an entity field and schedules an action at that time:

```toml
[[action]]
name = "TriggerComplete"
from = ["Active"]
params = ["last_session_id", "last_result", "next_run_at"]
effect = [{ type = "schedule_at", field = "next_run_at", action = "Trigger" }]
```

Computed as `delay = target - now` (clamped to 0 if the timestamp is in the past). Reuses the same `ScheduledAction` dispatch path as `schedule`.

**Key design detail**: `schedule_at` is a *deferred effect* — it is collected during `apply_effects()` but resolved AFTER `sync_fields()` in `process_action_with_xref()`. This ensures that WASM-provided params (like a computed `next_run_at`) are available in entity state when the timestamp field is read.

**Motivation**: Enables self-scheduling entities where the next run time is computed by WASM integrations (e.g., cron expression parsing). Eliminates the need for polling infrastructure — no external cron trigger, no heartbeat hacks, no scheduler entities.

**DST compliance**: Uses `sim_now()` for timestamp comparison, so simulation tests see deterministic scheduling. Same non-durable timer model as `schedule` — reconstructed from event replay on restart.

## Rollout Plan

1. **Phase 0 (This ADR)** — Document decisions before any code.
2. **Phase 1 (Secret Templates)** — `resolve_secret_templates()` + wire into dispatch. Independent, no spec format changes.
3. **Phase 2 (Webhook Receiver)** — `[[webhook]]` spec section + axum handler + registry. Independent, new spec section.
4. **Phase 3 (Scheduled Actions)** — `Schedule` effect type + `send_at()` + timer scheduling. Independent, new effect variant.
5. **Phase 4 (E2E Proof)** — Gmail OAuth2 spec + DST test exercising all three features together.

Phases 1-3 are independent and can be implemented in parallel.

## Consequences

### Positive
- End-to-end OAuth2 flows become pure specs — no custom code required.
- Any external integration with callbacks (Stripe, GitHub, Twilio) is now expressible.
- Token refresh and periodic tasks work through the self-scheduling pattern.
- All new capabilities go through existing Cedar governance — no new security surface.

### Negative
- Production timer scheduling via `tokio::spawn` is not durable across server restarts. Timers in flight are lost if the server restarts. Mitigation: on replay, check if a scheduled action's deadline passed without a corresponding callback event, then re-schedule with remaining delay.
- Webhook endpoints expand the HTTP attack surface. Mitigation: Cedar default-deny + optional HMAC validation.

### Risks
- Timer drift in production (tokio::time::sleep is not guaranteed exact). Acceptable for OAuth2 refresh (45-min window, few seconds of drift is irrelevant).
- Webhook path collisions across entity types within a tenant. Mitigation: paths are scoped per entity type and uniqueness is validated during registration.

### DST Compliance

**Secret templates**: Pure string replacement in production-only WASM invocation path (already `// determinism-ok`). Not simulation-visible — secrets are not available in simulation.

**Webhook receiver**: Inbound webhooks dispatch through `dispatch_tenant_action()` which is already the shared code path for simulation and production. In DST, webhook arrivals are modeled as direct action dispatch calls — no HTTP layer in simulation.

**Scheduled actions**: `send_at()` uses the same `BinaryHeap<SimMessage>` as `send()` — fully deterministic. Delay is in ticks (1 tick = 1 simulated second by convention). Fault injection still applies (message drops test what happens when a timer fails). Production uses `tokio::spawn` with `// determinism-ok: timer delivery is a background side-effect`.

## Non-Goals

- **OAuth2 provider-specific logic** — No Gmail module, no Stripe module. The `http_fetch` built-in module handles all REST API calls. Provider-specific logic lives in the spec's integration config.
- **Built-in WASM modules for OAuth** — The existing `http_fetch` module is sufficient for token exchange and refresh. Agents can use `compile_wasm()` for complex response parsing if needed.
- **Recurring timer primitives** — Recurring behavior is achieved through the self-scheduling spec pattern. A dedicated `recurring` timer type adds complexity without benefit.
- **Webhook authentication beyond HMAC** — OAuth2 bearer tokens, API keys, and other auth schemes for webhook validation are out of scope. Cedar is the primary security gate; HMAC is optional defense-in-depth.
- **Timer persistence in a separate store** — Timers are reconstructed from event replay. A dedicated timer database is over-engineering for the current use case.

## Alternatives Considered

1. **Dedicated OAuth2 WASM module** — A built-in module that handles the full OAuth2 flow (authorization URL generation, token exchange, refresh). Rejected because it hardcodes provider-specific logic into the framework, violating the "no entity-specific hardcoding" constraint. The `http_fetch` module + spec composition achieves the same result generically.

2. **External webhook gateway (e.g., Hookdeck)** — Route callbacks through an external service that handles validation and delivery. Rejected because it adds an operational dependency and breaks the "single binary" deployment model. The in-process axum handler is simpler and sufficient.

3. **Cron-style timer definitions** — `schedule = "*/45 * * * *"` in the spec. Rejected because cron expressions are harder to model-check than explicit delay values, and the self-scheduling pattern covers all use cases while remaining verifiable.
