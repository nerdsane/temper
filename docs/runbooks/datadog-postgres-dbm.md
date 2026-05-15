# Datadog Postgres DBM Runbook

Status: OBS-002 repair package
Owner: Temper/OpenPaw operator
Target: Railway/Postgres production databases monitored by Datadog DBM

## Why This Exists

On May 14, 2026, live Datadog DBM showed `temperpaw-postgres` as healthy, but
plan search returned 2,408 samples with `plan.collection_errors` and
`invalid_schema`. That means Datadog can see query workload signatures, but it
cannot reliably collect explain plans for the queries we need to optimize.

Datadog's current Postgres DBM setup requires:

- a `datadog` schema in every collected database,
- helper functions including `datadog.explain_statement(TEXT)`,
- `pg_stat_statements`,
- an Agent role with the right grants,
- a search path that can resolve the same unqualified schemas as the
  application.

Temper adds one important wrinkle: tenant-scoped Postgres tables use RLS
policies that reference `current_setting('app.current_tenant', true)`. The DBM
explain helper therefore sets a synthetic tenant value before explaining a
sampled statement. The value must not match a real tenant; it exists to keep
planning deterministic and explicit.

## Repair Steps

1. Confirm the Datadog Agent Postgres instance has `dbm: true` and tags that
   include the production service and environment, for example:

   ```yaml
   instances:
     - dbm: true
       host: <postgres-host>
       port: 5432
       username: datadog
       password: ENC[datadog_user_database_password]
       tags:
         - service:temperpaw
         - env:prod
   ```

2. Run the setup SQL in every logical database the Agent monitors:

   ```sh
   psql "$DATABASE_URL" -f scripts/datadog-postgres-dbm-setup.sql
   ```

3. Verify as the Datadog role:

   ```sh
   psql "$DATABASE_URL" -U datadog -A \
     -c "select count(*) from datadog.pg_stat_activity();"

   psql "$DATABASE_URL" -U datadog -A \
     -c "select count(*) from datadog.pg_stat_statements();"

   psql "$DATABASE_URL" -U datadog -A \
     -c "select * from datadog.explain_statement('SELECT tenant, COUNT(*) FROM entity_catalog GROUP BY tenant ORDER BY tenant') limit 1;"

   psql "$DATABASE_URL" -U datadog -A \
     -c "select * from datadog.explain_statement('SELECT entity_id FROM entity_catalog WHERE tenant = ''__datadog_probe__'' AND entity_type = ''Session'' LIMIT 1') limit 1;"
   ```

4. Restart the Datadog Agent or trigger an integration reload.

5. In Datadog DBM, re-run the plan search:

   ```text
   dbm_type:plan service:temperpaw -@db.plan.collection_errors:*
   ```

   Then inspect the same query signatures that previously showed
   `invalid_schema`, especially:

   - `entity_catalog` tenant count queries,
   - `entity_catalog` batch loads,
   - `entity_field_index` filter pushdown queries.

## Completion Criteria

- DBM plan records exist for the top Temper query signatures without
  `plan.collection_errors`.
- `invalid_schema` no longer appears for routine `entity_catalog` and
  `entity_field_index` plans.
- Dashboard evidence includes plan count, example query signatures, and at
  least one screenshot or Datadog link showing an explain plan for a hot query.
- Remaining missing plans are categorized as unsupported query types,
  low-frequency/fast queries, permissions, or provider limitations.

## Cautions

- Do not enable `auto_explain` globally until the team explicitly accepts the
  log-volume and sensitive-data tradeoff. It can capture actual parameters in
  logs.
- The synthetic RLS tenant in `datadog.explain_statement` is for planning only.
  It is not a correctness proof for projected reads.
- If production tables move out of `public`, update the `datadog` role
  `search_path` and this runbook before expecting DBM plans to recover.
