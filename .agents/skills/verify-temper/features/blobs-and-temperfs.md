# Blobs and TemperFS

## Sub-features
Large field values overflow to blob refs; a per-tenant blob object store (local FS or S3/R2); media/file value streams. Overflow logic in `crates/temper-server/src/entity_actor/effects.rs` + `src/blobs.rs`; store selection in `src/blob_store/state.rs`.

## How to get to it (user POV)
An entity field larger than the inline ceiling is transparently stored as a blob and hydrated back on read; media/file entities stream their bytes.

## Driving it
The inline ceiling is `DEFAULT_FIELD_INLINE_MAX = 131_072` (128 KB; per-field override via `overflow_inline_max_bytes`). A field over the ceiling is stored as `{"__temper_blob_ref":"field-overflow/sha256/<hex>.json"}`; reads inline refs whose stored size is <= the read ceiling and defer larger ones (WASM guests read deferred bytes via `host_read_field_stream`).

```bash
# force overflow: dispatch an action that sets a field > 128 KB, then read the
# entity back (governed - needs the bearer + tenant header):
A=(-H "Authorization: Bearer $TEMPER_API_KEY" -H "X-Tenant-Id: default")
curl -sS "http://localhost:3600/tdata/<Set>('id')" "${A[@]}"   # overflowed field shows __temper_blob_ref
# local FS blob backend for a scratch server: TEMPER_LOCAL_BLOB_DIR=/some/dir before serve
```
`$value` primitive streaming is only for Blob binary fields (`Content` / `CanonicalBytes`, per `odata/blob_media.rs`) - NOT a general read for an overflowed ordinary field. An overflowed ordinary field is read by reading the entity: small ones hydrate inline, large ones come back as the `__temper_blob_ref` descriptor (WASM guests read the deferred bytes via `host_read_field_stream`; there is no user-facing "read at a higher ceiling" knob).

## What proves it
An overflowed field returns the `__temper_blob_ref` descriptor at the default read ceiling (or inlined below it), and the referenced object exists at `field-overflow/sha256/<hex>.json` in the configured store. Small overflow fields hydrate inline on the entity read - that inline value is the round-trip proof.

## Gotchas
- **The blob/TemperFS 503 is NOT storage-backend-gated.** The stale advice "503 when not on turso" is wrong: both turso and postgres stacks provide the query plane + metadata + blob API. The real 503s are transient/config: object store unconfigured (`BlobStoreUnavailable`, `content_addressed.rs`), object-store read error (`BlobMediaUnavailable`, `blob_media.rs`), file-stream projection read failure (`FileReadIndexUnavailable`, `stream_fast_path.rs`), and storage-cap exhaustion (503 + `Retry-After`, ADR-0048). Report the 503 by its code, not by the backend.
- Store selection is per tenant: a `blob_endpoint` secret routes to S3/R2, otherwise local FS (`TEMPER_LOCAL_BLOB_DIR` or `data_dir/blobs`); with neither, blob ops error "not configured". The default tenant also has a legacy DB-blob read fallback.
- Fields overflow at 128 KB - do not assume small inline limits; the retired "32 KB, use file refs" advice is stale.
