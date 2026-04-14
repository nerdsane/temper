# Crucible Overview

A governed agent runtime built on Temper. Define agents, environments, and sessions as entities — then let agents manage themselves.

---

Crucible turns Temper into an agent platform. Instead of writing custom orchestration code, you declare agents, tools, and execution environments as governed entities. Temper handles state machines, authorization, persistence, and verification. You focus on what your agents should do.

Crucible shares the conceptual shape of Anthropic's [Managed Agents API](https://platform.claude.com/docs/en/managed-agents/overview) — agents, environments, sessions, events — but is not wire-compatible. It speaks OData, runs on your infrastructure, and adds governance features (Cedar policies, verified state machines, cross-entity invariants) that don't exist in the upstream API.

## Core concepts

| Concept | Description |
|---------|-------------|
| **Agent** | The model configuration: system prompt, model ID, tools, and which sub-agents it can delegate to |
| **Environment** | Where tools execute — on the host machine or inside a Modal cloud sandbox |
| **Session** | A running conversation between a user and an agent, with full event history |
| **Events** | The append-only log of everything that happens — messages, tool calls, delegations, status changes |
| **Threads** | Isolated execution scopes for sub-agent delegation within a session |

## How it works

**1. Define an agent**

Create a `ManagedAgent` with a model, system prompt, and tools. Optionally link sub-agents via `CallableAgent` for multi-agent orchestration.

**2. Configure an environment**

Create an `Environment` that controls where tools run. `Local` executes on the host. `Modal` provisions a cloud sandbox — isolated, configurable, and lazily created on first use.

**3. Start a session**

Launch a `Session` referencing your agent and environment. The session tracks its own lifecycle, event history, and token usage.

**4. Send messages and get responses**

Post user messages as events. The agent reads the conversation history, calls the LLM, executes tools, and writes its response back as new events. Every step is recorded.

**5. Delegate across agents**

When an agent has callable sub-agents, it can delegate tasks to them. The platform creates a thread, runs the sub-agent with its own system prompt, and returns the result. The full delegation trajectory is captured in the event feed.

## Getting started

From the Monty shell or any HTTP client:

```python
# 1. Create an environment
env = await temper.create('Environment', {
    'Name': 'dev',
    'ConfigType': 'Local',
    'NetworkingType': 'Unrestricted'
})

# 2. Create an agent
agent = await temper.create('ManagedAgent', {
    'Name': 'assistant',
    'System': 'You are a helpful coding assistant.',
    'ModelId': 'claude-sonnet-4-6'
})

# 3. Create a session
session = await temper.create('Session', {
    'AgentId': agent['entity_id'],
    'EnvironmentId': env['entity_id']
})
```

Then drive the agent loop:

```bash
crucible-chat send <session-id> "What files are in the current directory?"
crucible-chat respond <session-id>
# Agent uses bash tool, lists files, responds with findings
```

See the [Quickstart](QUICKSTART.md) for the full walkthrough.

## When to use Crucible

Crucible is a good fit when you need:

- **Governed agent execution** — every state transition, tool call, and delegation is verified and authorized, not just logged
- **Multi-agent orchestration** — coordinators delegate to specialized sub-agents, each with their own model and system prompt
- **Sandboxed tool execution** — agents run bash, edit files, and grep codebases inside Modal containers without host access
- **Full observability** — the event feed captures every LLM call, tool result, and delegation with token counts and timing
- **Declarative configuration** — agents, environments, and their relationships are entities you create and query, not code you deploy

## Session lifecycle

Sessions follow a governed five-state lifecycle:

```
Rescheduling ──► Running ◄──► Idle ──► Terminated ──► Archived
```

| Transition | Action | When |
|-----------|--------|------|
| Rescheduling → Running | `StartSession` | Agent begins processing |
| Running → Idle | `IdleSession` | Agent finishes a turn |
| Idle → Running | `ResumeSession` | New user message arrives |
| Idle → Terminated | `TerminateSession` | Session ends |
| Terminated → Archived | `ArchiveSession` | Cleanup |

These are bound actions enforced by Temper's state machine — not arbitrary status fields.

## Tools

Agents have access to six built-in tools:

| Tool | Description |
|------|-------------|
| **bash** | Run shell commands |
| **read** | Read file contents with line numbers |
| **write** | Create or overwrite files |
| **edit** | Find-and-replace within a file |
| **glob** | Find files by pattern |
| **grep** | Search file contents by regex |

Where tools execute depends on the environment:

- **Local** — directly on the host machine
- **Modal** — inside a cloud sandbox, provisioned on first use. The agent never sees infrastructure credentials.

## Multi-agent orchestration

A coordinator agent delegates work to sub-agents, each running in an isolated thread:

```
User message
  └─► Coordinator (LLM call)
        └─► delegate_to_agent("code-reviewer", "Review this file")
              └─► Thread created
                    └─► Sub-agent (LLM call)
                          └─► bash("python -m pytest tests/")
                          └─► returns findings
              └─► Result flows back
        └─► Coordinator (LLM call)
              └─► Synthesizes final response
```

Sub-agents get their own system prompt and model. They share the session's environment — if it's Modal, their tools execute in the same sandbox. The event feed records every step with thread-scoped sequencing.

Define the delegation graph by creating `CallableAgent` entities:

```python
await temper.create('CallableAgent', {
    'AgentId': coordinator['entity_id'],
    'CalleeAgentId': reviewer['entity_id']
})
```

## Event kinds

Everything is recorded as a `SessionEvent` with one of 20+ kinds:

| Group | Kinds | Purpose |
|-------|-------|---------|
| **User** | `user.message`, `user.tool_confirmation`, `user.hard_limit_message`, `user.tool_result` | User input |
| **Agent** | `agent.message`, `agent.thinking`, `agent.tool_use`, `agent.tool_result`, `agent.mcp_tool_use`, `agent.thread_message_sent`, `agent.thread_message_received` | Agent actions |
| **Session** | `session.status_*`, `session.thread_created`, `session.thread_idle`, `session.error` | Lifecycle pulses |
| **Span** | `span.model_request_start`, `span.model_request_end` | LLM call observability with token counts |

Query the event feed for any session:

```bash
curl '/tdata/SessionEvents?$filter=SessionId eq "<id>"&$orderby=Sequence asc'
```

## Governance

Every entity in Crucible is governed by Temper's verification and authorization stack:

- **State machines** — session lifecycle, thread lifecycle, and entity status transitions are declared in IOA specs and enforced at runtime
- **Field invariants** — per-kind required fields, enum validations, and conditional constraints are checked on every write
- **Cross-invariants** — referential integrity rules (e.g., sessions require non-archived agents) are enforced across entity boundaries
- **Cedar policies** — fine-grained authorization controls who can create, read, and invoke actions on entities
- **Verification cascade** — L0 through L3 (symbolic, model checking, simulation, property tests) proves spec correctness before deployment

## Supported providers

The agent loop works with any LLM through two provider interfaces:

| Provider | Config | Examples |
|----------|--------|----------|
| **Anthropic** (default) | `ANTHROPIC_API_KEY` | Claude Sonnet, Opus, Haiku |
| **OpenAI-compatible** | `OPENAI_API_KEY` + `OPENAI_BASE_URL` | GPT-4o, Fireworks, Together, Ollama |
| **Mock** | `CRUCIBLE_RESPONDER_MODE=mock` | Deterministic echo — no API key needed |

## Key differences from Anthropic's Managed Agents

| | Anthropic Managed Agents | Crucible |
|---|---|---|
| **Runs on** | Anthropic's cloud | Your infrastructure (via Temper) |
| **Transport** | REST with nested paths | OData with flat entity sets |
| **Agent loop** | Server-side, SSE streaming | Client-side sidecar, polling |
| **Authorization** | API keys | Cedar policies with entity-level granularity |
| **Verification** | None (trust the implementation) | 4-level cascade proves spec correctness |
| **Multi-agent** | Research preview | Fully implemented with thread isolation |
| **Tool execution** | Anthropic containers | Local or Modal sandboxes |
| **State management** | Implicit | Explicit state machines with bound actions |

## Learn more

- [Quickstart](QUICKSTART.md) — create your first agent session end-to-end
- [Architecture](ARCHITECTURE.md) — how it works under the hood
- [Live Agent Walkthrough](../LIVE_AGENT_WALKTHROUGH.md) — captured session against a real LLM
- [Multi-Agent Test Findings](../MULTI_AGENT_TEST_FINDINGS.md) — delegation + Modal sandbox results
