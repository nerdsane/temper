# ADR-0021: Task/Plan Integration and Sub-Agent Spawning

- Status: Accepted
- Date: 2026-03-03
- Deciders: Temper core maintainers
- Related:
  - ADR-0015: Agent OS cross-entity primitives (spawn, cross-entity guards)
  - ADR-0020: `temper agent` CLI command (agent loop foundation)
  - `packages/temper-pi/demo/specs/Agent.ioa.toml`
  - `packages/temper-pi/demo/specs/Plan.ioa.toml`
  - `packages/temper-pi/demo/specs/Task.ioa.toml`
  - `crates/temper-cli/src/agent/mod.rs`
  - `crates/temper-server/src/state/dispatch/cross_entity.rs`

## Context

The `temper agent` CLI (ADR-0020) runs a single-loop LLM agent that executes tool calls toward a goal. It has no concept of planning or task decomposition -- the LLM decides what to do turn by turn. For complex goals, the agent needs to decompose work into discrete tasks, track progress, and optionally delegate sub-tasks to child agents.

The Plan and Task IOA specs already exist but are not wired into the agent loop. The Plan.AddTask action declares `initial_action = "Create"` but the Task spec lacks a Create action, causing spawn failures. The Agent spec lacks fields for tracking child agents and has no SpawnChild action.

Additionally, the cross-entity guard resolver treats an empty `entity_id_source` (no children) as a guard failure, which incorrectly blocks an agent with no children from completing.

## Decision

### Sub-Decision 1: Plan-Driven Agent Loop

The agent loop is refactored into two phases:

1. **Planning phase**: After the Agent transitions to Working, create a Plan entity, activate it, and ask the LLM to decompose the goal into tasks. For each task, call Plan.AddTask which spawns Task child entities.

2. **Execution phase**: Iterate over spawned Tasks. For each task: Claim it, StartWork, run the existing LLM tool-call loop scoped to the task, then SubmitForReview and Approve. After all tasks complete, complete the Plan and the Agent.

**Why this approach**: Tasks are the natural unit of work in the IOA model. Each Task tracks its own lifecycle and can be independently claimed, reviewed, and completed. The Plan entity provides an aggregate view and the task_count guard ensures no plan completes without work.

### Sub-Decision 2: Task.Create Action

Add a `Create` action to the Task spec (from=["Open"], params=["title", "description", "plan_id"]) so that Plan.AddTask's `initial_action = "Create"` succeeds. This initializes the task's fields when spawned.

**Why this approach**: The Task entity is spawned in the Open state. Without a Create action, the initial_action dispatch fails silently. The Create action stays in Open (self-loop) and sets the task's identity fields.

### Sub-Decision 3: Agent.SpawnChild Action

Add `executor_id` and `child_agent_ids` state fields to the Agent spec, plus a SpawnChild action that spawns a child Agent entity and tracks its ID. The Complete action gains a cross-entity guard requiring all children to be in Completed or Failed state.

**Why this approach**: Parent-child agent relationships are tracked via the child_agent_ids list. The cross-entity guard on Complete ensures a parent cannot finish until all delegated sub-tasks are resolved. This uses the existing Effect::SpawnEntity and Guard::CrossEntityStateIn primitives from ADR-0015.

### Sub-Decision 4: Empty List Guard Passthrough

When `resolve_cross_entity_guards()` encounters an entity_id_source that resolves to an empty string or empty list, the guard passes (returns true) instead of failing. An agent with no children should be free to complete.

**Why this approach**: The vacuous truth principle -- "all children are complete" is trivially true when there are no children. This prevents the common case (an agent with no sub-agents) from being blocked.

## Rollout Plan

1. **Phase 0 (This PR)** -- Spec changes (Task.Create, Agent.SpawnChild), CSDL update, cross-entity guard fix, plan-driven agent loop in CLI.
2. **Phase 1 (Follow-up)** -- SSE streaming for task progress, SDK client wrappers.
3. **Phase 2** -- Recursive sub-agent spawning with depth limits.

## Consequences

### Positive
- Agents can decompose complex goals into trackable tasks.
- Task lifecycle provides natural checkpoints and review gates.
- Parent-child agent relationships enable delegation patterns.
- Empty-list guard fix unblocks the common single-agent case.

### Negative
- Planning phase adds latency (one extra LLM call for decomposition).
- Plan/Task entities add storage overhead per agent run.

### Risks
- LLM may produce poor task decompositions. Mitigation: tasks can be re-planned or cancelled.
- Deep agent hierarchies could exhaust spawn budgets. Mitigation: MAX_SPAWNS_PER_TRANSITION and future depth limits.

### DST Compliance

- Cross-entity guard changes are in temper-server (simulation-visible). The fix is a pure logic change (empty -> pass) with no new I/O or non-determinism.
- CLI agent code is not simulation-visible.

## Non-Goals

- Modifying llm.rs (handled by Workstream B: SSE Streaming).
- Creating a temper-sdk crate (handled by Workstream D).
- Recursive sub-agent spawning with depth limits (Phase 2).

## Alternatives Considered

1. **Flat tool-call loop with no planning** -- Current approach. Rejected because complex goals need structure and progress tracking.
2. **External task queue (Redis/RabbitMQ)** -- Rejected because Temper entities already provide durable task lifecycle management with IOA guarantees.
