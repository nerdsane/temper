+++
name = "evolution"
description = "GEPA-based self-improvement loop for Temper skills"
entity_types = ["EvolutionRun", "SentinelMonitor"]
dependencies = ["project-management"]
+++

## When to use

Use when agent execution trajectories reveal friction patterns — missing actions, guard
rejections, or repeated failures on specific entity types. The evolution skill closes the
loop: detect friction, propose spec mutations via LLM, verify through the L0-L3 cascade,
and deploy improvements with human-gated or auto-approved governance.

## Entity Types

### EvolutionRun

Orchestrates one GEPA evolution cycle targeting a skill's entity specs.

**States**: Created → Selecting → Evaluating → Reflecting → Proposing → Verifying → Scoring → Updating → AwaitingApproval → Deploying → Completed

**Key actions**:
- **Start**: Begin evolution targeting a skill (e.g., `project-management`)
- **SelectCandidate**: Pick a spec from the Pareto frontier or seed pool
- **RecordEvaluation**: Replay trajectories against the candidate spec (WASM)
- **RecordDataset**: Build reflective dataset from OTS traces (WASM)
- **RecordMutation**: TemperAgent proposes spec edits guided by reflective data (spec/WASM path)
- **RecordVerificationPass/Failure**: L0-L3 cascade result
- **RecordScore**: Multi-objective scoring (WASM)
- **RecordFrontier**: Pareto frontier update (WASM)
- **Approve/Reject**: Human or verified agent gates deployment
- **Deploy**: Hot-deploy via SpecRegistry::swap_table()

**Verification retry loop**: On L0-L3 failure, errors feed back as reflective data.
Max 3 attempts per candidate before transitioning to Failed.

### SentinelMonitor

Self-scheduling entity that periodically checks for trajectory failure clusters.
Uses ADR-0012 schedule effects — the entity IS the cron job.

**States**: Active → Checking → Triggering → Active (loop)

**Key actions**:
- **CheckSentinel**: Scheduled every 5 minutes via schedule effects
- **AlertsFound**: Trajectory failure cluster detected
- **CreateEvolutionRun**: Spawns an EvolutionRun for the affected skill

## Autonomy Slider

Cedar policies control who can approve evolution candidates:

| Level | Who approves | Use case |
|-------|-------------|----------|
| `human` (default) | Only humans | Production, high-risk |
| `supervised` | Verified agents for low-risk | Staging, trusted agents |
| `auto` | Any verified agent | Testing, CI/CD |

Self-approval is always forbidden: the agent that proposed a mutation cannot approve it.

## Example Workflow

### Agent detects missing action
1. Agent attempts `Reassign` on Issue → fails (action not in spec)
2. OTS trajectory records the failure
3. SentinelMonitor detects 5+ failures on Issue entity type
4. SentinelMonitor creates EvolutionRun targeting `project-management`
5. EvolutionRun replays trajectories → builds reflective dataset → LLM proposes adding `Reassign`
6. L0-L3 verification passes → Cedar approval → hot-deploy
7. Agent retries `Reassign` → succeeds
