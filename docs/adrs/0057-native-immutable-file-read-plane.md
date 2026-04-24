# ADR-0057: Native Immutable File Read Plane for TemperFS

- Status: Accepted
- Date: 2026-04-23
- Deciders: Temper core maintainers
- Related:
  - ADR-0027: OS App Catalog
  - ADR-0029: TemperFS — A Governed File System on Temper Primitives
  - ADR-0045: Reactions as a First-Class App Primitive
  - ADR-0048: Dispatch Retry and Error Taxonomy
  - `crates/temper-platform/src/os_apps/mod.rs`
  - `crates/temper-server/src/state/file_reads.rs`
  - `os-apps/temper-fs/specs/file.ioa.toml`
  - `os-apps/temper-fs/specs/file_version.ioa.toml`

## Context

TemperFS already established the right high-level split: mutable filesystem namespace in entities, immutable bytes in blob storage.

But the hot read path still behaved like a generic control-plane lookup:

1. read one file at a time
2. route through OData or generic entity lookup
3. pay entity-state lookup and WASM/blob adapter overhead per file

That shape was survivable for occasional file access, but it became the dominant latency cost for session context preparation and would become worse for any future FUSE-like filesystem surface.

We also discovered a platform gap in OS-app installation: `reactions.toml` was not loaded into `AppBundle`, and app installs bootstrapped specs without rebuilding the live reaction dispatcher. As a result, the TemperFS lineage model could exist in specs without actually executing at runtime after install.

## Decision

### 1. File content reads are split from generic entity control reads

Temper remains entity-first for writes, lifecycle, and authorization.

TemperFS hot reads now use a native immutable read plane:

- `File` remains the mutable namespace and head pointer entity
- `FileVersion` is the immutable version record
- blob bytes remain content-addressed by `content_hash`
- hot-path read APIs load projected metadata first, then fetch blob bytes directly

This preserves Temper-native ownership while avoiding per-file control-plane overhead for immutable reads.

### 2. TemperFS version lineage is explicit and first-class

`File.StreamUpdated` now carries version metadata:

- `version_number`
- `previous_version_id`
- `created_by`

Each content update spawns a fresh `FileVersion`, stores its id back into `File.last_version_id`, and links lineage through `previous_version_id`.

The previous version is superseded by a reaction on `FileVersion.Create`, not by mutating the new version in place.

### 3. Batch text reads exist for both mutable file heads and immutable versions

Temper now exposes internal HTTP/API surfaces for:

- batch read by current `File`
- batch read by immutable `FileVersion`

These APIs:

- preserve request order
- load projected fields in bulk where possible
- fall back safely when projection data is missing
- fetch blob bytes directly once the content hash is known

This makes the read plane usable for session context assembly and future filesystem consumers.

### 4. OS-app reactions are loaded and activated as part of install

OS-app bundles now load `reactions/reactions.toml` as first-class bundle content.

App installation:

- carries reaction rules into bootstrap registration
- preserves them in the tenant registry
- rebuilds the live `ReactionDispatcher` immediately after install

This makes app-installed choreography real at runtime instead of depending on a later specs reload.

## Rollout Plan

1. **Phase 0** — Land explicit `FileVersion` lineage, batch read APIs, and OS-app reaction activation.
2. **Phase 1** — Move OpenPaw session context consumers onto immutable version reads where available.
3. **Phase 2** — Reuse the same read plane for future filesystem-shaped surfaces such as `lookup`, `getattr`, and `read`.

## Consequences

### Positive

- Immutable content reads no longer pay full per-file control-plane cost.
- TemperFS version lineage is explicit and auditable.
- OS-app reactions behave consistently after install and recovery.
- The same architecture can support session context prep and future FUSE/NFS-style access.

### Negative

- TemperFS now depends more heavily on projection correctness for hot-path performance.
- The platform has another explicit read surface to maintain alongside generic OData reads.

### Risks

- Projection drift could produce stale or missing batch-read metadata. The implementation mitigates this with safe fallback to entity-state reads for misses.
- Future callers could overuse the batch API for non-hot-path workloads. The design expectation is that this API is for immutable content-heavy paths, not general orchestration.

### DST Compliance

- Determinism is preserved because write semantics still flow through entity actions and event-sourced state transitions.
- Batch read APIs are read-only projections over committed state plus blob lookup; they do not introduce background mutation or hidden orchestration.

## Non-Goals

- Replacing OData as the general-purpose platform API
- Introducing an external sidecar cache or database outside Temper ownership
- Delivering a full FUSE implementation in this ADR

## Alternatives Considered

1. **Keep reading files through generic OData/entity paths** — rejected because latency scales with file count and punishes immutable content reads with control-plane overhead.
2. **Store session/app content in an external cache outside Temper** — rejected because it breaks Temper-native ownership and complicates authorization and recovery.
3. **Treat `File` head state as sufficient and skip explicit version lineage** — rejected because historical session references need immutable identity, not a mutable head pointer.
