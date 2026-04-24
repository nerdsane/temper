# Action Triggers Merge-Main Live E2E

Date: 2026-04-24
Repo: `/Users/seshendranalla/Development/temper-action-triggers`
Branch: `feat/action-triggers-unified`

## Scope

- Verify the merged branch after folding in `origin/main`.
- Prove standalone Temper still executes the new `temper-fs` trigger architecture live.
- Confirm explicit `FileVersion` lineage and immutable batch reads on a fresh local server.

## Server

Started a fresh standalone Temper server:

```bash
PORT=4461 \
TEMPER_API_KEY=temper-live-key \
TURSO_URL=file:/tmp/temper-merge-e2e.db \
RUST_LOG=info \
cargo run -p temper-cli -- serve --no-observe --port 4461
```

## Commands

1. `curl -fsS http://127.0.0.1:4461/healthz`
   Result: healthy
2. `curl -fsS -H 'Authorization: Bearer temper-live-key' -H 'content-type: application/json' -d '{"tenant":"default"}' http://127.0.0.1:4461/api/os-apps/temper-fs/install`
   Result: `temper-fs` installed for tenant `default`
3. Live file lineage replay:
   - `POST /tdata/Files`
   - `PUT /tdata/Files('<id>')/$value` with `first version from temper merge proof`
   - `PUT /tdata/Files('<id>')/$value` with `second version from temper merge proof`
   - `GET /tdata/Files('<id>')`
   - `GET /tdata/FileVersions?$top=200`
   - `POST /api/files/read-version-text-batch`

## Observed Result

```json
{
  "file_id": "fl-019dbff6-27d2-70c3-ab13-a8db28ff6be6",
  "file_status": "Ready",
  "version_count": 2,
  "last_version_id": "019dbff6-27f8-70f0-95ec-1bfed8a1ca62",
  "current_version_status": "Current",
  "previous_version_status": "Superseded",
  "latest_text": "second version from temper merge proof",
  "batch_texts": [
    "first version from temper merge proof",
    "second version from temper merge proof"
  ]
}
```

## What This Proves

- The merged standalone Temper server still installs `temper-fs` successfully after folding in `origin/main`.
- Inline entity triggers create and supersede `FileVersion` entities live.
- `File.fields.last_version_id` updates to the newest version.
- Immutable batch reads return historical content from the live server, not just the mutable file head.
