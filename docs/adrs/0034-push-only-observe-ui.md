# ADR-0034: Push-Only Observe UI

- Status: Accepted
- Date: 2026-03-18
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/observe/` (all observe handlers)
  - `crates/temper-server/src/odata/read_support.rs` (trajectory writes on read ops)
  - `crates/temper-server/src/state/mod.rs` (broadcast channels)
  - `ui/observe/` (frontend polling infrastructure)

## Context

The Observe UI polls every backend endpoint on intervals (2s–30s). Each poll generates OTEL spans via `TraceLayer::new_for_http()` and `#[instrument]` handlers. Additionally, `EntitySetLookup` failures (phantom entity types like `CodexSessions`, `Issues` plural, `Projects` plural) write trajectory rows to Turso on every failed OData GET — 4,269 junk rows, 83% of all trajectories. The unmet intents endpoint then scans all these rows on every 30s poll, amplifying memory pressure. This feedback loop caused a Railway OOM crash on 2026-03-18.

The SSE infrastructure already exists — four broadcast channels (`event_tx`, `design_time_tx`, `pending_decision_tx`, `agent_progress_tx`), six SSE endpoints, and a reconnecting `EventSource` factory on the frontend — but the UI doesn't use it. Only the Activity page and DecisionNotifier subscribe to SSE streams.

## Decision

### Sub-Decision 1: Eliminate all polling from the Observe UI

Replace every `usePolling` / `setInterval` call with SSE-driven data refresh. No data-fetching intervals anywhere in `ui/observe/`.

**Why this approach**: Polling creates O(pages × interval) HTTP requests per minute regardless of whether data changed. SSE pushes only when state actually mutates. This eliminates the OTEL span amplification that caused the OOM.

### Sub-Decision 2: Add a single `ObserveRefreshHint` broadcast channel

Instead of adding a broadcast channel per observe domain (specs, policies, agents, evolution, etc.), add one `observe_refresh_tx: broadcast::Sender<ObserveRefreshHint>` channel that carries typed hint variants. Backend mutation points emit hints; a single SSE endpoint (`GET /observe/refresh/stream`) streams them to the frontend.

**Why this approach**: Keeps the broadcast surface minimal. The frontend already does HTTP GETs for full data — the hint just tells it *when* to refetch, not *what* changed. This avoids duplicating serialization logic between broadcast payloads and REST responses.

### Sub-Decision 3: Stop writing trajectories for EntitySetLookup failures

Read-only OData collection lookups (`GET /odata/{tenant}/{EntitySet}`) should never write to the database. Remove the `persist_trajectory_entry()` call from `record_entity_set_not_found()`. Keep the `tracing::warn!` for debugging.

**Why this approach**: A read operation that writes on every failure creates unbounded growth. Trajectory data should capture meaningful user/agent actions, not infrastructure polling artifacts.

### Sub-Decision 4: SSE auth via Next.js proxy

The `EventSource` browser API doesn't support custom headers. However, the Observe UI's Next.js middleware already injects `Authorization`, `X-Temper-Principal-Id`, and `X-Temper-Principal-Kind` headers on all proxied requests. Since SSE URLs use relative paths through the Next.js rewrite, auth headers are applied automatically. The existing `DecisionNotifier` comment claiming "SSE can't send admin headers" is incorrect — the fallback polling can be removed.

**Why this approach**: No new auth mechanism needed. The proxy already solves this.

## Rollout Plan

1. **Phase 0 (This PR)** — All five phases ship together:
   - Fix trajectory writes (backend)
   - Add `ObserveRefreshHint` broadcast + SSE endpoint (backend)
   - Create `useSSERefresh` hook + `SSERefreshProvider` (frontend)
   - Migrate all pages, remove `usePolling`
   - Update tests
   - Clean up 4,269 junk trajectory rows from Turso (one-time SQL)

2. **Phase 1 (Post-merge)** — Monitor Railway logs for span volume reduction and memory stability.

## Consequences

### Positive
- Eliminates OTEL span amplification from polling (the OOM root cause)
- Data updates appear instantly on mutations instead of after interval delay
- Reduces Turso row reads by ~83% (no more junk trajectory scans)
- Single SSE connection per browser tab instead of 10+ concurrent polling intervals

### Negative
- SSE connections are long-lived; Railway/proxy must not aggressively timeout them
- If the SSE connection drops, data goes stale until reconnection (mitigated by `createReconnectingEventSource` with exponential backoff + full refetch on reconnect)

### Risks
- Browser SSE connection limit (6 per domain): With refresh stream + existing entity/design-time/decision streams = ~4 connections. Safe margin.
- Next.js rewrite and SSE: Already proven working by existing SSE endpoints in production.

### DST Compliance
- `observe_refresh_tx` is a broadcast channel for external observation only. Does not affect actor state transitions or deterministic simulation.
- Annotated with `// determinism-ok: broadcast channel for external observation only`.

## Non-Goals

- Replacing the OData REST API with a streaming protocol (entities are still fetched via HTTP GET)
- Server-push of full data payloads (hints only; frontend refetches via existing HTTP endpoints)
- Addressing the `CodexSessions` polling source (temper-mcp npm package) — tracked separately

## Alternatives Considered

1. **Unified multiplexed SSE stream** — One endpoint that sends all observe data (specs, entities, trajectories, etc.) as typed events. Rejected: forces every client to receive all events; adds serialization complexity; existing REST endpoints already work well for full data.

2. **WebSocket instead of SSE** — Bidirectional communication. Rejected: observe is read-only; SSE is simpler, has automatic reconnection, works through proxies without upgrade negotiation, and is already implemented.

3. **Longer polling intervals** — Increase from 2-30s to 60-120s. Rejected: doesn't fix the fundamental feedback loop; still writes junk trajectories; just delays the OOM.
