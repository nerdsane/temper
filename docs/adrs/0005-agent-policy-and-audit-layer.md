# ADR-0005: Agent Policy & Audit Layer

## Status
Accepted

## Context
ADR-0004 established Cedar authorization at three levels (entity actions, WASM host functions, secret pre-filtering) and a three-tier policy lifecycle. But the system lacked the human-facing loop: when Cedar denied an agent's action, the denial was logged but invisible — no UI to surface it, no API to approve it, no mechanism to generate and reload policies from approvals.

The Agent OS vision requires a deny-by-default posture where denials surface to humans with scope options (narrow/medium/broad), and approved decisions become persistent Cedar policies. Without this, Cedar authorization is write-only: denials accumulate but never resolve.

## Decision

### Denial Interception at OData Dispatch

Intercept Cedar denials in `dispatch_bound_action()` after entity state is fetched but before action execution. This placement ensures resource attributes (entity state snapshot) are captured at denial time, enabling accurate Cedar policy generation.

Each denial creates two artifacts:
1. **PendingDecision** — stored in a bounded dedup log, broadcast via SSE for live dashboard updates
2. **TrajectoryEntry** — pushed to the trajectory log with `authz_denied: true` for long-term audit

### PendingDecision Data Model

Bounded in-memory log with 1,000-entry FIFO capacity:
- **Dedup key**: `tenant:agent_id:action:resource_type:resource_id` — prevents duplicate decisions for repeated denied retries
- **Dedup index**: `BTreeMap<String, String>` mapping dedup_key → decision ID (deterministic iteration order)
- **Status lifecycle**: `Pending` → `Approved` or `Denied`
- **Fields captured**: tenant, agent_id, action, resource_type, resource_id, resource_attrs snapshot, denial_reason, optional module_name

Uses `sim_now()` for timestamps and `sim_uuid()` for IDs (DST compliant).

### Policy CRUD API

| Method | Route | Purpose |
|--------|-------|---------|
| GET | `/api/tenants/{t}/policies` | Read current Cedar policy text |
| PUT | `/api/tenants/{t}/policies` | Replace all policies with validation + atomic reload |
| POST | `/api/tenants/{t}/policies/rules` | Append a single rule |

Policy updates are validated by dry-run loading into a Cedar engine before committing. Tenant policies are combined at load time — a single Cedar evaluator serves all tenants.

### Decision Approve/Deny with Scope-Based Cedar Generation

**Approve**: `POST /api/tenants/{t}/decisions/{id}/approve` with a `scope` parameter:

| Scope | Generated Cedar Rule |
|-------|---------------------|
| Narrow | `permit(principal == Agent::"{id}", action == Action::"{action}", resource == {Type}::"{id}");` |
| Medium | `permit(principal == Agent::"{id}", action == Action::"{action}", resource is {Type});` |
| Broad | `permit(principal == Agent::"{id}", action, resource is {Type});` |

On approval: generate Cedar rule → append to tenant policies → reload Cedar engine atomically. The agent can retry immediately.

**Deny**: `POST /api/tenants/{t}/decisions/{id}/deny` — marks the decision as denied with no policy change.

### SSE Streaming for Live Dashboard

`GET /api/tenants/{t}/decisions/stream` returns Server-Sent Events via a tokio broadcast channel. Events are filtered to the requesting tenant. Lagged receivers silently skip missed messages.

### Agent Audit Trail

Two new observe endpoints:
- `GET /observe/agents` — list all agents with summary stats (total actions, success rate, denial count, entity types, last active)
- `GET /observe/agents/{id}/history` — per-agent action timeline with denial highlights

Data derived from the existing trajectory log — no separate storage.

### Observe Dashboard Pages

Three new pages wired to these APIs:
- **Decisions**: pending decision cards with approve/deny actions, scope picker, SSE live updates, history table
- **Agents**: summary list with success rate badges, click-through to detail
- **Agent Detail**: action timeline with denial highlights

## Consequences

### Positive
- Cedar denials are no longer write-only — they surface to humans and resolve through approval
- Policy converges over time as agents exercise the system (deny-by-default bootstrapping)
- Agent audit trail enables compliance reporting and debugging
- SSE streaming provides real-time governance visibility without polling

### Negative
- In-memory bounded log (1K entries) means old unresolved decisions are evicted under load
- Combined tenant policies in a single Cedar engine means one tenant's invalid policy blocks all tenants' reloads
- Dashboard requires the observe Next.js app to be running alongside the Temper server

### DST Compliance
- `BTreeMap` for dedup index (deterministic iteration), `VecDeque` for bounded log
- `sim_now()` / `sim_uuid()` for all timestamps and IDs
- SSE uses tokio broadcast (passive subscribers, no spawned tasks)
- `RwLock` unwraps annotated with `// ci-ok: infallible lock`
