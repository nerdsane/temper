# ADR-0166: Field-Overflow Inline Ceiling

- Status: Accepted
- Date: 2026-04-16
- Deciders: Temper core maintainers
- Supersedes: —
- Related:
  - ADR-0040: Blob-Backed Overflow for Large Entity Field Values
  - `crates/temper-server/src/entity_actor/effects.rs` (`FieldSyncMode`, `sync_fields`, `project_field_value`)
  - `crates/temper-server/src/entity_actor/actor.rs` (mode selection sites)
  - `crates/temper-server/src/blobs.rs` (blob-ref machinery)

## Context

ADR-0040 introduced content-addressed blob overflow for oversized entity-field values (`FieldSyncMode::BlobRefs`). The OData read path resolves refs transparently via `hydrate_blob_refs_in_value`. All of that works.

What ADR-0040 did not change was the inline ceiling itself: `MAX_FIELD_VALUE_BYTES = 32_768` at `effects.rs:501` is the single hardcoded threshold that decides whether a value is stored inline in `fields` or moved to a blob. The comment justifies it as "bloat the WASM invocation context beyond CTX_BUF_LEN (256 KB)" — the reference to CTX_BUF being 256KB is outdated. `temper-wasm-sdk/src/host.rs:11` now declares `CTX_BUF_LEN = 524288` (512KB), and `temper-wasm-sdk/src/context.rs:54-80` heap-reallocates on context-size overflow, so the static buffer is no longer the binding constraint. The 32KB ceiling is now ~6× more conservative than the machine needs.

The consequence shows up in OpenPaw's paw-agent: any `Session.user_message` above 32KB lands in the `fields` object as a `{"__temper_blob_ref": ...}` object, and while OData clients see the hydrated value, WASM modules like `workspace_provisioner` read the raw ref object via their invocation context, treat `as_str()` as empty, and fail the session. This blocks the foresight judge flow, which needs to pass two 15–25KB foresight outputs plus a rubric into a single session (tracked in nerdsane/openpaw#58 and nerdsane/temper#106).

Raising the inline ceiling from 32KB to 128KB moves the vast majority of realistic oversized-field traffic back onto the fast inline path — no blob write, no WASM hydration — while keeping a bounded limit that still protects invocation-context size. It is a small, reversible configuration change that unlocks paw-agent's foresight judge without requiring any WASM-side code changes and without replacing ADR-0040's overflow primitive.

## Decision

### Sub-Decision 1: 128KB default inline ceiling

Raise the default field-inline ceiling from 32_768 to 131_072 bytes. A 128KB value serializes comfortably inside the 512KB `CTX_BUF_LEN`, leaves headroom for the rest of `entity_state` (Status, counters, booleans, lists, other fields), and covers p99 of observed paw-agent traffic (user prompts, research notes, tool-call results, conversation snippets).

The ceiling applies to both modes:
- `FieldSyncMode::BlobRefs` — values ≤ ceiling stay inline; values > ceiling get written to the overflow blob store, exactly as ADR-0040 specifies.
- `FieldSyncMode::InlineTruncate` — values ≤ ceiling stay inline; values > ceiling get the existing `[truncated: N bytes exceeds M limit]` placeholder.

### Sub-Decision 2: Runtime-configurable ceiling via `FieldSyncMode::BlobRefs`

`FieldSyncMode::BlobRefs` becomes `FieldSyncMode::BlobRefs { default_inline_max: usize }` so the ceiling travels with the mode selection rather than being hardcoded. Call sites in `EntityActor` construct `FieldSyncMode::BlobRefs { default_inline_max: DEFAULT_FIELD_INLINE_MAX }` by default, where `DEFAULT_FIELD_INLINE_MAX = 131_072` is the exported constant.

`FieldSyncMode` remains `Copy` (the payload is `usize`). `InlineTruncate` uses `DEFAULT_FIELD_INLINE_MAX` directly — there is no compelling reason to tune it per-call for the truncation mode.

**Why not per-field override in this ADR**: per-field tuning (`overflow_inline_max_bytes` on `[[state]]`) is valuable but requires parser + `TransitionTable` + `SpecRegistry` plumbing. Deferred to a later ADR alongside `overflow_ttl_seconds` (Phase 4), so both pieces of field-level metadata ship together.

### Sub-Decision 3: Warn on truncation under `InlineTruncate`

When `project_field_value` truncates under `InlineTruncate`, emit `tracing::warn!` with `entity_type`, `entity_id`, `field_name`, and `size_bytes`. Today the truncation is silent — a Postgres- or memory-backed deployment can lose arbitrary user data and only notice because something downstream breaks. A warn log surfaces the failure mode to operators and makes the case for extending `BlobRefs` to non-Turso stores (out of scope here; future track).

### Sub-Decision 4: Keep the "App authors should use Files for document-sized artifacts" guidance from ADR-0040

Raising the ceiling does not change Temper's opinion on where durable documents belong. Accidental large fields get caught by blob overflow; intentional document storage should still go through the TemperFS `File` entity pattern. This ADR does not revisit that boundary.

## Rollout Plan

1. **Phase 1 (this ADR)** — Code-only change in `temper-server`. Ship the ceiling bump, the `FieldSyncMode::BlobRefs { default_inline_max }` variant, and the InlineTruncate warn log. No spec format change. No schema migration.
2. **Phase 4 (future ADR)** — Per-field `overflow_inline_max_bytes` and `overflow_ttl_seconds` declarations in IOA specs; parser + `TransitionTable` metadata plumbing.
3. **Phase 2 (future ADR)** — `host_read_field_stream` host function for WASM modules to read fields exceeding the ceiling (applies when a field is stored as a blob ref because it exceeds the per-field or default ceiling).

## Consequences

### Positive

- `Session.user_message` and similar fields up to 128KB flow through the inline path with no overflow write. OpenPaw's foresight judge flow (sum of inputs well under 128KB) works without any OpenPaw code change, immediately after this ships.
- Overflow blobs become rarer (only fields >128KB trigger blob writes) — fewer writes to the `blobs` table, faster common-case action processing.
- The new warn log for `InlineTruncate` makes non-Turso-store data loss visible, which the previous silent truncation hid.

### Negative

- `WasmInvocationContext` serialization for an entity with a 128KB field is 4× larger than before. CPU and memory cost per WASM invocation grow correspondingly. Mitigated by the fact that the pre-ADR behavior silently corrupted the field, so callers were already broken.
- Replay and simulation paths now re-project 128KB fields on every event. For entities with many events, this is a measurable cost. Mitigated by snapshot compaction (existing machinery).

### Risks

- An entity with many near-ceiling fields could push `entity_state` past `CTX_BUF_LEN`. The SDK heap-reallocates on overflow, so this is correctness-safe, but can cost an allocation per invocation. Flag for future observability: emit a WideEvent on context size > 256KB.
- A rogue caller that writes 120KB fields on every action would balloon `blobs` once Phase 4 adds overflow — not a risk today.

### DST Compliance

- `FieldSyncMode` stays `Copy` and `PartialEq`; the new `default_inline_max: usize` field is `Copy`. No change to iteration order or randomness.
- `tracing::warn!` is deterministic (pure logging side effect), does not consume fuel, does not perturb event ordering. Not in the DST-forbidden list.
- `DEFAULT_FIELD_INLINE_MAX` is a compile-time constant — no runtime env/config reads that would cause simulation divergence.

## Non-Goals

- Per-field ceiling overrides (deferred to Phase 4).
- Per-field TTL on overflow blobs (deferred to Phase 4).
- Postgres / in-memory store `put_blob` (future track).
- WASM-side blob-ref hydration (Phase 2 ADR).
- Revisiting the File-vs-field boundary for document storage.

## Alternatives Considered

1. **Bump the hardcoded constant, no variant change.** Simpler diff, but leaves us with a single global ceiling and no path to per-field override without another mode refactor. Rejected — the variant shape is the natural place to thread future per-entity / per-field configuration.
2. **Hydrate blob refs at the WASM invocation handoff.** Removes the need for a ceiling bump entirely. Rejected — unbounded hydration is an unbounded WASM-heap write; covered in Phase 2 ADR with a stream-based opt-in instead.
3. **Variable ceiling per store backend.** E.g., 128KB for Turso, 32KB for memory. Rejected — adds complexity for no user benefit; memory store is a test fixture and the 128KB cap is safe there too.

## Rollback Policy

Single-commit revert. Change the constant back to 32_768 and collapse `FieldSyncMode::BlobRefs { default_inline_max }` to `FieldSyncMode::BlobRefs`. No data migration needed: blob rows written during the 128KB window remain valid and keep being resolvable; inline fields ≥32KB under `InlineTruncate` would retrigger truncation on the next re-projection, which is the pre-ADR state.
