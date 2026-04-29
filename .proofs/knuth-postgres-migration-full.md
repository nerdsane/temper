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
- `b80fa68` Record Datadog migration evidence
- `9bce8ad` Introduce StorageStack object-safe adapter
- `42bfdd2` Record Postgres cutover ADR gates
- `f43e0ab` Use versioned Postgres migrations
- `a7fc9fd` Remove stale bootstrap import

## Verification Run

- Red tests observed:
  - `cargo test -p temper-store-postgres postgres_long_tail_methods_are_part_of_the_store_surface` failed before long-tail methods existed.
  - `cargo test -p temper-cli test_cli_parse_migrate_turso_to_postgres` failed before the CLI command existed.
  - `cargo test -p temper-store-postgres postgres_query_projection_batch_method_is_part_of_the_store_surface` failed before the Postgres batch projection reader existed.
  - `cargo test -p temper-server --test storage_stack` failed before `temper_server::storage` existed.
  - `cargo test -p temper-store-postgres versioned_migration_is_the_schema_source` failed before `migrations/0001_initial.sql` existed.
- Green checks:
  - `cargo fmt`
  - `cargo check -p temper-server`
  - `cargo check -p temper-cli`
  - `cargo test -p temper-server --test storage_stack`
  - `cargo test -p temper-store-postgres`
  - `DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper cargo test -p temper-store-postgres`
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
  - The boot path ran `sqlx::migrate!()` against `crates/temper-store-postgres/migrations/0001_initial.sql`; logs showed `_sqlx_migrations` present and Postgres migrations applied.
  - `information_schema` check found all 7 sampled platform tables: `events`, `specs`, `trajectories`, `entity_catalog`, `blobs`, `tenant_secrets`, `policy_denial_patterns`.

## Post-Review Remediation

- Added `crates/temper-server/src/storage/mod.rs` with `DynEventStore`, `BoxedEventStore`, `BackendLabel`, and `StorageStack`.
- Wired `ServerState::set_event_store` so serve/bootstrap paths derive a first-class storage stack alongside the transitional `ServerEventStore` compatibility handle.
- Updated ADR-0066 to remove the object-safety deferral. Remaining work is caller migration from compatibility methods to dedicated query-plane and trajectory traits, not the object-safe adapter itself.
- Replaced the hand-rolled Postgres migration runner with `sqlx::migrate!()` and added `crates/temper-store-postgres/migrations/0001_initial.sql`.
- Added or corrected ADRs for the missing architectural records:
  - ADR-0069: HttpEndpoint (renumbered from the duplicate ADR-0056)
  - ADR-0070: Postgres multi-tenant isolation
  - ADR-0071: storage retry classification
  - ADR-0072: ProgressMade cadence
  - ADR-0073: runtime index recovery
  - ADR-0074: Turso to Postgres ETL methodology
  - ADR-0075: tenant secrets key management
- Added `docs/runbooks/postgres-cutover.md` with Railway Postgres dry-run instructions, real-stack e2e gates, Datadog tripwires, and rollback steps.

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
- Real Katagami `CurationJob` review loop was not exercised locally; the cutover runbook now gates production on that staging e2e.
- Production-shaped ETL into a disposable Railway Postgres database was not run from this environment.
- Production cutover and 48-hour Postgres soak have not happened; Turso write-gate/priority/bypass removals remain correctly gated on that soak.
- StorageStack is now present with an object-safe event adapter, but some callers still use the transitional `ServerEventStore` compatibility handle until query-plane and trajectory traits are split out.
