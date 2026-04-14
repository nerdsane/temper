# Crucible — Known Gaps

Features from Anthropic's Managed Agents API that Crucible does not
yet implement, ordered by impact.

---

## High-impact

### SSE Streaming
**Anthropic:** `GET /v1/sessions/{id}/stream` — real-time event
delivery via Server-Sent Events.

**Crucible:** Not supported. The sidecar polls the event feed every
N seconds. For interactive UIs, the user doesn't see incremental
output during a turn — only the final result after the turn completes.

### web_fetch + web_search
**Anthropic:** 2 of 8 built-in tools.

**Crucible:** Not implemented. Would need an external search API
(e.g., SerpAPI, Tavily) and a URL fetcher.

### Custom Tool Flow
**Anthropic:** Agent emits `agent.custom_tool_use` → session pauses
→ client executes → sends `user.custom_tool_result` → agent continues.

**Crucible:** Spec has both event kinds. The sidecar responder does
not pause for client-executed tools — it only handles built-in tools.

### Tool Confirmation (always_ask)
**Anthropic:** Per-tool permission policies. `always_ask` pauses the
session for client approval.

**Crucible:** `AgentToolConfig` with `PermissionPolicy` exists in the
spec. The sidecar ignores policies and always executes immediately.

### Context Compaction
**Anthropic:** Automatically compacts long histories with
`agent.thread_context_compacted`.

**Crucible:** No compaction. Full history re-sent every turn. Long
sessions will exceed the LLM's token limit.

### user.interrupt (mid-turn)
**Anthropic:** Stops the agent mid-execution.

**Crucible:** The sidecar checks for interrupt events between turns
(during the poll cycle), but cannot cancel a turn that's already
in flight (the LLM HTTP call blocks until completion).

---

## Medium-impact

### Batch Event POST
**Anthropic:** `POST /v1/sessions/{id}/events` with array body.

**Crucible:** Individual POSTs. Multiple round-trips for multi-event
batches.

### Agent Version Listing
**Anthropic:** `GET /v1/agents/{id}/versions` — browse past configs.

**Crucible:** `AgentVersion` entity exists but no auto-version-on-
update logic.

### Cron Expression Evaluation
**Crucible's** cron scheduler fires on every heartbeat cycle and uses
a rate limit (MIN_INTERVAL_SECS) to prevent rapid-fire. Proper cron
expression parsing (matching `CronExpression` against the current
time) is not yet implemented in the WASM trigger — every active
schedule fires on every check cycle.

---

## Research Preview (Anthropic beta)

| Feature | Spec | Runtime | Notes |
| ------- | ---- | ------- | ----- |
| **Multi-agent** | full | pending | `CallableAgent` entity, `SessionThread` with Running/Idle/Terminated lifecycle, 4 thread event kinds, `SessionThreadId` on events, cross-invariants. Missing: sidecar orchestration (dispatching to callable agents, managing threads). |
| **Outcomes** | none | none | Rubric-driven evaluation with grader loop. |
| **Custom Skills** | spec only | none | `AgentSkill` entity exists but no skill runtime. |
| **Files API** | none | none | Upload/download files scoped to sessions. |

---

## Wire-format divergences (by design)

| Area | Anthropic | Crucible |
| ---- | --------- | -------- |
| Transport | REST | OData |
| Config | Nested objects | Scalar columns |
| Metadata | `map[string]string` | JSON string |
| Packages | 6 parallel arrays | Child entity |
| Agent loop | Server-side SSE | Sidecar polling |
| Event batch | Array POST | Individual POSTs |
| Memory paths | Nested under store | Flat with FK |
| Cron | Not in Anthropic API | Crucible-specific |

---

## Completeness Summary

| Category | Coverage |
| -------- | -------- |
| Core entities (Env, Agent, Session, Event, Resource) | full |
| Agent children (Tool, MCP, Skill, ToolConfig, Version, Callable) | full (spec) |
| Session lifecycle (5 states, 6 actions) | full |
| Event kinds (24, incl. 4 thread events) | full (spec) |
| Memory Stores + Memories + Versions | full |
| Built-in tools | 6/8 (missing web_fetch, web_search) |
| Tool execution (Local) | full |
| Tool execution (Modal sandbox) | full |
| Sidecar agent loop (watch + send + interrupt) | full |
| Cron scheduling | full (rate-limited, no cron parse) |
| Multi-provider LLM | full (Anthropic, OpenAI, Fireworks) |
| SSE streaming | 0% |
| Custom tool flow | spec only |
| Tool confirmation | spec only |
| Context compaction | 0% |
| Multi-agent (spec: CallableAgent, SessionThread, 4 event kinds) | full spec, runtime pending |
| Outcomes | 0% |
