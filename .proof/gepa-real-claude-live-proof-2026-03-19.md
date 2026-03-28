# GEPA Live Proof (OTS Portfolio + Workflow Metrics) — 2026-03-19

> Superseded by [`docs/GEPA_E2E_PROOF.md`](./GEPA_E2E_PROOF.md), which contains:
> - the latest fresh-tenant end-to-end run (`evo-live-fresh-20260319-v4`)
> - full OTS/entity/authz taxonomy and trigger semantics
> - full raw artifacts (OTS/replay/reflective) and explicit blockers

## Scope
- Worktree: `/Users/seshendranalla/Development/temper-gepa-tarjan`
- Server: `temper serve --port 4455 --storage turso --no-observe`
- Tenant: `gepa-live-portfolio-20260319`
- Proof date: March 19, 2026
- Primary run: `EvolutionRun('evo-live-ots-portfolio-20260319-v3')`

## What Was Proven
1. Real OTS trajectories were produced automatically by real `temper mcp` sessions (not fabricated JSON).
2. `SelectCandidate` omitted both `TrajectoryActions` and `Trajectories`; `gepa-replay` auto-loaded OTS trajectories from tenant storage.
3. `gepa-replay` produced workflow-level metrics (`workflows[]`, `workflow_completion_rate`, `partial_adjusted_rate`) plus action-level metrics.
4. `gepa-reflective` produced workflow-level triplets with:
   - `score` (`1.0` completed, `0.5` partial, `0.0` failed)
   - `preserve=true` on successful workflows
   - `patterns.missing_capabilities`, `patterns.common_failure_points`, `patterns.successful_patterns`
5. `flush_trajectory()` works live through MCP (`{"status":"flushed","trajectory_id":"..."}`) and uploads mid-session OTS snapshots.

## What Was Not Fully Proven End-to-End
- Full terminal success of the proposer/deploy leg in this run was blocked by invalid Anthropic credentials:
  - `Anthropic API returned 401 ... invalid x-api-key`
- Result: run reached `Proposing` with correct replay/dataset artifacts, then failed before `RecordMutation/RecordScore/RecordFrontier/Deploy`.

## Exact OTS Production Path

### MCP sessions used to generate trajectory portfolio
- `success` workflow: `Assign -> Reassign` (real entity)
- `partial` workflow: `Assign -> PromoteToCritical` (`PromoteToCritical` unknown)
- `failed` workflow: `Reassign` from backlog (invalid transition)
- `flush` proof session: `Assign`, then `await temper.flush_trajectory()` mid-session, then another execute call

These were real `temper mcp` `tools/call -> execute` invocations. Temper auto-uploaded OTS trajectories at session end, and uploaded a snapshot on flush.

### Full OTS example (real row)
`row_trajectory_id = 019d082e-74dc-7d30-8122-1bd451a6a352`

```json
{
  "ots_trajectory_id": "019d082e-74db-7d43-b5b4-6b7dcbb3eaa6",
  "metadata": {
    "task_description": "mcp-session",
    "agent_id": "unknown",
    "outcome": "success"
  },
  "turns": [
    {
      "messages": [
        {"role": "user", "content": {"type": "text", "text": "...temper.action(...Assign...) ... temper.action(...PromoteToCritical...)"}},
        {"role": "assistant", "content": {"type": "text", "text": "RuntimeError: HTTP 409 Conflict: Unknown action: PromoteToCritical"}}
      ],
      "decisions": [
        {
          "choice": {
            "action": "execute: ...",
            "arguments": {
              "trajectory_actions": [
                {"action": "Assign", "params": {"AgentId": "agent-partial-a", "Reason": "ots-partial-1"}},
                {"action": "PromoteToCritical", "params": {"Reason": "ots-partial-1"}}
              ]
            }
          },
          "consequence": {"success": false, "error_type": "RuntimeError: HTTP 409 Conflict: Unknown action: PromoteToCritical"}
        }
      ]
    }
  ]
}
```

## How Decisions, Actions, and Reasons Are Extracted
1. `temper-mcp` captures each `execute` turn into OTS.
2. For replay, `gepa-replay` reads each trajectory turn and prefers `decision.choice.arguments.trajectory_actions`.
3. For reflective reasoning context, `gepa-reflective` reads decision reasoning + assistant messages (`reasoning_chain`).
4. If `trajectory_actions` are absent, replay falls back to parsing user code for `temper.action(...)` calls.

## Workflow-Level Replay Output (v3)
From `ReplayResultJson` in `EvolutionRun('evo-live-ots-portfolio-20260319-v3')`:

```json
{
  "workflows_total": 5,
  "workflows_completed": 2,
  "workflows_partial": 2,
  "workflows_failed": 1,
  "workflow_completion_rate": 0.4,
  "partial_adjusted_rate": 0.6,
  "actions_attempted": 7,
  "succeeded": 4,
  "success_rate": 0.5714285714285714,
  "coverage": 0.8571428571428572
}
```

Per-workflow outcomes included both preserved successes and failure/partial paths:
- completed: `Assign`
- partial: `Assign -> PromoteToCritical`
- failed: `Reassign` from `Backlog`

## Workflow-Level Reflective Output (v3)
From `DatasetJson`:

```json
{
  "workflow_triplet_count": 5,
  "success_count": 2,
  "failure_count": 3,
  "workflow_completion_rate": 0.4,
  "workflow_counts": {"completed": 2, "partial": 2, "failed": 1},
  "patterns": {
    "common_failure_points": [
      {"action": "Reassign", "from_state": "Backlog", "occurrences": 2},
      {"action": "PromoteToCritical", "from_state": "Backlog", "occurrences": 1}
    ],
    "missing_capabilities": ["PromoteToCritical"],
    "successful_patterns": [
      {"trajectory_id": "019d082f-b5df-7381-ad61-d59327351a0d", "actions": ["Assign"]}
    ]
  }
}
```

Triplets now include `preserve=true` for completed workflows and targeted mutation feedback for failed/partial workflows.

## Before/After Evidence (Flat vs Workflow-Layered)

### Before (older module output, flat/action-centric)
```json
{
  "actions_attempted": 7,
  "succeeded": 0,
  "success_rate": 0.0,
  "has_workflows": false,
  "has_workflow_completion_rate": false
}
```

### After (current implementation)
```json
{
  "workflows_total": 5,
  "workflows_completed": 2,
  "workflows_partial": 2,
  "workflows_failed": 1,
  "workflow_completion_rate": 0.4,
  "partial_adjusted_rate": 0.6,
  "actions_attempted": 7,
  "succeeded": 4,
  "success_rate": 0.5714285714285714
}
```

## Live Blockers and Limits (Explicit)
1. Proposer failure root cause in this proof run: invalid Anthropic keys provided (`401 invalid x-api-key`).
2. Because proposer failed, this specific run did not reach scoring/frontier/deploy.
3. Replay/reflective/scoring modules are functioning and producing workflow-level outputs before proposer step.

## Architecture Diagram (What Was Proven)
```text
Real MCP sessions (execute) -> OTS persisted in ots_trajectories
                           -> (optional) temper.flush_trajectory() snapshot upload

EvolutionRun.Start
  -> SelectCandidate (no TrajectoryActions/Trajectories)
  -> gepa-replay auto-loads OTS portfolio from tenant
  -> RecordEvaluation (workflow metrics + action metrics)
  -> gepa-reflective builds workflow triplets + patterns
  -> RecordDataset
  -> gepa-proposer-agent (TemperAgent + Anthropic)
       -> BLOCKED in this run by invalid x-api-key (401)
```

## Code Fixes Verified in This Proof Iteration
- `gepa-replay` now infers initial state from candidate IOA (`initial = "..."`) instead of hardcoded fallback.
- `gepa-replay` ignores `execute:` pseudo-actions when no `trajectory_actions` are present.
- `gepa-replay` emits `actions_attempted` and `breakdown_point` at workflow level (in addition to existing fields).
- Added replay unit tests for:
  - initial-state inference
  - execute pseudo-action filtering
  - embedded trajectory action extraction

## Artifacts
- `/tmp/mcp_traj_success_in.jsonl`, `/tmp/mcp_traj_success_out.jsonl`
- `/tmp/mcp_traj_partial_in.jsonl`, `/tmp/mcp_traj_partial_out.jsonl`
- `/tmp/mcp_traj_failed_in.jsonl`, `/tmp/mcp_traj_failed_out.jsonl`
- `/tmp/mcp_traj_flush_in.jsonl`, `/tmp/mcp_traj_flush_out.jsonl`
- `/tmp/ots_portfolio_list.json`, `/tmp/ots_portfolio_rows.json`, `/tmp/ots_partial_full.json`
- `/tmp/evo_portfolio_v3_final.json`
- `/tmp/evo_portfolio_v3_replay.json`
- `/tmp/evo_portfolio_v3_dataset.json`

## Bottom Line
- Working now: OTS capture, OTS auto-injection, workflow-level replay, workflow-level reflective dataset, preserve/failure pattern extraction, flush snapshot upload.
- Not fully completed in this run: proposer mutation/deploy, blocked solely by invalid external Anthropic credentials.
