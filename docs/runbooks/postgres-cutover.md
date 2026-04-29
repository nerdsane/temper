# Postgres Cutover Runbook

Status: pre-cutover gate
Owner: Temper/OpenPaw operator
Target: Railway managed Postgres

## Required Inputs

- Production Turso source URL and auth token.
- Disposable Railway Postgres dry-run database.
- Final Railway Postgres production database.
- `TEMPER_VAULT_KEY` copied into the target service environment before any secret migration.
- Private Discord dev guild and `DISCORD_BOT_TOKEN` for staging e2e.
- Datadog dashboard: `mn4-k3k-i66` (`TemperPaw -- Platform Overview`).

## Pre-Cutover Gates

1. Run ETL against production-shaped data into a disposable Railway Postgres database:

   ```sh
   temper migrate-turso-to-postgres --tenant all --verify --from-snapshot
   ```

   Record manifest path, row counts, checksums, and wall time. Required result: zero divergence and at least 2x headroom under the planned maintenance window.

2. Boot staging Temper/OpenPaw against the migrated Railway Postgres target:

   ```sh
   TEMPER_EVENT_STORE=postgres \
   TEMPER_PLATFORM_STORE=postgres \
   TEMPER_QUERY_PROJECTION_STORE=postgres \
   DATABASE_URL=<dry-run-railway-postgres-url> \
   temper serve --no-observe
   ```

   Required result: `/readyz` passes within 60 seconds.

3. Run real-stack e2e against staging:

   - Discord DM round-trip in a private dev guild.
   - Katagami `CurationJob` review loop through child sessions, trajectory rows, and terminal review completion.
   - Backend flip parity: run the same smoke scenario with `TEMPER_EVENT_STORE=turso`, then with `TEMPER_EVENT_STORE=postgres`.
   - Restart resilience: kill `temper-server` mid-session, restart on Postgres, and confirm recovery or explicit bounded failure.

4. Query Datadog before the window:

   - `service:openpaw (orphan OR "orphaned session" OR "stale session" OR "unreadable Session")`
   - `service:openpaw timeout`
   - `service:openpaw (recovery OR recovered OR "Session recovery")`

   Record the counts and set `TEMPERPAW_ORPHANED_SESSION_RECOVERY_MAX` above the observed recoverable orphan count, capped by operator comfort.

## Maintenance Window

1. Announce the window and stop the OpenPaw Railway service.
2. Run final ETL from production Turso into the production Railway Postgres database:

   ```sh
   temper migrate-turso-to-postgres --tenant all --verify --from-snapshot
   ```

3. Verify the manifest has zero divergence.
4. Set Railway service env vars:

   ```text
   TEMPER_EVENT_STORE=postgres
   TEMPER_PLATFORM_STORE=postgres
   TEMPER_QUERY_PROJECTION_STORE=postgres
   DATABASE_URL=<production-railway-postgres-url>
   TEMPER_VAULT_KEY=<existing-base64-32-byte-key>
   ```

   Keep Turso env vars available until the 48-hour soak completes.

5. Restart OpenPaw and wait for `/readyz`.
6. Send a real Discord DM and confirm the reply path.
7. Run the Katagami review-job smoke.

## Datadog Tripwires

Rollback if any of these hold:

- `p99:temper_event_store_append_wait_ms{service:openpaw} by {backend}` > 2 seconds for 5 minutes.
- Event-store append error rate > 0.5% for 5 minutes.
- Any unhandled `PersistenceError::ConcurrencyViolation` rate above Turso baseline.
- `/readyz` fails after 5 restart attempts.
- `sum:temper_state_timeout_fired_total{service:openpaw} by {entity_type,state}.as_count()` exceeds the `[Temper] Abnormal State Timeout Firing` threshold.
- User-facing Discord DM smoke fails twice after `/readyz` is healthy.

## Rollback

1. Stop the OpenPaw Railway service.
2. Restore Turso env vars:

   ```text
   TEMPER_EVENT_STORE=turso
   TEMPER_PLATFORM_STORE=turso
   TEMPER_QUERY_PROJECTION_STORE=turso
   TEMPER_TURSO_URL=<production-turso-url>
   TEMPER_TURSO_AUTH_TOKEN=<production-turso-token>
   ```

3. Restart the service.
4. Confirm `/readyz` and a Discord DM smoke.
5. Preserve the failed Postgres database and ETL manifest for diffing.

## Post-Cutover Soak

For 48 hours, watch:

- Event append wait p95/p99 by backend.
- Dispatch attempts p95/p99.
- State timeout fired/reset/cancelled rates.
- WASM default-timeout fallback rate.
- Session recovery/orphan/stale-session logs.

Only after a clean soak should the Turso write gate, priority lanes, atomic bypasses, and Turso-only env knobs be removed.
