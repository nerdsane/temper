# Golden Soaring Cerf Proof Report

## Scope

Implemented the plan from `~/.claude/plans/golden-soaring-cerf.md` in the dedicated worktree:

- Worktree: `/Users/seshendranalla/Development/temper/.claude/worktrees/golden-soaring-cerf`
- Branch: `worktree-golden-soaring-cerf`
- Required base branch: `feat/ticklish-weaving-tarjan`
- Verified merge-base: `64fe5b54353092349e66ebf18b8413ac32e369f0`

## Deliverables Implemented

### ADR

- `docs/adrs/0035-intent-discovery-evolution-loop.md`

### New OS App

- `os-apps/intent-discovery/specs/intent_discovery.ioa.toml`
- `os-apps/intent-discovery/csdl/intent_discovery.csdl.xml`
- `os-apps/intent-discovery/policies/intent_discovery.cedar`
- `os-apps/intent-discovery/skill.md`
- `os-apps/intent-discovery/wasm/gather_signals/src/lib.rs`
- `os-apps/intent-discovery/wasm/spawn_analyst/src/lib.rs`
- `os-apps/intent-discovery/wasm/create_proposals/src/lib.rs`
- `os-apps/intent-discovery/wasm/build.sh`

### Agent / Observability Changes

- `os-apps/temper-agent/prompts/evolution_analyst.md`
- `os-apps/temper-agent/specs/temper_agent.ioa.toml`
- `os-apps/temper-agent/wasm/llm_caller/src/lib.rs`
- `os-apps/temper-agent/wasm/tool_runner/src/lib.rs`
- `crates/temper-observe/src/otel.rs`

### Platform / Server Changes

- `crates/temper-server/src/api/mod.rs`
- `crates/temper-server/src/observe/evolution.rs`
- `crates/temper-server/src/observe/evolution/operations.rs`
- `crates/temper-server/src/observe/entities.rs`
- `crates/temper-server/src/observe/mod.rs`
- `crates/temper-server/src/observe/mod_test.rs`
- `crates/temper-server/src/state/policy_suggestions.rs`
- `crates/temper-store-turso/src/schema.rs`
- `crates/temper-store-turso/src/store/policy.rs`
- `crates/temper-platform/src/os_apps/mod.rs`
- `crates/temper-platform/src/os_apps/tests.rs`
- `os-apps/project-management/policies/issue.cedar`

## Final Architecture

### IntentDiscovery workflow

`IntentDiscovery` is the spec-governed orchestrator:

- `Trigger -> Gathering` via `gather_signals`
- `Gathering -> Analyzing` via `spawn_analyst`
- `Analyzing -> Proposing` via `create_proposals`
- `Proposing -> Complete`

### Real analyst execution

The analyst path now supports both:

- deterministic local `mock` runs
- real Anthropic-backed runs

For the real proof run, `IntentDiscovery` configured `TemperAgent` with:

- `provider = anthropic`
- `model = claude-sonnet-4-20250514`
- `tools_enabled = logfire_query`

### Logfire design

Logfire was implemented as a WASM-backed agent tool, not a Rust-only orchestration adapter.

The live flow was:

1. local Temper server exported telemetry to Logfire via `LOGFIRE_TOKEN`
2. `TemperAgent` invoked `logfire_query` through `tool_runner`
3. the agent fed Logfire evidence back into the next LLM turn
4. final analysis was materialized into records and PM issues

### Orchestration fix

The intent-shaped real-agent run exposed two orchestration defects:

Fix applied:

- added `GET /observe/entities/{entity_type}/{entity_id}/wait`
- changed `spawn_analyst` to use that bounded server-side wait endpoint instead of hot polling from WASM
- added `timeout_secs = "420"` to the `spawn_analyst` integration so the orchestrator can wait for a real multi-turn agent run instead of failing at the default 30 second WASM budget

### Intent-shaped changes completed

The five changes requested after the first shallow run are now implemented:

1. Redefined upstream evidence around `intent_evidence`, not just grouped errors.
2. Fed richer signals into `gather_signals`, including intent candidates, workaround patterns, abandonment patterns, plans, comments, and projects.
3. Split analyst output into `symptom_title`, `intent_title`, `recommended_issue_title`, and `problem_statement`.
4. Materialized PM issues from intent-shaped titles instead of raw operational symptoms.
5. Used Logfire as a real agent tool for evidence deepening, not just passive export/validation.

## Commands Executed

### WASM builds

```bash
bash os-apps/intent-discovery/wasm/build.sh
bash os-apps/temper-agent/wasm/build.sh
```

### Rust verification

```bash
cargo fmt --all
cargo check -p temper-server -p temper-cli -p temper-observe -p temper-platform
cargo test -p temper-store-turso
cargo test -p temper-platform
cargo test -p temper-server
```

### Real local proof server

```bash
TURSO_URL='file:/.../.tmp/intent-discovery-proof-intent-shaped-20260323-r5/intent-proof.db' \
TEMPER_VAULT_KEY='...' \
LOGFIRE_TOKEN='...' \
LOGFIRE_ENVIRONMENT='local' \
cargo run -p temper-cli -- serve \
  --port 3463 \
  --storage turso \
  --no-observe \
  --skill project-management \
  --skill temper-agent \
  --skill intent-discovery
```

### Real end-to-end proof harness

```bash
ANTHROPIC_TOKEN='...' \
LOGFIRE_READ_TOKEN='...' \
BASE='http://127.0.0.1:3463' \
LOGFIRE_QUERY_BASE='https://logfire-us.pydantic.dev' \
bash .tmp/intent-discovery-proof-intent-shaped-20260323-r5/run_proof.sh
```

## End-to-End Proof Result

Proof summary from `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/proof_summary.json`:

```json
{
  "discovery_id": "intent-discovery-019d1cad-bbe7-7e01-9efe-b314ab29697d",
  "analyze_response_status": "Analyzing",
  "entity_status": "Complete",
  "analyst_agent_id": "intent-analyst-intent-discovery-019d1cad-bbe7-7e01-9efe-b314ab29697d",
  "issues_created": 2,
  "records_created": 5,
  "issues_before": 1,
  "issues_after": 3,
  "evolution_record_total": 5,
  "finding_count": 2,
  "intent_titles_present": 2,
  "enable_titles": 1
}
```

## Verified Real-Agent Evidence

### Anthropic was actually called

From the live server log for the `r5` proof run:

- `llm_caller: calling Anthropic API, model=claude-sonnet-4-20250514, oauth=true, messages=1`
- `llm_caller: calling Anthropic API, model=claude-sonnet-4-20250514, oauth=true, messages=3`

### The agent actually used Logfire

From the live server log for the `r5` proof run:

- `tool_runner: executing tool 'logfire_query'`
- `tool_runner: querying Logfire, query_kind=alternate_success_paths`
- `tool_runner: querying Logfire, query_kind=intent_failure_cluster`
- `HandleToolResults`
- follow-up Anthropic turn after the tool results
- `RecordResult -> Completed`

### Local server actually posted to Logfire

From `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/logfire_probe.json`:

- recent `temper-platform` records were queryable from Logfire before analysis started

That proves both sides of the observability loop:

- local Temper wrote telemetry to Logfire
- the real analyst agent read Logfire back through `logfire_query`

## Verified Analysis Output

From `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/analysis.json`:

- finding 1:
  - `symptom_title`: `GenerateInvoice hits EntitySetNotFound on Invoice`
  - `intent_title`: `Enable invoice generation workflow`
  - `recommended_issue_title`: `Enable invoice generation workflow`
- finding 2:
  - `symptom_title`: `MoveToTodo denied with no matching permit policy`
  - `intent_title`: `Allow worker agents to transition issues to todo`
  - `recommended_issue_title`: `Allow worker agents to transition issues to todo`

The returned summary was:

- the billing workflow had an unmet intent surfaced through workaround evidence, not just raw `EntitySetNotFound`
- the issue workflow had a governance gap surfaced as a blocked workflow outcome, not just a denial string

## Verified Materialization Output

From `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/materialization_report.json`:

- `issues_created_count = 2`
- `records_created_count = 5`

Created issues:

- `Enable invoice generation workflow`
- `Allow worker agents to transition issues to todo`

Created evolution records:

- `5 total` in the successful `r5` run

Issue state after materialization, from `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/issues_after.json`:

- seed issue remained `Backlog`
- both new issues advanced to `Todo`

This is the key regression fix relative to the earlier real run: the created PM issues are now intent-shaped rather than error-shaped.

## Verified Intent Evidence

From `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/intent_evidence_before.json`:

- candidate 1: `Send An Invoice To The Customer`
  - had `workaround_count = 2`
  - had `abandonment_count = 2`
  - showed failed `GenerateInvoice` followed by successful `CreateDraft`
- candidate 2: `Allow issue to reach todo`
  - had `authz_denials = 3`
  - had `abandonment_count = 1`
  - showed repeated `MoveToTodo` denials

That proves the run was no longer naming work directly from raw error strings. The upstream evidence already expressed unmet outcomes, workaround patterns, and abandonment patterns before the model produced findings.

## Build / Test Results

### WASM builds

- `IntentDiscovery` WASM build: passed
- `TemperAgent` WASM build: passed

### Cargo check

- `cargo check -p temper-server -p temper-cli -p temper-observe -p temper-platform`: passed

### Rust suites

- `cargo test -p temper-store-turso`: 14 passed, 0 failed
- `cargo test -p temper-platform`: 213 passed, 0 failed
- `cargo test -p temper-server`: 303 passed, 0 failed

Total verified tests after final fixes: `530 passed, 0 failed`

## Remaining Limitations

- The proof dataset is still synthetic. The run is real, but the seeded signals were intentionally constructed local examples rather than long-horizon production history.
- Intent inference upstream is still heuristic. It uses explicit `intent`, `session_id`, action sequences, workaround detection, abandonment detection, and authz/error clustering, but it is not yet learning latent intents from arbitrary free-form user behavior.
- Logfire is a tool the agent can query for deeper evidence; it is not yet the primary storage/query layer for all intent mining. The first pass still comes from local Temper evidence and then the agent drills into Logfire selectively.
- `sandbox_provisioner` still falls back around the missing `Workspaces` entity set. That noise is no longer dominating the findings, but the platform gap still exists.
- There is still no first-class Temper environment model beyond passing `LOGFIRE_ENVIRONMENT=local` and tagging traces with the local deployment environment.

## Definition Of Done

- [x] ADR written for the IntentDiscovery evolution loop
- [x] `IntentDiscovery` IOA spec, CSDL, policy, and skill added
- [x] `gather_signals`, `spawn_analyst`, and `create_proposals` WASM modules implemented
- [x] evolution analyst prompt added for `TemperAgent`
- [x] `POST /api/evolution/analyze` implemented
- [x] policy denial suggestions persisted to Turso and surfaced to analysis
- [x] project management Cedar policies widened for system-driven issue materialization
- [x] real Anthropic-backed analyst run executed locally
- [x] local server exported telemetry to Logfire
- [x] analyst agent queried Logfire through a WASM-backed tool
- [x] `IntentDiscovery` reached `Complete` in the real run
- [x] real run created PM issues and evolution records
- [x] orchestration bug fixed with bounded wait endpoint for terminal agent state
- [x] build, check, and Rust test verification completed after final fixes
- [ ] GIF / screencast recorded

## Remaining Non-Code Follow-Up

The plan requested a GIF / screencast for a tweet demo. That artifact was not produced in this terminal-only implementation run.

## Evidence Files

- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/proof_summary.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/intent_discovery_entity.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/intent_discovery_history.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/analyst_agent.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/analysis.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/materialization_report.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/issues_after.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/evolution_records_after.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/intent_evidence_before.json`
- `.tmp/intent-discovery-proof-intent-shaped-20260323-r5/logfire_probe.json`
- `.tmp/intent-discovery-proof-real-20260323-r2/run_proof.sh`
