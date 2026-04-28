# 2026-04-28 Idempotent Spec Persistence Local E2E

## Build and Tests

- `cargo test -p temper-store-turso --lib -- --nocapture`
  - Result: 41 passed.
- `cargo test -p temper-platform test_hashes_requiring_persistence_skip_cached_verified_specs -- --nocapture`
  - Result: passed.
- `cargo test -p temper-cli -- --nocapture`
  - Result: 44 passed.
- `cargo check -p temper-cli -p temper-platform -p temper-store-turso`
  - Result: passed.
- `cargo build -p temper-cli`
  - Result: passed.

## Live E2E

Started `temper serve` twice against the same file-backed Turso/libSQL database
with an isolated home directory:

```bash
HOME=/tmp/temper-spec-e2e-home \
XDG_DATA_HOME=/tmp/temper-spec-e2e-xdg \
TURSO_URL=file:/tmp/temper-spec-e2e-clean.db \
RUST_LOG=error \
./target/debug/temper serve \
  --storage turso \
  --no-observe \
  --app pipeline=docs/examples/pipeline-specs \
  --port 3123
```

First boot:

- `/healthz`: 200
- Spec counts:
  - `default|8|8|8`
  - `pipeline|12|12|12`
  - `temper-system|13|13|13`
- Snapshot saved from:
  - `tenant`
  - `entity_type`
  - `updated_at`
  - `version`
  - `verified`
  - `committed`
  - `content_hash`

Second boot:

- `/healthz`: 200
- Log evidence: `[verify] Skipped 4 unchanged verified specs for tenant pipeline`
- Spec counts:
  - `default|8|8|8`
  - `pipeline|12|12|12`
  - `temper-system|13|13|13`
- `diff -u /tmp/temper-spec-e2e-before.tsv /tmp/temper-spec-e2e-after.tsv`
  returned no diff.

## Result

Warm boot with unchanged verified specs did not rewrite any persisted spec rows,
including `updated_at`, `version`, `verified`, `committed`, and `content_hash`.
