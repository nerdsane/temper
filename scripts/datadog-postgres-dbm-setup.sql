-- Datadog Database Monitoring setup for Temper Postgres databases.
--
-- Run this in every logical database the Datadog Agent may collect query
-- samples or explain plans from. Execute as a database owner/superuser or a
-- role with enough privileges to create schemas, extensions, functions, and
-- grants.
--
-- This script intentionally does not create or rotate the DBM Agent role
-- password. Manage the role credential through the database provider and
-- Datadog Agent secret backend.
--
-- The Agent role defaults to "datadog", but production Railway DBM currently
-- runs the Datadog Postgres Agent as "postgres". Override when needed:
--
--   psql "$DATABASE_URL" -v dbm_agent_role=postgres -f scripts/datadog-postgres-dbm-setup.sql

\set ON_ERROR_STOP on

\if :{?dbm_agent_role}
\else
\set dbm_agent_role datadog
\endif

SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'dbm_agent_role') AS dbm_agent_role_exists \gset

\if :dbm_agent_role_exists
\else
\echo 'role "' :dbm_agent_role '" does not exist; create it with a managed password or pass -v dbm_agent_role=<existing-role>'
\quit 1
\endif

CREATE SCHEMA IF NOT EXISTS datadog;

GRANT USAGE ON SCHEMA datadog TO :"dbm_agent_role";
GRANT USAGE ON SCHEMA public TO :"dbm_agent_role";
GRANT pg_monitor TO :"dbm_agent_role";

-- Temper application SQL uses unqualified table names in the public schema.
-- Datadog explains sampled statements from a separate Agent session, so its
-- role must have the same schema lookup context the application has.
ALTER ROLE :"dbm_agent_role" SET search_path = "$user", public, datadog, pg_catalog;

CREATE EXTENSION IF NOT EXISTS pg_stat_statements;

CREATE OR REPLACE FUNCTION datadog.pg_stat_activity()
RETURNS SETOF pg_stat_activity AS
$$
    SELECT * FROM pg_catalog.pg_stat_activity;
$$
LANGUAGE sql
SECURITY DEFINER;

CREATE OR REPLACE FUNCTION datadog.pg_stat_statements()
RETURNS SETOF pg_stat_statements AS
$$
    SELECT * FROM pg_stat_statements;
$$
LANGUAGE sql
SECURITY DEFINER;

CREATE OR REPLACE FUNCTION datadog.explain_statement(
   l_query TEXT,
   OUT explain JSON
)
RETURNS SETOF JSON AS
$$
DECLARE
    curs REFCURSOR;
    plan JSON;
BEGIN
    SET TRANSACTION READ ONLY;

    -- Keep a tenant setting present for Temper RLS policy planning. The value
    -- is intentionally non-production and should not match real tenant data.
    PERFORM set_config('app.current_tenant', '__datadog_explain__', true);

    OPEN curs FOR EXECUTE pg_catalog.concat('EXPLAIN (FORMAT JSON) ', l_query);
    FETCH curs INTO plan;
    CLOSE curs;
    RETURN QUERY SELECT plan;
END;
$$
LANGUAGE 'plpgsql'
RETURNS NULL ON NULL INPUT
SECURITY DEFINER;

GRANT EXECUTE ON FUNCTION datadog.pg_stat_activity() TO :"dbm_agent_role";
GRANT EXECUTE ON FUNCTION datadog.pg_stat_statements() TO :"dbm_agent_role";
GRANT EXECUTE ON FUNCTION datadog.explain_statement(TEXT) TO :"dbm_agent_role";

-- Optional direct SELECT grants for operator-run validation queries. The DBM
-- explain function is SECURITY DEFINER, but these grants make psql smoke tests
-- less surprising and support future custom DBM metrics.
GRANT SELECT ON TABLE entity_catalog TO :"dbm_agent_role";
GRANT SELECT ON TABLE entity_field_index TO :"dbm_agent_role";
GRANT SELECT ON TABLE events TO :"dbm_agent_role";
GRANT SELECT ON TABLE snapshots TO :"dbm_agent_role";
