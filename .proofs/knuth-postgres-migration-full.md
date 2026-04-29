# Knuth Postgres Migration Full Proof

Date: 2026-04-28
Updated: 2026-04-29
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
- `199ed52` Record Postgres migration proof

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

## Production Datadog Evidence

Queried Datadog on 2026-04-29 through the Datadog MCP connector. The local shell environment still has no `DD_`/`DATADOG_` credentials, so dashboard, monitor, and log evidence below came from the connector rather than local env vars.

- Dashboard found: `mn4-k3k-i66` (`TemperPaw -- Platform Overview`)
  - URL: `https://app.datadoghq.com/dashboard/mn4-k3k-i66`
  - The `State Liveness (ADR-0049 / ADR-0050)` group contains the production metric queries for:
    - `sum:temper_state_timeout_fired_total{service:openpaw} by {entity_type,state}.as_count()`
    - `sum:temper_state_timeout_reset_total{service:openpaw}.as_count() / (sum:temper_state_timeout_reset_total{service:openpaw}.as_count() + sum:temper_state_timeout_fired_total{service:openpaw}.as_count())`
    - `avg:temper_scheduler_pending_timers{service:openpaw} by {entity_type}`
    - `sum:temper_scheduler_overdue_on_replay_total{service:openpaw} by {entity_type}.as_count()`
    - `sum:temper_spec_liveness_violations_total{service:openpaw} by {entity_type,state}.as_count()`
    - `sum:temper_state_timeout_cancelled_total{service:openpaw} by {entity_type,state}.as_count()`
    - `sum:temper_state_timeout_reset_total{service:openpaw} by {entity_type,state}.as_count()`
- Monitor search for `openpaw AND (session OR timeout OR orphan)` returned active production monitors:
  - `275383770` `[Temper] Abnormal State Timeout Firing`: `OK`
    - Query: `sum(last_15m):sum:temper_state_timeout_fired_total{service:openpaw} by {entity_type,state}.as_count() > 25`
  - `275384441` `[Temper] State Timeout Reset Rate Drop`: `No Data`
    - Query: `sum(last_1h):sum:temper_state_timeout_reset_total{service:openpaw,state:Executing}.as_count() < 1`
  - `275383795` `[Temper] WASM Default Timeout Fallback Rate`: `OK`
    - Query: `sum(last_1h):sum:temper_wasm_integration_default_timeout_used_total{service:openpaw}.as_count() > 500`
  - `275383796` `[Temper] Session Memory Externalization Spike`: `No Data`
  - `275384307` `[Temper] Session Memory Budget Exceeded`: `No Data`
- Log pattern query `service:openpaw (session OR timeout OR orphan)`, `from=now-7d`, `to=now`, grouped by `service,env,status`, returned 42 patterns. Relevant production counts:
  - `Orphaned session recovery skipped; set TEMPERPAW_ORPHANED_SESSION_RECOVERY=true to enable bounded recovery`: 24 info logs from 2026-04-27 to 2026-04-28.
  - `Failing orphaned session`: 22 info logs from 2026-04-22 to 2026-04-26.
  - `Session recovery complete`: 8 info logs from 2026-04-22 to 2026-04-26.
  - `Deferred session recovery complete`: 6 info logs from 2026-04-27 to 2026-04-28.
  - `Deferred session recovery scheduled after readiness`: 6 info logs from 2026-04-27 to 2026-04-28.
  - `Session recovery deferred until after readiness`: 6 info logs from 2026-04-27 to 2026-04-28.
  - `ChannelSession ... points at unreadable Session ... HTTP 404; starting a fresh session`: 3 warn logs on 2026-04-28.
  - `route_message: routed ... to fresh session ... after stale binding`: 3 info logs on 2026-04-28.
  - `route_message: dispatched ResumeTools on stale session ...`: 2 info logs from 2026-04-26 to 2026-04-28.
- Aggregate Datadog log analytics over `now-7d`:
  - `service:openpaw (orphan OR "orphaned session" OR "stale session" OR "unreadable Session")`: 51 logs total (`info=48`, `warn=3`).
  - `service:openpaw timeout`: 114 logs total (`warn=105`, `error=9`).
  - `service:openpaw (recovery OR recovered OR "Session recovery")`: 226 logs total (`info=226`).
  - Daily liveness-related query `service:openpaw (orphan OR timeout OR recovery OR stale)`:
    - 2026-04-28: `info=45`
    - 2026-04-27: `info=97`, `error=2`
    - 2026-04-26: `info=46`, `error=6`, `warn=5`
    - 2026-04-25: `info=22`, `error=1`
    - 2026-04-24: `info=5`
    - 2026-04-23: `info=6`
    - 2026-04-22: `warn=100`, `info=10`

## Not Exercised

- Real Discord DM flow was not exercised because this local environment has no `DISCORD_BOT_TOKEN`.
- The full StorageStack trait-object replacement and sqlx versioned migration-file inversion remain architectural follow-up work; this branch made Postgres operational through the existing `ServerEventStore` adapter and documented/verified the cutover path.
