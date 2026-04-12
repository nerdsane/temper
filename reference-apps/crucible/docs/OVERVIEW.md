# Crucible — Overview

Crucible is a reference implementation of Anthropic's
[Managed Agents API](https://platform.claude.com/docs/en/managed-agents/overview)
built on [Temper](https://github.com/nerdsane/temper). It demonstrates how
an agent-runtime control plane maps onto Temper's modelling primitives:
entities, state machines, Cedar policies, OData routes, and field
invariants.

Crucible is **not wire-compatible** with Anthropic's API. It shares the
conceptual shape but speaks OData, flattens config into scalar columns,
and makes several deliberate divergences documented below. The goal is to
show *how* these concepts work on Temper, not to be a drop-in replacement.

---

## Concepts

### Environments

An **Environment** defines where agent tools execute.

| ConfigType | Description |
| ---------- | ----------- |
| `Local` | Tools execute on the host machine. Requires `Unrestricted` networking; forbids MCP servers and package managers. The simplest path for development. |
| `Modal` | Tools execute inside a [Modal](https://modal.com) sandbox — an isolated remote container with configurable CPU, memory, GPU, and image. Provisioned lazily on the first tool call. |

**Lifecycle:** Active → Archived (terminal). DELETE supported with
referential integrity (rejects if Sessions reference the environment).

**Key fields:** `Name`, `ConfigType`, `NetworkingType`,
`AllowMcpServers`, `AllowPackageManagers`, and (for Modal)
`ModalImage`, `ModalCpu`, `ModalMemory`, `ModalTimeout`, `ModalGpu`,
`ModalWorkdir`, `ModalBlockNetwork`.

### Agents (ManagedAgent)

A **ManagedAgent** is the agent configuration: which model to use,
what system prompt to give it, and what tools/skills/MCP servers are
available.

> The entity is called `ManagedAgent` rather than `Agent` because
> Temper's Agent OS app already owns the `Agent` type name.

**Key fields:** `Name`, `ModelId` (e.g.,
`accounts/fireworks/routers/kimi-k2p5-turbo`), `System` (system
prompt), `ModelSpeed` (`standard`/`fast`), `Version`.

**Lifecycle:** Active → Archived (terminal).

**Child entities:**
- `AgentTool` — tool configurations (`agent_toolset`, `custom`, `mcp_toolset`)
- `AgentToolConfig` — per-tool permission policies (`always_allow`, `always_ask`)
- `AgentMcpServer` — MCP server declarations
- `AgentSkill` — skill references
- `AgentVersion` — immutable snapshots of past agent configs
- `CallableAgent` — multi-agent delegation references

### Sessions

A **Session** is a running conversation between a user and an agent
in a specific environment. It is the primary runtime entity — you
send messages to a session and the agent responds.

**Lifecycle** (5-state machine):

```
Rescheduling ──▶ Running ◀──▶ Idle
                   │            │
                   │            ├──▶ Rescheduling
                   │            ├──▶ Terminated
                   ▼            ▼
               Terminated ◀────┘
                   │
                   ▼
                Archived (terminal)
```

**Key fields:** `AgentId`, `EnvironmentId`, `Status`, `Title`, plus
usage counters (`InputTokens`, `OutputTokens`, etc.).

**Bound actions:** `StartSession`, `IdleSession`, `ResumeSession`,
`RescheduleSession`, `TerminateSession`, `ArchiveSession`.

The agent loop is driven by the **sidecar** (`crucible-chat watch`),
which polls the event feed, detects new `user.message` events, calls
the LLM, executes tools, and writes response events. The sidecar
calls `StartSession`/`ResumeSession` to enter Running, and
`IdleSession` when done.

### Session Events

**SessionEvents** are the append-only event log for a session. Every
user message, agent response, tool call, tool result, and
observability span is a SessionEvent row.

Crucible supports **24 event kinds** across four groups:

| Group | Kinds |
| ----- | ----- |
| **User** | `user.message`, `user.interrupt`, `user.tool_confirmation`, `user.custom_tool_result` |
| **Agent** | `agent.message`, `agent.thinking`, `agent.tool_use`, `agent.tool_result`, `agent.custom_tool_use`, `agent.mcp_tool_use`, `agent.mcp_tool_result`, `agent.thread_context_compacted`, `agent.thread_message_sent`, `agent.thread_message_received` |
| **Session** | `session.status_running`, `session.status_idle`, `session.status_rescheduled`, `session.status_terminated`, `session.deleted`, `session.error`, `session.thread_created`, `session.thread_idle` |
| **Span** | `span.model_request_start`, `span.model_request_end` |

Each event has a `Sequence` (monotonic), `Kind`, `Content` (JSON
blob), and kind-specific scalar columns (`ToolName`, `StopReason`,
`ModelInputTokens`, etc.) enforced by field invariants.

### Session Resources

**SessionResources** attach external resources to a session:

| Kind | Description |
| ---- | ----------- |
| `github_repository` | A Git repo (requires `Url`) |
| `file` | A file reference (requires `FileId`) |
| `memory_store` | A memory store (requires `MemoryStoreId`, optional `Access` and `Prompt`) |

### Memory Stores

**Memory Stores** are workspace-scoped collections of text documents
that persist across sessions. When attached to a session, the agent
can read and write memories to build up durable knowledge.

**Entities:**
- `MemoryStore` — named collection (Active → Archived)
- `Memory` — text document at a file-like `Path` (e.g., `/preferences/formatting.md`), up to 100KB
- `MemoryVersion` — immutable audit record of every mutation (Active → Redacted via `RedactVersion`)

**Version operations:** `created`, `modified`, `deleted`.

**Redaction:** `RedactVersion` clears content fields while preserving
audit metadata (for compliance — leaked secrets, PII, GDPR).

### Tools

Crucible supports **6 built-in tools** from Anthropic's agent toolset:

| Tool | Description |
| ---- | ----------- |
| `bash` | Execute a bash command |
| `read` | Read a file (with line numbers, offset, limit) |
| `write` | Write a file (creates parent directories) |
| `edit` | Find-and-replace in a file (must be unique match) |
| `glob` | File pattern matching |
| `grep` | Regex text search |

For **Local** environments, tools execute directly on the host via
the sidecar process. For **Modal** environments, the tool server
(`modal_bridge/server.py`) proxies tool calls into the Modal sandbox.

Two additional Anthropic tools (`web_fetch`, `web_search`) are not
yet implemented.

### Multi-Agent (Callable Agents + Threads)

Crucible supports **multi-agent delegation** at the spec level. A
coordinator agent can delegate work to callable agents, each running
in its own session thread with an isolated context window.

**Entities:**
- `CallableAgent` — child of ManagedAgent, declares which agents the coordinator can delegate to. Requires `CalleeAgentId` and optional `CalleeAgentVersion`.
- `SessionThread` — child of Session, represents a sub-agent execution thread. Lifecycle: Running → Idle ↔ Running → Terminated.

**Event kinds** (4 thread-specific):
- `session.thread_created` — a delegation thread was spawned
- `session.thread_idle` — a thread finished its current work
- `agent.thread_message_sent` — coordinator sent a message to a sub-agent
- `agent.thread_message_received` — coordinator received a result

Events carry `SessionThreadId` to scope them to specific threads.
Field invariants enforce that thread events always have
`SessionThreadId` set.

> **Note:** The spec surface is complete but the sidecar agent loop
> does not yet implement multi-agent orchestration (dispatching to
> callable agents, managing threads). This is a follow-up.

### Cron Scheduling

**SessionSchedules** trigger agent sessions on a time basis. A
`CrucibleScheduler` heartbeat entity periodically checks active
schedules and fires the `crucible_cron_trigger` WASM module, which
posts a `user.message` to the target session. The sidecar `watch`
picks it up and drives the turn — the scheduler doesn't know about
LLMs or tools.

**Entities:**
- `SessionSchedule` — cron definition (Draft → Active → Paused → Expired) with `CronExpression`, `MessageTemplate`, and template variables (`{{now}}`, `{{run_count}}`, `{{last_result}}`)
- `CrucibleScheduler` — per-tenant heartbeat (Idle ↔ Checking loop) that queries active schedules and fires due ones

**WASM modules** (3, uploaded at runtime):
- `crucible_cron_trigger` — template substitution + POST user.message
- `crucible_scheduler_check` — query active schedules, dispatch Trigger
- `crucible_scheduler_heartbeat` — wait N seconds, re-trigger check

---

## Entity Relationship Diagram

```
MemoryStore ◄─── Memory ◄─── MemoryVersion
                   ▲
                   │ (via SessionResource)
                   │
Environment ◄─── Session ──▶ ManagedAgent
     │              │              │
     ├── AllowedHost│              ├── AgentTool ──▶ AgentToolConfig
     └── Package    │              ├── AgentMcpServer
                    │              ├── AgentSkill
                    ├── SessionResource    ├── AgentVersion
                    ├── SessionEvent       └── CallableAgent
                    ├── SessionThread
                    └── SessionSchedule ◄── CrucibleScheduler (heartbeat)
```

---

## Deliberate Divergences from Anthropic

| Area | Anthropic | Crucible |
| ---- | --------- | -------- |
| Transport | REST with nested paths | OData with flat entity sets |
| Config | Nested JSON objects | Flattened to scalar CSDL columns |
| Metadata | `map[string]string` | `Edm.String` holding JSON |
| Packages | 6 parallel arrays (`pip`, `npm`, etc.) | `EnvironmentPackage` child entity |
| Allowed hosts | `string[]` inline | `EnvironmentAllowedHost` child entity |
| Agent name | `Agent` | `ManagedAgent` (Temper owns `Agent`) |
| Agent loop | Server-side (SSE streaming) | Sidecar polling (`crucible-chat watch`) |
| Session archive | Any non-archived → Archived | Requires `TerminateSession` first |
| Event batch POST | `POST /sessions/{id}/events` array | Individual POSTs to `/tdata/SessionEvents` |
| SSE streaming | `GET /sessions/{id}/stream` | Not supported — poll with `$filter` |
| State drivers | Status events drive lifecycle | Bound actions drive lifecycle |
| Memory stores | Nested paths (`/stores/{id}/memories`) | Flat: `/tdata/Memories` with `MemoryStoreId` FK |
| Cron scheduling | Not in Anthropic API | Crucible-specific via WASM heartbeat loop |

---

## Related ADRs

| ADR | Topic |
| --- | ----- |
| [0041](../../docs/adrs/0041-ioa-field-invariants.md) | Field invariants + cross-invariant grammar |
| [0042](../../docs/adrs/0042-crucible-reference-app.md) | Crucible design + Phase 0 (Environment) |
| [0043](../../docs/adrs/0043-crucible-agents-slice.md) | Phase 1 (ManagedAgent + children) |
| [0044](../../docs/adrs/0044-crucible-sessions-slice.md) | Phase 2 (Session lifecycle) |
| [0045](../../docs/adrs/0045-crucible-session-events-full-coverage.md) | Phase 3 (20-kind event log) |
| [0046](../../docs/adrs/0046-crucible-agent-loop-phase-4.md) | Phase 4 (agent loop + crucible-chat) |
