# AgentOS — Actor Model for LLM Agents

## The Gap: What `main` Has vs What AgentOS Needs

On `main`, the EntityActor is a **pure state machine evaluator**:

```
Request → evaluate_ctx(state, ctx, action) → TransitionResult → Persist
```

It has **zero LLM code**. No inference calls, no tool dispatch, no agentic loop.
The actor only evaluates guards, applies effects, and stores events.

AgentOS needs an actor that runs an **agentic loop**:

```
While conversation != end:
    resp = invoke_llm(messages, tools)
    for tool in resp.tool_calls:
        result = dispatch_tool(tool)
        append_to_conversation(result)
    if resp.is_final:
        return resp.answer
```

## What Exists (on branch `bf54c6cd`, not merged to `main`)

The AgentOS implementation lives in a separate branch with **two new layers** built on top of the existing actor model:

### Layer 1: `temper-agentos` crate (kernel)

A full OS-inspired kernel with:

```
┌─────────────────────────────────────────────────┐
│  temper-agentos (kernel crate)                   │
│                                                   │
│  ProcessTable ─── ProcessEntry (PCB) per agent   │
│      │              state, pid, parent, children  │
│      │              capabilities, handles, budget │
│      │              signal_inbox, vruntime        │
│      │                                            │
│  Syscall ─── 33 syscalls (numbered 0-32)          │
│      │       Spawn, Exit, Kill, Wait              │
│      │       LlmInvoke, ToolCall                  │
│      │       OpenHandle, CloseHandle              │
│      │       Read, Write                          │
│      │       MemSet/Get/Delete/List               │
│      │       Signal, SigHandle, Poll              │
│      │       YieldNow, SetPriority, TransferBudget│
│      │                                            │
│  SyscallDispatch ─── routes syscall → kernel      │
│      │  1. Identify caller (AgentPid)             │
│      │  2. Capability check (Cedar)               │
│      │  3. Validate args                          │
│      │  4. Execute                                │
│      │  5. Return result + emit WideEvent         │
│      │                                            │
│  SignalBus ─── pub/sub event loop                 │
│      │  SIGTERM, SIGINT, SIGSTOP/SIGCONT          │
│      │  SIGTOOL, SIGLLM, SIGCHILD                 │
│      │  Buffered inbox per process                │
│      │                                            │
│  IntegrationDriver trait                          │
│      │  fn dispatch(tool_name, input, tenant)     │
│      │  → ToolResult { success, data, error }     │
│      │  Implementations: Datadog, Auth/OAuth      │
│      │                                            │
│  DriverRegistry ─── tool_name → driver lookup     │
│                                                   │
└─────────────────────────────────────────────────┘
```

### Layer 2: `temper-server` additions (agentic loop + scheduler)

```
┌─────────────────────────────────────────────────┐
│  temper-server (additions on AgentOS branch)      │
│                                                   │
│  agentic_loop/mod.rs                              │
│      Parses Anthropic API responses               │
│      Returns NextAction:                          │
│        • ToolCall { tool_use_id, name, input }    │
│        • Complete { answer }                      │
│        • Continue (hit max_tokens)                │
│      Builds messages[] for Anthropic API          │
│      Manages conversation history                 │
│      Normalizes tool names (: → _)                │
│                                                   │
│  state/process_scheduler.rs                       │
│      Priority queues (P0-P3)                      │
│      Fair-share scheduling (CFS-inspired vruntime)│
│      Global + per-driver concurrency caps         │
│      Budget tracking (tokens + turns)             │
│      Slot model: release on block                 │
│                                                   │
└─────────────────────────────────────────────────┘
```

## How the Pieces Connect

The trick: **the Process IS a Temper entity**. It uses the same `EntityActor` + `TransitionTable` as any other entity (Order, Ticket, etc). The IOA spec for a Process defines the states and transitions. The agentic loop is the **integration layer** that drives those transitions from outside.

```
                     EntityActor (same generic actor as always)
                           │
                           │ TransitionTable from process.ioa.toml
                           │ States: Created, Ready, Running,
                           │         BlockedInference, BlockedToolCall,
                           │         BlockedChild, Terminated, ...
                           │
   ┌───────────────────────┼───────────────────────────┐
   │                       │                           │
   ▼                       ▼                           ▼
StartProcess          ContinueWithInference     ContinueWithToolCall
(Ready → Running)     (Ready → BlockedInf)      (Ready → BlockedTool)
                           │                           │
                     ┌─────┘                     ┌─────┘
                     │                           │
            ┌────────▼──────────┐       ┌────────▼──────────┐
            │ Integration layer │       │ Integration layer │
            │                   │       │                   │
            │ POST anthropic.com│       │ driver.dispatch() │
            │ → parse response  │       │ → get tool result │
            │ → agentic_loop    │       │                   │
            │   parse_response()│       │                   │
            └────────┬──────────┘       └────────┬──────────┘
                     │                           │
                     ▼                           ▼
            CompleteInference             CompleteToolCall
            (BlockedInf → Ready)          (BlockedTool → Ready)
                     │                           │
                     └─────────┬─────────────────┘
                               │
                    scheduler picks again
                               │
                               ▼
                    NextAction from agentic_loop:
                    ├── ToolCall → ContinueWithToolCall
                    ├── Continue → ContinueWithInference
                    └── Complete → CompleteProcess (→ Terminated)
```

**Each step of the agentic loop = one state transition** on the Process entity actor.

The loop isn't a code `while` loop. It's a sequence of transitions:
```
StartProcess → ContinueWithInference → CompleteInference
  → ContinueWithToolCall → CompleteToolCall
  → ContinueWithInference → CompleteInference
  → CompleteProcess
```

All persisted to Postgres via event sourcing. All replayable. All observable.

## What the ACTOR-LLM Diagram Maps To

```
ACTOR-LLM.md Diagram              Implementation
═══════════════════                ═══════════════

Top: Spawn/Wait/Kill           →   Syscalls 0-3 (in temper-agentos/syscall.rs)
    spawn()                         Syscall::Spawn(SpawnArgs)
    wait()                          Syscall::Wait(WaitArgs)
    kill()                          Syscall::Kill(KillArgs)
    results()?                      Syscall::Read / ProcessStatus
    Healthcheck()                   Syscall::Status

Middle: Admission/Scheduler    →   ProcessScheduler (process_scheduler.rs)
    budgets                         token_budget + turn_budget
    rate limits                     per-driver concurrency caps
                                    fair-share (vruntime)

Actor: while loop              →   Process entity actor + agentic_loop
    invoke_llm()                    ContinueWithInference action
                                    → Anthropic API call
                                    → parse_response() → NextAction
    for t in resp.tools:            ContinueWithToolCall action
      call_t()                      → IntegrationDriver.dispatch()

Bottom: Driver layer           →   Integration drivers (temper-agentos/drivers/)
    BASH, READ, WRITE               Env driver (shell, docker, k8s)
    search_logs                     Datadog driver (dd:logs:search)
    DD MCP                          Tool driver (MCP)
    Sandbox                         Env driver
    VFS                             Storage driver
    codegen, AgentFS                Custom drivers
```

## What's NOT on `main` Yet

| Component | Status | Location |
|-----------|--------|----------|
| Process entity spec (IOA) | ✅ Exists | On branch, not merged |
| ProcessTable (kernel PCB) | ✅ Exists | `temper-agentos/src/process_table.rs` |
| 33 Syscalls | ✅ Exists | `temper-agentos/src/syscall.rs` |
| SyscallDispatch | ✅ Exists | `temper-agentos/src/dispatch.rs` |
| SignalBus | ✅ Exists | `temper-agentos/src/signal_bus.rs` |
| Integration drivers | ✅ Exists | `temper-agentos/src/drivers/` |
| Agentic loop (response parser) | ✅ Exists | `temper-server/src/agentic_loop/mod.rs` |
| ProcessScheduler | ✅ Exists | `temper-server/src/state/process_scheduler.rs` |
| Generic EntityActor | ✅ On main | Works for any entity including Process |
| Verification cascade | ✅ On main | Can verify Process IOA spec too |
| Claude API client | ✅ On main | `temper-platform/src/agent/claude.rs` |

## Key Insight: Process = Entity

The Process doesn't need a special actor. It uses the same `EntityActor` that Order, Ticket, etc. use. What makes it special is:

1. **The IOA spec** — defines states like `BlockedInference`, `BlockedToolCall`, transitions like `ContinueWithInference`, `SpawnChild`
2. **The integration layer** — hooks that fire on specific transitions and actually call the LLM / tools / spawn children
3. **The scheduler** — gates admission for inference/tool transitions based on priority, budget, and concurrency

The entity actor stays generic. The intelligence is in the spec + the integration layer.

## Current `main` Limitation

On `main`, the actor can only do:
- Pure state transitions (guards + effects)
- Webhooks (outbound HTTP on transition)
- WASM modules (HTTP calls, secrets, logging)
- Custom effects (post-transition hooks)
- SpawnEntity / ScheduleAction

It **cannot**:
- Call an LLM and parse the response
- Drive a multi-turn agentic loop
- Manage conversation context
- Enforce scheduling/budgets for concurrent agents

All of that exists on the AgentOS branch but hasn't been merged.

## Who Actually Calls What (the runtime call chain)

The agentic loop module (`agentic_loop/mod.rs`) is **pure functions** — it parses JSON,
builds arrays, normalizes names. Zero IO. Zero side effects. It's the brain, but
**`ServerState` is the body** that actually makes HTTP calls and drives the loop.

### Who calls the LLM?

**`ServerState::dispatch_anthropic_integration()`** in `dispatch/anthropic.rs`:

```rust
// dispatch/anthropic.rs lines 182-190
let response = reqwest::Client::new()
    .post(&api_url)                          // "https://api.anthropic.com/v1/messages"
    .header("x-api-key", api_key)
    .header("anthropic-version", anthropic_version)
    .timeout(Duration::from_secs(timeout_seconds))
    .json(&payload)                          // messages + tools + model + system
    .send()
    .await;
```

Triggered by the IOA spec's `[[integration]] type = "anthropic"` when a transition
fires `trigger run_anthropic` (i.e. `ContinueWithInference`).

### Who calls the tools?

**`ServerState::dispatch_tool_integration()`** in `dispatch/datadog.rs`, with a
**three-tier dispatch**:

```
1. WASM tool?     → self.invoke_wasm_module_direct()              (line 106)
2. Builtin syscall? → self.dispatch_builtin_syscall_tool()        (line 132)
   └── sys_spawn_child, sys_wait_child, sys_spawn_and_wait_child
3. Registry tool?  →                                              (lines 136-179)
   tool_registry.get(&tool_name)         // find which driver owns it
   driver_registry.get(&tool_entry.driver) // get the driver instance
   driver.dispatch(&tool_name, &tool_input, &tenant)  // DatadogDriver, etc.
```

Triggered by the IOA spec's `[[integration]] type = "tool"` when a transition
fires `trigger run_tool` (i.e. `ContinueWithToolCall`).

### Who stitches the loop together?

Two methods on ServerState form the loop glue:

**After every LLM response** — `chain_agentic_next_action()` (`dispatch/anthropic.rs`):
```
1. Append assistant turn to ConversationHistory (entity field update via actor ask)
2. agentic_loop::parse_response() → NextAction
3. match next {
     ToolCall  → enqueue ContinueWithToolCall    (goes through scheduler)
     Complete  → dispatch CompleteProcess          (entity → Terminated)
     Continue  → enqueue ContinueWithInference    (hit max_tokens, retry)
   }
```

**After every tool completion** — `chain_tool_result_to_inference()` (`dispatch/datadog.rs`):
```
1. Read current ConversationHistory from entity actor (GetState)
2. agentic_loop::append_tool_result() → add tool_result to history
3. Persist updated history back to entity actor (UpdateFields)
4. enqueue ContinueWithInference → fires run_anthropic again
```

### Full call chain for one loop iteration

```
IOA spec: ContinueWithInference fires "trigger run_anthropic"
    │
    ▼
ServerState::dispatch_integration()            ← generic router
    │ sees type = "anthropic"
    ▼
ServerState::dispatch_anthropic_integration()  ← CALLS THE LLM (reqwest POST)
    │ response arrives
    ├── agentic_loop::build_messages()         ← pure: reads entity fields
    ├── agentic_loop::build_tools()            ← pure: normalizes tool names
    │
    ▼
ServerState::chain_agentic_next_action()       ← THE DECISION POINT
    │ agentic_loop::append_assistant_turn()    ← pure: adds to history vec
    │ agentic_loop::parse_response()           ← pure: inspects stop_reason
    │   → NextAction::ToolCall
    ▼
enqueue ContinueWithToolCall                   ← goes through ProcessScheduler
    │
    ▼
IOA spec: ContinueWithToolCall fires "trigger run_tool"
    │
    ▼
ServerState::dispatch_tool_integration()       ← CALLS THE TOOL (driver)
    │ tool result arrives
    ▼
ServerState::chain_tool_result_to_inference()  ← LOOP BACK
    │ agentic_loop::append_tool_result()       ← pure: adds tool_result to history
    │ persist updated ConversationHistory       ← entity actor UpdateFields
    ▼
enqueue ContinueWithInference                  ← fires run_anthropic again
    │
    ▼
... (repeat until NextAction::Complete → CompleteProcess → Terminated)
```

### The two independent LLM call sites (not connected)

There are **two separate systems** that can call the Anthropic API:

| System | Location | Used by |
|--------|----------|---------|
| **ServerState** (runtime) | `dispatch/anthropic.rs` | Entity actor integration pipeline (the real loop) |
| **SyscallDispatch** (kernel) | `temper-agentos/src/dispatch.rs:invoke_anthropic_sync()` | Kernel tests, in-process syscall path |

They are **not connected** to each other. The kernel's `invoke_anthropic_sync()` makes
its own `reqwest::Client::new().post("https://api.anthropic.com/v1/messages")` call
directly inside the syscall handler. It blocks on a spawned thread and returns
`LlmResponseInfo` synchronously.

The ServerState path goes through the integration dispatch pipeline with authz gates,
secrets vault, wide event telemetry, invocation logging, and conversation management.

This is the main architectural gap on this branch: the kernel-level syscall path and
the server-level integration path do the same thing but aren't unified.

## Design Asymmetry: LLM Calls Are Not Drivers

Tools go through the `IntegrationDriver` trait:

```
ToolRegistry.get(name) → ToolEntry { driver: "datadog" }
DriverRegistry.get("datadog") → DatadogDriver
DatadogDriver.dispatch(name, input, tenant) → ToolResult
```

But the Anthropic call is **hardcoded inline** — `reqwest::Client::new().post(api_url)`
directly in `dispatch_anthropic_integration()`. There is no `LlmDriver` trait, no
`AnthropicDriver` struct, no registry lookup. The integration dispatch router has a
hardcoded match:

```rust
// dispatch/mod.rs line 220
match integration.integration_type.as_str() {
    "anthropic" => dispatch_anthropic_integration(...)   // SPECIAL-CASED
    "tool"      => dispatch_tool_integration(...)        // goes through DriverRegistry
    "wasm"      => dispatch_wasm_integration(...)        // SPECIAL-CASED
    other       => warn!("unsupported")
}
```

Conceptually the LLM call **should** be a driver — it takes input (messages + tools),
makes an HTTP call, returns structured output. Same pattern as tools. But it's
special-cased because it needs conversation history management, next-action chaining,
and tool schema normalization — none of which is inherent to the transport. A clean
version would be:

```
IntegrationDriver trait
├── DatadogDriver    (tools — dd:logs:search etc.)
├── AnthropicDriver  (LLM — messages API)     ← doesn't exist yet
├── OpenAIDriver     (LLM — chat completions) ← doesn't exist yet
└── EnvDriver        (shell, docker, k8s)     ← doesn't exist yet
```

With the loop glue living in the dispatch router, not in the driver.

## Where Tools Come From (AvailableTools)

Tools available to a process are **pre-defined data**, not discovered at runtime.
Three sources:

### 1. Tool entities (per-program) — `collect_program_tools()` in `schedule_runtime.rs`

```rust
// For each Tool entity where ProgramId matches and status == "Registered":
tools.push(json!({
    "name":         state.fields["ToolName"],
    "description":  state.fields["Description"],
    "input_schema": state.fields["SchemaJson"],
}));
```

These Tool entities are created via the OData API — by the dev-time conversation
that generates specs. They're persisted before any runtime process starts.

### 2. Action params (passed at spawn)

When a parent spawns a child, it can pass `ChildAvailableTools`:
```rust
// dispatch/actions.rs line 949
if let Some(tools) = action_params.get("ChildAvailableTools") {
    start_params.insert("AvailableTools", tools.clone());
}
```

### 3. Tool manifests (static config) — `manifest.toml` files

```toml
[integration]
name = "datadog"
driver = "datadog"

[[tool]]
name = "dd:logs:search"
description = "Search logs"
input_schema = '{"type": "object", "properties": {"query": {"type": "string"}}}'
```

### Summary

**Dev-time LLM defines the tool menu. Runtime LLM orders from it. The menu is frozen
before the process starts.** The runtime LLM never negotiates "I want a new tool" — it
picks from `AvailableTools` as pre-loaded entity state. The only exception is
`sys_spawn_child`, which can pass a subset of tools to a child, but still from the
existing set.

## Self-Hosting: AgentOS Is a Temper App

The `process.ioa.toml` and all 14 specs in `reference-apps/agentos/specs/` were
**generated through the Temper conversational pipeline** — the same way the ecommerce
Order spec was. AgentOS is the Temper platform describing itself as a Temper app.

### Evidence

- **Lives in `reference-apps/`, not `crates/`** — same directory structure as
  ecommerce, oncall, weather-tracker demo apps
- **Same spec structure** as any conversation-generated app: `[automaton]`, `[[state]]`,
  `[[action]]` with from/to/guard/effect/hint, `[[invariant]]`, `[[integration]]`
- **`hint` fields on every action** — natural language descriptions for the LLM to
  understand itself (e.g. `hint = "Process called llm_invoke. Dispatches to LLM driver."`)
- **957-line `model.csdl.xml`** — full OData schema with 13 entity types, navigation
  properties, bound actions, StateMachine annotations. Not hand-written.
- **All 14 specs dropped in a single commit** (`8d7d14c`) — characteristic of LLM
  generation, not incremental human authoring

### What was generated vs what was hand-written

```
GENERATED FROM CONVERSATION (via Temper pipeline):
├── reference-apps/agentos/specs/
│   ├── process.ioa.toml       ← states, transitions, guards, effects, invariants
│   ├── program.ioa.toml
│   ├── tool.ioa.toml
│   ├── handle.ioa.toml
│   ├── capability_grant.ioa.toml
│   ├── memory_entry.ioa.toml
│   ├── shared_segment.ioa.toml
│   ├── driver_quota.ioa.toml
│   ├── driver_registration.ioa.toml
│   ├── policy_rule.ioa.toml
│   ├── program_schedule.ioa.toml
│   ├── topic.ioa.toml
│   ├── channel.ioa.toml
│   └── model.csdl.xml         ← OData schema for all 13 entity types
│
│   All runs on the EXISTING generic machinery on main:
│   EntityActor, TransitionTable, event sourcing, OData API,
│   verification cascade, persistence.

HAND-WRITTEN PLATFORM CODE (by rita-aga, likely AI-assisted):
├── dispatch/anthropic.rs       ← type = "anthropic" integration handler
├── dispatch/datadog.rs         ← type = "tool" integration handler
├── agentic_loop/mod.rs         ← conversation management (pure functions)
├── process_scheduler.rs        ← fair-share scheduling logic
└── temper-agentos/ (entire crate) ← kernel abstractions
```

### Why the hand-written code can't be generated from specs

Before this branch, the platform knew two integration types:
- `"webhook"` → HTTP POST to a URL (existed on main)
- `"wasm"` → execute a WASM module (existed on main)

That's why ecommerce Order works with zero new code:
```toml
# order.ioa.toml — uses only types the platform already knows
[[integration]]
type = "webhook"   ← platform handles this generically
```

AgentOS needed the platform to learn two new integration types:
```toml
# process.ioa.toml — needs types the platform didn't have
[[integration]]
type = "anthropic"  ← NEW: call an LLM, parse response, chain next action
[[integration]]
type = "tool"       ← NEW: look up driver, dispatch, chain back to inference
```

**No spec can teach the platform a new integration type.** That's a platform extension,
not an application. The spec defines WHAT the lifecycle looks like; someone had to write
HOW `type = "anthropic"` actually works.

### The generation boundary

```
Layer 3: Application specs ← GENERATED via conversation
  "Process has states Created/Ready/Running/Blocked*/Zombie/Terminated"
  "ContinueWithInference triggers run_anthropic"
  These are data. Loadable, verifiable, regenerable.

Layer 2: Integration type handlers ← HAND-WRITTEN platform code
  "When type = 'anthropic', POST to api.anthropic.com, parse response,
   chain next action based on stop_reason"
  Must be written in Rust, compiled, deployed.

Layer 1: Generic entity runtime ← ALREADY ON MAIN
  TransitionTable evaluation, guards, effects, event sourcing,
  OData API, persistence, verification cascade.
  Runs any spec, including Process.
```

The Order app is 100% spec-driven. AgentOS is ~70% spec-driven, ~30% new platform
code. That 30% is precisely the files we've been reading in this research.
