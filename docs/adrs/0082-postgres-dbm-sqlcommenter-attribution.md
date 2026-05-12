# ADR-0082: Postgres DBM SQLCommenter Attribution

Date: 2026-05-12

## Status

Accepted

## Context

Temper's Postgres backend emits APM traces, logs, and Database Monitoring samples, but Datadog DBM could not identify TemperPaw as a calling service for sampled Postgres queries. Datadog's DBM/APM correlation model relies on SQL comment propagation for supported database clients. Rust `sqlx` is not one of the Datadog first-class automatic tracer integrations, so setting Datadog environment variables alone does not mutate SQL text or attach DBM propagated tags.

TemperPaw needs humans and agents to answer "which service is creating this database load?" from DBM without guessing from host names or deployment variables.

## Decision

`temper-store-postgres` owns a small SQLCommenter tagging layer for Postgres statements.

When `DD_DBM_PROPAGATION_MODE` is `service` or `full`, static Postgres statements are routed through DBM helper macros that prepend SQLCommenter tags before handing the SQL to `sqlx`. Dynamic SQL uses an owned tagged string for the lifetime of the query. The propagated tags are:

- `dddbs`: database service name, from `DD_DBM_DATABASE_SERVICE`, `DD_DB_SERVICE`, or `<DD_SERVICE>-postgres`.
- `ddps`: parent application service, from `DD_SERVICE`.
- `dde`: Datadog environment, from `DD_ENV` when set.
- `ddpv`: Datadog version, from `DD_VERSION` when set.

`DD_DBM_PROPAGATION_MODE=disabled` or an unset value leaves SQL untouched.

For now, both `service` and `full` enable the same service-level SQLCommenter attribution. We deliberately do not inject per-trace `traceparent` into every prepared statement yet because doing so would create a distinct prepared statement for every trace/span and can churn Postgres and `sqlx` statement caches. A future change may add bounded full-mode propagation if it can preserve prepared-statement behavior and avoid cardinality explosions.

## Consequences

- Datadog DBM query samples can attribute Postgres load to Temper/TemperPaw service identity.
- The implementation is centralized in `temper-store-postgres` instead of scattered through business logic.
- Static SQL comments are cached per rendered statement to avoid repeated allocation.
- Query signatures may include the comment prefix before Datadog normalization; DBM query monitors should use normalized signatures or propagated tags rather than raw SQL text.
- Individual APM trace-to-DBM sample joins remain a follow-up until safe full propagation exists for Rust `sqlx`.

## Rollback

Set `DD_DBM_PROPAGATION_MODE=disabled` to stop SQL comment propagation without a code rollback. Reverting this ADR's code removes the helper macros and returns Postgres statements to direct `sqlx` calls.
