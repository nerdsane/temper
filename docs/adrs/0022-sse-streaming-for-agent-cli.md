# ADR-0022: SSE Streaming for Agent CLI

- Status: Partially Implemented (server-side SSE endpoints + broadcast channels on main; CLI SSE client removed in 84706247; reference impl on agent-runtime-v1 branch)
- Date: 2026-03-03
- Deciders: Temper core maintainers
- Related:
  - ADR-0020: Temper Agent CLI Command
  - `crates/temper-cli/src/agent/` (agent CLI code)
  - `crates/temper-server/src/api.rs` (management API routes)
  - `crates/temper-server/src/state/mod.rs` (server state)

## Context

The `temper agent` CLI command (ADR-0020) polls the server for decision status at 2-second
intervals (300 polls max = 10 minutes). This is wasteful and adds latency — the agent waits
up to 2 seconds after a decision is resolved before it can proceed. Additionally, the LLM
response from Anthropic arrives as a single blob, with no output until the entire response
is generated. Both of these create a poor interactive experience.

The agent executor (future Workstream C) will also need to observe agent progress remotely.
All three use cases — decision watching, LLM streaming, and remote observation — are
natural fits for Server-Sent Events (SSE).

## Decision

### Sub-Decision 1: Replace Decision Polling with SSE Subscription

Add a `watch_decision()` method to `TemperClient` that subscribes to `/tdata/$events` via
an SSE client. The method filters entity state change events for the relevant decision ID
and resolves immediately when the decision status changes. The 10-minute timeout budget is
preserved as a fallback. The existing `poll_decision()` method is kept as a fallback for
servers that do not support SSE.

**Why this approach**: SSE eliminates polling overhead and reduces decision resolution
latency from up to 2 seconds to near-instant. Keeping the polling fallback ensures backward
compatibility.

### Sub-Decision 2: Anthropic Streaming API

Add a `send_streaming()` method to `AnthropicClient` that sets `"stream": true` in the API
request and parses the SSE event stream from Anthropic. Text deltas are printed to stdout
in real-time. Tool-use JSON deltas are accumulated. The final `LlmResponse` is assembled
from accumulated content blocks, matching the non-streaming `send()` return type.

**Why this approach**: Token-by-token output gives the user immediate feedback. Returning
the same `LlmResponse` type means the agent loop does not need structural changes.

### Sub-Decision 3: Agent Progress SSE Channel

Add a `broadcast::Sender<AgentProgressEvent>` to `ServerState` and a
`GET /api/agents/{agent_id}/stream` SSE endpoint. Events include tool call lifecycle
(started, completed) and agent lifecycle (task started, completed, agent completed).
This prepares for the executor binary (Workstream C) to observe agent progress remotely.

**Why this approach**: The broadcast channel pattern is already used for entity state
changes and pending decisions. Reusing the same pattern keeps the codebase consistent.

### Sub-Decision 4: Reusable SSE Client Module

The SSE client is implemented as a standalone module at `crates/temper-cli/src/agent/sse.rs`.
It parses the SSE wire format (`event:`, `data:`, `id:` fields), handles reconnection with
`Last-Event-ID`, and returns `impl Stream<Item = Result<SseEvent>>`. Both the CLI decision
watcher and the future executor will reuse this module.

**Why this approach**: A reusable SSE client avoids duplicating SSE parsing logic across
consumers. Using `reqwest` streaming response keeps dependencies minimal.

## Rollout Plan

1. **Phase 0 (Immediate)** — SSE client module, decision watching, Anthropic streaming,
   agent progress channel. All in this PR.
2. **Phase 1 (Follow-up)** — Executor binary uses SSE client to observe agent progress.
3. **Phase 2** — SDK wrappers for SSE subscriptions (Rust + TypeScript).

## Consequences

### Positive
- Decision resolution latency drops from ~2s to near-instant.
- Users see LLM output token-by-token instead of waiting for the full response.
- Agent progress is observable remotely, enabling the executor architecture.
- SSE client is reusable across CLI and executor.

### Negative
- SSE connection requires a persistent HTTP connection per subscriber.
- More complex error handling for stream reconnection.

### Risks
- Server-side broadcast channel may lag under high event volume. Mitigated by the existing
  256-event buffer and lag-skip pattern already used in decision streaming.

### DST Compliance
- The SSE client lives in `temper-cli`, which is NOT simulation-visible.
- The agent progress broadcast channel in `temper-server` uses `tokio::sync::broadcast`,
  consistent with existing `event_tx`, `design_time_tx`, and `pending_decision_tx`.
  Annotated with `// determinism-ok: broadcast channel for external observation`.

## Non-Goals

- WebSocket support (SSE is sufficient for server-to-client streaming).
- Bidirectional streaming (agent commands flow through OData actions, not SSE).
- SSE authentication (deferred to executor security design).

## Alternatives Considered

1. **WebSocket** — More complex, bidirectional not needed. SSE is simpler and sufficient.
2. **Long polling** — Still has latency. SSE is strictly better for this use case.
3. **gRPC streaming** — Adds a heavy dependency. Not justified for this use case.
