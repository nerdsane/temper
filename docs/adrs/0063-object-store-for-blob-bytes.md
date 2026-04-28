# ADR-0063: Object Store for Blob Bytes

Date: 2026-04-28

## Status

Accepted

## Context

Temper had three paths that could write large byte payloads into SQL-backed
storage:

- WASM module artifacts (`wasm-modules/{sha256}`) were written through the
  Turso `blobs` table while `wasm_modules` held metadata.
- Field-overflow values (`field-overflow/sha256/{hash}`) were persisted through
  the same DB blob table.
- Local TemperFS fallback used `/_internal/blobs`, which also wrote to Turso.

Production traces showed these writes as mostly idle waits, for example
`turso.upsert_wasm_module` taking tens of seconds with almost no busy CPU. That
is the wrong storage shape: SQL should own metadata and transactional state,
not opaque byte blobs.

## Decision

Blob bytes are stored outside the metadata database.

- Introduce a Temper server `BlobStore` boundary with S3/R2-compatible and
  local filesystem implementations.
- Store WASM module bytes in object storage at `wasm-modules/{sha256}`.
- Store field-overflow bytes in object storage at
  `field-overflow/sha256/{hash}`.
- Preserve TemperFS object addressing: external R2/S3 uses
  `{bucket}/{content_hash}` to match the existing `blob_adapter` WASM contract;
  local/internal fallback uses `temper-fs/{content_hash}` because the internal
  route has no separate bucket namespace.
- Keep SQL rows as metadata only. `wasm_modules.wasm_bytes` is empty for new
  writes; `size_bytes` and `sha256_hash` remain in SQL.
- Keep the Turso `blobs` table as a read-only legacy fallback so old deployments
  can still hydrate data written before this ADR.
- Production applications must configure an external object store. Local
  development may use the filesystem object store.

## Consequences

WASM app reconcile no longer pays Turso blob-write latency for every module.
The remaining SQL work is metadata upsert and version bookkeeping.

Field-overflow hydration reads object storage first and falls back to legacy DB
blobs only when the object is absent. New field-overflow writes never create DB
blob rows.

Object-store TTL is delegated to the provider lifecycle policy. The legacy
Turso blob sweeper remains only for legacy rows.

Local development still works without R2/S3, but local bytes are files under the
server data directory rather than DB rows. Existing external TemperFS objects
remain readable at their current `{bucket}/{content_hash}` keys.
