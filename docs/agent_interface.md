# Lassie-ng Agent Interface

> **Canonical API Direction (Temper service)**: APIs are CSDL-based and exposed via `/tdata`.
> All interface examples below use CSDL entity sets, OData operations, and bound actions.

> **Status**: Draft — for team discussion
> **Authors**: Arun Parthiban, Gabriele Baldoni
> **Last updated**: 2026-03-11

## Scope

This document defines the **programming interface** that lassie-ng exposes in two directions:

1. **Outward** — to programs that create, configure, and manage agents (the OData control surface)
2. **Inward** — to running agents that need to interact with the platform (the "syscall" surface)

Everything below is a contract. Internals (scheduler, signal bus, process table) can change; these interfaces stay stable.

---

## 1. Agent Definition (The "Container Image")

An agent is defined by its configuration — not by code. This is the static artifact that describes *what* an agent is before it runs.

### 1.1 Create Agent Definition

Agent definitions are CSDL entities. Creation happens via the `AgentDefinitions` entity set.

```
POST /tdata/AgentDefinitions
```

```json
{
  "id": "ci-fixer-001",
  "name": "ci-fixer",
  "system_prompt": "You are a CI failure analyst. You diagnose test failures, identify root causes, and propose fixes.",
  "model_provider": "anthropic",
  "model_name": "claude-sonnet-4-6",
  "model_max_tokens": 8192,
  "tools_json": "[\"datadog_logs_search\",\"bash\",\"github_get_pr\"]",
  "labels_json": "{\"team\":\"ci-platform\"}",
  "created_at": "2026-03-11T10:00:00Z",
  "updated_at": "2026-03-11T10:00:00Z",
  "status": "ready"
}
```

Response (`201 Created`):

```json
{
  "@odata.context": "$metadata#AgentDefinitions/$entity",
  "id": "ci-fixer-001",
  "status": "ready",
  "created_at": "2026-03-11T10:00:00Z"
}
```

Read/update/delete use standard OData entity paths:

```
GET    /tdata/AgentDefinitions('ci-fixer-001')
PATCH  /tdata/AgentDefinitions('ci-fixer-001')
DELETE /tdata/AgentDefinitions('ci-fixer-001')
GET    /tdata/AgentDefinitions
```

### 1.2 Program Definition (WASM Modules)

Not all agents are LLM-driven. A **program** is a WASM module that runs as a process and makes syscalls through host functions — the same way temper's WASM SDK provides `host_http_call`, `host_get_secret`, etc., but extended with the full syscall surface.

```
POST /tdata/ProgramDefinitions
```

```json
{
  "id": "log-roller-001",
  "name": "log-roller",
  "module_uri": "s3://programs/log-roller-v3.wasm",
  "tools_json": "[\"datadog_logs_search\",\"datadog_metrics_query\"]",
  "labels_json": "{\"team\":\"ci-platform\"}",
  "created_at": "2026-03-11T10:00:00Z",
  "updated_at": "2026-03-11T10:00:00Z"
}
```

The WASM module uses the guest SDK to make syscalls:

```rust
use lassie_wasm_sdk::*;

lassie_module! {
    fn run(ctx: Context) -> Result<Value> {
        // tool_call — dispatched by the kernel, respects governance
        let logs = ctx.tool_call("datadog_logs_search", json!({
            "query": "service:caniche status:error",
            "from": "now-30m"
        }))?;

        // spawn a child agent (LLM-driven) and wait for its result
        let child = ctx.spawn("ci-fixer-001", json!({
            "user_prompt": "Summarize these errors"
        }))?;
        let result = ctx.wait(child.pid, 120)?;

        // signal — emit to the bus
        ctx.signal("ci_errors_summarized", json!({
            "summary": result.output
        }))?;

        Ok(json!({ "status": "done", "summary": result.output }))
    }
}
```

The host functions available to WASM programs (the "syscall ABI"):

| Host Function | Maps to Syscall | Description |
|---|---|---|
| `host_tool_call(name, input) → output` | `tool_call` | Invoke a tool through the kernel |
| `host_spawn(agent_id, config, prompt) → pid` | `spawn` | Create a child process (agent or program) |
| `host_wait(pid, timeout) → result` | `wait` | Block until child terminates |
| `host_spawn_and_wait(agent_id, config, prompt, timeout) → result` | `spawn_and_wait` | Spawn + wait in one call |
| `host_send(pid, message)` | `send` | Send message to another process |
| `host_recv(timeout) → message` | `recv` | Block until a message arrives |
| `host_signal(topic, payload)` | `signal` | Emit a signal on the bus |
| `host_subscribe(topic) → handle` | `subscribe` | Subscribe to a signal topic |
| `host_poll(handles, timeout) → event` | `poll` | Wait on any of N handles |
| `host_get_secret(key) → value` | — | Read a secret (resolved per-tenant) |
| `host_log(level, message)` | — | Structured logging through the host |

This is a direct extension of temper's existing WASM host ABI (`host_http_call`, `host_get_context`, `host_set_result`, `host_get_secret`, `host_log`) — same pattern, bigger surface.

### 1.3 Key Decisions

- **Declarative, not imperative.** You describe the agent/program, not how to run it.
- **Two kinds of process brain:** LLM (AgentDefinition) or code (ProgramDefinition via WASM). Both use the same syscall surface, same governance, same lifecycle.
- **Tools declare where they execute** (server, client, wasm) and fallback behavior.
- **Tool names are flat** — underscore-separated everywhere (`datadog_logs_search`, not `datadog.logs.search`). One canonical name used by the LLM, the API, policies, and WASM guest code.
- **Governance is part of the definition**, not bolted on after.
- **Triggers are first-class** — agents and programs aren't only request-response.

---

## 2. Agent Lifecycle API (CSDL `/tdata` Surface)

Programs that manage agents use OData entity operations and bound actions over `/tdata`.

### 2.1 CRUD Operations

```
# Agent definitions (LLM-driven)
POST   /tdata/AgentDefinitions
GET    /tdata/AgentDefinitions('ci-fixer-001')
PATCH  /tdata/AgentDefinitions('ci-fixer-001')
DELETE /tdata/AgentDefinitions('ci-fixer-001')
GET    /tdata/AgentDefinitions

# Program definitions (WASM-driven)
POST   /tdata/ProgramDefinitions
GET    /tdata/ProgramDefinitions('log-roller-001')
PATCH  /tdata/ProgramDefinitions('log-roller-001')
DELETE /tdata/ProgramDefinitions('log-roller-001')
GET    /tdata/ProgramDefinitions

# Processes (running instances — of either type)
POST   /tdata/Processes
GET    /tdata/Processes('proc-abc123')
PATCH  /tdata/Processes('proc-abc123')
DELETE /tdata/Processes('proc-abc123')         # terminate
GET    /tdata/Processes

# Query examples (OData)
GET /tdata/Processes?$filter=agent_id eq 'ci-fixer-001' and status eq 'running'
GET /tdata/Processes?$filter=program_id eq 'log-roller-001' and status eq 'running'
```

### 2.2 Process Control (Bound Actions)

All operations are **non-blocking**. Use the stream surface (§ 2.4) to observe progress.

```
# Start with a user prompt (LLM-driven processes)
POST /tdata/Processes('proc-abc123')/Temper.AgentV1.StartProcess
{
  "user_prompt": "Diagnose the CI failure on PR #4521"
}

# Start without prompt (WASM-driven processes)
POST /tdata/Processes('proc-prog-001')/Temper.AgentV1.StartProcess
{}

# Send user input to a running or blocked process
POST /tdata/Processes('proc-abc123')/Temper.AgentV1.SendInput
{
  "user_prompt": "Yes, go ahead and push the fix"
}

# Suspend the process mailbox
POST /tdata/Processes('proc-abc123')/Temper.AgentV1.SuspendProcess
{}

# Resume the process mailbox
POST /tdata/Processes('proc-abc123')/Temper.AgentV1.ResumeProcess
{}

# Terminate process with reason
POST /tdata/Processes('proc-abc123')/Temper.AgentV1.TerminateProcess
{
  "reason": "user_requested"
}
```

### 2.3 Process State (Read)

```
GET /tdata/Processes('proc-abc123')
```

```json
{
  "id": "proc-abc123",
  "agent_id": "ci-fixer-001",
  "status": "blocked_tool_call",
  "blocked_on_json": "{\"type\":\"approval\",\"tool\":\"bash\",\"input\":{\"command\":\"git push origin fix-branch\"}}",
  "turns": 12,
  "tokens_used": 34200,
  "tokens_remaining": 65800,
  "parent_pid": null,
  "children_json": "[\"proc-def456\"]",
  "started_at": "2026-03-11T10:00:00Z"
}
```

### 2.4 Streaming

For interactive use (CLI, GUI), subscribe to the process/event stream via the standard OData events endpoint:

```
GET /tdata/$events
Accept: text/event-stream
```

Clients filter events for the target process (`entity_type == "Process"`, `entity_id == "proc-abc123"`).

Example events:

```
data: {"entity_type":"Process","entity_id":"proc-abc123","action":"Started","status":"Running"}

data: {"entity_type":"Process","entity_id":"proc-abc123","action":"ToolCall","status":"BlockedToolCall"}

data: {"entity_type":"Process","entity_id":"proc-abc123","action":"Resumed","status":"Running"}

data: {"entity_type":"Process","entity_id":"proc-abc123","action":"Completed","status":"Terminated"}
```


---

## 3. Execution Model (Actor-Based)

Every process is an **actor**. The agentic loop is not a function that "drives" execution — it's a sequence of messages flowing through actor mailboxes.

### 3.1 Process as Actor

Each process is an actor in the temper actor system. It has:
- A **mailbox** (bounded mpsc channel, FIFO)
- **State** (entity state: status, events, counters, fields)
- A **transition table** (compiled from the process spec — guards and effects for each action)
- **Supervision** (restart with exponential backoff on failure)

The process actor handles messages one at a time. No concurrent access to state. This is the temper invariant.

### 3.2 The Agentic Loop as Messages

For an LLM-driven agent, the loop is a chain of actions flowing through the actor:

```
StartProcess (from API)
    │
    ▼
┌─────────────────────────────────────────────────────┐
│  Process Actor                                       │
│                                                      │
│  StartProcess                                        │
│    → sets status = Running                           │
│    → sends ContinueWithInference to self             │
│                                                      │
│  ContinueWithInference                               │
│    → sets status = BlockedInference                  │
│    → dispatches to LLM driver (Tell, async)          │
│    ... actor yields, processes other messages ...     │
│                                                      │
│  CompleteInference(response)        ◄── from LLM     │
│    → response has tool_use blocks?                   │
│      yes → sends ContinueWithToolCall to self        │
│      no  → sends CompleteProcess to self             │
│                                                      │
│  ContinueWithToolCall(tool, input)                   │
│    → sets status = BlockedToolCall                   │
│    → checks Cedar policy (allow/deny)                │
│    → if approval_required: status = BlockedApproval  │
│    → else: dispatches to tool driver (Tell, async)   │
│    ... actor yields ...                              │
│                                                      │
│  CompleteToolCall(result)           ◄── from driver   │
│    → appends result to context                       │
│    → sends ContinueWithInference to self             │
│                                                      │
│  CompleteProcess                                     │
│    → sets status = Terminated                        │
│    → notifies parent (if child process)              │
└─────────────────────────────────────────────────────┘
```

Every state transition is a message. Every message is an action evaluated against the transition table. Every action is persisted as an event. This is temper's model — the agentic loop is just another state machine.

### 3.3 For WASM Programs

A WASM program's loop is different — the code drives itself, but syscalls still flow through the actor system:

```
StartProcess (from API)
    │
    ▼
┌─────────────────────────────────────────────────────┐
│  Process Actor                                       │
│                                                      │
│  StartProcess                                        │
│    → sets status = Running                           │
│    → invokes WASM module via WasmEngine              │
│    → WASM guest calls host_tool_call(...)            │
│      → actor sends ToolCall to tool driver           │
│      → blocks WASM execution (fuel-metered)          │
│      → receives result, returns to WASM guest        │
│    → WASM guest calls host_spawn(...)                │
│      → actor sends SpawnChild                        │
│      → creates child actor                           │
│      → returns pid to WASM guest                     │
│    → WASM guest calls host_wait(pid, ...)            │
│      → actor sends BlockOnChild                      │
│      → status = BlockedChild                         │
│      → WASM execution suspended                      │
│      ... child runs to completion ...                │
│    → UnblockFromChild(result)                        │
│      → resumes WASM execution                        │
│      → returns result to WASM guest                  │
│    → WASM guest returns Ok(result)                   │
│      → actor sends CompleteProcess to self            │
│                                                      │
│  CompleteProcess                                     │
│    → sets status = Terminated                        │
└─────────────────────────────────────────────────────┘
```

The WASM module sees synchronous syscalls (`ctx.tool_call(...)` blocks and returns). Under the hood, each syscall becomes a message to the actor system. The actor suspends the WASM execution (fuel-metered via wasmtime), dispatches the work, and resumes the WASM when the result arrives.

### 3.4 Agent-to-Agent Communication

All inter-process communication is message-passing through actor mailboxes:

**Parent-child (spawn/wait):**
```
Parent Actor                         Child Actor
     │                                    │
     │ SpawnChild(agent_id, prompt)       │
     │ ──────────────────────────────►    │ (created)
     │                                    │
     │ BlockOnChild(child_pid)            │
     │ status = BlockedChild              │ (running...)
     │                                    │
     │                                    │ CompleteProcess
     │     UnblockFromChild(result)       │
     │ ◄──────────────────────────────    │ (terminated)
     │ status = Ready                     │
     │ result delivered as tool_result    │
```

**Async messaging (send/recv):**
```
Actor A                              Actor B
     │                                    │
     │ send(B.pid, message)               │
     │ ──────────Tell──────────────►      │
     │                                    │ (message lands in B's mailbox)
     │                                    │
     │                                    │ recv()
     │                                    │ → dequeues message
     │                                    │ → for LLM agents: injected into
     │                                    │   context as tool_result on next turn
```

**`wait` vs `poll`:**
- `wait(pid)` — blocks on **one specific child**. Like `waitpid(pid)` — the actor enters `BlockedChild` until that child terminates.
- `poll(handles[])` — blocks on **any of N handles** (child pids, signal subscriptions, recv inbox). The actor enters `BlockedPoll` and resumes when the first handle fires. Like `epoll` — you multiplex.

---

## 4. Syscall Interface (The "Kernel ABI")

These are the operations available to agent logic. For LLM-driven agents, they appear as tools (the LLM emits tool_use blocks). For WASM programs, they're host functions. Both map to the same underlying actor messages.

### 4.1 Core Syscalls

| Syscall | Signature | Actor Message | Description |
|---|---|---|---|
| `tool_call` | `(tool_name, input) → output` | `ContinueWithToolCall` → `CompleteToolCall` | Invoke a tool. Kernel handles routing, auth, execution environment. |
| `yield` | `() → ()` | `Yield` | Voluntarily give up execution turn. Scheduler re-enqueues. |
| `terminate` | `(reason) → !` | `Terminate` | End this process. |

Note: `llm_invoke` is intentionally absent. It is an internal actor message (`ContinueWithInference`), not a syscall. The kernel sends it; agents do not.

### 4.2 Agent-to-Agent Syscalls

| Syscall | Signature | Actor Messages | Description |
|---|---|---|---|
| `spawn` | `(definition_id, config, user_prompt?) → pid` | `SpawnChild` | Create a child process actor. Returns immediately. |
| `wait` | `(pid, timeout?) → result` | `BlockOnChild` → `UnblockFromChild` | Block until specific child terminates. |
| `spawn_and_wait` | `(definition_id, config, user_prompt, timeout?) → result` | `SpawnChild` + `BlockOnChild` → `UnblockFromChild` | Convenience: spawn + wait. |
| `send` | `(pid, message) → ()` | `Tell` to target actor | Send message to another process. Non-blocking. |
| `recv` | `(timeout?) → message` | `BlockOnSignal` → `UnblockFromSignal` | Block until a message arrives in inbox. |

### 4.3 Signal Syscalls

| Syscall | Signature | Actor Messages | Description |
|---|---|---|---|
| `signal` | `(topic, payload) → ()` | `Tell` to signal bus actor | Emit a signal. |
| `subscribe` | `(topic) → handle` | `OpenHandle` | Subscribe to a signal topic. |
| `poll` | `(handles[], timeout?) → event` | `BlockOnSignal` → `UnblockFromSignal` | Wait on any of N handles. |

### 4.4 How LLM-Driven Agents Use Syscalls

The LLM sees syscalls as **tools** and emits tool_use blocks. The kernel maps them to actor messages:

```
LLM emits:                              Actor receives:
─────────                               ───────────────
tool_use: bash {"cmd": "ls"}        →   ContinueWithToolCall("bash", {"cmd": "ls"})
tool_use: sys_spawn_and_wait        →   SpawnChild + BlockOnChild
tool_use: datadog_logs_search       →   ContinueWithToolCall("datadog_logs_search", ...)
```

The LLM never sees: `ContinueWithInference`, `CompleteInference`, `Schedule`, `Admit`, `Preempt`, `Reap`. Those are kernel-internal actor messages.

---

## 5. Tool Registration Interface

Tools are registered with the platform. Agents and programs reference them by name; the platform handles routing, permissions, and credential resolution.

### 5.1 Permissions

Tool access is governed at two layers:

1. **Definition allowlist** — the `tools` array in AgentDefinition or ProgramDefinition. If a tool isn't listed, the process cannot use it.

2. **Cedar policies** — fine-grained rules evaluated at runtime on every `tool_call`. Policies can gate on process identity, tool name, input parameters, parent workflow, etc.

```
// Cedar policy: ci-fixer can search logs but cannot delete
permit(
  principal == Agent::"ci-fixer-001",
  action == Action::"tool_call",
  resource == Tool::"datadog_logs_search"
);

forbid(
  principal == Agent::"ci-fixer-001",
  action == Action::"tool_call",
  resource == Tool::"datadog_logs_delete"
);
```

When a tool call is denied, the process actor transitions to `BlockedPolicy` and the denial is recorded in the event log. Depending on governance config, it may surface for human approval or terminate the process.

### 5.2 Credential Scoping

Credentials are **scoped to the user/tenant, not the agent**:

- Definitions reference credentials symbolically: `secret://github/token`
- At runtime, the kernel resolves this to the **specific user's** token based on tenant context
- Different users running the same definition get their own credentials
- Agents and programs never see raw credentials — the kernel injects them at dispatch time

```
Definition references:               Runtime resolution:
──────────────────────               ───────────────────
secret://github/token            →   user:arun's GitHub PAT (tenant: dd-corp)
secret://datadog/api_key         →   org:12345's DD API key (tenant: dd-corp)
```

For WASM programs, this works through `host_get_secret(key)` — the same pattern as temper's existing `host_get_secret` in the WASM SDK, resolved per-tenant by the host.

### 5.3 Built-in Tools (First-Party)

Registered by the platform. Always server-side. Credentials resolved from tenant context.

Examples: `datadog_logs_search`, `datadog_metrics_query`.

### 5.4 MCP Server Tools

```
POST /tdata/ToolServers
```

```json
{
  "id": "github-mcp",
  "type": "mcp",
  "transport": "stdio",
  "command": ["github-mcp-server"],
  "env": {
    "GITHUB_TOKEN": "secret://github/token"
  },
  "tools": ["get_pr", "create_comment", "get_check_runs"],
  "execution": "server"
}
```

### 5.5 Client-Side Tools

```json
{
  "id": "shell-tools",
  "type": "native",
  "execution": "client",
  "requires_capabilities": ["shell", "filesystem"],
  "tools": [
    { "name": "bash", "approval_required": true },
    { "name": "read_file" },
    { "name": "write_file", "approval_required": true }
  ]
}
```

### 5.6 WASM Extension Tools

Distinct from WASM programs (§ 1.2). Extension tools are stateless handlers invoked per-call — the same model as temper's integration modules. Programs are long-lived processes with their own loop.

```json
{
  "id": "custom-lint",
  "type": "wasm",
  "module": "s3://extensions/lint-checker-v2.wasm",
  "execution": "wasm_sandbox",
  "capabilities": {
    "network": false,
    "filesystem": "virtual",
    "max_memory_mb": 64,
    "max_execution_ms": 5000
  },
  "tools": [
    { "name": "lint_check", "input_schema": {} }
  ]
}
```

---

## 6. Key Interface Principles

1. **Agents are configuration, not code.** The loop is commodity. What varies is prompt + tools + governance. Programs are code (WASM), but they use the same syscall surface and governance.

2. **Everything is an actor.** Each process is an actor with a mailbox. The agentic loop is a chain of messages (`ContinueWithInference` → `CompleteInference` → `ContinueWithToolCall` → ...). State transitions are evaluated against a transition table and persisted as events.

3. **Two kinds of process brain.** LLM (AgentDefinition) or code (ProgramDefinition via WASM). Both produce processes. Both use the same syscalls, governance, lifecycle, and actor model.

4. **Syscalls are the stability contract.** Everything above (SDK, REST API) can evolve. The syscall semantics — and their mapping to actor messages — are the invariant.

5. **Tool names are flat.** Underscore-separated, everywhere. One canonical name for the LLM, the API, policies, and WASM guest code.

6. **Governance is declarative and platform-enforced.** Cedar policies + approval gates + token budgets enforced at the syscall boundary (i.e., before the actor dispatches the tool call).

7. **Credentials belong to users, not processes.** Definitions reference credentials symbolically. The kernel resolves them per-user/tenant at runtime.

8. **Streaming is first-class.** Every interface supports event streams. All process control operations are non-blocking.

9. **Agent-to-agent is message-passing.** `spawn`/`wait` for parent-child. `send`/`recv` for async. All routed through actor mailboxes.

---

## 7. Open Questions

1. **SDK priority** — Rust and Go. Rust for the kernel/runtime and WASM guest SDK. Go for CSDL authoring, OData client tooling, and CLI flows?

2. **`recv` semantics for LLM-driven agents** — When a message arrives via `send`, the kernel injects it into the LLM's context on the next turn. But what if the LLM is mid-turn? Queue and deliver at turn boundary, or interrupt?

3. **Budget enforcement granularity** — Token budgets are per-process today. Should they also be per-definition (across all processes), per-user, per-tenant?

4. **Child process credential inheritance** — When a parent spawns a child, does the child inherit the parent's credential scope, or resolve independently from tenant context?

5. **WASM program blocking semantics** — When a WASM program calls `host_wait(pid, timeout)`, the WASM execution must suspend. Temper today uses `block_in_place` for `host_http_call`. For longer waits (child agent running for minutes), do we suspend the WASM Store and resume later, or keep the thread blocked? Suspending is more efficient but requires Store serialization.
