# ADR-0026: Background Agent Capabilities

- Status: Accepted
- Date: 2026-03-05
- Deciders: Temper core maintainers
- Related:
  - ADR-0023: Agent Executor Binary
  - ADR-0024: Temper SDK (Rust/TypeScript)
  - `crates/temper-executor/src/main.rs`
  - `crates/temper-agent-runtime/src/runner.rs`
  - `crates/temper-platform/src/specs/agent.ioa.toml`
  - `crates/temper-server/src/reaction/`

## Context

Temper agents currently run as foreground CLI processes tied to a terminal session. The executor (ADR-0023) is already a daemon that watches for Agent entities and runs them headlessly, but lacks:

1. **Sub-agent delegation** — Agents can't spawn children via tool calls, even though `Agent.SpawnChild` is already spec'd
2. **Agent type definitions** — No way to define reusable agent configurations (system prompt, tool set, model)
3. **Event-triggered agents** — No way to auto-spawn agents in response to entity state changes
4. **Scheduled agents** — No way to run agents on a recurring schedule
5. **Executor robustness** — No health endpoint, graceful shutdown, or daemonization

The key insight is that most infrastructure already exists: the executor IS the daemon, reactions ARE the event bus, scheduled actions already work, SpawnChild is already spec'd. The gaps are wiring and orchestration.

## Decision

### Sub-Decision 1: Sub-Agent Orchestration via Tool Calls

Wire `Agent.SpawnChild` into the tool registries so agents can delegate and wait for children.

- Add `spawn_child_agent(role, goal, model)` and `check_children_status()` tools to both `TemperToolRegistry` and `LocalToolRegistry`
- Add `set_agent_id(&self, id: &str)` to `ToolRegistry` trait for propagating agent identity
- In `AgentRunner::resume()`, retry `Agent.Complete` with polling when cross-entity guard blocks (children not finished)

**Why this approach**: The SpawnChild action + cross-entity guard already exist in the spec. The executor's SSE loop already picks up new Agent.Start events. We just need tools to invoke SpawnChild and poll child status — no new infrastructure.

### Sub-Decision 2: Agent Type Entity

Define AgentType as a Temper entity with its own IOA spec.

- States: Draft → Active → Deprecated
- Fields: name, system_prompt, tool_set, model, max_turns
- Add `agent_type_id` field to Agent spec's Assign params
- Executor resolves AgentType at claim time for system prompt/model/tools

**Why this approach**: Entity-based rather than config-file-based. Reuses existing spec verification, CSDL exposure, and OData API. Developers configure agent types through the same conversational interface as everything else.

### Sub-Decision 3: Event-Triggered Agents via Reactions

Add `[[agent_trigger]]` sections to IOA specs. At registration time, synthesize ReactionRules.

- Parse `[[agent_trigger]]` in toml_parser.rs (name, on_action, to_state, agent_role, agent_goal, agent_type_id)
- Synthesize two ReactionRules per trigger: source action → Agent.Assign, Agent.Assign → Agent.Start
- Use `TargetResolver::CreateIfMissing` for auto-creating Agent entities

**Why this approach**: Reuses existing ReactionDispatcher, ReactionRegistry, and TargetResolver infrastructure. No new dispatch code needed. The executor SSE loop picks up spawned agents automatically.

### Sub-Decision 4: Scheduled Agents via Schedule Entity

- New Schedule entity: Draft → Active → Paused → Completed
- Fields: agent_type_id, goal_template, cron_expr, last_run, run_count, max_runs
- Fire action spawns Agent child via spawn effect
- Executor runs a 60s ticker that evaluates cron expressions

**Why this approach**: Schedule as an entity means it's governed, versioned, and queryable via OData. The spawn effect reuses existing infrastructure. The ticker is a simple addition to the executor's `tokio::select!` loop.

### Sub-Decision 5: Executor Enhancement

- Health endpoint via Axum on `--health-port`
- Graceful shutdown via `tokio::signal::ctrl_c()`
- `--detach` flag for double-fork daemonization
- Restructure `main()` with `tokio::select!` over all concurrent tasks

**Why this approach**: The executor is already the daemon. These are operational necessities for production deployment — health checks for load balancers, graceful shutdown for zero-downtime deploys, detach for systemd-less environments.

## Rollout Plan

1. **Phase 1 (Sub-Agent Orchestration)** — Tool additions + runner retry logic. Immediate impact: agents can delegate.
2. **Phase 2 (Agent Types)** — New spec + bootstrap + executor resolution. Enables typed agents.
3. **Phase 3 (Event Triggers)** — Spec parsing + reaction synthesis. Depends on Phase 2 (agent_type_id). Enables reactive agent spawning.
4. **Phase 5 (Executor Ops)** — Health, shutdown, detach. Independent of other phases.
5. **Phase 4 (Scheduling)** — New spec + executor ticker. Depends on Phase 2 + 5. Enables recurring agents.

## Consequences

### Positive
- Agents can delegate sub-tasks and aggregate results
- Reusable agent configurations via AgentType entities
- Declarative event-driven agent spawning via specs
- Scheduled recurring agent execution
- Production-ready executor with health checks and graceful shutdown

### Negative
- AgentType lookup adds one OData call per agent claim
- Schedule ticker adds a background task to the executor
- More entity types to manage (AgentType, Schedule)

### Risks
- Cross-entity guard polling for child completion could delay parent completion. Mitigated by bounded retries (60 max, 5s interval = 5 min budget).
- Cron evaluation adds `cron` + `chrono` dependencies to executor. Mitigated by keeping them executor-only (not simulation-visible).
- Reaction cascade depth for agent triggers. Mitigated by existing MAX_REACTION_DEPTH=8 limit.

### DST Compliance
- Tool registries (temper-agent-runtime) are not simulation-visible — no DST concerns.
- Executor is not simulation-visible — `Instant::now()`, `chrono`, `tokio::spawn` are fine with `// determinism-ok`.
- Agent trigger synthesis happens at registration time in temper-server — uses BTreeMap ordering for deterministic iteration.
- Schedule ticker uses wall clock time in executor only — not simulation-visible.

## Non-Goals

- Multi-executor coordination (load balancing, leader election) — single-executor for now
- Agent-to-agent messaging beyond parent-child spawn/complete
- Priority queuing for agent execution
- Agent resource budgeting (CPU/memory limits per agent)

## Alternatives Considered

1. **External scheduler (cron/systemd timers)** — Would require separate process management outside Temper. Rejected: Schedule-as-entity is more governable and observable.
2. **Polling-based trigger instead of reactions** — Would require new polling infrastructure. Rejected: Reactions already exist and are proven.
3. **Config-file agent types** — Would bypass spec verification and OData exposure. Rejected: Entity-based approach is consistent with Temper philosophy.

## Rollback Policy

Each phase is independently deployable and reversible:
- Phase 1: Remove tools from registries, revert runner retry logic
- Phase 2: Remove AgentType spec from bootstrap (entities created during uptime become orphaned but harmless)
- Phase 3: Remove agent_trigger parsing (reaction rules stop being synthesized)
- Phase 4: Remove Schedule spec + ticker
- Phase 5: Remove health endpoint / shutdown handler / detach flag
