# ADR-0039: Authorization Policy Traceability for Allow Decisions

- Status: Accepted
- Date: 2026-04-03
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-authz/src/engine/mod.rs` (Cedar evaluation)
  - `crates/temper-server/src/state/trajectory.rs` (trajectory log)
  - `crates/temper-store-turso/src/schema.rs` (persistent storage)
  - `crates/temper-server/src/observe/agents.rs` (history API)

## Context

Temper uses Cedar 4.9.1 for authorization. The Cedar SDK's `Authorizer::is_authorized()` returns a `Response` with `diagnostics().reason()` — an iterator of `PolicyId` values that contributed to the authorization decision. This works for **both** allow and deny outcomes.

Currently, Temper captures `reason()` only for **deny** decisions (storing the policy IDs in `AuthzDenial::PolicyDenied`). For allow decisions, the code discards diagnostics entirely:

```rust
Decision::Allow => AuthzDecision::Allow,  // policy IDs lost
```

This means the observe/history API can tell a consumer *that* an action was allowed, but not *which policy rule* allowed it. For governance dashboards (like OpenPaw's Factory Floor), this is a significant observability gap — operators cannot trace an individual agent action back to the specific Cedar policy that authorized it.

The data is already computed by Cedar during evaluation. We are paying the cost but discarding the result.

## Decision

### Sub-Decision 1: Capture permit policy IDs in `AuthzDecision::Allow`

Change the `Allow` variant from a unit variant to a struct variant carrying the permit policy IDs:

```rust
pub enum AuthzDecision {
    Allow { policy_ids: Vec<String> },
    Deny(AuthzDenial),
}
```

In the Cedar evaluation path, capture `diagnostics().reason()` for allow decisions the same way we already do for deny:

```rust
Decision::Allow => {
    let policy_ids: Vec<String> = response
        .diagnostics()
        .reason()
        .map(|id| id.to_string())
        .collect();
    AuthzDecision::Allow { policy_ids }
}
```

**Why this approach**: The Cedar SDK already computes and returns this information. We just need to stop discarding it. The struct variant is backward-compatible at the API level since `is_allowed()` still works. The policy IDs use the `{tenant}:{policy_id}:{idx}` naming convention we already assign in `reload_tenant_policies_named()`.

### Sub-Decision 2: Add `matched_policy_ids` to `TrajectoryEntry`

Add an optional field to capture the policy IDs for both allow and deny outcomes:

```rust
pub struct TrajectoryEntry {
    // ... existing fields ...
    pub matched_policy_ids: Option<Vec<String>>,
}
```

Persist as a JSON array string in the `trajectories` table:

```sql
ALTER TABLE trajectories ADD COLUMN matched_policy_ids TEXT;
```

**Why this approach**: Using `Option<Vec<String>>` with JSON serialization matches the existing pattern for nullable structured fields in trajectory entries. The TEXT column with JSON avoids schema complexity while remaining queryable via SQLite JSON functions if needed.

### Sub-Decision 3: Expose in observe/history API

The `/observe/agents/{agent_id}/history` and `/observe/agents/system/history` endpoints will include `matched_policy_ids` in their response. Consumers can cross-reference these IDs with the `/api/tenants/{tenant}/policies/list` endpoint to get full policy details (Cedar text, source, created_by).

## Rollout Plan

1. **Phase 0 (This PR)** — All changes ship together: enum change, trajectory field, storage migration, API exposure.
2. **Phase 1 (Follow-up)** — OpenPaw dashboard consumes the new field, showing exact policy matches in the session terminal drill-down.

## Consequences

### Positive
- Full authorization traceability: every action can be traced to the specific Cedar policy that allowed or denied it.
- Dashboard consumers can show policy source, creator, and Cedar text for any action without client-side guessing.
- Zero additional Cedar evaluation cost — we're capturing data that's already computed.

### Negative
- `AuthzDecision::Allow` is no longer a unit variant, requiring match arm updates across the codebase (~6 sites in production code + tests).
- Trajectory storage grows slightly (one extra JSON column per entry).

### Risks
- Cedar's `reason()` set for allow decisions may be empty when the allow is via a "default allow" policy. In practice, Temper uses default-deny, so permit policy IDs should always be present. The `Vec` handles the empty case gracefully.

## Non-Goals

- Modifying Cedar evaluation semantics or policy resolution order.
- Adding per-request policy evaluation metrics (latency, cache hit rate).
- Changing the `WasmAuthzDecision` enum (it remains a simple Allow/Deny enum in the WASM layer).

## Alternatives Considered

1. **Log policy IDs only, don't change the enum** — Simpler but loses the data for programmatic consumers. The whole point is API-level traceability, not just logs.
2. **Add a separate `AuthzTrace` struct alongside decisions** — More complex, requires threading a separate struct through the dispatch path. The enum variant change is minimal and self-contained.
