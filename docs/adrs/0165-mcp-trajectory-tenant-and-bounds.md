# ADR-0165: MCP trajectory is owned by the session identity and bounded

- Status: Accepted
- Date: 2026-08-15
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-mcp/src/runtime.rs` (MCP trajectory capture + upload)
  - ARN-222 (security finding)

> This is the landed version for ARN-222. It is based on Fable's arena entry
> (winner of the head-to-head), with one addition ported from the competing entry
> (#365): a bound on stdio frame size. #365 raised that gap but bounded it only
> *after* reading the whole line; this version bounds the allocation during the
> read (Sub-Decision 3).

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
- the **total** recorded text across the trajectory → capped at
  `MAX_TRAJECTORY_TOTAL_BYTES` (1.8 MB of metered serialized cost), kept under the
  server's 2 MiB ingest limit so a
  session that is within the per-turn and turn-count caps still cannot produce a
  trajectory the server rejects with 413 (which the client treats as non-retryable
  and would silently drop, suppressing the audit trail);
- the per-session `tenants_seen` map → capped in both distinct keys
  (`MAX_SEEN_KEYS`) **and** per-key byte length (`MAX_SEEN_KEY_BYTES`); an oversized
  key is dropped rather than retained, so 256 near-1-MiB keys cannot retain hundreds
  of MiB. The turn/byte budgets reset on re-`initialize`.

No code-derived value is placed in a request header. The previous, code-derived
`X-Entity-Type` header (which had no server-side reader) is removed: an illegal byte
such as `\n` in an attacker-controlled entity type would otherwise make the HTTP
client reject the whole upload and silently lose the trajectory. Only the session's
authenticated tenant and startup-config agent/session ids remain as headers.

The bounds and stdio framing live in `trajectory_bounds.rs` so they (and their
tests) are auditable in one place.

### Sub-Decision 3: Bounded stdio frames

`run_stdio_server` read JSON-RPC frames with `BufReader::lines()`, which buffers a
whole line into one allocation — a peer that never sends a newline could exhaust
memory before any parse. Frames are now read through `read_stdio_frame`, which caps
each frame at `MAX_STDIO_LINE_BYTES` (1 MiB) **during** the read: it never
allocates more than the budget plus one byte, drains an oversized frame to the next
newline in bounded chunks, drops it with a warning, and resynchronizes on the
following frame. Invalid UTF-8 frames are dropped rather than aborting the session.

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
- **Server-side tenant binding is already in place.** The
  `POST /api/ots/trajectories` handler keys storage on the typed
  `AuthenticatedRequestContext::tenant()` (`observe/evolution/trajectories.rs`), not
  the raw `X-Tenant-Id` header, and the bearer edge resolves the credential within
  the requested tenant (ADR-0157/ARN-187). So the storage boundary is enforced on
  both sides: this fix removes the client-side code-derived-tenant vector, and the
  server independently ignores an untrusted tenant header. The only residual is a
  bearer token that is valid in multiple tenants — orthogonal to ARN-222.
- Peak *transient* processing memory (an error string materialized by the sandbox
  before truncation, or actions/metadata parsed from a frame before the caps apply)
  is bounded by the 1 MiB stdio frame cap and the sandbox's own memory budget, then
  truncated before retention. Retained trajectory size is what this ADR bounds;
  reducing transient peaks further is not required to close the vector.

## Alternatives Considered
1. **Reject the session when code references another tenant.** Rejected: legitimate
   cross-tenant reads may be authorized server-side; the fix is to store under the
   authenticated identity and log the cross-tenant signal, not to block execution.
