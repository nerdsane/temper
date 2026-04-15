# Agent Orchestration

Multi-agent orchestration with organizations, budget ledgers, and heartbeat-driven execution runs. Manages who can run agents, how much they can spend, and tracks each execution from scheduling through completion.

## Entity Types

### Organization

Team and budget controls for orchestration.

**States**: Setup → Active → Paused → Archived

**Key actions**:
- **Configure**: Set name, description, and monthly budget
- **Activate**: Enable the organization for scheduling and execution
- **AddMember / RemoveMember**: Manage organization roster
- **RecordCost**: Record budget consumption from orchestration work
- **ResetBudgetCycle**: Roll over the monthly budget counter
- **Pause / Resume**: Temporarily halt execution activity
- **Archive**: Terminal state

### HeartbeatRun

Agent execution heartbeat lifecycle. Tracks a single agent run from scheduling through budget approval, execution, and completion.

**States**: Scheduled → CheckingIn → Working → Completed | Failed | Cancelled

**Key actions**:
- **Schedule**: Initialize run context with agent, org, wake reason, and adapter type
- **ApproveBudget**: Approve execution budget before work can begin
- **CheckIn**: Agent checks in (requires budget approval and agent binding)
- **StartExecution**: Start execution and trigger the configured adapter integration
- **RecordTurn**: Record a completed execution turn with token/cost totals
- **SaveCheckpoint**: Persist resumable session checkpoint
- **RecordResult**: Record execution output payload
- **Complete**: Finish run (requires a result)
- **Fail / Cancel**: Terminal error or cancellation from any active state

### BudgetLedger

Append-only organization cost records for audit and reporting.

**States**: Recorded (single state, append-only)

**Key actions**:
- **Record**: Append a normalized budget event with org, agent, run, amount, tokens, and category

## Setup

```
temper.install_app("<tenant>", "agent-orchestration")
```
