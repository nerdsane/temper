# ADR-0069: HttpEndpoint — inbound path-prefix routing to streaming WASM integrations

- Status: Proposed
- Date: 2026-04-20
- Deciders: Temper core maintainers
- Related:
  - ADR-0002: wasm-integration-for-agent-generated-api-calls (WASM integration shape)
  - ADR-0012: oauth2-enablement-webhooks-timers-secret-templates (inbound Webhook receiver — action-centric, not streaming)
  - ADR-0157: host-connect-call (outbound streaming host call; this is the inbound dual)
  - `crates/temper-server/src/router.rs` (axum router to extend)
  - `crates/temper-server/src/webhooks/receiver.rs` (existing inbound receiver — useful contrast)

## Context

Temper today has three inbound HTTP surfaces and no more:

1. `/tdata/{*path}` — the OData Data API for entity reads/writes and bound actions.
2. `/webhooks/{tenant}/{*path}` — inbound webhooks that dispatch to a single IOA entity action and return a short response synchronously.
3. `/_admin`, `/observe`, `/api` — platform-internal routes, compile-time wired in `router.rs`.

This covers entity CRUD + callback hooks but falls short for apps that must expose **custom protocol handlers** whose wire format is defined externally and whose responses may be streamed:

- Git's smart-HTTP transport (`/<repo>.git/info/refs`, `/<repo>.git/git-upload-pack`, `/<repo>.git/git-receive-pack`) — byte-exact, uses pkt-line framing, and clone/push responses routinely run into hundreds of megabytes.
- S3-compatible object APIs an app might implement.
- A GitHub-REST-v3-shaped `/api/v3/...` subset where the app controls response shape and status.
- Any other wire-protocol-compatibility layer where the app is not in a position to say "please rewrite your client to call OData instead."

Today the only way to ship any of those is to write a bespoke Rust service next to Temper and teach it to talk to Temper's event store. That violates Temper's "app = specs + Cedar + WASM, no sidecar code" discipline. **temper-git is the first concrete driver**, but the surface is general.

The existing Webhook receiver is close but insufficient: it dispatches one entity action, returns a single short response, and does not stream. A 1 GiB `git clone` cannot round-trip through `handle_webhook`.

## Decision

Introduce `HttpEndpoint`, an IOA entity that registers a path prefix with a WASM integration. At request time, the kernel performs longest-prefix match against the live set of `HttpEndpoint` rows in the target tenant, resolves auth, and dispatches to the integration with a streaming request/response channel.

### Sub-Decision 1: `HttpEndpoint` is a first-class IOA entity, not a config file

The entity lives in each tenant's spec registry — same lifecycle and audit story as every other IOA row. State machine:

```
Active ──Pause──▶ Paused ──Resume──▶ Active ──Delete──▶ Deleted (terminal)
```

Key properties:

| Field | Type | Note |
|---|---|---|
| Id | UUIDv7 string, `he-` prefix | stable pointer |
| PathPrefix | `Edm.String` | starts with `/`, e.g. `/{owner}/{repo}.git/git-upload-pack` |
| Methods | comma-separated `GET,POST,...` | uppercase |
| IntegrationModule | `Edm.String` | references a declared WASM integration by name |
| RequiresAuth | `Edm.Boolean` | if true, kernel must resolve a Principal before dispatch |
| TimeoutSecs | `Edm.Int32`, 1..600 | hard cap on total invocation wall time |
| Status | state | Active / Paused / Deleted |

Full IOA TOML + CSDL additions live under `crates/temper-platform/src/specs/http_endpoint.ioa.toml` and the shared platform CSDL (see Sub-Decision 6 for placement).

**Why an entity, not a config file**: every operational thing in Temper is an entity (Webhook, Cron, ManagedAgent, …). Reconciliation is observational — watch the entity set, rebuild the router table on change. Audit, Cedar, and replay all come for free. A config file would need a bespoke reload mechanism and have no audit trail.

### Sub-Decision 2: Path matching is longest-prefix over registered `PathPrefix` values

At request time, the kernel does longest-prefix match against registered `Active` endpoints. Templated segments (`{name}`) match any path segment and are extracted into `ctx.params`. This is deliberately simpler than axum's matchit trie — we expect O(hundreds) of endpoints per tenant, not O(thousands), and the longest-prefix rule is what git clients expect.

Rules:

- Static segments match literally.
- `{param}` segments match a single path segment (no `/`).
- `{*rest}` is **not** supported in v1 — if an app needs tail matching it can register multiple sibling prefixes. (Follow-up ADR to revisit if this proves painful.)
- Conflict resolution: longest `PathPrefix` wins. Ties on length are rejected at `Create` time by a cross-entity invariant.
- Built-in routes (`/tdata`, `/webhooks`, `/_admin`, `/observe`, `/api`, `/_internal`) are reserved. `Create` rejects a `PathPrefix` that begins with any reserved namespace.

### Sub-Decision 3: Dispatch uses a new `WasmHost::handle_http` entrypoint

The WASM SDK exports a module-defined `handle_http` function:

```rust
// In the integration module:
#[temper_wasm_sdk::http_handler]
fn handle_http(ctx: HttpRequestContext) -> HttpResponse {
    // ctx.method, ctx.path, ctx.params, ctx.headers, ctx.principal
    // ctx.body_stream: impl Read         (streaming in)
    // ctx.response_stream: impl Write    (streaming out)
    // return the status + final headers; body is already streamed
}
```

Both request and response bodies are streamed through kernel-managed bounded channels. Integrations never hold the full body in linear memory. The kernel's host side bridges the channels to the axum request/response. **This depends on ADR-005N `http_call_streaming` (K-2 in temper-git RFC-0001), which adds the bidirectional streaming primitives to `WasmHost`.** K-2 is a prerequisite and has its own ADR.

Timeout: if the integration does not complete within `TimeoutSecs`, the kernel aborts the invocation and returns `504 Gateway Timeout`. The integration is given one last chance to flush buffered response bytes (drain semantics).

### Sub-Decision 4: Cedar — platform-admin only for management, per-request check for dispatch

Two Cedar gates:

- **Management** (`Create` / `Pause` / `Resume` / `Delete` on the `HttpEndpoint` entity itself): platform-admin only. Adding or changing a route reshapes the tenant's attack surface, so this lives outside the app's own admin scope.
- **Dispatch** (per inbound request): the kernel runs the bound integration's existing Cedar policy with `principal` = resolved request principal, `action` = `Action::"HandleHttp"`, `resource` = `HttpEndpoint::<Id>`. Apps write the policy they want; the kernel just ensures the check runs before the integration gets byte one of the request body.

Auth resolution is standard: if `RequiresAuth == true`, the kernel runs the configured authentication chain (bearer token → `GitToken`/`Agent`/`Customer`/... depending on the tenant) before dispatch. If resolution fails, the kernel returns `401 Unauthorized` and the integration is never invoked.

### Sub-Decision 5: Reconciliation via a router fallback + in-memory cache

Implementation shape in `temper-server/src/router.rs`:

```rust
// After the built-in routes, but before 404:
.fallback(http_endpoint_dispatcher::handle)
```

`http_endpoint_dispatcher` consults an in-memory `HttpEndpointTable` keyed on tenant. The table is rebuilt eagerly when:

- Any `HttpEndpoint` entity-state change event arrives (reuses the existing entity_actor change feed).
- Tenant registry is reloaded.

Lookups are `O(path_segments × tenant_endpoint_count)` — a 128-entry linear scan for a 20-segment path is a few µs, well inside budget. If this becomes hot we swap to a matchit trie; the interface doesn't change.

Built-in routes retain priority (they are matched first, before the fallback ever runs) so an app cannot shadow `/tdata` or `/_admin` even by accident.

### Sub-Decision 6: Spec placement — platform-level, not os-app-level

`http_endpoint.ioa.toml` and its Cedar policy ship in `crates/temper-platform/src/specs/` and are loaded by every tenant automatically, same treatment as the platform-provided `Observation`/`Problem`/`Analysis`/`EvolutionDecision`/`Insight` entities (see `specs/PLATFORM-PROVIDED.md` in downstream apps). App authors do not redefine it; they just write `HttpEndpoint` rows via OData.

### Sub-Decision 7: Precedent-preserving: `Webhook` receiver stays as-is

The existing `/webhooks/{tenant}/{*path}` receiver is not folded into `HttpEndpoint`. It serves a different contract (one-shot, action-dispatching, short-response) and downstream apps rely on its exact shape. `HttpEndpoint` is additive.

## Rollout Plan

1. **Phase 0 (this PR)** — ADR only. No code changes.
2. **Phase 1** — Land `http_endpoint.ioa.toml`, Cedar policy, CSDL additions, and the Cedar-surface `HttpEndpointTable` in-memory reconciler. Kernel router gains the fallback but dispatch is stubbed (`501 Not Implemented`) until Phase 2. L0–L3 verification cascade for the entity.
3. **Phase 2** — Wire the dispatcher end-to-end on top of K-2 (`http_call_streaming`). Add integration tests: a minimal `echo_handler` WASM module that streams request body back as response body, driven from a test axum client. Verify `GET /info/refs` from real `git` CLI against a stubbed temper-git build (external driver).
4. **Phase 3** — Observability: one span per dispatch carrying `http.route`, `http.status_code`, `temper.endpoint_id`, `temper.integration_module`, `temper.tenant`. Add a `temper_http_endpoint_dispatch_duration` histogram.

## Readiness Gates

- **Gate 1** — IOA L0–L3 green on `http_endpoint.ioa.toml` across all existing tenants (merging this adds the entity to every tenant's registry — must not cause regressions).
- **Gate 2** — `echo_handler` round-trip of 1 GiB payload with bounded resident memory (<64 MiB in both the WASM guest and the host bridge). This is the gate that K-2 itself can't satisfy — ensures the path is actually streaming end-to-end and neither side accumulates.
- **Gate 3** — Reserved-namespace check: attempting `Create` with `PathPrefix = "/tdata/..."` is rejected synchronously with a documented error code, verified by a unit test and a DST property.
- **Gate 4** — Platform-admin-only Cedar enforcement verified: an app-level admin principal cannot create or delete `HttpEndpoint` rows.

## Alternatives considered

- **Generalize `Webhook` to cover streaming** — rejected. The entity's contract (one action per request, short response) is used by OAuth callbacks, payment gateways, and the Discord channel adapter. Retrofitting streaming would either break those consumers or make the entity's semantics bimodal.
- **Ship a per-app bespoke HTTP shim crate** — rejected. It reintroduces "code outside Temper's primitives." The whole point of this ADR is to keep apps declarative.
- **Use axum's `nest` with a compile-time module registry** — rejected. Route registration is a runtime concern (tenants come and go), and forcing a server rebuild to add a route is not an acceptable operator experience.
- **Regex-based path matching** — rejected for v1. Too much rope for too little gain; longest-prefix + `{param}` segments covers the motivating cases (git smart-HTTP + GitHub REST subset) and is trivially auditable.
