# Knuth Postgres Migration Full Proof

Date: 2026-04-28
Branch: `codex/knuth-postgres-migration-full`

## Commits

- `f6b1996` Add Postgres platform storage parity
- `f4a07ee` Bound trajectory persistence outbox
- `e222a3b` Select storage backend from environment
- `477ec47` Route Cedar policies through Postgres storage
- `0e6b52d` Allow Railway storage selection from env
- `d43a879` Fix local Postgres compose bootstrap
- `7749307` Route platform long-tail reads through Postgres
- `f66a2a2` Add Turso to Postgres migration command
- `727dab6` Route batched projections through Postgres

## Verification Run

- Red tests observed:
  - `cargo test -p temper-store-postgres postgres_long_tail_methods_are_part_of_the_store_surface` failed before long-tail methods existed.
  - `cargo test -p temper-cli test_cli_parse_migrate_turso_to_postgres` failed before the CLI command existed.
  - `cargo test -p temper-store-postgres postgres_query_projection_batch_method_is_part_of_the_store_surface` failed before the Postgres batch projection reader existed.
- Green checks:
  - `cargo fmt`
  - `cargo check -p temper-server`
  - `cargo check -p temper-cli`
  - `cargo test -p temper-store-postgres`
  - `DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper cargo test -p temper-store-postgres query_projection`
  - `cargo test -p temper-store-turso`
  - `DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper cargo test -p temper-cli`
  - `DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper cargo test -p temper-cli smoke_migration_copies_events_snapshots_specs_projections_and_blobs_when_database_url_set -- --nocapture`
  - `git diff --check`

## Local E2E Evidence

- Started local Postgres with `docker compose up -d postgres`.
- Ran a real migration smoke test from a local Turso DB into Docker Postgres. It copied event, snapshot, spec, query projection, and blob rows and asserted all landed in Postgres.
- Ran the CLI in dry-run/verify mode:
  - `temper migrate-turso-to-postgres --tenant all --dry-run --verify --from-snapshot --turso-url file:/tmp/temper-cli-migration-empty.db`
  - Manifest written to `/tmp/temper-cli-migration-manifest.json` with verified checksums for all empty source tables.
- Booted the actual server:
  - `DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper TEMPER_EVENT_STORE=postgres cargo run -p temper-cli -- serve --port 43123 --no-observe`
  - `/healthz` returned `HTTP/1.1 200 OK`.
  - `information_schema` check found all 7 sampled platform tables: `events`, `specs`, `trajectories`, `entity_catalog`, `blobs`, `tenant_secrets`, `policy_denial_patterns`.

## Not Exercised

- Real Discord DM flow was not exercised because this local environment has no `DISCORD_BOT_TOKEN`.
- Production Datadog orphan-session count was not queried from this environment.
- The full StorageStack trait-object replacement and sqlx versioned migration-file inversion remain architectural follow-up work; this branch made Postgres operational through the existing `ServerEventStore` adapter and documented/verified the cutover path.
