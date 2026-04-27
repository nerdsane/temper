# AgentOS, Lassie-ng, and Temper: How They Fit Together

> **Authors**: Gabriele Baldoni, Arun Parthiban
> **Status**: Living document — last updated 2026-03-17
> **Audience**: Anyone wondering "what is what" in the Datadog agentic platform

---

## TL;DR

```
┌─────────────────────────────────────────────────────────────────┐
│  DogSled, Incident Investigator, CI Fixer, etc.                  │
│  (Workflows / Harnesses — applications built on the platform)   │
├─────────────────────────────────────────────────────────────────┤
│  AgentOS                                                         │
│  (Building blocks: agent definitions, tools, memory, triggers,  │
│   governance, scheduling, multi-agent orchestration)             │
├─────────────────────────────────────────────────────────────────┤
│  Lassie-ng (Control Plane) + Temper (Runtime)                    │
│  (HTTP API, auth, persistence,   (Process execution, state      │
│   LLM calls, tool dispatch,       machine, scheduling, budget,  │
│   streaming, observability)        formal verification, Cedar)   │
└─────────────────────────────────────────────────────────────────┘
```

**Temper** is the runtime engine (like containerd in K8s).
**Lassie-ng** is the control plane (like the K8s API server).
**AgentOS** is the platform built on top (like the K8s ecosystem).
**Harnesses** (DogSled, etc.) are applications that run on AgentOS.

---

## The Components

### Temper — The Runtime Engine

**What it is**: A Rust actor system with formal state machines (I/O Automata).

**What it does**: Runs agent processes. Every agent interaction — from a simple
"what's the CPU?" chat to a multi-agent background investigation — runs as a
Temper Process with defined states, transitions, and invariants.

**K8s analogy**: Container runtime (like containerd/CRI-O). You don't run
containers directly on the host — everything goes through the container runtime.
Same here: every agent loop goes through Temper.

**Key capabilities**:
- Process state machine (`process.ioa.toml`): Created → Ready → BlockedInference → Ready → ...
- Formal verification (L0-L3 cascade) — every spec change is verified
- ProcessScheduler: fair-share, priority queues, budget enforcement
- Cedar governance: default-deny, tool permissions, approval flows
- Multi-agent: Spawn / Wait / Kill — parent spawns children, waits for results
- Block/resume: process survives disconnection, can resume from any state
- Event sourcing: full audit trail, replayable
- Pluggable context management: WASM-based strategies for teams
- Background execution: runs without a connected client

**What it does NOT do** (lassie-ng does these):
- HTTP endpoints, authentication, SSE streaming
- Tool discovery and MCP server management
- Persistence (messages, conversations, runs, steps → Postgres)
- Token counting, context window management

### Lassie-ng — The Control Plane

**What it is**: A Go service (Rapid framework) that implements the Letta-compatible
API for Bits CLI and other interfaces.

**What it does**: Everything that faces the outside world. HTTP endpoints, auth,
streaming, tool routing, persistence, observability.

> **Note**: LLM calls currently live in Temper (the `ProcessRuntime` calls
> AI Gateway directly via `AiGatewayClient`). The boundary between what Temper
> owns vs lassie-ng owns for LLM calls is still evolving — see the open question
> on callback direction below.

**K8s analogy**: The API server + etcd. Handles all external requests, stores
state, manages configuration.

**Key capabilities**:
- HTTP/SSE endpoint (Rapid framework, OrgStore Postgres)
- Authentication (AAA, DD API keys, OBO)
- Agent CRUD, memory blocks, tools, conversations
- LLM calls via AI Gateway (Anthropic, OpenAI, Gemini)
- Tool dispatch: server-side (MCP), client-side (CLI), approval flows
- Persistence: messages, runs, steps (append-only event log)
- Token counting, context window, compaction
- SSE streaming (real-time token delivery to connected clients)
- Observability (traces, metrics, logs)

### AgentOS — The Platform

**What it is**: The layer that provides building blocks for creating, running,
and orchestrating agents. It's built ON TOP of Temper + lassie-ng.

**What it does**: Defines the agent abstractions — how agents are configured,
how they communicate, how they're scheduled, how they're governed.

**K8s analogy**: The K8s ecosystem (Deployments, Services, RBAC, CRDs). Not the
underlying container runtime, but the platform primitives that make it useful.

**Key abstractions** (from `agent_interface.md`):
- **Agent Definitions**: configuration-based (LLM-driven) or code-based (WASM programs)
- **Processes**: running instances of agents (identified by pid)
- **Syscalls**: operations available to agents (`tool_call`, `spawn`, `wait`, `signal`, `poll`)
- **Tool system**: built-in, MCP servers, client-side, WASM extensions
- **Governance**: Cedar policies, capability grants, default-deny
- **Triggers**: cron, webhook, monitor alert, agent spawn
- **Signals**: pub/sub event bus for inter-agent communication

### Harnesses / Workflows — The Applications

**What they are**: Specific workflows built using AgentOS primitives.

**Examples**:
- **DogSled**: A user-facing way to build, configure, and run harnesses — the
  Datadog packaging of harness creation, configuration, observability, and execution
- **Incident Investigator**: Triggered by monitor alerts, investigates and reports
- **CI Fixer**: Watches CI, spawns agents to fix failures
- **Cost Analyzer**: Nightly cron, analyzes spend per team, Slacks results

A harness is an application that uses AgentOS to define and run a workflow.
AgentOS provides the building blocks; the harness is the specific assembly.

---

## FAQ: Common Points of Confusion

### "Is Temper the same as AgentOS?"

No. Temper is the **runtime engine** that AgentOS runs on. AgentOS defines the
agent abstractions (tools, memory, governance, scheduling). Temper executes
them as formal state machines with verification.

Think of it like: AgentOS is the operating system, Temper is the kernel.

### "Is DogSled a harness or a product?"

DogSled is a **harness** — a specific workflow application built on AgentOS.
It uses AgentOS primitives (agents, tools, triggers, multi-agent orchestration)
to implement a coding workflow.

"Harness" has two meanings in our context:
1. **Framework sense**: AgentOS is a harness framework — it provides the
   building blocks for any workflow
2. **Workflow sense**: DogSled is a harness instance — a specific workflow
   built using those building blocks

### "Where does Bits fit in?"

Bits (the CLI) is one of several **control interfaces** — a way for humans to
interact with agents. Others include Slack, Web UI, and the API directly.

The **memory system** (memory blocks, archival memory) is implemented in the
backend (lassie-ng today, but the memory primitives are pluggable in Temper —
how this evolves is an open question).

```
Control Interfaces:  Bits CLI, Slack, Web UI, Mobile, API
                         │
                         ▼
                    Lassie-ng (control plane)
                         │
                         ▼
                    Temper (runtime) → Process
```

### "Why can't lassie-ng just run agent loops itself?"

It can, and it does today (`messages.go` ~4200 lines). But the current approach
has limitations:

| Without Temper runtime | With Temper runtime |
|---|---|
| Loop = HTTP handler goroutine | Loop = persistent Process entity |
| Dies when request ends | Survives disconnection, restarts |
| No background execution | Foreground + background modes |
| No multi-agent spawning | Spawn / Wait / Kill |
| No formal verification | L0-L3 verification cascade |
| No Cedar governance | Default-deny, approval flows |
| Hard to debug loops | Event-sourced, replayable |
| Per-handler metrics only | Every transition is a span |

### "Who calls the LLM?"

> **Note**: This boundary is not settled. Currently Temper calls the LLM
> directly (via `AiGatewayClient` in `ProcessRuntime`). The TEMPER-LASSIE.md
> doc proposes Temper calling lassie-ng internal APIs instead. See the open
> question on callback direction below.

**Today**: Temper's `ProcessRuntime` calls AI Gateway directly. It owns the
agentic loop, parses responses, and drives transitions.

**Future** (if lassie-ng integration happens): Temper's driver would call
lassie-ng, which handles token counting, streaming, step persistence, and
then returns the full response. The decision depends on the callback direction.

### "Who calls the tools?"

**Both**, depending on the mode:

- **Foreground** (user connected): Temper asks lassie-ng, which routes to
  the right executor (server-side MCP, client-side CLI, approval flow)
- **Background** (no user): Temper calls MCP servers directly (server-only
  tools, no client-side tools available)

### "What about WASM?"

The primary goal of WASM in this architecture is **extensions and drivers**.
Teams compile their logic to WASM, Temper runs it in-process — sandboxed,
any language, zero network overhead.

Current uses:
- **Context strategy extensions**: Teams upload `.wasm` modules that implement
  hooks (prepare_context, after_tool_result, on_error, etc.) to customize how
  conversation history is managed. See the pluggable context management feature.
- **Integration drivers**: WASM modules that implement tool execution, called
  via the `temper-wasm` engine with fuel budgets and host functions.
- **Agent programs** (future): Teams write full agent logic as WASM modules
  instead of relying on LLM-driven loops. Same syscall interface, same scheduling.

### "Is the callback direction settled?"

**No.** The current design has Temper calling back to lassie-ng internal APIs.
Alternatives under consideration:
1. Temper calls lassie-ng (current doc)
2. Lassie-ng calls Temper ("what's next?")
3. Shared library (Rust crate, FFI/WASM)

This is the main open architectural decision.

---

## How a Request Flows (End-to-End)

```
1. User types "What's the CPU?" in Bits CLI
     │
     ▼
2. Bits CLI → POST /v1/agents/{id}/messages (lassie-ng HTTP)
     │
     ▼
3. Lassie-ng: auth, create Run, create Process in Temper
     │
     ▼
4. Temper Process: Created → [StartProcess] → BlockedInference
     │
     ▼
5. LassieInferenceDriver → lassie-ng → AI Gateway → Claude
     │  (tokens stream to SSE → CLI shows typing)
     ▼
6. Response: "Let me check. [tool_call: get_datadog_metric]"
     │
     ▼
7. Temper: CompleteInference → Ready → ContinueWithToolCall
     │
     ▼
8. LassieToolDriver → lassie-ng → ODP MCP server → metric result
     │
     ▼
9. Temper: CompleteToolCall → Ready → ContinueWithInference
     │
     ▼
10. LassieInferenceDriver → lassie-ng → Claude → "CPU is at 42%"
     │
     ▼
11. Temper: CompleteInference → Ready → CompleteProcess → Terminated
     │
     ▼
12. User sees: "CPU is at 42%."
```

Total Temper overhead: ~5 state transitions, ~0.1ms of logic.
Total wall time: dominated by LLM latency (seconds).

---

## Current State (March 2026)

| Component | Status |
|-----------|--------|
| Temper runtime (actor system, IOA, event sourcing) | ✅ In dd-source |
| Temper agentic loop (parse response, tool dispatch) | ✅ In dd-source |
| Temper drivers (Bash, Code Sandbox, LLM client) | ✅ In dd-source |
| Temper WASM engine (wasmtime, module cache) | ✅ In dd-source |
| Pluggable context management (8 hooks, WASM strategies) | ✅ In PR |
| Lassie-ng control plane (full Letta API) | ✅ In production |
| LassieInferenceDriver (Temper → lassie-ng LLM calls) | 🔲 Not started |
| LassieToolDriver (Temper → lassie-ng tool dispatch) | 🔲 Not started |
| Background execution (direct drivers, sinks) | 🔲 Not started |
| Multi-agent orchestration (Spawn/Wait/Kill) | 🔲 Spec exists, not wired |
| ProcessScheduler (fair-share, budgets) | 🔲 Spec exists, not wired |
| Cedar governance | 🔲 Spec exists, not wired |
| Trigger API (cron, webhook, monitor) | 🔲 Not started |

---

## Where to Find Things

| Document | Location |
|----------|----------|
| K8s for Agents vision | `lassie-ng/docs/k8s_for_agents_vision.md` |
| Temper as CRI (integration design) | `temper/TEMPER-LASSIE.md` |
| Agent Interface (north-star API) | `lassie-ng/docs/agent_interface.md` |
| Temper architecture | `temper/docs/ARCHITECTURE.md` (in dd-source) |
| ADRs (architectural decisions) | `temper/docs/adrs/` (in dd-source) |
| Process IOA spec | `temper/specs/process.ioa.toml` (in dd-source) |
| CSDL entity model | `temper/specs/model.csdl.xml` (in dd-source) |
| Pluggable context PLAN | `temper/docs/plans/pluggable-context/PLAN.md` |
