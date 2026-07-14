# ADR-0163: Feature-Request Reads Are Idempotent

## Status

Accepted (2026-07-13)

(Numbered 0163: 0156–0162 are claimed by concurrently open arena branches.)

## Context

`GET /observe/evolution/feature-requests` regenerates feature requests from
trajectory gap analysis on every read and persists what it generates. Every
generated record minted a fresh UUID-suffixed id (`RecordHeader::new`), so
the store "upsert" — keyed on that id — inserted a NEW row on every GET, and
a fresh `FR-{uuid}` system entity was dispatched per generated record per
GET. Reads spawned unbounded duplicates (ARN-240). The upsert also
overwrote `disposition` and `developer_notes` with generator defaults, so a
developer's WontFix could be silently reset by any read racing a re-listing.

## Decision

1. **Identity is derived from content, not minted.** A feature request's id
   is a stable hash of its gap group key — `FR-{sha256(action, error_pattern)
   [..12]}`. The same gap always maps to the same record, making generation
   idempotent by construction rather than by bookkeeping.
2. **Insert and update are separated, and developer-owned fields are only
   ever written on insert.** `upsert_feature_request` first attempts
   `INSERT … ON CONFLICT (id) DO NOTHING`; if the row already existed, an
   UPDATE refreshes only the generator-owned fields (category, description,
   frequency, trajectory_refs, updated_at). `disposition` and
   `developer_notes` belong to the developer after creation and are never
   touched by re-generation. The method returns whether it inserted.
3. **System entities are created once, keyed by the record id.** The handler
   dispatches `CreateFeatureRequest` only when the store reports a fresh
   insert, and the entity id IS the deterministic record id (the previous
   code minted a second, unrelated `FR-{uuid}` per GET).

## Consequences

- A GET is now a read: repeating it changes nothing. The regeneration that
  runs inside it converges on the same rows and skips entity creation for
  anything already known.
- Existing duplicate rows from the previous behavior are not migrated —
  they remain until dispositioned. (A cleanup migration was considered and
  rejected: rows may carry developer notes; deletion is a human decision.)
- Two concurrent GETs race benignly: both compute the same id, one wins the
  insert, the other's `DO NOTHING` reports "existed" and skips the entity.
- **Entity dispatch is at-most-once with no reconciliation.** If the process
  dies between the successful insert and the entity dispatch — or the
  dispatch fails (it is warn-only) — the system entity for that record is
  never created: every later GET sees an existing row and skips. The
  previous behavior was strictly worse (a new entity per read), so this is
  accepted here; a reconciliation sweep (create entities for rows whose
  journal is empty) is the follow-up if the entity plane becomes
  load-bearing.
- The frequency/description of an existing record now tracks the latest
  generation window rather than accumulating forever — which is what the
  listing already claimed to show.

## Alternatives Considered

- **Moving generation out of the GET entirely** (event-driven, e.g. on
  trajectory write or sentinel schedule): the cleaner long-term shape —
  reads should not write at all — but it changes when insights appear and
  belongs with the broader evolution-engine scheduling work. The
  deterministic identity fix is required under any scheduling model, and
  once it is in, the in-GET generation is harmless (idempotent).
- **Deduplicating in the generator against existing rows** (read-modify-
  write): racy without a transaction spanning generation, and still leaves
  identity random — the class survives anywhere a second writer appears.
