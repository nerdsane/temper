# Temper Agent

Governed agent loop with souls, skills, memories, tool hooks, heartbeat monitoring, and cron scheduling. The agent turn cycle is expressed as state transitions with WASM integration triggers -- no Rust while loop. Depends on `temper-fs` for conversation storage and file operations.

## Entity Types

### TemperAgent

The core agent execution entity. Drives the think-execute loop via state machine transitions and WASM callbacks.

**States**: Created → Provisioning → Thinking → Executing → Compacting → Steering → Completed | Failed | Cancelled

**Key actions**:
- **Configure**: Set system prompt, user message, model, provider, tools, soul, and overrides
- **Provision**: Start sandbox provisioning (triggers WASM)
- **SandboxReady**: Callback from provisioner; starts the think loop
- **ProcessToolCalls**: LLM returned tool_use blocks; transition to Executing
- **HandleToolResults**: Tool results received; increment turn and call LLM again
- **NeedsCompaction / CompactionComplete**: Context window management when tokens exceed threshold
- **CheckSteering / ContinueWithSteering**: Mid-run message injection from external callers
- **Steer**: Queue a steering message from any active state (self-loop)
- **FinalizeResult / RecordResult**: Complete the agent with a result
- **Heartbeat**: Record liveness during long operations
- **TimeoutFail**: No heartbeat within timeout period
- **Resume**: Resume from saved state with workspace restore
- **Fail / Cancel**: Terminal states

### AgentSoul

Versioned agent identity document (the SOUL.md equivalent). Defines WHO the agent is -- personality, instructions, constraints. Multiple agent runs can share the same soul.

**States**: Draft → Active → Archived

**Key actions**:
- **Create**: Initialize with name, description, and content file reference
- **Publish**: Make available for agent assignment (increments version)
- **Update**: Update content (increments version)
- **Archive**: Terminal state

### AgentSkill

Lazy-loaded capability description (the SKILL.md equivalent). Defines WHAT the agent can do. Only descriptions are injected into the system prompt; full content loaded on demand.

**States**: Active → Disabled

**Key actions**:
- **Register**: Register with name, description, content file, scope, and agent filter
- **Update**: Update description or content
- **Disable / Enable**: Toggle availability

### AgentMemory

Cross-session persistent knowledge scoped to a soul. Types: user, feedback, project, reference.

**States**: Active → Archived

**Key actions**:
- **Save**: Initialize a memory entry with key, content, type, and soul
- **Update**: Update memory content
- **Recall**: Read-only access for audit trail
- **Archive**: Remove from agent prompts (terminal)

### ToolHook

Before/after hooks for tool execution. Evaluated by tool_runner with regex tool matching.

**States**: Active → Disabled

**Key actions**:
- **Register**: Register with name, hook type (before/after), tool pattern, action (block/log/modify), soul, and priority
- **Disable / Enable**: Toggle the hook

### CronJob

Scheduled agent runs. Creates and tracks TemperAgent entities on a cron schedule with template substitution ({{now}}, {{run_count}}, {{last_result}}).

**States**: Created → Active → Paused → Expired

**Key actions**:
- **Configure**: Set schedule, soul, system prompt, user message template, model, and limits
- **Activate / Pause / Resume**: Control the schedule
- **Trigger**: Fire the job -- creates and provisions a TemperAgent (WASM)
- **TriggerComplete / TriggerFailed**: Callbacks from trigger WASM
- **Expire**: Max runs reached or manually expired (terminal)

### CronScheduler

Self-scheduling heartbeat that checks for due cron jobs. One per tenant.

**States**: Idle → Checking (loop)

**Key actions**:
- **Start / ScheduledCheck**: Begin checking for due jobs (triggers WASM)
- **CheckComplete**: Check finished; schedule next check
- **CheckFailed / ScheduleFailed**: Error handling; returns to Idle

### HeartbeatMonitor

Periodic scanner for stale agents. One per tenant. Self-scheduling via heartbeat pattern.

**States**: Idle → Scanning (loop)

**Key actions**:
- **Start / ScheduledScan**: Begin scanning for stale agents (triggers WASM)
- **ScanComplete**: Scan finished; schedule next scan
- **ScanFailed / ScheduleFailed**: Error handling; returns to Idle

## Setup

```
temper.install_app("<tenant>", "temper-agent")
```

Requires `temper-fs` to be installed first (declared dependency).
