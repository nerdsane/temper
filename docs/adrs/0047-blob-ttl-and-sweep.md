# ADR-0047: Blob TTL + Lazy Sweep

- Status: Accepted
- Date: 2026-04-16
- Deciders: Temper core maintainers
- Supersedes: —
- Related:
  - ADR-0040: Blob-Backed Overflow for Large Entity Field Values
  - ADR-0045: Field-Overflow Inline Ceiling
  - ADR-0046: WASM Host Function for Blob-Ref Field Reads
  - `crates/temper-store-turso/src/schema.rs` (blobs table)
  - `crates/temper-store-turso/src/store/blobs.rs` (`put_blob`, new `put_blob_with_ttl`, `sweep_expired_blobs`)

## Context

ADR-0040 introduced content-addressed overflow blobs for oversize entity fields. ADR-0045 raised the inline ceiling to 128KB. ADR-0046 plumbed blob-ref bytes into the WASM invocation context. The combined system is functionally correct but has no lifecycle policy — every blob written to `field-overflow/sha256/...` lives forever.

The growth model is "one row per unique large value". Content-addressed dedupe helps, but across a paw-agent deployment that processes many distinct large payloads (judge inputs, tool-call dumps, web_fetch results once Phase 5 migrates off File entities), the `blobs` table grows without bound. Long-running production tenants already see multi-GB blob tables in practice.

Most overflow blobs fall into two categories with different retention needs:

- **Audit-trail fields** like `Session.user_message`, tool-call result blocks, foresight judge inputs/outputs. Value for replay and debugging extends for the life of the session and beyond.
- **Transient fields** like `WebQuery.result`, adapter `raw_output`, scratch dumps. Value ends the moment the consuming turn completes.

A global TTL is wrong for the first category and a global "keep forever" is wasteful for the second. The right shape is per-field declaration: apps opt fields into TTL explicitly, and the blob rows that correspond to those writes get an expiry.

## Decision

Add an `expires_at` column to the `blobs` table (nullable; `NULL` means permanent) and a `put_blob_with_ttl(key, bytes, ttl: Option<Duration>)` variant on `TursoEventStore`. Add a sweeper function `sweep_expired_blobs` that deletes rows where `expires_at IS NOT NULL AND expires_at < datetime('now')`. The default stays permanent — callers opt in per-write.

### Sub-Decision 1: Opt-in TTL with permanent default

`put_blob` keeps its existing signature and writes with `expires_at = NULL`. A new `put_blob_with_ttl(key, bytes, ttl: Option<Duration>)` takes the TTL. Existing call sites (`put_blob_bytes` → `put_blob`) are unchanged unless the caller explicitly wants expiry.

**Why permanent default**: most field-overflow blobs are audit-trail data (user prompts, tool-call results, judge inputs). Silent expiry on those would be data loss. Callers that know their field is transient (Phase 5's `WebQuery.result`, future adapter dumps) opt in. Matches the guidance in ADR-0040: blob-backed overflow is a safety net for accidental large fields, not a cache.

### Sub-Decision 2: Schema migration is additive and backwards-compatible

Pure `ALTER TABLE blobs ADD COLUMN expires_at TEXT` — SQLite accepts this against an existing table and fills the new column with `NULL` for all existing rows. Behavior for existing deployments: every prior blob keeps the permanent default they already had. The migration runs on every startup via a `try-and-ignore-duplicate-column` guard around the ALTER, so it is idempotent.

Index `CREATE INDEX IF NOT EXISTS idx_blobs_expires_at ON blobs(expires_at) WHERE expires_at IS NOT NULL` keeps the sweeper's predicate cheap without costing storage for permanent blobs (the partial index excludes `NULL`).

### Sub-Decision 3: Sweep is a callable, not an embedded cron

The first cut ships `sweep_expired_blobs` as an explicit method on `TursoEventStore`. It runs exactly the query `DELETE FROM blobs WHERE expires_at IS NOT NULL AND expires_at < datetime('now') LIMIT ?`, bounded by a caller-supplied max-delete budget (default 10_000). Operators or a future scheduler task invoke it on whatever cadence makes sense.

**Why not a bundled cron**: bundling adds a new always-on task to the server that runs even in tests and simulations. Opt-in matches ADR-0047's broader posture: nothing happens until a caller asks for it. A scheduler wrapper is trivial to add in a follow-up ADR once we have operational experience with the sweep's cost.

### Sub-Decision 4: `LIMIT`, not `DELETE ... ALL`

Each sweep deletes up to N rows and returns the deleted count. Callers re-invoke until the count is < N. This keeps any single sweep bounded, avoids long-held write locks on large backlogs, and composes with the existing `blob_io_semaphore` throttle. A million-row cleanup takes many short calls instead of one long-blocking one.

### Sub-Decision 5: Per-field declaration is explicitly out of scope here

Wiring `overflow_ttl_seconds` through the IOA spec parser → `TransitionTable` → `sync_fields` → `OverflowBlobWrite` → `put_blob_with_ttl` is the natural next step and the foreseen production consumer. It's deferred to a follow-up ADR (tentatively 0047b or 0048) because it touches `temper-spec`, `temper-jit`, and `temper-server` and is better reviewed as its own surface. This ADR ships only the storage primitive.

**Impact**: until the spec wiring lands, no field-overflow blob writer in production calls `put_blob_with_ttl`. Everything keeps writing permanent blobs, matching pre-ADR behavior. This is acceptable because the problem ADR-0047 fixes (unbounded growth of `field-overflow/sha256/...`) is not urgent — we have months of runway on the current dedup-only table.

## Rollout Plan

1. **Phase 4 (this ADR)** — Schema + `put_blob_with_ttl` + `sweep_expired_blobs`. Storage primitive only. Nothing calls it yet; no behavioral change in production.
2. **Phase 4b (follow-up ADR)** — Spec parser extension for `overflow_ttl_seconds` on `[[state]]` blocks. `OverflowBlobWrite` carries per-field TTL. `put_overflow_blobs` calls `put_blob_with_ttl`.
3. **Phase 5 (OpenPaw)** — `WebQuery.result` declares `overflow_ttl_seconds = "3600"` once 4b lands. First production caller of opt-in TTL.
4. **Phase 6 (future)** — Scheduler wrapper for `sweep_expired_blobs`. Probably a 6h-cadence task guarded by a feature flag.

## Consequences

### Positive

- Operators get a surgical knob for transient fields without opting into blanket expiry.
- Schema change is safe against existing deployments (all prior blobs survive; sweep never touches them).
- Sweep budget keeps locks short under load.
- Content-addressed dedupe still works: multiple fields writing the same bytes share one row. The earliest writer's TTL wins, so opt-in TTL can cut shared rows; this is the correct semantic for "if someone declared this field transient, its storage contract is transient everywhere."

### Negative

- Per-field declaration isn't here yet. Phase 5 can't declare TTL until 4b lands. Acceptable because Phase 5 retires an explicit File-entity workaround with a known short lifetime; leaving it at "permanent default" for one release is strictly better than today's File-entity-orphan problem.
- Two `put_blob` variants on the store — minor API-surface cost. Deprecating the no-TTL variant in a future cleanup is trivial.
- Sweep is manual until the scheduler ships. If Phase 5 lands without Phase 6, operators have to invoke sweep by hand or build their own wrapper. Acceptable for the near term.

### Risks

- A caller that declares TTL on a field whose values are actually audit-trail-relevant will lose data at expiry. Mitigation: the permanent default prevents accidental expiry, and the follow-up ADR that wires `overflow_ttl_seconds` into the spec parser must require the declaration to be explicit (no string-form boolean; no "inherit").
- Very large TTLs (e.g., 1 year) still pay the sweep-scan cost per cadence. `LIMIT` + partial index mitigate; measure in the Phase 6 rollout.

### DST Compliance

- `temper-store-turso` is persistence-layer, not simulation-visible. No `sim_now()` / `BTreeMap` concerns on the new code.
- `datetime('now')` in the INSERT/SELECT is SQL-side wall-clock; deterministic simulations don't use the real blob store. The `sweep` method is callable-only and simulation configs never invoke it, so no DST reviewer sign-off required for that surface.

## Non-Goals

- Scheduled cron for sweep (future Phase 6).
- Per-field TTL declaration in IOA specs (Phase 4b).
- Blob vacuum / SQLite space reclamation after delete (operator task; `VACUUM` is out of scope for an online sweep).
- Reference-counted GC (replaces pragmatic TTL-GC; only if we see real data-loss incidents).

## Alternatives Considered

1. **Global TTL on all blobs.** Rejected — wrong for audit-trail fields; would silently lose user data.
2. **TTL at the key-prefix level.** E.g., `field-overflow/sha256/*` gets 30d, `temper-fs/sha256:*` gets permanent. Rejected — different fields under the same prefix have different lifecycles; this conflates them.
3. **Embed sweep as a background task at server startup.** Rejected for the first cut — adds an always-on surface that needs simulation gating, DST review, and operational config; better to ship the primitive first and wrap it later once we know the cadence.
4. **Reference-counted deletion on entity archival.** Rejected — would correctly GC audit-trail blobs when the owning entity is archived, but costs a join on every entity lifecycle event and requires walking `fields` for blob refs on every archive. Over-engineered for the current scale.

## Rollback Policy

Two-step revert.

1. Remove the sweeper call site (none yet — no rollback needed).
2. Drop the `expires_at` column: `ALTER TABLE blobs DROP COLUMN expires_at`. libsql supports this in recent versions. Rows with declared TTL revert to permanent (correct outcome; we're removing the feature, not re-purposing expired rows).

If the column drop is impractical, leaving `expires_at` in place with no writers/sweepers is also safe — it's an inert column.
