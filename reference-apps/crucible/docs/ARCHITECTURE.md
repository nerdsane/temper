# Crucible — Architecture

How things work under the hood.

---

## The Big Picture

```
┌──────────────────────────────────────────────────────────┐
│  temper serve (:3000)                                     │
│                                                          │
│  Pure OData server + entity actors + WASM dispatch.       │
│  No agent logic — just stores state and runs specs.       │
│                                                          │
│  Entities: Environment, ManagedAgent, Session,            │
│    SessionEvent, SessionResource, MemoryStore, Memory,    │
│    MemoryVersion, SessionSchedule, CrucibleScheduler,     │
│    AgentTool, AgentVersion, SessionThread, ...            │
│                                                          │
│  WASM integrations (cron only):                           │
│    crucible_cron_trigger, crucible_scheduler_check,       │
│    crucible_scheduler_heartbeat                           │
└──────────────┬───────────────────────────────────────────┘
               │ OData HTTP
               │
┌──────────────┴───────────────────────────────────────────┐
│  crucible-chat (sidecar)                                  │
│                                                          │
│  Subcommands:                                             │
│    seed      — create Environment + Agent + Session       │
│    send      — POST user.message (rejects if Running)     │
│    watch     — poll event feed, drive LLM turns           │
│    interrupt — POST user.interrupt + optional redirect     │
│    respond   — one-shot turn (legacy)                     │
│                                                          │
│  Calls LLM (Fireworks/OpenAI/Anthropic) directly.         │
│  Executes tools in-process (Local) or via tool server.    │
└──────────────┬───────────────────────────────────────────┘
               │ (Modal only)
               │
┌──────────────┴───────────────────────────────────────────┐
│  modal_bridge/server.py (:3100)                           │
│  Tool server: Local execution + Modal sandbox proxy       │
│  Lazy sandbox provisioning on first Modal tool call        │
└──────────────────────────────────────────────────────────┘
```

The server holds state. The sidecar drives the agent loop. They
communicate only via OData HTTP. Two separate processes, fully
decoupled.

---

## Layer by Layer

### 1. OData Router

Every entity is exposed via Temper's OData surface:

| Operation | Method | Path |
| --------- | ------ | ---- |
| Create | POST | `/tdata/{EntitySet}` |
| Read | GET | `/tdata/{EntitySet}('{id}')` |
| List | GET | `/tdata/{EntitySet}?$filter=...&$orderby=...` |
| Update | PATCH | `/tdata/{EntitySet}('{id}')` |
| Delete | DELETE | `/tdata/{EntitySet}('{id}')` |
| Bound action | POST | `/tdata/{EntitySet}('{id}')/Temper.Crucible.{Action}` |

Tenant isolation via `X-Tenant-Id` header. Field invariants and
cross-invariants run on every write. Violations return 409.

### 2. Entity Actors

Each entity instance is backed by an actor. The actor holds a
`TransitionTable` built from the IOA spec and processes actions
sequentially. Post-dispatch effects (including WASM triggers) run
after successful transitions.

### 3. IOA Specs

Every entity's lifecycle is declared in I/O Automaton TOML:

```toml
[automaton]
name = "Session"
states = ["Rescheduling", "Running", "Idle", "Terminated", "Archived"]
initial = "Rescheduling"

[[action]]
name = "StartSession"
from = ["Rescheduling"]
to = "Running"
```

No effects on Session actions — the sidecar drives the loop
externally. Effects are only used for cron scheduling.

### 4. Field Invariants

Cross-field validation on every write:

```toml
[[field_invariant]]
name = "LocalNetworkingMustBeUnrestricted"
when = { field = "ConfigType", equals = "Local" }
require = { field = "NetworkingType", equals = "Unrestricted" }
message = "Local environments must use Unrestricted networking"
```

### 5. Cross-Invariants

Parent-field lookup rules spanning entity boundaries:

```toml
[[invariant]]
name = "MemoryRequiresActiveStore"
kind = "hard"
on = "Memory.*"
assert = 'related(MemoryStore, MemoryStoreId).Status not in ["Archived"]'
```

`default_delete_policy = "restrict"` enforces referential integrity
on DELETE.

### 6. Cedar Authorization

Permissive stubs for all entities. WASM modules have specific
permits for `http_call` and `access_secret`:

```cedar
permit(principal is Agent, action == Action::"http_call", resource is HttpEndpoint)
when { ["crucible_cron_trigger", "crucible_scheduler_check",
        "crucible_scheduler_heartbeat"].contains(context.module) };
```

### 7. The Sidecar Agent Loop

`crucible-chat watch` implements a polling agent:

```
loop every N seconds:
    GET Session status
    if Terminated/Archived → exit
    if Running → skip (turn in flight)

    GET all SessionEvents
    if no pending user.message after last session.status_idle → skip

    drive_to_running (StartSession or ResumeSession)

    for iteration in 0..25:
        POST span.model_request_start
        build messages[] from event history
        call LLM (OpenAI-compatible /chat/completions)

        if tool_calls:
            POST agent.tool_use per tool
            execute tool (local or tool server)
            POST agent.tool_result per tool
            POST span.model_request_end
            continue

        else (text):
            POST agent.message
            POST span.model_request_end
            POST session.status_idle
            break

    POST IdleSession (Running → Idle)
```

**Stateless.** Every invocation re-reads the full event history.
Multi-turn memory is a free consequence — no in-process state
survives between turns.

**Interrupt detection:** `user.interrupt` events in the feed cause
the watch to skip the turn and transition to Idle.

### 8. Tool Execution

For **Local** environments, tools run in-process via `chat::tools`:
```rust
tools::execute_tool("bash", &json!({"command": "ls -la"}))
```

For **Modal** environments, the sidecar calls the Python tool server
(`modal_bridge/server.py`), which proxies to the Modal SDK:
```
POST /tools/bash {"arguments":{"command":"ls"},"environment_id":"env-modal"}
→ tool server checks ConfigType=Modal
→ lazy-provisions sandbox if needed
→ sandbox.exec("bash", "-c", "ls")
→ returns {"output":"...", "is_error": false}
```

### 9. Cron Scheduling

Three WASM modules create a self-scheduling heartbeat:

```
CrucibleScheduler (Idle ↔ Checking)
  Start → crucible_scheduler_check WASM
    query active SessionSchedules
    dispatch Trigger on each due schedule
  CheckComplete → crucible_scheduler_heartbeat WASM
    wait N seconds (observe long-poll)
    re-trigger ScheduledCheck
  → loop

SessionSchedule.Trigger → crucible_cron_trigger WASM
  template substitution ({{now}}, {{run_count}})
  check session status (skip if Running)
  rate limit (skip if < 4s since last trigger)
  POST user.message to session
  PATCH LastTriggeredAt
```

The cron system runs entirely inside `temper serve` via WASM. The
sidecar `watch` sees the posted user.message and drives the turn.

### 10. Secrets Vault

API keys are stored encrypted (AES-256-GCM):

```
PUT /api/tenants/crucible/secrets/llm_api_key {"value":"fw_..."}
→ encrypt with TEMPER_VAULT_KEY → persist to Turso → cache in memory
```

The sidecar reads the key from its own environment. The WASM cron
trigger reads it via `{secret:llm_api_key}` template resolution.

### 11. Multi-turn Memory

The sidecar is stateless. Every turn re-reads the full SessionEvent
feed:

```
Turn 1: events = [user.message₁] → messages = [user₁]
Turn 2: events = [..., agent.message₁, ..., user.message₂]
         → messages = [user₁, assistant₁, user₂]
```

No process state survives. If the sidecar crashes between turns, the
next invocation reads from the event feed and continues.

---

## Component Summary

| Component | Language | Role |
| --------- | -------- | ---- |
| `temper serve` | Rust | OData server, entity actors, WASM dispatch |
| `crucible-chat` | Rust | Sidecar: seed/send/watch/interrupt, LLM calls, tool execution |
| `modal_bridge/server.py` | Python | Tool server (Local + Modal), sandbox provisioning |
| `wasm/crucible_cron_trigger` | Rust→WASM | Posts user.message on cron trigger |
| `wasm/crucible_scheduler_check` | Rust→WASM | Queries active schedules, fires due ones |
| `wasm/crucible_scheduler_heartbeat` | Rust→WASM | Heartbeat wait + re-trigger |

---

## Data Flow: Agent Turn

```
1. Client: crucible-chat send <session-id> "question"
   → POST user.message event to temper serve

2. Sidecar: crucible-chat watch <session-id>
   → poll: detects pending user.message
   → POST ResumeSession (Idle → Running)
   → POST span.model_request_start
   → call LLM (Fireworks /v1/chat/completions)
   → POST agent.message
   → POST span.model_request_end
   → POST session.status_idle
   → POST IdleSession (Running → Idle)
```

## Data Flow: Cron-Triggered Turn

```
1. CrucibleScheduler heartbeat fires
   → crucible_scheduler_check WASM queries active schedules
   → dispatches SessionSchedule.Trigger

2. crucible_cron_trigger WASM
   → template substitution
   → POST user.message to session

3. Sidecar picks up the user.message
   → same flow as above (step 2 of Agent Turn)
```
