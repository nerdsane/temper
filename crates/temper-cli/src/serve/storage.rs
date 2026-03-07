//! Storage backend connection and persistence functions (Postgres, Turso).

use anyhow::{Context, Result};

use temper_evolution::PostgresRecordStore;
use temper_server::event_store::ServerEventStore;
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

use super::LoadedTenantSpecs;

pub(super) async fn connect_postgres_store(
    database_url: &str,
) -> Result<(ServerEventStore, sqlx::PgPool)> {
    eprintln!("  Connecting to Postgres...");
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .context("Failed to connect to Postgres")?;
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .context("Failed to run migrations")?;
    let pg_record_store: PostgresRecordStore = PostgresRecordStore::new(pool.clone());
    pg_record_store
        .migrate()
        .await
        .context("Failed to migrate evolution_records")?;
    eprintln!("  Postgres connected, migrations applied.");
    Ok((
        ServerEventStore::Postgres(PostgresEventStore::new(pool.clone())),
        pool,
    ))
}

pub(super) fn redact_connection_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some(at_idx) = rest.find('@') else {
        return url.to_string();
    };
    let creds = &rest[..at_idx];
    let host_and_path = &rest[at_idx + 1..];
    if let Some((user, _password)) = creds.split_once(':') {
        format!("{scheme}://{user}:***@{host_and_path}")
    } else {
        format!("{scheme}://***@{host_and_path}")
    }
}

pub(super) async fn upsert_loaded_specs_to_postgres(
    pool: &sqlx::PgPool,
    tenant: &str,
    loaded: &LoadedTenantSpecs,
) -> Result<()> {
    for (entity_type, ioa_source) in &loaded.ioa_sources {
        sqlx::query(
            "INSERT INTO specs \
             (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at) \
             VALUES ($1, $2, $3, $4, 1, false, 'pending', now()) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = EXCLUDED.ioa_source, \
                 csdl_xml = EXCLUDED.csdl_xml, \
                 version = specs.version + 1, \
                 verified = false, \
                 verification_status = 'pending', \
                 levels_passed = NULL, \
                 levels_total = NULL, \
                 verification_result = NULL, \
                 updated_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(ioa_source)
        .bind(&loaded.csdl_xml)
        .execute(pool)
        .await
        .with_context(|| format!("Failed to persist spec {tenant}/{entity_type}"))?;
    }
    if let Some(source) = loaded.cross_invariants_toml.as_deref() {
        sqlx::query(
            "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at) \
             VALUES ($1, $2, 1, now()) \
             ON CONFLICT (tenant) DO UPDATE SET \
                 cross_invariants_toml = EXCLUDED.cross_invariants_toml, \
                 version = tenant_constraints.version + 1, \
                 updated_at = now()",
        )
        .bind(tenant)
        .bind(source)
        .execute(pool)
        .await
        .with_context(|| format!("Failed to persist tenant constraints for {tenant}"))?;
    } else {
        sqlx::query("DELETE FROM tenant_constraints WHERE tenant = $1")
            .bind(tenant)
            .execute(pool)
            .await
            .with_context(|| format!("Failed to clear tenant constraints for {tenant}"))?;
    }
    Ok(())
}

// Registry restoration logic has been moved to temper_server::registry_bootstrap.
// The CLI now calls restore_registry_from_postgres / restore_registry_from_turso
// from the server crate, keeping storage-specific row translation out of the CLI.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_with_user_and_password() {
        assert_eq!(
            redact_connection_url("postgres://admin:secret@db.example.com:5432/mydb"),
            "postgres://admin:***@db.example.com:5432/mydb"
        );
    }

    #[test]
    fn redact_user_only_no_password() {
        assert_eq!(
            redact_connection_url("postgres://admin@db.example.com:5432/mydb"),
            "postgres://***@db.example.com:5432/mydb"
        );
    }

    #[test]
    fn redact_no_credentials() {
        assert_eq!(
            redact_connection_url("postgres://db.example.com:5432/mydb"),
            "postgres://db.example.com:5432/mydb"
        );
    }

    #[test]
    fn redact_no_scheme() {
        assert_eq!(
            redact_connection_url("no-scheme-here"),
            "no-scheme-here"
        );
    }

    #[test]
    fn redact_libsql_scheme() {
        assert_eq!(
            redact_connection_url("libsql://user:token@turso.example.com"),
            "libsql://user:***@turso.example.com"
        );
    }

    // row_to_registry_status tests moved to temper_server::registry_bootstrap::tests
}

/// Upsert loaded specs to Turso (mirrors `upsert_loaded_specs_to_postgres`).
pub(super) async fn upsert_loaded_specs_to_turso(
    turso: &TursoEventStore,
    tenant: &str,
    loaded: &LoadedTenantSpecs,
) -> Result<()> {
    for (entity_type, ioa_source) in &loaded.ioa_sources {
        turso
            .upsert_spec(tenant, entity_type, ioa_source, &loaded.csdl_xml)
            .await
            .with_context(|| format!("Failed to persist spec {tenant}/{entity_type} in Turso"))?;
    }
    if let Some(source) = loaded.cross_invariants_toml.as_deref() {
        turso
            .upsert_tenant_constraints(tenant, source)
            .await
            .with_context(|| {
                format!("Failed to persist tenant constraints for {tenant} in Turso")
            })?;
    } else {
        turso
            .delete_tenant_constraints(tenant)
            .await
            .with_context(|| {
                format!("Failed to clear tenant constraints for {tenant} in Turso")
            })?;
    }
    Ok(())
}
