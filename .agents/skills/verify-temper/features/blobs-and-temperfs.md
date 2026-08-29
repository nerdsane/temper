# Blobs and TemperFS

## Sub-features
Large field values overflow to blob refs; a per-tenant blob object store (local FS or S3/R2); media/file value streams. Overflow logic in `crates/temper-server/src/entity_actor/effects.rs` + `src/blobs.rs`; store selection in `src/blob_store/state.rs`.

## How to get to it (user POV)
An entity field larger than the inline ceiling is transparently stored as a blob and hydrated back on read; media/file entities stream their bytes.

## Driving it
The inline ceiling is `DEFAULT_FIELD_INLINE_MAX = 131_072` (128 KB; per-field override via `overflow_inline_max_bytes`). A field over the ceiling is stored as `{"__temper_blob_ref":"field-overflow/sha256/<hex>.json"}`; reads inline refs whose stored size is <= the read ceiling and defer larger ones (WASM guests read deferred bytes via `host_read_field_stream`).

```bash
# force overflow: dispatch an action that sets a field > 128 KB, then read it back
curl -sS "http://localhost:3600/tdata/<Set>('id')" -H "X-Tenant-Id: default"   # overflowed field shows __temper_blob_ref
# media/file value
curl -sS "http://localhost:3600/tdata/Files('id')/\$value" -H "X-Tenant-Id: default"
# local FS blob backend for a scratch server:
#   TEMPER_LOCAL_BLOB_DIR=/some/dir before serve
```

## What proves it
An overflowed field returns the `__temper_blob_ref` descriptor at the default read ceiling (or inlined below it); the referenced object exists at `field-overflow/sha256/<hex>.json` in the configured store; the full value round-trips when read via `$value` or with a higher ceiling.

## Gotchas
- **The blob/TemperFS 503 is NOT storage-backend-gated.** The stale advice "503 when not on turso" is wrong: both turso and postgres stacks provide the query plane + metadata + blob API. The real 503s are transient/config: object store unconfigured (`BlobStoreUnavailable`, `content_addressed.rs`), object-store read error (`BlobMediaUnavailable`, `blob_media.rs`), file-stream projection read failure (`FileReadIndexUnavailable`, `stream_fast_path.rs`), and storage-cap exhaustion (503 + `Retry-After`, ADR-0048). Report the 503 by its code, not by the backend.
- Store selection is per tenant: a `blob_endpoint` secret routes to S3/R2, otherwise local FS (`TEMPER_LOCAL_BLOB_DIR` or `data_dir/blobs`); with neither, blob ops error "not configured". The default tenant also has a legacy DB-blob read fallback.
- Fields overflow at 128 KB - do not assume small inline limits; the retired "32 KB, use file refs" advice is stale.
