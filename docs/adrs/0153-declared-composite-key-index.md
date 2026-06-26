# ADR-0153: Declared Composite-Key Index (A Negative-Existence Access Path)

- Status: Proposed
- Date: 2026-06-23
- Deciders: Temper core maintainers
- Related:
  - ADR-0091: Query projection diff index upserts
  - ADR-0142: Dispatch acknowledges after projection
  - ADR-0148: Bound derived writes off the dispatch hot path
  - ADR-0134: Query plane read contract
  - ARN-68 (the 413 / QueryTooLarge issue), ARN-89 (read-after-write reconcile), ARN-102 (3-year runtime vision)
  - `crates/temper-server/src/odata/query_plane_read/{types.rs,mod.rs}`
  - `crates/temper-server/src/odata/filter_sql.rs`
  - `crates/temper-store-turso/src/store/field_index.rs`
  - `crates/temper-server/src/state/query_projection_queue.rs`
  - `crates/temper-store-postgres/src/schema.rs`, `crates/temper-server/src/state/entity_ops.rs`

## Context

### The bug
Point reads return **413 QueryTooLarge** at tenant scale (`Files`, `SessionEntries`, `Directories`). The read plane can prove a key **present** cheaply — an equality probe against the EAV field index — but it cannot prove a key **absent** without scanning the whole entity type. Three facts combine:

1. **`entity_id` is a surrogate** (a `sim_uuid`). The business keys the platform actually resolves by — `WorkspaceId+Path` (Files), `SessionId+EntryId` (SessionEntries), `Name+WorkspaceId+ParentId` (Directories) — live **inside event payloads**, not in any keyed structure.
2. **The query projection is eventually consistent.** The broad EAV field index is written by the async coalescing queue (ADR-0148), so an empty equality page is **ambiguous**: the key is absent, *or* it is present but the projection lags.
3. **To stay read-your-writes correct on that ambiguity**, `should_reconcile_empty_exact_match_against_authoritative` (ARN-89, commit `40b4f22a`) falls back to scanning the workspace's authoritative state on an empty page. At scale that scan exceeds `scan_candidate_budget` (`odata_max_entities × 10`) → **413**.

Read-after-write correctness for a *present* key was bought by making *absence* cost `O(workspace)`. Every prior fix — raising the budget, pushing down lossless conjuncts (`bdd15d42`), caller-side query rewrites — only **moves the cliff**; the trip is a function of tenant data volume. (Proven in production: a caller-side fix to the `Directories` root lookup shifted the 413 from the root to the *subdirectory* lookup — same class, one level down.)

### How we got here
ADR-0091 (diff-based field-index upserts) → ADR-0142 (inline projection on dispatch, to fix a real read-your-writes bug) → ADR-0148 (move the broad field index to the async coalescing queue, because under Foresight's dispatch fan-out the inline projection dominated DB latency). Moving the broad index async is **exactly** what created the absent-vs-lagging ambiguity behind the 413. This ADR does not revert that — it adds a second, narrow index on a different axis.

### Measurement (the gate, real data)
Measured on the real Foresight Postgres (`service:foresight`, tenant `deep-sci-fi`): broad EAV index rows per entity **S = 7–46** (File 15, World 13, EventNode 10, SessionEntry 13, Session 46). Path A's declared keys **K = 1–3** per entity. **K ≪ S (≈ 1/10).** See `aya/brain/temper-read-write-architecture/foresight-measurement-20260623.md`.

## Decision

Add a dedicated **declared composite-key index** and **decouple consistency by query class**.

1. **`entity_key_index(tenant, entity_type, key_name, key_hash, entity_id, sequence_nr, …)`** — one primary-key row per `(declared key, entity)`, holding the business-key → `entity_id` mapping the read plane lacks today.
2. **Co-commit the key row in the same store transaction as the journal append.** The keyed row is **synchronous and strongly consistent** — unlike the broad EAV index.
3. **The broad EAV field index stays async** (ADR-0148 unchanged). Path A is **additive on a different axis, not a revert.**
4. **A declared-key read becomes a single `O(log n)` probe:** hit → `entity_id`; miss → **authoritatively absent**. **Delete the ARN-89 / `#324` reconcile scan.**
5. **Plan-time query taxonomy** — `PointRead` / `RangeScan` / `Unbounded`. Unbounded shapes are rejected **at plan time** with a paging contract, never as a mid-scan budget trip.

### Key declaration — Temper-native (resolved)
The 413 entities are all CSDL `Key = ["Id"]` (verified: File, SessionEntry, Directory, Workspace). The business keys that 413 (`WorkspaceId+Path`, `SessionId+EntryId`, `Name+WorkspaceId+ParentId`) are **not keys today** — so Path A must *declare* them. They are declared **Temper-native in the IOA spec**, via a new `[[key]] name="..." properties=[...]` block (a unique / alternate key), **not** in the OData CSDL. Rationale: the IOA spec is the source of truth and the CSDL is a *derived* projection; a uniqueness guarantee is a domain invariant the verification cascade can check; and the declaration stays portable across spec languages — under a future move to P it becomes a spec monitor, unchanged in intent. The kernel indexes the CSDL `Key` **plus** each declared `[[key]]`; the **OData alternate-key annotation is derived** from the `[[key]]` declaration (so `Files(WorkspaceId=…,Path=…)` addressing comes for free). Apps add one `[[key]]` block per business key; the kernel does the rest.

### Resolved decisions
- **Uniqueness → reject + surface.** A declared composite key is a `UNIQUE` constraint; a duplicate (two entities at the same key tuple) **rejects the write** with a typed error naming the key. A silent duplicate is a latent data bug, not a thing to last-writer-wins.
- **One transaction.** The journal append and the key-index upsert commit in a **single store transaction**; if the key write fails, the write fails (no ack-with-log). Read-after-write correctness is the entire point of the index.
- **Measured on Postgres — done.** K ≪ S confirmed on the real Foresight Postgres (above); the "not a write-amplification revert" claim is measured, not assumed. Foresight is also the worst case (its dispatch fan-out is what drove ADR-0148).
- **Co-location invariant.** Turso **XOR** Postgres, never split — confirmed. This is what makes co-committing the key row with the journal append (and deleting the `#324` scan) safe.

## Consequences
- **Reads** of declared keys are `O(log n)` for present **and** absent. The 413 class is removed for point reads.
- **Writes** gain `K` (1–3) synchronous single-row keyed upserts per write, co-committed with the journal. `K ≪ S` (measured); the expensive `S`-wide projection stays async. **No write-amplification regression.**
- **DST** must follow. The sim store implements `EventStore` but not the query plane, so it gains the key map + a store-agnostic canonical `key_hash`, and deterministic simulation must prove present/absent the same way prod does. **Hard prerequisite, not a tradeoff.**
- **Backfill.** Pre-existing entities have no key row. Keep `#324` as a transitional fallback behind a per-tenant **backfill watermark**; only after backfill passes does a keyed miss authoritatively prove ABSENT (otherwise we re-create ARN-89 for old data).
- **`key_hash` is type-tagged** and canonical — this removes the current EAV limits (string-only ≤ 2000B; no Int/Guid/null keys) for the declared-key path.

## Implementation phases
1. This ADR.
2. **Storage-boundary co-commit.** Thread the key row into `append_batch`'s transaction in **both** stores. Today the journal append and `upsert_projection` open separate transactions — making them one is the real work, not a feature flag.
3. **`entity_key_index` table** (turso + postgres + sim store) + the type-tagged canonical `key_hash`; write/delete in the inline transaction carrying `sequence_nr`.
4. **Backfill-before-trust gate** (per-tenant watermark; `#324` stays as transitional fallback until it passes).
5. **Planner rewrite + retire `#324`** in `query_plane_read/mod.rs`: three-class plan; plan-time rejection instead of a mid-scan 413.
6. **DST.** Key map + canonical `key_hash` in the sim store; property tests proving present/absent under simulation, plus a fault-injection test for a lagging broad index (the original ambiguity).

## Gate — no Foresight regression
The **structural** gate is passed (K ≪ S, measured on real data). The **final** gate before shipping is a load test after phase 5 confirming (a) the synchronous key write adds negligible append latency under corridor fan-out, and (b) broad-index throughput is unchanged. Path A does not ship until that load test is green. (Note: the live Foresight deployment currently stalls at the *seed* stage on provider auth, before fan-out — so the load baseline needs that cleared first, or a synthetic fan-out harness.)

## Alternatives considered
- **Raise the read budget.** Rejected — moves the cliff; does not add the missing access path.
- **Keep caller-side query rewrites** (the deployed ARN-68 mitigations). Rejected as *the* fix — they only delay the trip (proven by the Directories root→subdirectory shift). They remain a valid stopgap until this lands.
- **Postgres actor runtime (ARN-26).** Gets keyed present/absent for free (the business key becomes the primary key), but it is outside DST verification, partial, and multi-node-scoped. That is the mid-horizon multi-node bet (ARN-26 / ARN-27 / ARN-102), not this single-node, verified fix. Path A proves the same "key-as-a-real-key" property inside the verified kernel first.
