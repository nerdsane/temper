# ADR-0082: Postgres DBM SQLCommenter Attribution

Date: 2026-05-12

## Status

Accepted

## Context

Temper's Postgres backend emits APM traces, logs, and Database Monitoring samples, but Datadog DBM could not identify TemperPaw as a calling service for sampled Postgres queries. Datadog's DBM/APM correlation model relies on SQL comment propagation for supported database clients. Rust `sqlx` is not one of the Datadog first-class automatic tracer integrations, so setting Datadog environment variables alone does not mutate SQL text or attach DBM propagated tags.

TemperPaw needs humans and agents to answer "which service is creating this database load?" from DBM without guessing from host names or deployment variables.

## Decision

`temper-store-postgres` owns a small SQLCommenter tagging layer for Postgres statements.

When `DD_DBM_PROPAGATION_MODE` is `service` or `full`, Postgres statements are routed through DBM helper macros that prepend SQLCommenter tags before handing the SQL to `sqlx`. Dynamic SQL uses an owned tagged string for the lifetime of the query. The propagated tags are:

- `dddbs`: database service name, from `DD_DBM_DATABASE_SERVICE`, `DD_DB_SERVICE`, or `<DD_SERVICE>-postgres`.
- `ddps`: parent application service, from `DD_SERVICE`.
- `dde`: Datadog environment, from `DD_ENV` when set.
- `ddpv`: Datadog version, from `DD_VERSION` when set.
- `traceparent`: W3C trace context from the active OpenTelemetry span when `DD_DBM_PROPAGATION_MODE=full`.

`DD_DBM_PROPAGATION_MODE=disabled` or an unset value leaves SQL untouched.

The store owns query execution wrappers instead of returning borrowed `sqlx::Query` values directly. The wrapper keeps SQL text alive across the awaited call, records a stable `postgres <OPERATION> <relation>` client span around each query, and lets `full` mode use an owned SQL string without leaking per-trace statements. `service` mode keeps prepared statement caching enabled. `full` mode disables per-query persistence so trace-specific SQLCommenter text does not churn `sqlx` or Postgres statement caches.

## Consequences

- Datadog DBM query samples can attribute Postgres load to Temper/TemperPaw service identity.
- APM traces now contain first-class Postgres client spans with `db.system`, `db.operation`, `db.collection.name`, `db.statement`, and `peer.service`.
- Full DBM mode can join sampled Postgres work to the active APM trace through `traceparent` while avoiding cached prepared statements for trace-specific SQL text.
- The implementation is centralized in `temper-store-postgres` instead of scattered through business logic.
- Query signatures may include the comment prefix before Datadog normalization; DBM query monitors should use normalized signatures or propagated tags rather than raw SQL text.

## Rollback

Set `DD_DBM_PROPAGATION_MODE=disabled` to stop SQL comment propagation without a code rollback. Reverting this ADR's code removes the helper macros and returns Postgres statements to direct `sqlx` calls.
