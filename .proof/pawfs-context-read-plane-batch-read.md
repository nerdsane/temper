# PawFS Context Read Plane Batch Read

Date: 2026-04-23

## What changed

- Added `load_query_projection_fields_many(...)` in `temper-store-turso` to fetch sparse projected fields for many entities in one query.
- Added `ServerState::read_file_texts_batch(...)` in `temper-server` to:
  - resolve `File` metadata from the durable query plane
  - fall back to actor state only for projection misses
  - read blob bytes directly from the local blob store or external blob endpoint
- Added `POST /api/files/read-text-batch` in `temper-server`.

## Verification

### Projection loader

Command:

```bash
cargo test -p temper-store-turso load_query_projection_fields_many_returns_requested_fields_by_entity -- --nocapture
```

Result:

- passed
- proved projected `content_hash`, `mime_type`, and `has_content` load in one round trip

### Batch read API

Command:

```bash
cargo test -p temper-server --features observe batch_file_text_read_returns_projected_file_contents_in_request_order -- --nocapture
```

Result:

- passed
- verified `POST /api/files/read-text-batch` returns:
  - a ready file with text content
  - a found-but-empty file with empty text
  - a missing file with `found=false`
- preserved request order in the response

