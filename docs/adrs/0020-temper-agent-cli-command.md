# ADR-0020: temper agent CLI Command — Entity-Native Agent

- Status: Accepted
- Date: 2026-03-03
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar authorization for agents
  - ADR-0005: Agent policy and audit layer
  - ADR-0015: Agent OS cross-entity primitives
  - `.vision/agent-os.md` (Agent OS operating layer)
  - `crates/temper-cli/src/agent/` (new code)

## Context

Temper's Agent OS vision: "This is not a framework for building agents. It is the operating layer that agents run on top of." Integrating Temper into existing agent frameworks (PI, Claude Code, LangGraph) produces Frankenstein integrations — each framework has its own tool/plugin system that doesn't naturally accommodate external governance.

The cleanest path: build the agent entirely from Temper entities. The agent IS Temper entities. Every tool call is a governed, persisted entity with its own lifecycle. The `temper agent` CLI is a thin loop that creates and transitions these entities. All durable state lives on the server. The CLI is stateless — it can crash and restart, read the entities, and pick up exactly where it left off.

This is NOT "Temper as a governance API that agents call." This is "the agent is made of Temper entities, and Temper provides state + authorization + durability."

## Decision

### Sub-Decision 1: Entity-Native Architecture

The agent is composed of four entity types:
- **Agent**: The agent session with conversation state, role, goal, model.
- **Plan**: Orchestrates a collection of tasks toward a goal (renamed from Pipeline).
- **Task**: A discrete unit of work (renamed from WorkItem).
- **ToolCall**: Each tool invocation as a durable entity with lifecycle (Pending → Authorized → Executing → Completed/Failed/Denied).

**Why this approach**: Every meaningful operation is an entity state transition. Crash recovery = read entities. The conversation field on Agent provides durable checkpointing. ToolCall entities provide a complete audit trail of every tool invocation with Cedar authorization decisions.

### Sub-Decision 2: Client-Side Execution with Server-Side Durability

The `temper agent` CLI runs the LLM loop and executes tools locally. The server provides state (entity persistence), authorization (Cedar), and audit (trajectory). The CLI is stateless — it can be killed and restarted with `--agent-id` to resume from the last checkpoint.

**Why this approach**: Local execution avoids the complexity of server-side LLM orchestration. The server stays focused on what it does well: governed state management. The Bitter Lesson applies — no config flags, Cedar governs what agents can do, agents decide how.

### Sub-Decision 3: Lightweight Authorization Endpoint

A new `POST /api/authorize` endpoint provides Cedar policy checks without entity creation. The agent CLI calls this before executing each tool to check permissions. On denial, a PendingDecision is created for human review.

**Why this approach**: Separates authorization checks from entity transitions. The agent can check permissions before creating the ToolCall entity, reducing noise from denied operations. Follows the existing pattern in `authz_helpers.rs`.

### Sub-Decision 4: Tool Definitions as Cedar Resources

Each tool maps to a Cedar resource type and action:
- `file_read` → FileSystem::read
- `file_write` → FileSystem::write
- `file_list` → FileSystem::list
- `shell_execute` → Shell::execute

**Why this approach**: Reuses the existing Cedar authorization model. Policies can be written naturally: "permit agent X to read files but not execute shell commands."

## Rollout Plan

1. **Phase 0 (This PR)** — ADR, entity specs (Agent, Plan, Task, ToolCall), CSDL update.
2. **Phase 1** — Server endpoints (`/api/authorize`, `/api/audit`).
3. **Phase 2** — CLI command (`temper agent`), LLM client, tool execution.
4. **Phase 3** — Blocked state, decision polling, resume flow.

## Consequences

### Positive
- Complete audit trail of every agent action as entity state transitions.
- Crash-resilient: CLI can restart and resume from last checkpoint.
- Cedar-governed: all tool calls pass through authorization.
- Clean separation: CLI handles execution, server handles state + auth.

### Negative
- More HTTP round-trips per tool call (create entity + authorize + transition).
- Conversation serialized as JSON string in entity field (size limits apply).

### Risks
- Large conversations may exceed entity field size limits. Mitigation: checkpoint summarization in future PR.
- Network latency between CLI and server adds to tool execution time. Mitigation: local server mode for development.

### DST Compliance
- `temper-cli` is not simulation-visible — `Instant::now()`, `uuid::Uuid::now_v7()`, `reqwest` calls are all OK.
- New server endpoints in `api.rs` use `sim_now()`, `BTreeMap`, and existing persistence methods.

## Non-Goals
- Server-side LLM dispatch (deferred to `type = "llm"` integration).
- Streaming SSE output (future PR).
- Sub-agent spawning (requires ADR-0015 Effect::Spawn).
- Task/Plan integration (future PR).

## Alternatives Considered

1. **Framework plugin approach** — Build Temper as a plugin for LangGraph/PI. Rejected: each framework's plugin system fights Temper's governance model.
2. **Server-side agent execution** — Run LLM loop on the server. Rejected: adds complexity, harder to debug, CLI execution is simpler and sufficient.
3. **Config-flag governance** — Use config files to control agent permissions. Rejected: violates Bitter Lesson. Cedar policies are more expressive and auditable.
