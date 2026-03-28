# GEPA End-to-End Proof (TemperAgent + OTS + Workflow Replay)

**Date**: 2026-03-23  
**Workspace**: `/Users/seshendranalla/Development/temper-gepa-tarjan`  
**Server**: `temper serve --port 4455 --storage turso --no-observe`  
**Primary tenant**: `gepa-live-fresh-20260319`  
**Primary run**: `EvolutionRun('evo-live-fresh-20260319-v4')`

## Scope and Constraint
- This document is the canonical live-proof report.
- It includes the full trajectory taxonomy and trigger semantics discussed in chat.
- GEPA naming and data-model naming are intentionally unchanged in this update.
- This report focuses on what was *actually* proven in live runs, and explicitly lists what did not work.

## GEPA Optimizer-Only Policy (2026-03-23 update)
- GEPA is now explicitly scoped to optimization of existing capability.
- Structural mutations are blocked in `gepa-proposer-agent`:
  - no entity rename/introduction/removal
  - no action add/remove
  - no state add/remove
- When a proposal implies net-new capability, proposer performs unmet-intent handoff:
  - emits `UnmetIntentHandoff` metadata in proposer output
  - best-effort POSTs to `/api/evolution/trajectories/unmet` for separate unmet-intent processing
- GEPA returns a no-op mutation (`MutatedSpecSource = original`) when the structural gate blocks mutation.
- `patterns.missing_capabilities` remains available in reflective data, but is routed to unmet-intent handoff rather than direct structural edits by GEPA.

## 2026-03-23 Full-Loop Re-Proof (Latest)
- **Tenant**: `gepa-live-20260323-121726`
- **Primary terminal run**: `EvolutionRun('evo-live-20260323-121726-v3')`
- **Artifacts dir**: `/tmp/gepa_run_20260323-121726`

### What was proven in this latest run
1. **Automatic verify/deploy path now works end-to-end** (no manual steering):
   - Terminal action chain:
     `Created -> Start -> SelectCandidate -> RecordEvaluation -> RecordDataset -> RecordMutation -> RecordVerificationPass -> RecordScore -> RecordFrontierAutoApprove -> Deploy`
   - Run status reached `Completed` with:
     - `VerificationReport = "verification passed: 4 levels passed"`
     - `DeploymentId = "gepa-deploy-evo-live-20260323-121726-v3-m1"`
2. **Unmet-intent handoff persists even when optimizer mutation path is allowed/continues**:
   - `UnmetIntentReport` present in run fields with `reported = 3, failed = 0`.
   - Reported intents included:
     - `Add action 'Reassign'`
     - `PromoteToCritical`
     - `Reassign`
   - These were also persisted into trajectory telemetry as `source=Platform` unmet records (visible in `/observe/trajectories`).
3. **Workflow-level GEPA path remained active**:
   - OTS was seeded by real `temper mcp` sessions (success/partial/failure).
   - `SelectCandidate` still omitted `TrajectoryActions`/`Trajectories`; replay consumed OTS auto-injected server-side.

### What was fixed during this cycle
- `EvolutionRun` automation:
  - Added `gepa-verify` module + `verify_candidate` trigger from `RecordMutation`.
  - Added `gepa-deploy` module + `deploy_candidate` trigger from auto-approve and manual approve paths.
  - `gepa-pareto` now emits dynamic callback action based on `AutonomyLevel` (`RecordFrontierAutoApprove` vs `RecordFrontier`).
- SDK-callback pitfall addressed:
  - `gepa-verify`, `gepa-deploy`, and `gepa-pareto` were moved to explicit callback action emission (not macro-default `callback`) so action dispatch works.
- Proposer unmet-intent behavior:
  - `gepa-proposer-agent` now reports unmet intents even when mutation proceeds.

### Still open (not yet fully proven fixed)
1. **OTS ID consistency issue remains**:
   - `flush_trajectory()` returned IDs still did not match IDs listed by `/api/ots/trajectories`.
   - This means row ID alignment between MCP flush/finalize and listed OTS rows is still not proven fixed in live evidence.
2. This is tracked as an active blocker for the “single stable OTS ID per session” guarantee.

## 2026-03-23 Bounded Re-Proof (Current)
- **Tenant**: `gepa-live-20260323-125726`
- **Primary run**: `EvolutionRun('evo-live-20260323-125726')`
- **Artifacts dir**: `/tmp/gepa_run_20260323-125726`

### What was proven in this run
1. **WASM + secret setup path worked**:
   - 12/12 module uploads succeeded (`wasm_upload_results.json`).
   - Tenant secret `anthropic_api_key` stored successfully (`put_secret_anthropic_code.txt = 204`).
2. **Real OTS generation path worked**:
   - Real `temper mcp` sessions produced success/partial/failed trajectories.
   - `flush_trajectory()` returned concrete trajectory IDs for each session.
3. **OTS row-vs-payload ID mismatch is fixed in this isolated DB run**:
   - `ots_id_consistency_summary.json`:
     - `total_rows = 3`
     - `matching_rows = 3`
     - `mismatching_rows = 0`
   - This shows persisted row `trajectory_id` now matches payload `$.trajectory_id`.

### What failed in this run
1. The GEPA run reached `Proposing` and then failed:
   - Event trail:
     - `Created -> Start -> SelectCandidate -> RecordEvaluation -> RecordDataset -> Fail`
   - Failure payload:
     - `error = "authorization denied for http_call: no matching permit policy"`
     - `integration = "propose_mutation"`
     - `authz_denied = true`
2. Because proposer failed at `Proposing`, this run did not reach:
   - `RecordMutation`
   - `RecordVerificationPass`
   - `RecordFrontierAutoApprove`
   - `Deploy`
3. This run therefore cannot be used to re-prove auto verify/deploy or unmet-intent persistence; those remain proven by the prior successful run (`evo-live-20260323-121726-v3`).

## 2026-03-23 Consolidated Full-Loop Re-Proof (All Three Aspects)
- **Tenant**: `gepa-live-20260323-134346`
- **Run**: `EvolutionRun('evo-live-20260323-134346')`
- **Artifacts dir**: `/tmp/gepa_run_20260323-134346`
- **Terminal status**: `Completed`

### End-to-end path proven in this run
- `Created -> Start -> SelectCandidate -> RecordEvaluation -> RecordDataset -> RecordMutation -> RecordVerificationPass -> RecordScore -> RecordFrontierAutoApprove -> Deploy`
- Final run fields include:
  - `VerificationReport = "verification passed: 4 levels passed"`
  - `DeploymentId = "gepa-deploy-evo-live-20260323-134346-m1"`
  - `UnmetIntentReport.attempted = 4`, `reported = 4`, `failed = 0`

### The three requested aspects are now proven together
1. **Unmet-intent storage during optimizer flow**
   - Missing-capability suggestions were surfaced by reflective/proposer and persisted via `/api/evolution/trajectories/unmet`.
   - Evidence:
     - `UnmetIntentReport` in final `EvolutionRun` fields with all reports successful.
     - DB rows (`trajectories`) with `source = Platform`, `intent != null` for this tenant (`4` rows).
   - This confirms unmet-intent handoff is not dropped when GEPA continues optimizer flow.

2. **OTS trajectory ID consistency (flush vs stored rows)**
   - `ots_id_consistency_summary.json` reports:
     - `total_rows = 3`
     - `matching_rows = 3`
     - `mismatching_rows = 0`
   - Flush IDs now match persisted OTS row payload IDs in this live run.

3. **No manual verification/deploy steering**
   - Verifier and deploy steps fired from integrations automatically and reached `Completed`.
   - No manual `RecordVerificationPass`, `Approve`, or `Deploy` action calls were needed.

### Root causes fixed to get this full loop green
1. **WASM Cedar authz tenant scope**
   - `CedarWasmAuthzGate` now uses tenant-scoped authorization (`authorize_for_tenant_or_bypass`) for `http_call` and `access_secret`.
2. **WASM HTTP policy context mismatch**
   - Evolution policy switched from `resource.domain` to `context.domain` for HTTP-call host checks.
3. **Internal API auth for proposer/verifier**
   - `gepa-proposer-agent` and `gepa-verify` now attach `Authorization: Bearer ...` using:
     - integration config (`temper_api_key = {secret:temper_api_key}`), plus
     - fallback `get_secret("temper_api_key")`.
4. **Policy permissions for proposer/verifier ops**
   - Added Cedar permits for:
     - `http_call` from `gepa-proposer-agent` and `gepa-verify` to localhost
     - `access_secret` for those modules
     - `write_trajectories` for proposer (`Agent::"gepa-proposer-agent"`) so unmet intents persist.
5. **State-machine terminal handling on verifier faults**
   - `EvolutionRun.Fail` now allows `from = "Verifying"` so verifier integration failures terminate cleanly instead of stalling.

### Remaining caveat observed (non-blocking for this proof)
- `sandbox_provisioner` logs `TemperFS setup failed: Workspace creation failed (HTTP 404)` during TemperAgent provisioning in this environment.
- Despite that warning, the GEPA run still completed end-to-end (proposer response returned, verifier passed, deploy completed).

## Executive Result
1. Real OTS trajectories were generated by real `temper mcp` sessions (no fabricated JSON).
2. `SelectCandidate` was executed without `TrajectoryActions` and without `Trajectories`; replay still consumed OTS from server-side auto-injection.
3. `gepa-replay` produced workflow-level results (`workflows[]`, `workflow_completion_rate`, `partial_adjusted_rate`) and action-level aggregates.
4. `gepa-reflective` produced workflow-level triplets and cross-trajectory patterns (missing capabilities, common failure points, successful patterns).
5. Latest consolidated run (`evo-live-20260323-134346`) completed end-to-end through verify, score, frontier update, and deploy.
6. Unmet-intent handoff now persists successfully during optimizer flow (`attempted=4`, `reported=4`, `failed=0`) while GEPA remains optimizer-only.
7. OTS row ID and payload trajectory ID matched for all seeded trajectories in the latest run (`3/3`).
8. Historical failures (proposer authz/401, verifier authz, `Fail` not valid from `Verifying`) are documented in prior sections and were resolved for the latest run.

## What "the run" means in this report
A "run" here means one full `EvolutionRun` entity state-machine attempt from `Start` through terminal state (`Completed` or `Failed`).

For the latest consolidated proof run `evo-live-20260323-134346`, the terminal path was:
- `Created -> Start -> SelectCandidate -> RecordEvaluation -> RecordDataset -> RecordMutation -> RecordVerificationPass -> RecordScore -> RecordFrontierAutoApprove -> Deploy -> Completed`

No manual trajectory payload was provided to `SelectCandidate`; OTS data came from tenant OTS storage.

## Trajectory Taxonomy (Current Project)

### 1. OTS trajectories (`ots_trajectories`)
- Purpose: full agent/session traces (turns, messages, decisions, consequences).
- Producer: MCP runtime (`TrajectoryBuilder`) auto-records each `execute` call turn.
- Upload paths:
  - End-of-session upload (`finalize_trajectory`)
  - Mid-session snapshot upload (`flush_trajectory`)
- Consumer in GEPA pipeline today:
  - `gepa-replay` gets OTS auto-injected when `SelectCandidate` does not provide trajectory params.
  - `gepa-reflective` works from replay output.

### 2. Entity/platform/authz trajectories (`trajectories`)
- Purpose: action/event telemetry per entity action (`source = Entity|Platform|Authz`, success/failure, authz denied, etc).
- Producer: entity dispatch and related platform/authz paths.
- Consumer in GEPA run today:
  - Not directly consumed by `gepa-replay` in `evaluate_candidate` (that path currently uses OTS injection for GEPA).
- Consumer elsewhere:
  - Observe/Evolution insight/sentinel pipelines.

### 3. Unmet intents
- Representation: unmet-intent signals are derived from trajectory data / failures (and can be recorded through evolution unmet endpoint path).
- Consumer today:
  - Observe/Evolution insight generation and sentinel monitoring.
- Consumer in GEPA run today:
  - Not directly wired into `gepa-replay`/`gepa-reflective` input payload for this run.

## Should OTS + entity/authz/unmet be merged right now?
Current behavior is intentionally separated:
- GEPA run path: OTS-centric (session workflow replay).
- Observe evolution path: trajectory/authz/unmet-intent analytics and sentinel records.

This report does **not** rename or merge those pipelines. It documents current behavior and limitations only.

## Triggering Model (Current State)

### What triggers evolution runs now
- Primary proven path in this report: manual `EvolutionRun.Start` + `SelectCandidate` action invocation.
- Sentinel path exists (`temper.check_sentinel(tenant)` / server sentinel check endpoint), but in this run it is not the reliable automatic launcher for the GEPA loop.

### What happened when sentinel was called live
- `temper.check_sentinel('gepa-live-fresh-20260319')` returned HTTP 500.
- Server logs show sentinel alerts were generated, but persistence hit `UNIQUE constraint failed: evolution_records.id` while writing multiple records in same check path.
- So sentinel currently has a real blocker in this environment.

## Real OTS Generation in this proof

### How the OTS rows were produced
All OTS rows below were produced by real MCP sessions (`temper mcp` with `execute` calls), not manual DB insertion.

Session patterns used:
1. Success workflow: `Assign -> Reassign`
2. Partial workflow: `Assign -> PromoteToCritical` (`PromoteToCritical` unknown)
3. Failed workflow: `Reassign` from `Backlog` (invalid transition)
4. Flush workflow: action turn -> `flush_trajectory()` -> action turn (same session, 3 turns)

### Important nuance found during live proof
- Tenant extraction for OTS upload is based on parsed calls.
- If calls use a variable (`tenant = ...`) instead of literal tenant string in `temper.action(...)`, uploader can fall back to `default` tenant.
- For this proof, final portfolio sessions were rerun with literal tenant strings to guarantee storage under `gepa-live-fresh-20260319`.

## How decisions/actions/reasons are extracted
1. MCP runtime records each execute turn as OTS:
   - user message = submitted code
   - assistant message = runtime result / error
   - decision.consequence.success = execution success/failure
2. Runtime extracts `trajectory_actions` from code and stores under `decision.choice.arguments.trajectory_actions`.
3. In replay:
   - It iterates OTS turn -> decision -> `choice.arguments.trajectory_actions` first.
   - If absent, it can fall back to parsing user code for action calls.
4. In reflective dataset:
   - It consumes replay workflows and outcomes.
   - Produces triplets + pattern summaries.

## Fresh E2E Run (`evo-live-fresh-20260319-v4`)

### Start/select invocation
- `Start` invoked with:
  - `SkillName = project-management`
  - `TargetEntityType = Issue`
  - `AutonomyLevel = auto`
- `SelectCandidate` invoked with:
  - `CandidateId`
  - `SpecSource`
- Omitted intentionally:
  - `TrajectoryActions`
  - `Trajectories`

### Observed status timeline
- `Evaluating`
- `Proposing`
- `Failed`

### Final failure reason
`TemperAgent Failed on retry 1: Anthropic API returned 401: invalid x-api-key`

## Workflow-level replay result from the fresh run
- `workflows_total = 8`
- `workflows_completed = 1`
- `workflows_partial = 3`
- `workflows_failed = 1`
- `workflows_empty = 3`
- `workflow_completion_rate = 0.2`
- `partial_adjusted_rate = 0.5`
- `actions_attempted = 8`
- `succeeded = 4`
- `success_rate = 0.5`
- `coverage = 0.875`

## Reflective dataset result from the fresh run
- `success_count = 1`
- `failure_count = 4`
- `workflow_counts = {completed:1, partial:3, failed:1}`
- `patterns.missing_capabilities = ["PromoteToCritical"]`
- `patterns.common_failure_points` includes repeated `Reassign` from `Backlog`
- `patterns.successful_patterns` includes preserved success pattern with `Assign`

## What worked
1. Real MCP-generated OTS capture and persistence.
2. Mid-session OTS flush API path (`flush_trajectory`) returns real trajectory IDs.
3. OTS auto-injection into `gepa-replay` when trajectory params are omitted.
4. Workflow-level replay and reflective outputs produced in-run.
5. TemperAgent proposer integration is invoked (reaches proposer stage).

## What did not work / current blockers
1. Anthropic auth for proposer failed (`401 invalid x-api-key`), so no mutation was produced in this run.
2. Sentinel check endpoint produced `500` due duplicate `evolution_records.id` collisions.
3. OTS row trajectory id and payload trajectory id are different values in storage (documented below); this can confuse artifact tracing if not explicitly mapped.
4. Outcome at OTS metadata level is often `success` even when inner decision consequence is failure; replay still classifies workflow failure correctly from decision/action-level errors.

## Architecture Diagram (Proven Path)
```text
MCP execute sessions
  -> OTS TrajectoryBuilder (turns/decisions)
  -> /api/ots/trajectories persisted
  -> EvolutionRun.Start
  -> SelectCandidate (without TrajectoryActions/Trajectories)
  -> server auto-injects OTS into gepa-replay trigger params
  -> gepa-replay (workflow outcomes + action stats)
  -> gepa-reflective (triplets + patterns)
  -> gepa-proposer-agent via TemperAgent
  -> FAILED in this run (Anthropic 401 invalid key)
```

## Data-Pipeline Diagram (Taxonomy)
```text
                        +-------------------------------+
                        | trajectories (Entity/Platform/Authz)
Actions/dispatch ------>| source-tagged action records  |----+
                        +-------------------------------+    |
                                                             | used by
                                                             v
                        +-------------------------------+   Observe evolution
                        | unmet intent / insight paths  |--- sentinel / insights
                        +-------------------------------+

MCP execute sessions ---> OTS (turn/message/decision traces) ---> GEPA replay -> reflective -> proposer
                                      ^
                                      |
                            flush_trajectory() snapshot
```

## Evidence: entity/authz/platform/unmet in this environment
- For `gepa-live-fresh-20260319`, `trajectories` table had only `source=Entity` rows in this proof run.
- Authz/platform trajectory rows exist in other tenants (captured separately below).
- `intent IS NOT NULL` rows count is `0` in this DB snapshot.

## Artifact Index
- OTS list (API): `/tmp/ots_fresh2_list.json`
- OTS row metadata (sqlite): `/tmp/ots_fresh2_rows_sqlite.json`
- OTS row-vs-payload trajectory IDs: `/tmp/ots_fresh2_row_vs_payload_ids.json`
- Full OTS examples:
  - `/tmp/ots_fresh2_success_full.json`
  - `/tmp/ots_fresh2_partial_full.json`
  - `/tmp/ots_fresh2_failed_full.json`
  - `/tmp/ots_fresh2_flushseq_full.json`
- Evolution run artifacts:
  - `/tmp/evo_live_fresh_v4_report.json`
  - `/tmp/evo_live_fresh_v4_final.json`
  - `/tmp/evo_live_fresh_v4_replay.json`
  - `/tmp/evo_live_fresh_v4_dataset.json`
- Auxiliary telemetry snapshots:
  - `/tmp/fresh_entity_traj_source_counts.json`
  - `/tmp/fresh_entity_traj_totals.json`
  - `/tmp/fresh_entity_traj_recent20.json`
  - `/tmp/trajectory_authz_platform_counts.json`
  - `/tmp/trajectory_unmet_intents_count.json`

---

## Appendix A: OTS Row vs Payload Trajectory IDs

```json
[{"row_trajectory_id":"019d087a-6c0d-7801-8f8e-e9955ebebe01","payload_trajectory_id":"019d087a-6c0d-7e40-a0b1-a5aefd7b87bb","created_at":"2026-03-19 23:42:14","turn_count":1},
{"row_trajectory_id":"019d087a-6c17-7be0-8413-40ff7c95bbfd","payload_trajectory_id":"019d087a-6c16-74b2-9094-5768718f8d71","created_at":"2026-03-19 23:42:14","turn_count":3},
{"row_trajectory_id":"019d087a-349e-7782-a1ba-1b7649495a7b","payload_trajectory_id":"019d087a-349d-7071-b3b0-301fc9464305","created_at":"2026-03-19 23:41:59","turn_count":1},
{"row_trajectory_id":"019d087a-34a3-7092-8fe7-904862e7baff","payload_trajectory_id":"019d087a-34a2-7cf1-a894-b4e50c0b0fd9","created_at":"2026-03-19 23:41:59","turn_count":1},
{"row_trajectory_id":"019d0879-90af-7f10-a572-6a6d7021dfb6","payload_trajectory_id":"019d0879-90af-7922-a5ea-b08864af0ca9","created_at":"2026-03-19 23:41:17","turn_count":1},
{"row_trajectory_id":"019d0874-845a-7a71-a9fd-023f18d71474","payload_trajectory_id":"019d0874-8459-7352-b4d4-e1cfc83f456b","created_at":"2026-03-19 23:35:47","turn_count":1},
{"row_trajectory_id":"019d0874-451c-7370-9d82-0a110cd8507b","payload_trajectory_id":"019d0874-451a-7e12-b13d-9fd40c41f1e2","created_at":"2026-03-19 23:35:30","turn_count":1},
{"row_trajectory_id":"019d0872-e05e-7430-8e89-32f8e4c2e41d","payload_trajectory_id":"019d0872-e05d-7953-87c6-99fcf0b68da0","created_at":"2026-03-19 23:33:59","turn_count":1}]
```

## Appendix B: Full OTS Example (Success)

```json
{
  "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
  "version": "0.1.0",
  "metadata": {
    "task_description": "mcp-session",
    "timestamp_start": "2026-03-19T23:41:17.849216Z",
    "timestamp_end": "2026-03-19T23:41:17.871124Z",
    "duration_ms": 21.0,
    "agent_id": "unknown",
    "outcome": "success",
    "human_reviewed": false
  },
  "context": {},
  "turns": [
    {
      "turn_id": 1,
      "span_id": "019d0879-90ae-7e22-8c55-bb311785afdb",
      "timestamp": "2026-03-19T23:41:17.870853Z",
      "duration_ms": 0.0,
      "error": false,
      "messages": [
        {
          "message_id": "019d0879-90ae-7e22-8c55-bb4bf38d9ef8",
          "role": "user",
          "timestamp": "2026-03-19T23:41:17.870853Z",
          "content": {
            "type": "text",
            "text": "created = await temper.create(\"gepa-live-fresh-20260319\", \"Issues\", {\"Id\": \"issue-fresh2-success-1\", \"Title\": \"fresh2 ots success\", \"CreatedAt\": \"2026-03-19T00:00:00Z\", \"UpdatedAt\": \"2026-03-19T00:00:00Z\"})\nissue_id = created[\"entity_id\"]\na1 = await temper.action(\"gepa-live-fresh-20260319\", \"Issues\", issue_id, \"Assign\", {\"AgentId\": \"agent-success2-1\", \"Reason\": \"fresh2-success\"})\na2 = await temper.action(\"gepa-live-fresh-20260319\", \"Issues\", issue_id, \"Reassign\", {\"NewAssigneeId\": \"agent-success2-2\", \"Reason\": \"fresh2-success\"})\nreturn {\"issue_id\": issue_id, \"assign\": a1, \"reassign\": a2}"
          }
        },
        {
          "message_id": "019d0879-90ae-7e22-8c55-bb554dd55c01",
          "role": "assistant",
          "timestamp": "2026-03-19T23:41:17.870853Z",
          "content": {
            "type": "text",
            "text": "{\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\",\"Status\":\"Backlog\",\"AgentId\":\"agent-success2-1\",\"Reason\":\"fresh2-success\",\"assignee_set\":true},\"events\":[{\"action\":\"Created\",\"from_status\":\"\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.852263Z\",\"params\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\"}},{\"action\":\"Assign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.857935Z\",\"params\":{\"AgentId\":\"agent-success2-1\",\"Reason\":\"fresh2-success\"}}],\"total_event_count\":2,\"sequence_nr\":2,\"@odata.context\":\"$metadata#Issues/$entity\"},\"reassign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\",\"Status\":\"Backlog\",\"AgentId\":\"agent-success2-1\",\"Reason\":\"fresh2-success\",\"assignee_set\":true,\"NewAssigneeId\":\"agent-success2-2\"},\"events\":[{\"action\":\"Created\",\"from_status\":\"\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.852263Z\",\"params\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\"}},{\"action\":\"Assign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.857935Z\",\"params\":{\"AgentId\":\"agent-success2-1\",\"Reason\":\"fresh2-success\"}},{\"action\":\"Reassign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.865255Z\",\"params\":{\"NewAssigneeId\":\"agent-success2-2\",\"Reason\":\"fresh2-success\"}}],\"total_event_count\":3,\"sequence_nr\":3,\"@odata.context\":\"$metadata#Issues/$entity\"}}"
          }
        }
      ],
      "decisions": [
        {
          "decision_id": "019d0879-90ae-7e22-8c55-bb6f3c7b52a4",
          "decision_type": "tool_selection",
          "choice": {
            "action": "execute: created = await temper.create(\"gepa-live-fresh-20260319\", \"Issues\", {\"Id\": \"issue-fresh2-success-1\",",
            "arguments": {
              "trajectory_actions": [
                {
                  "action": "Assign",
                  "params": {
                    "AgentId": "agent-success2-1",
                    "Reason": "fresh2-success"
                  }
                },
                {
                  "action": "Reassign",
                  "params": {
                    "NewAssigneeId": "agent-success2-2",
                    "Reason": "fresh2-success"
                  }
                }
              ]
            }
          },
          "consequence": {
            "success": true
          }
        }
      ]
    }
  ]
}
```

## Appendix C: Full OTS Example (Partial)

```json
{
  "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
  "version": "0.1.0",
  "metadata": {
    "task_description": "mcp-session",
    "timestamp_start": "2026-03-19T23:41:59.826047Z",
    "timestamp_end": "2026-03-19T23:41:59.842733Z",
    "duration_ms": 16.0,
    "agent_id": "unknown",
    "outcome": "success",
    "human_reviewed": false
  },
  "context": {},
  "turns": [
    {
      "turn_id": 1,
      "span_id": "019d087a-34a2-7cf1-a894-b4ab375b2689",
      "timestamp": "2026-03-19T23:41:59.842551Z",
      "duration_ms": 0.0,
      "error": false,
      "messages": [
        {
          "message_id": "019d087a-34a2-7cf1-a894-b4b497f31915",
          "role": "user",
          "timestamp": "2026-03-19T23:41:59.842551Z",
          "content": {
            "type": "text",
            "text": "created = await temper.create(\"gepa-live-fresh-20260319\", \"Issues\", {\"Id\": \"issue-fresh2-partial-1\", \"Title\": \"fresh2 ots partial\", \"CreatedAt\": \"2026-03-19T00:00:00Z\", \"UpdatedAt\": \"2026-03-19T00:00:00Z\"})\nissue_id = created[\"entity_id\"]\na1 = await temper.action(\"gepa-live-fresh-20260319\", \"Issues\", issue_id, \"Assign\", {\"AgentId\": \"agent-partial2-1\", \"Reason\": \"fresh2-partial\"})\na2 = await temper.action(\"gepa-live-fresh-20260319\", \"Issues\", issue_id, \"PromoteToCritical\", {\"Reason\": \"fresh2-partial\"})\nreturn {\"issue_id\": issue_id, \"assign\": a1, \"promote\": a2}"
          }
        },
        {
          "message_id": "019d087a-34a2-7cf1-a894-b4ce46dae713",
          "role": "assistant",
          "timestamp": "2026-03-19T23:41:59.842551Z",
          "content": {
            "type": "text",
            "text": "RuntimeError: HTTP 409 Conflict: Unknown action: PromoteToCritical"
          }
        }
      ],
      "decisions": [
        {
          "decision_id": "019d087a-34a2-7cf1-a894-b4d28293ce24",
          "decision_type": "tool_selection",
          "choice": {
            "action": "execute: created = await temper.create(\"gepa-live-fresh-20260319\", \"Issues\", {\"Id\": \"issue-fresh2-partial-1\",",
            "arguments": {
              "trajectory_actions": [
                {
                  "action": "Assign",
                  "params": {
                    "AgentId": "agent-partial2-1",
                    "Reason": "fresh2-partial"
                  }
                },
                {
                  "action": "PromoteToCritical",
                  "params": {
                    "Reason": "fresh2-partial"
                  }
                }
              ]
            }
          },
          "consequence": {
            "success": false,
            "error_type": "RuntimeError: HTTP 409 Conflict: Unknown action: PromoteToCritical"
          }
        }
      ]
    }
  ]
}
```

## Appendix D: Full OTS Example (Failed)

```json
{
  "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
  "version": "0.1.0",
  "metadata": {
    "task_description": "mcp-session",
    "timestamp_start": "2026-03-19T23:41:59.825756Z",
    "timestamp_end": "2026-03-19T23:41:59.837842Z",
    "duration_ms": 12.0,
    "agent_id": "unknown",
    "outcome": "success",
    "human_reviewed": false
  },
  "context": {},
  "turns": [
    {
      "turn_id": 1,
      "span_id": "019d087a-349d-7071-b3b0-2fd8152835bc",
      "timestamp": "2026-03-19T23:41:59.837691Z",
      "duration_ms": 0.0,
      "error": false,
      "messages": [
        {
          "message_id": "019d087a-349d-7071-b3b0-2fed8b3e5341",
          "role": "user",
          "timestamp": "2026-03-19T23:41:59.837691Z",
          "content": {
            "type": "text",
            "text": "created = await temper.create(\"gepa-live-fresh-20260319\", \"Issues\", {\"Id\": \"issue-fresh2-failed-1\", \"Title\": \"fresh2 ots failed\", \"CreatedAt\": \"2026-03-19T00:00:00Z\", \"UpdatedAt\": \"2026-03-19T00:00:00Z\"})\nissue_id = created[\"entity_id\"]\na1 = await temper.action(\"gepa-live-fresh-20260319\", \"Issues\", issue_id, \"Reassign\", {\"NewAssigneeId\": \"agent-failed2-1\", \"Reason\": \"fresh2-failed\"})\nreturn {\"issue_id\": issue_id, \"reassign\": a1}"
          }
        },
        {
          "message_id": "019d087a-349d-7071-b3b0-2ff9a7fb3691",
          "role": "assistant",
          "timestamp": "2026-03-19T23:41:59.837691Z",
          "content": {
            "type": "text",
            "text": "RuntimeError: HTTP 409 Conflict: Action 'Reassign' not valid from state 'Backlog'"
          }
        }
      ],
      "decisions": [
        {
          "decision_id": "019d087a-349d-7071-b3b0-300b1bfc5d0f",
          "decision_type": "tool_selection",
          "choice": {
            "action": "execute: created = await temper.create(\"gepa-live-fresh-20260319\", \"Issues\", {\"Id\": \"issue-fresh2-failed-1\", ",
            "arguments": {
              "trajectory_actions": [
                {
                  "action": "Reassign",
                  "params": {
                    "NewAssigneeId": "agent-failed2-1",
                    "Reason": "fresh2-failed"
                  }
                }
              ]
            }
          },
          "consequence": {
            "success": false,
            "error_type": "RuntimeError: HTTP 409 Conflict: Action 'Reassign' not valid from state 'Backlog'"
          }
        }
      ]
    }
  ]
}
```

## Appendix E: Full OTS Example (Flush Sequence)

```json
{
  "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
  "version": "0.1.0",
  "metadata": {
    "task_description": "mcp-session",
    "timestamp_start": "2026-03-19T23:42:14.020954Z",
    "timestamp_end": "2026-03-19T23:42:14.038870Z",
    "duration_ms": 17.0,
    "agent_id": "unknown",
    "outcome": "success",
    "human_reviewed": false
  },
  "context": {},
  "turns": [
    {
      "turn_id": 1,
      "span_id": "019d087a-6c0d-7e40-a0b1-a56042316d07",
      "timestamp": "2026-03-19T23:42:14.029227Z",
      "duration_ms": 0.0,
      "error": false,
      "messages": [
        {
          "message_id": "019d087a-6c0d-7e40-a0b1-a57fac37a429",
          "role": "user",
          "timestamp": "2026-03-19T23:42:14.029227Z",
          "content": {
            "type": "text",
            "text": "issue_id = \"019d0879-909a-73b3-a811-9b0cbfb0b89b\"\na1 = await temper.action(\"gepa-live-fresh-20260319\", \"Issues\", issue_id, \"Assign\", {\"AgentId\": \"agent-flush2-1\", \"Reason\": \"fresh2-flush\"})\nreturn {\"issue_id\": issue_id, \"assign\": a1}"
          }
        },
        {
          "message_id": "019d087a-6c0d-7e40-a0b1-a5890bb3544f",
          "role": "assistant",
          "timestamp": "2026-03-19T23:42:14.029227Z",
          "content": {
            "type": "text",
            "text": "{\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\",\"Status\":\"Backlog\",\"AgentId\":\"agent-flush2-1\",\"Reason\":\"fresh2-flush\",\"assignee_set\":true,\"NewAssigneeId\":\"agent-success2-2\"},\"events\":[{\"action\":\"Created\",\"from_status\":\"\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.852263Z\",\"params\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\"}},{\"action\":\"Assign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.857935Z\",\"params\":{\"AgentId\":\"agent-success2-1\",\"Reason\":\"fresh2-success\"}},{\"action\":\"Reassign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.865255Z\",\"params\":{\"NewAssigneeId\":\"agent-success2-2\",\"Reason\":\"fresh2-success\"}},{\"action\":\"Assign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:42:14.025360Z\",\"params\":{\"AgentId\":\"agent-flush2-1\",\"Reason\":\"fresh2-flush\"}}],\"total_event_count\":4,\"sequence_nr\":4,\"@odata.context\":\"$metadata#Issues/$entity\"}}"
          }
        }
      ],
      "decisions": [
        {
          "decision_id": "019d087a-6c0d-7e40-a0b1-a594fd403bee",
          "decision_type": "tool_selection",
          "choice": {
            "action": "execute: issue_id = \"019d0879-909a-73b3-a811-9b0cbfb0b89b\"\na1 = await temper.action(\"gepa-live-fresh-20260319",
            "arguments": {
              "trajectory_actions": [
                {
                  "action": "Assign",
                  "params": {
                    "AgentId": "agent-flush2-1",
                    "Reason": "fresh2-flush"
                  }
                }
              ]
            }
          },
          "consequence": {
            "success": true
          }
        }
      ]
    },
    {
      "turn_id": 2,
      "span_id": "019d087a-6c0f-71b1-a2e6-1649d65bf242",
      "timestamp": "2026-03-19T23:42:14.031216Z",
      "duration_ms": 0.0,
      "error": false,
      "messages": [
        {
          "message_id": "019d087a-6c0f-71b1-a2e6-1652f1b67067",
          "role": "user",
          "timestamp": "2026-03-19T23:42:14.031216Z",
          "content": {
            "type": "text",
            "text": "return await temper.flush_trajectory()"
          }
        },
        {
          "message_id": "019d087a-6c0f-71b1-a2e6-1668edd708d1",
          "role": "assistant",
          "timestamp": "2026-03-19T23:42:14.031216Z",
          "content": {
            "type": "text",
            "text": "{\"trajectory_id\":\"019d087a-6c0d-7e40-a0b1-a5aefd7b87bb\",\"status\":\"flushed\"}"
          }
        }
      ],
      "decisions": [
        {
          "decision_id": "019d087a-6c0f-71b1-a2e6-167dc178be5c",
          "decision_type": "tool_selection",
          "choice": {
            "action": "execute: return await temper.flush_trajectory()"
          },
          "consequence": {
            "success": true
          }
        }
      ]
    },
    {
      "turn_id": 3,
      "span_id": "019d087a-6c16-74b2-9094-572b526c89ed",
      "timestamp": "2026-03-19T23:42:14.038658Z",
      "duration_ms": 0.0,
      "error": false,
      "messages": [
        {
          "message_id": "019d087a-6c16-74b2-9094-5731acf871f4",
          "role": "user",
          "timestamp": "2026-03-19T23:42:14.038658Z",
          "content": {
            "type": "text",
            "text": "issue_id = \"019d0879-909a-73b3-a811-9b0cbfb0b89b\"\na2 = await temper.action(\"gepa-live-fresh-20260319\", \"Issues\", issue_id, \"Reassign\", {\"NewAssigneeId\": \"agent-flush2-2\", \"Reason\": \"fresh2-flush\"})\nreturn {\"issue_id\": issue_id, \"reassign\": a2}"
          }
        },
        {
          "message_id": "019d087a-6c16-74b2-9094-574dad1e8c03",
          "role": "assistant",
          "timestamp": "2026-03-19T23:42:14.038658Z",
          "content": {
            "type": "text",
            "text": "{\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"reassign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\",\"Status\":\"Backlog\",\"AgentId\":\"agent-flush2-1\",\"Reason\":\"fresh2-flush\",\"assignee_set\":true,\"NewAssigneeId\":\"agent-flush2-2\"},\"events\":[{\"action\":\"Created\",\"from_status\":\"\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.852263Z\",\"params\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T00:00:00Z\",\"UpdatedAt\":\"2026-03-19T00:00:00Z\"}},{\"action\":\"Assign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.857935Z\",\"params\":{\"AgentId\":\"agent-success2-1\",\"Reason\":\"fresh2-success\"}},{\"action\":\"Reassign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:41:17.865255Z\",\"params\":{\"NewAssigneeId\":\"agent-success2-2\",\"Reason\":\"fresh2-success\"}},{\"action\":\"Assign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:42:14.025360Z\",\"params\":{\"AgentId\":\"agent-flush2-1\",\"Reason\":\"fresh2-flush\"}},{\"action\":\"Reassign\",\"from_status\":\"Backlog\",\"to_status\":\"Backlog\",\"timestamp\":\"2026-03-19T23:42:14.035170Z\",\"params\":{\"NewAssigneeId\":\"agent-flush2-2\",\"Reason\":\"fresh2-flush\"}}],\"total_event_count\":5,\"sequence_nr\":5,\"@odata.context\":\"$metadata#Issues/$entity\"}}"
          }
        }
      ],
      "decisions": [
        {
          "decision_id": "019d087a-6c16-74b2-9094-5752c170c6e6",
          "decision_type": "tool_selection",
          "choice": {
            "action": "execute: issue_id = \"019d0879-909a-73b3-a811-9b0cbfb0b89b\"\na2 = await temper.action(\"gepa-live-fresh-20260319",
            "arguments": {
              "trajectory_actions": [
                {
                  "action": "Reassign",
                  "params": {
                    "NewAssigneeId": "agent-flush2-2",
                    "Reason": "fresh2-flush"
                  }
                }
              ]
            }
          },
          "consequence": {
            "success": true
          }
        }
      ]
    }
  ]
}
```

## Appendix F: Full Replay Output (`gepa-replay`)

```json
{
  "action_results": [
    {
      "action": "Assign",
      "error": null,
      "error_kind": null,
      "from_state": "Backlog",
      "params": {
        "AgentId": "agent-flush2-1",
        "Reason": "fresh2-flush"
      },
      "success": true,
      "to_state": "Backlog",
      "trajectory_id": "019d087a-6c0d-7e40-a0b1-a5aefd7b87bb",
      "turn_index": 0
    },
    {
      "action": "Assign",
      "error": null,
      "error_kind": null,
      "from_state": "Backlog",
      "params": {
        "AgentId": "agent-flush2-1",
        "Reason": "fresh2-flush"
      },
      "success": true,
      "to_state": "Backlog",
      "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
      "turn_index": 0
    },
    {
      "action": "Reassign",
      "error": null,
      "error_kind": "invalid_transition",
      "from_state": "Backlog",
      "params": {
        "NewAssigneeId": "agent-flush2-2",
        "Reason": "fresh2-flush"
      },
      "success": false,
      "to_state": "Backlog",
      "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
      "turn_index": 2
    },
    {
      "action": "Reassign",
      "error": null,
      "error_kind": "invalid_transition",
      "from_state": "Backlog",
      "params": {
        "NewAssigneeId": "agent-failed2-1",
        "Reason": "fresh2-failed"
      },
      "success": false,
      "to_state": "Backlog",
      "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
      "turn_index": 0
    },
    {
      "action": "Assign",
      "error": null,
      "error_kind": null,
      "from_state": "Backlog",
      "params": {
        "AgentId": "agent-partial2-1",
        "Reason": "fresh2-partial"
      },
      "success": true,
      "to_state": "Backlog",
      "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
      "turn_index": 0
    },
    {
      "action": "PromoteToCritical",
      "error": "unknown action 'PromoteToCritical' in state 'Backlog'",
      "error_kind": "unknown_action",
      "from_state": "Backlog",
      "params": {
        "Reason": "fresh2-partial"
      },
      "success": false,
      "to_state": "Backlog",
      "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
      "turn_index": 0
    },
    {
      "action": "Assign",
      "error": null,
      "error_kind": null,
      "from_state": "Backlog",
      "params": {
        "AgentId": "agent-success2-1",
        "Reason": "fresh2-success"
      },
      "success": true,
      "to_state": "Backlog",
      "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
      "turn_index": 0
    },
    {
      "action": "Reassign",
      "error": null,
      "error_kind": "invalid_transition",
      "from_state": "Backlog",
      "params": {
        "NewAssigneeId": "agent-success2-2",
        "Reason": "fresh2-success"
      },
      "success": false,
      "to_state": "Backlog",
      "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
      "turn_index": 0
    }
  ],
  "action_stats": {
    "attempted": 8,
    "coverage": 0.875,
    "guard_pass_rate": 1.0,
    "guard_rejections": 0,
    "invalid_transitions": 3,
    "succeeded": 4,
    "success_rate": 0.5,
    "transition_validity": 0.625,
    "unknown_actions": 1
  },
  "actions_attempted": 8,
  "coverage": 0.875,
  "errors": [
    {
      "action": "Reassign",
      "error_kind": "invalid_transition",
      "from_state": "Backlog",
      "message": "spec evaluation failed",
      "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
      "turn_index": 2
    },
    {
      "action": "Reassign",
      "error_kind": "invalid_transition",
      "from_state": "Backlog",
      "message": "spec evaluation failed",
      "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
      "turn_index": 0
    },
    {
      "action": "PromoteToCritical",
      "error_kind": "unknown_action",
      "from_state": "Backlog",
      "message": "unknown action 'PromoteToCritical' in state 'Backlog'",
      "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
      "turn_index": 0
    },
    {
      "action": "Reassign",
      "error_kind": "invalid_transition",
      "from_state": "Backlog",
      "message": "spec evaluation failed",
      "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
      "turn_index": 0
    }
  ],
  "guard_pass_rate": 1.0,
  "guard_rejections": 0,
  "invalid_transitions": 3,
  "partial_adjusted_rate": 0.5,
  "per_action": {
    "Assign": {
      "attempted": 4,
      "guard_rejections": 0,
      "invalid_transitions": 0,
      "succeeded": 4,
      "unknown_actions": 0
    },
    "PromoteToCritical": {
      "attempted": 1,
      "guard_rejections": 0,
      "invalid_transitions": 0,
      "succeeded": 0,
      "unknown_actions": 1
    },
    "Reassign": {
      "attempted": 3,
      "guard_rejections": 0,
      "invalid_transitions": 3,
      "succeeded": 0,
      "unknown_actions": 0
    }
  },
  "succeeded": 4,
  "success_rate": 0.5,
  "transition_validity": 0.625,
  "unknown_actions": 1,
  "workflow_completion_rate": 0.2,
  "workflows": [
    {
      "action_results": [
        {
          "action": "Assign",
          "error": null,
          "error_kind": null,
          "from_state": "Backlog",
          "params": {
            "AgentId": "agent-flush2-1",
            "Reason": "fresh2-flush"
          },
          "success": true,
          "to_state": "Backlog",
          "trajectory_id": "019d087a-6c0d-7e40-a0b1-a5aefd7b87bb",
          "turn_index": 0
        }
      ],
      "action_sequence": [
        "Assign"
      ],
      "actions_attempted": 1,
      "actions_succeeded": 1,
      "actions_total": 1,
      "agent_goal": "success",
      "breakdown": null,
      "breakdown_point": null,
      "errors": [],
      "final_state": "Backlog",
      "outcome": "completed",
      "reasoning_chain": "turn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0",
      "trajectory_id": "019d087a-6c0d-7e40-a0b1-a5aefd7b87bb"
    },
    {
      "action_results": [
        {
          "action": "Assign",
          "error": null,
          "error_kind": null,
          "from_state": "Backlog",
          "params": {
            "AgentId": "agent-flush2-1",
            "Reason": "fresh2-flush"
          },
          "success": true,
          "to_state": "Backlog",
          "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
          "turn_index": 0
        },
        {
          "action": "Reassign",
          "error": null,
          "error_kind": "invalid_transition",
          "from_state": "Backlog",
          "params": {
            "NewAssigneeId": "agent-flush2-2",
            "Reason": "fresh2-flush"
          },
          "success": false,
          "to_state": "Backlog",
          "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
          "turn_index": 2
        }
      ],
      "action_sequence": [
        "Assign",
        "Reassign"
      ],
      "actions_attempted": 2,
      "actions_succeeded": 1,
      "actions_total": 2,
      "agent_goal": "success",
      "breakdown": {
        "action": "Reassign",
        "error_kind": "invalid_transition",
        "from_state": "Backlog",
        "message": "spec evaluation failed",
        "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
        "turn_index": 2
      },
      "breakdown_point": {
        "action": "Reassign",
        "error_kind": "invalid_transition",
        "from_state": "Backlog",
        "message": "spec evaluation failed",
        "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
        "turn_index": 2
      },
      "errors": [
        {
          "action": "Reassign",
          "error_kind": "invalid_transition",
          "from_state": "Backlog",
          "message": "spec evaluation failed",
          "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
          "turn_index": 2
        }
      ],
      "final_state": "Backlog",
      "outcome": "partial",
      "reasoning_chain": "turn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0\nturn 2: {\"trajectory_id\":\"019d087a-6c0d-7e40-a0b1-a5aefd7b87bb\",\"status\":\"flushed\"}\nturn 3: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"reassign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19",
      "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71"
    },
    {
      "action_results": [
        {
          "action": "Reassign",
          "error": null,
          "error_kind": "invalid_transition",
          "from_state": "Backlog",
          "params": {
            "NewAssigneeId": "agent-failed2-1",
            "Reason": "fresh2-failed"
          },
          "success": false,
          "to_state": "Backlog",
          "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
          "turn_index": 0
        }
      ],
      "action_sequence": [
        "Reassign"
      ],
      "actions_attempted": 1,
      "actions_succeeded": 0,
      "actions_total": 1,
      "agent_goal": "success",
      "breakdown": {
        "action": "Reassign",
        "error_kind": "invalid_transition",
        "from_state": "Backlog",
        "message": "spec evaluation failed",
        "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
        "turn_index": 0
      },
      "breakdown_point": {
        "action": "Reassign",
        "error_kind": "invalid_transition",
        "from_state": "Backlog",
        "message": "spec evaluation failed",
        "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
        "turn_index": 0
      },
      "errors": [
        {
          "action": "Reassign",
          "error_kind": "invalid_transition",
          "from_state": "Backlog",
          "message": "spec evaluation failed",
          "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
          "turn_index": 0
        }
      ],
      "final_state": "Backlog",
      "outcome": "failed",
      "reasoning_chain": "turn 1: RuntimeError: HTTP 409 Conflict: Action 'Reassign' not valid from state 'Backlog'",
      "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305"
    },
    {
      "action_results": [
        {
          "action": "Assign",
          "error": null,
          "error_kind": null,
          "from_state": "Backlog",
          "params": {
            "AgentId": "agent-partial2-1",
            "Reason": "fresh2-partial"
          },
          "success": true,
          "to_state": "Backlog",
          "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
          "turn_index": 0
        },
        {
          "action": "PromoteToCritical",
          "error": "unknown action 'PromoteToCritical' in state 'Backlog'",
          "error_kind": "unknown_action",
          "from_state": "Backlog",
          "params": {
            "Reason": "fresh2-partial"
          },
          "success": false,
          "to_state": "Backlog",
          "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
          "turn_index": 0
        }
      ],
      "action_sequence": [
        "Assign",
        "PromoteToCritical"
      ],
      "actions_attempted": 2,
      "actions_succeeded": 1,
      "actions_total": 2,
      "agent_goal": "success",
      "breakdown": {
        "action": "PromoteToCritical",
        "error_kind": "unknown_action",
        "from_state": "Backlog",
        "message": "unknown action 'PromoteToCritical' in state 'Backlog'",
        "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
        "turn_index": 0
      },
      "breakdown_point": {
        "action": "PromoteToCritical",
        "error_kind": "unknown_action",
        "from_state": "Backlog",
        "message": "unknown action 'PromoteToCritical' in state 'Backlog'",
        "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
        "turn_index": 0
      },
      "errors": [
        {
          "action": "PromoteToCritical",
          "error_kind": "unknown_action",
          "from_state": "Backlog",
          "message": "unknown action 'PromoteToCritical' in state 'Backlog'",
          "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
          "turn_index": 0
        }
      ],
      "final_state": "Backlog",
      "outcome": "partial",
      "reasoning_chain": "turn 1: RuntimeError: HTTP 409 Conflict: Unknown action: PromoteToCritical",
      "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9"
    },
    {
      "action_results": [
        {
          "action": "Assign",
          "error": null,
          "error_kind": null,
          "from_state": "Backlog",
          "params": {
            "AgentId": "agent-success2-1",
            "Reason": "fresh2-success"
          },
          "success": true,
          "to_state": "Backlog",
          "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
          "turn_index": 0
        },
        {
          "action": "Reassign",
          "error": null,
          "error_kind": "invalid_transition",
          "from_state": "Backlog",
          "params": {
            "NewAssigneeId": "agent-success2-2",
            "Reason": "fresh2-success"
          },
          "success": false,
          "to_state": "Backlog",
          "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
          "turn_index": 0
        }
      ],
      "action_sequence": [
        "Assign",
        "Reassign"
      ],
      "actions_attempted": 2,
      "actions_succeeded": 1,
      "actions_total": 2,
      "agent_goal": "success",
      "breakdown": {
        "action": "Reassign",
        "error_kind": "invalid_transition",
        "from_state": "Backlog",
        "message": "spec evaluation failed",
        "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
        "turn_index": 0
      },
      "breakdown_point": {
        "action": "Reassign",
        "error_kind": "invalid_transition",
        "from_state": "Backlog",
        "message": "spec evaluation failed",
        "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
        "turn_index": 0
      },
      "errors": [
        {
          "action": "Reassign",
          "error_kind": "invalid_transition",
          "from_state": "Backlog",
          "message": "spec evaluation failed",
          "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
          "turn_index": 0
        }
      ],
      "final_state": "Backlog",
      "outcome": "partial",
      "reasoning_chain": "turn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0",
      "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9"
    },
    {
      "action_results": [],
      "action_sequence": [],
      "actions_attempted": 0,
      "actions_succeeded": 0,
      "actions_total": 0,
      "agent_goal": "success",
      "breakdown": null,
      "breakdown_point": null,
      "errors": [],
      "final_state": "Backlog",
      "outcome": "empty",
      "reasoning_chain": "turn 1: {\"module_name\":\"gepa-replay\",\"sha256_hash\":\"b9ee1c39570c57f5e652063595787082b0cc7a3a2ddefd74fda6977a05900467\",\"size_bytes\":275659}",
      "trajectory_id": "019d0874-8459-7352-b4d4-e1cfc83f456b"
    },
    {
      "action_results": [],
      "action_sequence": [],
      "actions_attempted": 0,
      "actions_succeeded": 0,
      "actions_total": 0,
      "agent_goal": "success",
      "breakdown": null,
      "breakdown_point": null,
      "errors": [],
      "final_state": "Backlog",
      "outcome": "empty",
      "reasoning_chain": "turn 1: RuntimeError: temper.upload_wasm missing required argument `wasm_path` at position 2",
      "trajectory_id": "019d0874-451a-7e12-b13d-9fd40c41f1e2"
    },
    {
      "action_results": [],
      "action_sequence": [],
      "actions_attempted": 0,
      "actions_succeeded": 0,
      "actions_total": 0,
      "agent_goal": "success",
      "breakdown": null,
      "breakdown_point": null,
      "errors": [],
      "final_state": "Backlog",
      "outcome": "empty",
      "reasoning_chain": "turn 1: {\"tenant\":\"gepa-live-fresh-20260319\",\"project-management\":{\"app\":\"project-management\",\"tenant\":\"gepa-live-fresh-20260319\",\"added\":[\"Comment\",\"Cycle\",\"Issue\",\"Label\",\"Project\"],\"updated\":[],\"skipped\":[],\"status\":\"installed\"},\"evolution\":{\"app\":\"evolution\",\"tenant\":\"gepa-live-fresh-20260319\",\"added\":[\"EvolutionRun\",\"Sent",
      "trajectory_id": "019d0872-e05d-7953-87c6-99fcf0b68da0"
    }
  ],
  "workflows_attempted": 5,
  "workflows_completed": 1,
  "workflows_empty": 3,
  "workflows_failed": 1,
  "workflows_partial": 3,
  "workflows_total": 8
}```

## Appendix G: Full Reflective Dataset (`gepa-reflective`)

```json
{
  "entity_type": "Issue",
  "failure_count": 4,
  "patterns": {
    "common_failure_points": [
      {
        "action": "Reassign",
        "from_state": "Backlog",
        "occurrences": 3
      },
      {
        "action": "PromoteToCritical",
        "from_state": "Backlog",
        "occurrences": 1
      }
    ],
    "guard_friction": [],
    "missing_capabilities": [
      "PromoteToCritical"
    ],
    "successful_patterns": [
      {
        "actions": [
          "Assign"
        ],
        "trajectory_id": "019d087a-6c0d-7e40-a0b1-a5aefd7b87bb"
      }
    ]
  },
  "skill_name": "project-management",
  "success_count": 1,
  "triplets": [
    {
      "actions_succeeded": 0,
      "actions_total": 1,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'Reassign' to allow transition from 'Backlog' (add 'Backlog' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d087a-349d-7071-b3b0-301fc9464305' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: RuntimeError: HTTP 409 Conflict: Action 'Reassign' not valid from state 'Backlog'",
      "outcome": "failed",
      "output": "Outcome=failed, actions_succeeded=0/1, final_state=Backlog. First failure: action='Reassign' from_state='Backlog' error_kind='invalid_transition' message='spec evaluation failed'.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
      "turn_id": 2
    },
    {
      "actions_succeeded": 0,
      "actions_total": 0,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'unknown' to allow transition from 'unknown' (add 'unknown' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0874-8459-7352-b4d4-e1cfc83f456b' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"module_name\":\"gepa-replay\",\"sha256_hash\":\"b9ee1c39570c57f5e652063595787082b0cc7a3a2ddefd74fda6977a05900467\",\"size_bytes\":275659}",
      "outcome": "empty",
      "output": "Outcome=empty, actions_succeeded=0/0, final_state=Backlog.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d0874-8459-7352-b4d4-e1cfc83f456b",
      "turn_id": 5
    },
    {
      "actions_succeeded": 0,
      "actions_total": 0,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'unknown' to allow transition from 'unknown' (add 'unknown' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0874-451a-7e12-b13d-9fd40c41f1e2' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: RuntimeError: temper.upload_wasm missing required argument `wasm_path` at position 2",
      "outcome": "empty",
      "output": "Outcome=empty, actions_succeeded=0/0, final_state=Backlog.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d0874-451a-7e12-b13d-9fd40c41f1e2",
      "turn_id": 6
    },
    {
      "actions_succeeded": 0,
      "actions_total": 0,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'unknown' to allow transition from 'unknown' (add 'unknown' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0872-e05d-7953-87c6-99fcf0b68da0' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"tenant\":\"gepa-live-fresh-20260319\",\"project-management\":{\"app\":\"project-management\",\"tenant\":\"gepa-live-fresh-20260319\",\"added\":[\"Comment\",\"Cycle\",\"Issue\",\"Label\",\"Project\"],\"updated\":[],\"skipped\":[],\"status\":\"installed\"},\"evolution\":{\"app\":\"evolution\",\"tenant\":\"gepa-live-fresh-20260319\",\"added\":[\"EvolutionRun\",\"Sent",
      "outcome": "empty",
      "output": "Outcome=empty, actions_succeeded=0/0, final_state=Backlog.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d0872-e05d-7953-87c6-99fcf0b68da0",
      "turn_id": 7
    },
    {
      "actions_succeeded": 1,
      "actions_total": 2,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'Reassign' to allow transition from 'Backlog' (add 'Backlog' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d087a-6c16-74b2-9094-5768718f8d71' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0\nturn 2: {\"trajectory_id\":\"019d087a-6c0d-7e40-a0b1-a5aefd7b87bb\",\"status\":\"flushed\"}\nturn 3: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"reassign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19",
      "outcome": "partial",
      "output": "Outcome=partial, actions_succeeded=1/2, final_state=Backlog. First failure: action='Reassign' from_state='Backlog' error_kind='invalid_transition' message='spec evaluation failed'.",
      "preserve": false,
      "score": 0.5,
      "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
      "turn_id": 1
    },
    {
      "actions_succeeded": 1,
      "actions_total": 2,
      "entity_type": "Issue",
      "feedback": "FIX: Add [[action]] section 'PromoteToCritical' to the Issue spec with 'from' including 'Backlog' and a valid 'to' state.",
      "input": "Trajectory '019d087a-34a2-7cf1-a894-b4e50c0b0fd9' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: RuntimeError: HTTP 409 Conflict: Unknown action: PromoteToCritical",
      "outcome": "partial",
      "output": "Outcome=partial, actions_succeeded=1/2, final_state=Backlog. First failure: action='PromoteToCritical' from_state='Backlog' error_kind='unknown_action' message='unknown action 'PromoteToCritical' in state 'Backlog''.",
      "preserve": false,
      "score": 0.5,
      "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
      "turn_id": 3
    },
    {
      "actions_succeeded": 1,
      "actions_total": 2,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'Reassign' to allow transition from 'Backlog' (add 'Backlog' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0879-90af-7922-a5ea-b08864af0ca9' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0",
      "outcome": "partial",
      "output": "Outcome=partial, actions_succeeded=1/2, final_state=Backlog. First failure: action='Reassign' from_state='Backlog' error_kind='invalid_transition' message='spec evaluation failed'.",
      "preserve": false,
      "score": 0.5,
      "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
      "turn_id": 4
    },
    {
      "actions_succeeded": 1,
      "actions_total": 1,
      "entity_type": "Issue",
      "feedback": "PRESERVE: This workflow completed successfully (1 actions). Preserve this behavior and do not regress it.",
      "input": "Trajectory '019d087a-6c0d-7e40-a0b1-a5aefd7b87bb' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0",
      "outcome": "completed",
      "output": "Outcome=completed, actions_succeeded=1/1, final_state=Backlog.",
      "preserve": true,
      "score": 1.0,
      "trajectory_id": "019d087a-6c0d-7e40-a0b1-a5aefd7b87bb",
      "turn_id": 0
    }
  ],
  "verification_feedback": [],
  "workflow_completion_rate": 0.2,
  "workflow_counts": {
    "completed": 1,
    "failed": 1,
    "partial": 3
  },
  "workflow_triplets": [
    {
      "actions_succeeded": 0,
      "actions_total": 1,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'Reassign' to allow transition from 'Backlog' (add 'Backlog' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d087a-349d-7071-b3b0-301fc9464305' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: RuntimeError: HTTP 409 Conflict: Action 'Reassign' not valid from state 'Backlog'",
      "outcome": "failed",
      "output": "Outcome=failed, actions_succeeded=0/1, final_state=Backlog. First failure: action='Reassign' from_state='Backlog' error_kind='invalid_transition' message='spec evaluation failed'.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d087a-349d-7071-b3b0-301fc9464305",
      "turn_id": 2
    },
    {
      "actions_succeeded": 0,
      "actions_total": 0,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'unknown' to allow transition from 'unknown' (add 'unknown' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0874-8459-7352-b4d4-e1cfc83f456b' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"module_name\":\"gepa-replay\",\"sha256_hash\":\"b9ee1c39570c57f5e652063595787082b0cc7a3a2ddefd74fda6977a05900467\",\"size_bytes\":275659}",
      "outcome": "empty",
      "output": "Outcome=empty, actions_succeeded=0/0, final_state=Backlog.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d0874-8459-7352-b4d4-e1cfc83f456b",
      "turn_id": 5
    },
    {
      "actions_succeeded": 0,
      "actions_total": 0,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'unknown' to allow transition from 'unknown' (add 'unknown' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0874-451a-7e12-b13d-9fd40c41f1e2' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: RuntimeError: temper.upload_wasm missing required argument `wasm_path` at position 2",
      "outcome": "empty",
      "output": "Outcome=empty, actions_succeeded=0/0, final_state=Backlog.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d0874-451a-7e12-b13d-9fd40c41f1e2",
      "turn_id": 6
    },
    {
      "actions_succeeded": 0,
      "actions_total": 0,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'unknown' to allow transition from 'unknown' (add 'unknown' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0872-e05d-7953-87c6-99fcf0b68da0' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"tenant\":\"gepa-live-fresh-20260319\",\"project-management\":{\"app\":\"project-management\",\"tenant\":\"gepa-live-fresh-20260319\",\"added\":[\"Comment\",\"Cycle\",\"Issue\",\"Label\",\"Project\"],\"updated\":[],\"skipped\":[],\"status\":\"installed\"},\"evolution\":{\"app\":\"evolution\",\"tenant\":\"gepa-live-fresh-20260319\",\"added\":[\"EvolutionRun\",\"Sent",
      "outcome": "empty",
      "output": "Outcome=empty, actions_succeeded=0/0, final_state=Backlog.",
      "preserve": false,
      "score": 0.0,
      "trajectory_id": "019d0872-e05d-7953-87c6-99fcf0b68da0",
      "turn_id": 7
    },
    {
      "actions_succeeded": 1,
      "actions_total": 2,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'Reassign' to allow transition from 'Backlog' (add 'Backlog' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d087a-6c16-74b2-9094-5768718f8d71' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0\nturn 2: {\"trajectory_id\":\"019d087a-6c0d-7e40-a0b1-a5aefd7b87bb\",\"status\":\"flushed\"}\nturn 3: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"reassign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19",
      "outcome": "partial",
      "output": "Outcome=partial, actions_succeeded=1/2, final_state=Backlog. First failure: action='Reassign' from_state='Backlog' error_kind='invalid_transition' message='spec evaluation failed'.",
      "preserve": false,
      "score": 0.5,
      "trajectory_id": "019d087a-6c16-74b2-9094-5768718f8d71",
      "turn_id": 1
    },
    {
      "actions_succeeded": 1,
      "actions_total": 2,
      "entity_type": "Issue",
      "feedback": "FIX: Add [[action]] section 'PromoteToCritical' to the Issue spec with 'from' including 'Backlog' and a valid 'to' state.",
      "input": "Trajectory '019d087a-34a2-7cf1-a894-b4e50c0b0fd9' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: RuntimeError: HTTP 409 Conflict: Unknown action: PromoteToCritical",
      "outcome": "partial",
      "output": "Outcome=partial, actions_succeeded=1/2, final_state=Backlog. First failure: action='PromoteToCritical' from_state='Backlog' error_kind='unknown_action' message='unknown action 'PromoteToCritical' in state 'Backlog''.",
      "preserve": false,
      "score": 0.5,
      "trajectory_id": "019d087a-34a2-7cf1-a894-b4e50c0b0fd9",
      "turn_id": 3
    },
    {
      "actions_succeeded": 1,
      "actions_total": 2,
      "entity_type": "Issue",
      "feedback": "FIX: Update action 'Reassign' to allow transition from 'Backlog' (add 'Backlog' to the action's 'from' states or correct transition topology).",
      "input": "Trajectory '019d0879-90af-7922-a5ea-b08864af0ca9' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0",
      "outcome": "partial",
      "output": "Outcome=partial, actions_succeeded=1/2, final_state=Backlog. First failure: action='Reassign' from_state='Backlog' error_kind='invalid_transition' message='spec evaluation failed'.",
      "preserve": false,
      "score": 0.5,
      "trajectory_id": "019d0879-90af-7922-a5ea-b08864af0ca9",
      "turn_id": 4
    },
    {
      "actions_succeeded": 1,
      "actions_total": 1,
      "entity_type": "Issue",
      "feedback": "PRESERVE: This workflow completed successfully (1 actions). Preserve this behavior and do not regress it.",
      "input": "Trajectory '019d087a-6c0d-7e40-a0b1-a5aefd7b87bb' goal='success' for entity 'Issue'.\nReasoning chain:\nturn 1: {\"issue_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"assign\":{\"entity_type\":\"Issue\",\"entity_id\":\"019d0879-909a-73b3-a811-9b0cbfb0b89b\",\"status\":\"Backlog\",\"item_count\":0,\"counters\":{},\"booleans\":{\"assignee_set\":true},\"lists\":{},\"fields\":{\"Id\":\"issue-fresh2-success-1\",\"Title\":\"fresh2 ots success\",\"CreatedAt\":\"2026-03-19T0",
      "outcome": "completed",
      "output": "Outcome=completed, actions_succeeded=1/1, final_state=Backlog.",
      "preserve": true,
      "score": 1.0,
      "trajectory_id": "019d087a-6c0d-7e40-a0b1-a5aefd7b87bb",
      "turn_id": 0
    }
  ]
}```

## Appendix H: Entity/Authz/Platform Trajectory Counts

### `gepa-live-fresh-20260319` source counts
```json
[{"source":"Entity","n":29,"ok":25,"fail":4}]
```

### `gepa-live-fresh-20260319` totals
```json
[{"total":29,"authz_denied":0}]
```

### Cross-tenant authz/platform counts
```json
[{"tenant":"gepa-live-ots-temperagent-20260319","source":"Authz","n":34,"failures":34,"authz_denied":34},
{"tenant":"gepa-live-ots-temperagent-20260319","source":"Platform","n":18,"failures":16,"authz_denied":0},
{"tenant":"rita-agents","source":"Platform","n":6,"failures":2,"authz_denied":0},
{"tenant":"gepa-codex-liveproof-20260319","source":"Platform","n":4,"failures":4,"authz_denied":0},
{"tenant":"rita-agents","source":"Authz","n":4,"failures":4,"authz_denied":4},
{"tenant":"gepa-e2e-proof","source":"Platform","n":3,"failures":1,"authz_denied":0},
{"tenant":"gepa-e2e-proof","source":"Authz","n":2,"failures":2,"authz_denied":2},
{"tenant":"gepa-live-portfolio-20260319","source":"Platform","n":2,"failures":2,"authz_denied":0}]
```

### Unmet-intent row count snapshot
```json
[{"intents_rows":0}]
```

## Appendix I: Run Outcome Snapshot

```json
{
  "final_status": "Failed",
  "status_timeline": [
    {
      "at": "2026-03-19T23:44:35.280334+00:00",
      "status": "Evaluating"
    },
    {
      "at": "2026-03-19T23:44:35.810859+00:00",
      "status": "Proposing"
    },
    {
      "at": "2026-03-19T23:44:36.340279+00:00",
      "status": "Failed"
    }
  ],
  "has_replay": true,
  "has_dataset": true,
  "has_mutation": false,
  "has_scores": false,
  "has_frontier": false,
  "errors": []
}
```

## Appendix J: Relationship to previous proof docs
- This file supersedes ad-hoc notes and includes both:
  - end-to-end GEPA run proof artifacts, and
  - taxonomy/triggering clarifications requested in chat.
- Existing `docs/gepa-real-claude-live-proof-2026-03-19.md` is retained as a historical run log.
