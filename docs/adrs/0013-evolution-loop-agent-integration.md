# ADR-0013: Evolution Loop Agent Integration

- Status: Accepted
- Date: 2026-02-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar authorization for agents
  - ADR-0006: Spec-aware agent interface for MCP
  - ADR-0008: Agent governance UX
  - `crates/temper-server/src/odata/write.rs` (404 gap fix)
  - `crates/temper-mcp/src/tools.rs` (evolution read methods)

## Context

Temper's evolution engine (trajectories, Sentinel, O-P-A-D-I records) is fully built. Trajectory entries are recorded automatically when actions fail in the dispatch layer (`state/dispatch.rs:600-661`). However, agents have **zero visibility** into evolution data.

Two gaps exist today:

1. **404 gap**: When an entity type doesn't exist at all (404 EntitySetNotFound), the failure is **not** recorded as a trajectory entry. The 404 happens in `resolve_entity_type_or_404` (`odata/write.rs:46-61`) before reaching the dispatch layer. These represent genuine unmet intents that the Evolution Engine should track.

2. **MCP visibility gap**: The MCP sandbox has no methods to read evolution data (trajectories, insights, evolution records, sentinel health). Agents can't observe the feedback that Temper already collects, so they can't close the feedback loop by discovering what's missing and proposing spec changes.

## Decision

### Sub-Decision 1: Auto-record 404s as trajectory entries at the server level

Add a new helper `resolve_entity_type_or_record_404` in `odata/write.rs` that records a `TrajectoryEntry` to the in-memory trajectory log when an entity set is not found. This covers both entity creation (POST to unknown set) and bound actions (POST action on unknown set) in `handle_odata_post`.

**Why this approach**: Recording at the server level means all clients (MCP agents, REST callers, future SDKs) benefit automatically. No special agent code needed. The trajectory entry uses the same `TrajectoryEntry` struct and `state.trajectory_log` as the dispatch layer, maintaining consistency.

### Sub-Decision 2: Expose 4 read-only MCP methods for evolution observability

Add four new methods to the MCP tool dispatch:

| Method | HTTP Endpoint | Purpose |
|--------|---------------|---------|
| `get_trajectories(tenant, entity_type?, failed_only?, limit?)` | `GET /observe/trajectories` | Read trajectory summaries with failed intents |
| `get_insights(tenant)` | `GET /observe/evolution/insights` | Read ranked insight records |
| `get_evolution_records(tenant, record_type?)` | `GET /observe/evolution/records` | Read O-P-A-D-I records |
| `check_sentinel(tenant)` | `POST /api/evolution/sentinel/check` | Trigger sentinel health check |

All methods are **read-only** — they query existing endpoints that the Observe UI already uses. No new write capabilities are added for agents.

**Why this approach**: Reuses existing HTTP endpoints rather than building new data paths. The endpoints are already tested and production-ready. Agents get the same view as the Observe UI.

### Sub-Decision 3: Document the natural evolution loop

Update the MCP agent skill documentation to describe how agents naturally participate in the evolution loop: try action → see failure → read trajectories → propose spec change (Cedar-gated) → retry.

**Why this approach**: No special modes or APIs needed. A single agent naturally discovers gaps and proposes fixes, with Cedar default-deny ensuring human approval at every step.

## Consequences

### Positive
- Agents can discover what entity types are missing (via trajectory data)
- Agents can see system-generated recommendations (via insights)
- The evolution feedback loop is fully closed: unmet intent → observation → insight → spec proposal → human approval → deployment
- All clients benefit from 404 trajectory recording, not just MCP agents

### Negative
- Slightly more work per 404 response in POST handlers (trajectory entry creation)
- Agents may generate noisy trajectory data with repeated 404s

### Risks
- Trajectory log could fill with 404 entries if an agent retries aggressively. Mitigated by the existing bounded ring-buffer (`TrajectoryLog` capacity limit).

### DST Compliance
- Uses `sim_now()` for trajectory timestamps (already the pattern in `dispatch.rs`)
- Uses `state.trajectory_log.write()` with `// ci-ok: infallible lock` annotation (matches existing pattern)
- No new `HashMap`, threads, or non-deterministic primitives introduced

## Non-Goals

- Agents cannot approve or deny evolution records — governance write methods remain blocked
- No new write methods added to MCP (only read-only evolution queries)
- PATCH/PUT/DELETE 404s are not recorded (these are less likely to represent unmet intents)

## Alternatives Considered

1. **Record 404s in middleware** — Would catch all 404s globally, but most 404s (bad URLs, typos) are not unmet intents. Recording only in POST handlers is more targeted.
2. **Add evolution write methods to MCP** — Rejected because it would bypass the Cedar governance model. Agents propose changes via `submit_specs`, which Cedar gates.
3. **Separate evolution API** — Rejected because the observe endpoints already exist and are tested. Adding a separate API would duplicate logic.
