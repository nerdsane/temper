# ADR-0023: Agent Executor Binary

- Status: Accepted
- Date: 2026-03-03
- Deciders: Temper core maintainers
- Related:
  - ADR-0020: Temper Agent CLI Command
  - ADR-0021: Task/Plan Integration and Sub-Agent Spawning
  - ADR-0022: SSE Streaming for Agent CLI
  - ADR-0024: Temper SDK (Rust + TypeScript)
  - `crates/temper-agent-runtime/` (new crate)
  - `crates/temper-executor/` (new crate)

## Context

The `temper agent` CLI command embeds the full agent execution loop: LLM provider, tool execution, plan decomposition, and task lifecycle. This design couples agent runtime logic to the CLI binary, preventing headless execution.

Production deployments need a separate executor process that watches for Agent entities in Working state and claims them for execution. This enables:
- Multiple executor instances for horizontal scaling
- Different tool registries per environment (local file I/O vs entity-only)
- Child agent handling through the same SSE watch loop

## Decision

### Sub-Decision 1: Temper Never Runs Agents Directly

The Temper server manages entity state (Agent, Plan, Task, ToolCall) but never executes agent logic. The executor is a separate process that connects via the SDK HTTP client and SSE event stream.

**Why this approach**: Separation of concerns. The server remains a governed state machine; compute-heavy LLM calls and file I/O happen outside the server boundary.

### Sub-Decision 2: Agent Runtime Crate (`temper-agent-runtime`)

Extract the core agent execution loop into a reusable library crate with:
- `AgentRunner` — orchestrates create/assign/start/plan/execute/complete lifecycle
- `LlmProvider` trait — pluggable LLM backend (Anthropic today, others later)
- `ToolRegistry` trait — pluggable tool set (local vs temper-only)
- `LocalToolRegistry` — file_read, file_write, file_list, shell_execute + entity ops
- `TemperToolRegistry` — entity CRUD only, no local file/shell access

**Why this approach**: The CLI and executor share the same runner logic. New frontends (CI/CD plugins, web workers) can embed `AgentRunner` with their own tool registry.

### Sub-Decision 3: SSE-Based Agent Claiming

The executor connects to `/tdata/$events` (or `/api/events`) via SSE and watches for Agent entities entering Working state with `executor_id == ""`. It claims agents by PATCHing `executor_id` to its own ID.

**Why this approach**: Pull-based claiming via SSE avoids polling overhead and is consistent with the existing SSE infrastructure (ADR-0022). The `executor_id` field prevents double-claiming.

### Sub-Decision 4: Two Tool Registries

- `LocalToolRegistry`: file_read, file_write, file_list, shell_execute + Temper entity operations via SDK. Used by the CLI and local executor.
- `TemperToolRegistry`: Entity CRUD only, no local filesystem or shell access. Used by sandboxed/remote executors.

The `--tool-mode local|temper` flag controls which registry the executor uses.

**Why this approach**: Local tools are needed for development. Production sandboxed executors must not have filesystem access.

### Sub-Decision 5: Concurrency via Semaphore

The executor uses a `tokio::Semaphore` bounded by `--max-concurrent N` to limit parallel agent runs. Each claimed agent spawns a tokio task that acquires a permit.

**Why this approach**: Simple, bounded concurrency without a custom thread pool. Child agents (from `SpawnChild`) are picked up by the same watch loop, so they count against the same limit.

## Rollout Plan

1. **Phase 0 (This PR)**: Extract `temper-agent-runtime` crate. Create `temper-executor` binary. Refactor CLI to use `AgentRunner`.
2. **Phase 1 (Follow-up)**: Add reconnection logic for SSE stream drops. Add health endpoint for executor liveness checks.
3. **Phase 2**: Docker image for executor. Kubernetes deployment manifest.

## Consequences

### Positive
- Agent execution is decoupled from the CLI binary.
- Horizontal scaling via multiple executor instances.
- Pluggable LLM providers and tool registries.
- CLI becomes a thin wrapper around `AgentRunner`.

### Negative
- Additional binary to build, deploy, and monitor.
- SSE-based claiming adds eventual consistency (small window for double-claim before PATCH).

### Risks
- SSE connection drops could delay agent claiming. Mitigated by reconnection in Phase 1.
- `executor_id` PATCH is not a CAS operation. Mitigated by server-side guard on `executor_id` field transitions.

## Non-Goals

- Multi-model orchestration (single LLM provider per runner for now).
- WASM tool execution within the executor (entity ops only via `TemperToolRegistry`).
- Distributed locking for agent claiming (simple PATCH-based claiming is sufficient at this scale).

## Alternatives Considered

1. **Embed executor in temper-server** — Rejected. Violates separation of concerns. Server should not run LLM calls.
2. **gRPC-based agent dispatch** — Rejected. Adds protocol complexity. SSE + HTTP PATCH is simpler and consistent with existing infrastructure.
3. **Queue-based claiming (Redis/SQS)** — Rejected. Over-engineered for current scale. SSE is already available.
