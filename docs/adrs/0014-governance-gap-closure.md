# ADR-0014: Governance Gap Closure

- Status: Accepted
- Date: 2026-02-28
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar authorization for agents
  - ADR-0006: Spec-aware agent interface for MCP
  - ADR-0008: Agent governance UX
  - ADR-0013: Evolution loop agent integration
  - `crates/temper-server/src/odata/write.rs` (404 trajectory recording)
  - `crates/temper-mcp/src/tools.rs` (MCP method dispatch)
  - `crates/temper-server/src/wasm/` (WASM integration host)
  - `crates/temper-evolution/` (O-P-A-D-I record chain)

## Context

A dry-run of an agent sending email through Temper exposed systemic governance gaps. The agent bypassed the entire evolution loop — going straight to `submit_specs` then `create` then `action` without encountering an unmet intent, without the evolution engine proposing anything, and without Cedar gating any of it. Five specific gaps were identified:

1. **Policy management has zero auth** — agents can rewrite Cedar policies through management endpoints with no authorization check. An agent can grant itself any permission.
2. **Spec submission has zero auth** — agents deploy entity types immediately via `submit_specs` with no approval gate. New entity types go live the moment they are submitted.
3. **WASM HTTP denials are silent** — Cedar denies outbound HTTP calls from WASM integrations, but the denial is swallowed. No decision surfaces for the human developer. The agent sees a generic error with no path to resolution.
4. **Agent doesn't know when human approves** — denial responses are unstructured error strings. There are no decision IDs, no polling endpoints, no way for an agent to wait for human approval and retry.
5. **Evolution loop never fires** — trajectories are recorded (per ADR-0013), but no insights are generated from them. Spec proposals don't create evolution records. The O-P-A-D-I chain is wired but never activated in the agent flow.

The root cause is that Temper's governance was built entity-action-first. Entity actions (create, update, invoke) are fully Cedar-gated with PendingDecisions and trajectory recording. But the two other surfaces agents interact with — policy/spec management and WASM integrations — were added later without the same governance rigor.

## Decision

### Sub-Decision 1: Cedar-gate policy management endpoints

Add Cedar authorization to all policy management endpoints. Every call to create, update, or delete Cedar policies must pass through the authorization layer.

- **Principal**: The agent or user making the request
- **Action**: `manage_policies`
- **Resource**: `PolicySet` (scoped to tenant)

Default-deny means agents cannot modify policies unless explicitly permitted. This closes the privilege escalation vector where an agent could grant itself arbitrary permissions.

**Why this approach**: Reuses the existing Cedar authorization infrastructure. Policy management becomes just another governed action, consistent with how entity actions are already gated.

### Sub-Decision 2: Cedar-gate spec submission with trajectory recording

Add Cedar authorization to `submit_specs`. Every spec submission must pass through the authorization layer before the spec is compiled and deployed.

- **Principal**: The agent or user submitting the spec
- **Action**: `submit_specs`
- **Resource**: `SpecRegistry` (scoped to tenant)

When Cedar denies submission, create a `PendingDecision` that surfaces in the Observe UI for human review. Record a trajectory entry for the denied submission so the evolution engine can track spec proposal patterns.

**Why this approach**: Spec submission is the highest-leverage action an agent can take — it defines what the entire system can do. It must have the strongest governance gate. Recording denied submissions as trajectories means the evolution engine can detect patterns like "agent keeps trying to add email capabilities."

### Sub-Decision 3: Surface WASM authorization denials as PendingDecisions

When Cedar denies an outbound HTTP call from a WASM integration, create a `PendingDecision` instead of silently swallowing the denial. The PendingDecision includes:

- The denied action (e.g., `http_request`)
- The target resource (e.g., the URL or domain being called)
- The context (which entity action triggered the integration)
- A human-readable explanation of what the agent was trying to do

The PendingDecision surfaces in the Observe UI alongside entity-action decisions, giving humans a unified view of everything the system is asking permission for.

**Why this approach**: Silent denials are the worst governance outcome — the system is doing its job (denying unauthorized actions) but nobody knows about it. Surfacing denials as PendingDecisions means humans can make informed allow/deny decisions. The agent gets a structured denial response instead of a generic error.

### Sub-Decision 4: Return structured denial JSON from MCP methods

When any MCP method is denied by Cedar, return a structured JSON response instead of an error string:

```json
{
  "denied": true,
  "decision_id": "dec_a1b2c3",
  "reason": "Cedar policy denied action 'submit_specs' on resource 'SpecRegistry'",
  "pending_decision": true,
  "poll_hint": {
    "method": "get_decision_status",
    "args": { "decision_id": "dec_a1b2c3" },
    "suggested_interval_seconds": 5
  }
}
```

This gives agents a structured path to resolution: receive denial, extract `decision_id`, poll for human approval, retry on approval.

**Why this approach**: Agents are programs — they need machine-readable responses to act on. An error string like "permission denied" is a dead end. A structured denial with a decision ID and polling hint turns a dead end into a wait-and-retry loop. The agent can inform the user "waiting for approval" instead of failing silently.

### Sub-Decision 5: Automatic insight generation from trajectories

Trigger insight generation during the sentinel health check. When the sentinel runs, it analyzes trajectory entries and generates `InsightRecord`s for patterns like:

- Repeated 404s for the same entity type (unmet intent pattern)
- Repeated Cedar denials for the same action (permission gap pattern)
- Denied spec submissions that match existing 404 trajectories (agent trying to fill a known gap)

Insights are ranked by frequency and recency, surfaced through the existing `get_insights` MCP method (ADR-0013) and the Observe UI.

**Why this approach**: Trajectories are raw data. Insights are actionable recommendations. Without automatic insight generation, the trajectory data sits unused. The sentinel already runs periodic checks — generating insights during the check is a natural extension.

### Sub-Decision 6: O-A-D evolution record chain for spec proposals

When an agent proposes a spec change (via `submit_specs`), create an evolution record chain:

1. **O-Record** (Observation): Created from trajectory data — "entity type X was requested N times but doesn't exist"
2. **A-Record** (Action proposal): Created when the agent submits a spec — "agent proposes adding entity type X with these states and actions"
3. **D-Record** (Decision): Created when the human approves or rejects the proposal in the Observe UI

The O-Record links back to the trajectory entries that motivated it. The A-Record links to the O-Record and the submitted spec. The D-Record links to the A-Record and records the human's decision with rationale.

**Why this approach**: The evolution record chain creates a complete audit trail from "user tried something that didn't work" through "agent proposed a fix" to "human approved the fix." This is the core value proposition of governed evolution — every change is traceable to a real need and a human decision.

### Sub-Decision 7: Correlate 404 trajectories with spec proposals

Track the lifecycle of unmet intents by correlating 404 trajectory entries with subsequent spec proposals:

- When a 404 trajectory is recorded, mark it as an **open unmet intent**
- When a spec proposal includes the entity type from a 404 trajectory, link the proposal to the trajectory and mark the intent as **proposed**
- When the human approves the spec, mark the intent as **resolved**
- Surface open vs. proposed vs. resolved counts in the sentinel health check

**Why this approach**: Without correlation, the system records problems (404s) and solutions (spec proposals) independently with no way to measure whether the evolution loop is actually closing gaps. Correlation turns the evolution engine from a logging system into a gap-closure tracker.

## The Governed Creation Flow

This is the canonical pattern for how an agent creates new capabilities in Temper after all gaps are closed:

1. **Agent tries to create entity** → 404 (trajectory recorded as open unmet intent)
2. **Agent reads insights** → sees recommendation "entity type X requested N times"
3. **Agent proposes spec** → Cedar gates `submit_specs` → PendingDecision if denied
4. **Human reviews in Observe UI** → approves → D-Record created → spec deploys
5. **Agent retries creation** → succeeds
6. **Every step produces evolution records** (O-Record from trajectory, A-Record from proposal, D-Record from approval)

No step is skippable. Default-deny ensures the agent cannot proceed without human approval. The evolution record chain provides a complete audit trail.

## Governance Maturity Tiers

| Surface | Before | After | Gap |
|---------|--------|-------|-----|
| Entity actions (create, update, invoke) | Mature: Cedar-gated, PendingDecisions, trajectory recording | No change | None |
| WASM integrations (outbound HTTP) | Partial: Cedar-gated but denials are silent | Mature: PendingDecision on denial, structured denial response | Sub-Decision 3 |
| Policy management | Missing: no auth at all | Mature: Cedar-gated, audit trail | Sub-Decision 1 |
| Spec submission | Missing: no auth at all | Mature: Cedar-gated, PendingDecision, trajectory recording, evolution records | Sub-Decisions 2, 6 |
| Evolution loop | Partial: trajectories recorded but no insights generated | Mature: automatic insights, O-A-D records, intent correlation | Sub-Decisions 5, 6, 7 |

## Rollout Plan

1. **Phase 0 (Immediate)** — Cedar-gate policy management and spec submission (Sub-Decisions 1-2). These are the most critical security gaps. Structured denial JSON (Sub-Decision 4) ships alongside since it's needed for agents to handle the new denials.
2. **Phase 1 (Follow-up)** — WASM denial surfacing (Sub-Decision 3) and automatic insight generation (Sub-Decision 5). These improve the agent experience but are not security-critical.
3. **Phase 2 (Evolution completeness)** — O-A-D record chain (Sub-Decision 6) and 404 correlation (Sub-Decision 7). These complete the evolution loop audit trail.

## Readiness Gates

- All MCP methods return structured denial JSON (no unstructured error strings for Cedar denials)
- Policy management endpoints reject unauthorized requests (verified by integration test)
- Spec submission endpoints reject unauthorized requests (verified by integration test)
- WASM HTTP denials create PendingDecisions visible in Observe UI
- Sentinel health check generates insights from trajectory patterns
- At least one full governed creation flow succeeds end-to-end (404 → insight → proposal → approval → deploy → retry → success)

## Consequences

### Positive
- Default-deny means agents are blocked until a human explicitly permits each capability
- Every spec proposal creates a complete audit trail from unmet intent through human decision
- The evolution engine can detect patterns and recommend changes proactively
- Agents get structured responses that enable wait-and-retry instead of fail-and-stop
- All three governance surfaces (entity actions, WASM, policy/spec management) reach the same maturity level
- Humans get a unified Observe UI view of everything the system is requesting permission for

### Negative
- Agent workflows become slower — every new capability requires human approval before proceeding
- More PendingDecisions to review — humans may experience approval fatigue for routine operations
- Structured denial JSON adds complexity to the MCP response format

### Risks
- **Approval fatigue**: If agents generate many PendingDecisions, humans may rubber-stamp approvals without review. Mitigated by insight ranking — the sentinel highlights the most impactful decisions.
- **Polling storms**: Agents polling for decision status could create load. Mitigated by `suggested_interval_seconds` in the poll hint and server-side rate limiting.
- **Evolution record volume**: Every spec proposal creates three records (O, A, D). For active agents this could grow quickly. Mitigated by the existing bounded storage and retention policies.

### DST Compliance
- PendingDecision creation uses `sim_uuid()` for decision IDs and `sim_now()` for timestamps
- Insight generation runs in the sentinel check which already uses deterministic time
- Evolution records use the existing `sim_now()`/`sim_uuid()` patterns from `temper-evolution`
- No new `HashMap`, threads, or non-deterministic primitives introduced

## Non-Goals

- **Automatic approval rules** — All approvals are manual human decisions. Automation of approval is a future consideration, not part of this ADR.
- **Agent-to-agent delegation** — One agent cannot approve another agent's request. Only humans can approve PendingDecisions.
- **Retroactive governance** — Existing deployed specs and policies are not retroactively gated. This ADR governs new submissions going forward.
- **Real-time push notifications** — Agents poll for decision status. WebSocket or SSE push is out of scope.

## Alternatives Considered

1. **Allowlist-based governance** — Pre-approve certain actions (e.g., "agents can always submit specs for entity types in this list"). Rejected because it shifts governance from per-decision review to upfront configuration, which is harder to reason about and easier to get wrong. Default-deny with per-decision approval is simpler and safer.
2. **Separate governance service** — Extract all authorization into a standalone governance microservice. Rejected because Temper's Cedar integration is already embedded in the server layer. A separate service would add network hops and operational complexity without clear benefit at current scale.
3. **Optimistic submission with rollback** — Let agents deploy specs immediately but allow humans to roll back within a time window. Rejected because a deployed spec immediately affects production chat users. Even a brief window of unauthorized behavior is unacceptable for a governed platform.
4. **Webhook-based approval** — Notify humans via webhook instead of PendingDecisions in the Observe UI. Rejected because it adds an external dependency and doesn't integrate with the existing evolution record chain. PendingDecisions are the established pattern for human-in-the-loop governance.
