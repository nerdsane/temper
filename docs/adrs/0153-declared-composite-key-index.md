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

## Implementation completion (2026-06-30, ARN-68 follow-through)

Phases 2–3 (co-commit + `entity_key_index` table + canonical `key_hash`) shipped earlier, but the soul write kept 413ing in production. Investigation against the **live openpaw Postgres** (read-only) found the index existed yet was not doing its job, for three distinct reasons — each fixed here. None was guessed; each was traced in code and confirmed against prod data (10,754 `Directory` entities; `entity_key_index` held 338 File + 25 Directory rows; **every root directory `Path='/'` was unkeyed**; the failing workspace had no root at all).

- **A — the keyed read resolved declared keys from the wrong source.** `keyed_candidate_ids` / `try_resolve_composite_entity_key` read `state.transition_tables`, which is populated only by `ServerState::with_specs` at boot. TemperPaw never calls `with_specs`; it installs os-app entities (File/Directory/SessionEntry) into the per-tenant `SpecRegistry` at runtime. So `transition_tables` was empty for exactly the 413 entities → the keyed path silently declined → every point read scanned → 413. Fix: a registry-first `ServerState::declared_keys_for` (mirroring how dispatch resolves tables), used by both keyed read entry points.
- **B — the backfill keyed almost nothing.** `populate_key_index_from_snapshots` enumerated `state.entity_index`, which is populated only when an actor spawns (lazy), so at the scheduled backfill it was near-empty (~25/10,754 keyed — those were inline co-commits, not the backfill). Fix: enumerate keyed types from the registry and entity ids from `store.list_entity_ids_by_type` (authoritative), taking each entity's current fields from its snapshot or, when absent, event replay — so snapshot-less entities are still keyed (a gap there would make the watermark unsound).
- **C — phases 4–5 (watermark + retire `#324`) were not actually implemented.** A keyed miss always fell back to the scan, so a genuinely-absent key (e.g. a new workspace's root) still scanned → 413. Fix: a persisted per-`(tenant, entity_type)` watermark (`key_index_backfill_watermark`) recorded only when a type's backfill completes with zero upsert failures; once set, a keyed miss returns an empty candidate set (authoritative absence) and the native re-query is skipped (the eventually-consistent field index must not override the co-committed keyed index). Telemetry: `QueryPlaneFallbackReason::KeyedAbsence`. The watermark only ever turns a 413-scan into an `O(log n)` answer — without it, misses stay scan-safe, so it can never make a present entity read as absent.

**Backend soundness gate.** The watermark is sound only on a store that co-commits key rows on *every* write (overrides `append_with_keys`), so the index stays complete after backfill. **Postgres** does (the current query-plane prod backend); the **sim store** does too (the DST reference). **Turso does NOT co-commit keys**, so it keeps the no-op/empty trait defaults for backfill + watermark and never becomes authoritative — a keyed miss on Turso always falls back to the scan (correct, just not bounded). Giving Turso the keyed oracle requires implementing live co-commit first (completing phase 2 for Turso); it is intentionally out of scope here (Turso is being migrated to Postgres — see `migrate_turso_to_postgres`). This gate is encoded in the `mark_key_index_backfilled` trait doc so a future contributor cannot naively watermark a non-co-committing backend.

- **D — the in-memory filter eval dropped roots on `eq null` (the duplicate-root cause).** Found by reproducing the full souls scenario *locally* against the real read path before redeploying. A keyed hit (or any read that falls to the source-cursor rather than the SQL native page) re-applies the `$filter` in `query_eval::evaluate_filter`. There, an *absent* property made `evaluate_value` return `None`, and the `?` collapsed the **entire** compound filter to false — so the root lookup `Name eq '/' and WorkspaceId eq … and ParentId eq null` dropped **every** root (roots have no `ParentId`). `ensure_dirs` therefore never found the existing root and recreated it on every write — the source of the duplicate roots observed in prod (one workspace held 1,688 roots named `/`). Fix: a `compare_nullable` helper that mirrors the native pushdown's operator→null mapping (`eq null` → IS NULL, matching an absent property; `ne null` → IS NOT NULL; any other comparison touching null → excluded), and a `comparison_operand` that distinguishes an absent property (→ NULL) from a non-evaluable operand (→ exclude the row). This also stops one absent operand from poisoning sibling `And`/`Or` branches — the SQL-correct behavior. In prod (Postgres) a keyed root hit materializes via the SQL native page (`IS NULL`), so this is primarily the historical dup mechanism plus defense for the source-cursor path; it is required for the existing-root case wherever the read does not push the filter to SQL.

Tests (against the sim store, which co-commits + watermarks like prod Postgres): `declared_keys_resolve_from_registry_not_just_transition_tables` (A), `key_index_backfill_keys_store_entities_absent_from_the_lazy_index` (B), `keyed_miss_returns_empty_without_scan_413_once_watermarked` (C), and `directory_root_lookup_souls_scenario_with_real_key_and_duplicates` — the end-to-end souls proof using the **real** Directory `name_parent` key (3-part, roots with absent `ParentId`, plus the duplicate-root case): it reproduces the 413, runs the backfill, then asserts a new workspace's root lookup is authoritative-absent (empty, no 413 → the soul write proceeds) while the existing root resolves to exactly one (no dup). Null-semantics unit tests (`root_lookup_eq_null_matches_absent_property_in_compound_filter`, `null_comparison_semantics_match_sql`) guard D. The standalone `temper serve` boot path now also runs the key-index backfill per tenant (it previously ran only the field-index backfill); TemperPaw already wired it via `spawn_key_index_backfill`.

## Key-set-aware watermark (2026-07-01, ARN-68 second-413 follow-through)

The above shipped the keyed root lookup. The soul/finalization flow then still 413'd on the *path* lookups — `Path eq '/souls'` on Directories and `find_file`'s `Path eq …` on Files — because those shapes were **not declared keys**, so they fell to the broad field index and scanned. The fix is Path A applied to those access paths: declare `[[key]] ws_path = [WorkspaceId, Path]` on paw-fs Directory + File (a path is unique within a workspace; both components non-null → a plain unique composite key). `name_parent` remains the root/child-by-name key; `ws_path` is the by-full-path key.

Declaring a key on a type that was **already backfilled** exposed a gap: the watermark recorded only `(tenant, entity_type)` with no memory of *which* keys it covered. So adding `ws_path` to `Directory` (already watermarked for `name_parent`) was treated as complete — the backfill's `if key_index_backfill_complete { continue }` skipped it, existing directories never got `ws_path` rows, and worse, the read claimed authoritative absence for the uncovered key: a keyed miss on `ws_path` for a *present* directory returned an empty set (a silent wrong "not found", worse than the 413).

**Fix — the watermark is now key-set aware:**
- `key_index_backfill_watermark` gains a `key_set` column (migration `0011`): the sorted, comma-joined declared key names the backfill covered (e.g. `"name_parent,ws_path"`), via `key_index::declared_key_set_signature`. Existing rows default to `''`.
- The upsert is `DO UPDATE` (was `DO NOTHING`) so a re-key overwrites a stale set.
- `key_index_backfill_complete(tenant, type, current_key_set)` is exact-match on the covered set. The read (`query_plane_read`) computes `current_key_set` over the full declared set and trusts absence only when covered == current — so a just-declared, not-yet-backfilled key never reads a present entity as absent; it falls back to the scan (correct, just not bounded) until the re-key completes.
- The backfill: skip iff covered == current; if a watermark exists with a **different** set, `force_full_rekey` re-loads and re-keys **every** entity under all currently-declared keys (idempotent upsert), bypassing the per-entity `already_keyed` resumability skip — which is per-entity/any-key and would otherwise skip a directory that has `name_parent` but not `ws_path`. It then re-watermarks with the current set. The one-time re-key (incl. the `0011` migration stamping every existing watermark to `''`) is `info`-logged per type so the expected full-reload is distinguishable in Datadog.

Proven e2e (real Postgres, temperpaw) on prod-like state — Directory watermarked `name_parent`, `ws_path` newly declared — booting auto-re-keys `ws_path` for the existing directories with no manual intervention, `Path eq '/souls'` binds via the key with the field index empty, and misses are authoritative-absent. Tests: `key_index_backfill_rekeys_existing_entities_when_a_key_is_added` (entities pre-keyed under an old key → in `keyed_entity_ids_for_type` → guards the `force_full_rekey` bypass), `declared_key_set_signature_is_sorted_and_joined`, and the store upsert-overwrite assertion in `store_projection_test`.
