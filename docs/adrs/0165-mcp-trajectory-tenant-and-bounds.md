# ADR-0165: MCP trajectory is owned by the session identity and bounded

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-mcp/src/runtime.rs` (MCP trajectory capture + upload)
  - ARN-222 (security finding)

> This is Fable's competing entry for ARN-222; compared head-to-head by the arena judge.

## Context

The MCP client captures an OTS trajectory of each session's `execute` turns and
uploads it to the server (`runtime.rs`). Two problems (ARN-222):

1. **Tenant mixing.** The trajectory is uploaded with
   `X-Tenant-Id: self.primary_tenant()`, and `primary_tenant()` returns the
   **most-referenced tenant in the executed code** (`tenants_seen`, populated from
   `extract_temper_call_metadata(code)`), falling back to the session identity only
   when no tenant appears in the code. The executed code is attacker-controlled, so
   a session authenticated as tenant A can inject `temper` calls referencing tenant
   B and cause its trajectory — containing A's session code and results — to be
   filed under **tenant B**. Trajectory storage is thus keyed by code content rather
   than by the authenticated session identity, mixing tenants.

2. **Unbounded code/results.** `record_execute_turn` records the full submitted
   `code` and the full execution `result` verbatim (`OTSMessageContent::text`) with
   no size cap, across an unbounded number of turns. A large or runaway session
   accumulates the whole thing in memory and uploads it, an unauthenticated
   memory/storage-exhaustion vector.

## Decision

### Sub-Decision 1: The trajectory belongs to the authenticated identity

The trajectory upload is keyed by `self.identity_tenant` — the session's
authenticated tenant — not by any code-derived tenant. Code content can never move
a trajectory into another tenant's storage. `tenants_seen` is retained only as an
observability signal: if the session's code referenced a tenant other than the
identity, that is logged (a cross-tenant-activity signal), but it does not
determine storage.

### Sub-Decision 2: Bounded capture

Every guest-controlled channel that feeds the trajectory is bounded, so total size
is bounded regardless of session input:
- recorded `code` / `result` text → truncated to `MAX_TRAJECTORY_TEXT_BYTES` (on a
  UTF-8 char boundary, marked);
- the decision's `error_type` → the same truncated text (not the raw error);
- the embedded `trajectory_actions` → capped in count (`MAX_TRAJECTORY_ACTIONS`) and
  collapsed to a summary when serialized size exceeds the text budget;
- the number of recorded turns → capped (`MAX_TRAJECTORY_TURNS`), further turns
  dropped with a warning;
- the per-session `tenants_seen` / `entity_types_seen` maps → capped in distinct
  keys (`MAX_SEEN_KEYS`).

## Consequences

### Positive
- A trajectory can only ever be stored under the session's authenticated tenant, so
  code content cannot cross tenant boundaries. Capture size is bounded, closing the
  memory/storage-exhaustion vector.

### Behavior
- Legitimate sessions are unaffected: their identity tenant is where their
  trajectory already belonged, and normal turn sizes are well under the caps.

### DST Compliance
- `temper-mcp` is not simulation-visible. The new logic is pure (string truncation,
  identity selection, a counter); no wall clock, threads, or ambient I/O added.

## Non-Goals / Follow-ups
- Redaction of secrets that a guest may print into a result is a separate content
  concern, tracked elsewhere. This ADR closes the tenant-attribution and unbounded
  size vectors.
- **Server-side authorization of the trajectory ingest route.** The
  `POST /api/ots/trajectories` handler keys storage on the `X-Tenant-Id` header
  without validating it against the bearer principal, so a raw HTTP caller with a
  valid token could still target another tenant's store. This fix closes the
  client-side code-derived-tenant vector (the disclosed ARN-222 issue); binding the
  header to the authenticated principal server-side is a worthwhile companion issue.
- The `tenants_seen` / `entity_types_seen` maps are populated from the full
  (un-truncated) `code`; their distinct-key growth is capped at `MAX_SEEN_KEYS`
  (existing keys still increment), so a single large code blob cannot insert
  unbounded unique keys.

## Alternatives Considered
1. **Reject the session when code references another tenant.** Rejected: legitimate
   cross-tenant reads may be authorized server-side; the fix is to store under the
   authenticated identity and log the cross-tenant signal, not to block execution.
