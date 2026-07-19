//! Storage backend connection and persistence functions (Postgres, Turso).

use anyhow::{Context, Result};

use temper_evolution::PostgresRecordStore;
use temper_server::storage::StorageStack;
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

use super::LoadedTenantSpecs;

pub(super) async fn connect_postgres_store(
    database_url: &str,
) -> Result<(StorageStack, sqlx::PgPool)> {
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
        StorageStack::from_postgres(PostgresEventStore::new(pool.clone())),
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
    let fingerprints = loaded
        .ioa_sources
        .iter()
        .map(|(entity_type, ioa_source)| {
            (
                entity_type.as_str(),
                ioa_source.as_str(),
                temper_store_turso::spec_content_hash(ioa_source),
            )
        })
        .collect::<Vec<_>>();
    let specs = fingerprints
        .iter()
        .map(|(entity_type, source, fingerprint)| (*entity_type, *source, fingerprint.as_str()))
        .collect::<Vec<_>>();
    PostgresEventStore::new(pool.clone())
        .persist_spec_catalog_update(
            tenant,
            &specs,
            &loaded.csdl_xml,
            &[],
            true,
            loaded.cross_invariants_toml.as_deref(),
        )
        .await
        .with_context(|| format!("Failed to persist spec catalog for {tenant} in Postgres"))?;
    Ok(())
}

// Registry restoration logic has been moved to temper_server::registry_bootstrap.
// The CLI now calls restore_registry_from_postgres / restore_registry_from_turso
// from the server crate, keeping storage-specific row translation out of the CLI.

/// Upsert loaded specs to Turso (mirrors `upsert_loaded_specs_to_postgres`).
pub(super) async fn upsert_loaded_specs_to_turso(
    turso: &TursoEventStore,
    tenant: &str,
    loaded: &LoadedTenantSpecs,
) -> Result<()> {
    let fingerprints = loaded
        .ioa_sources
        .iter()
        .map(|(entity_type, ioa_source)| {
            (
                entity_type.as_str(),
                ioa_source.as_str(),
                temper_store_turso::spec_content_hash(ioa_source),
            )
        })
        .collect::<Vec<_>>();
    let specs = fingerprints
        .iter()
        .map(|(entity_type, source, fingerprint)| (*entity_type, *source, fingerprint.as_str()))
        .collect::<Vec<_>>();
    turso
        .persist_spec_catalog_update(
            tenant,
            &specs,
            &loaded.csdl_xml,
            &[],
            true,
            loaded.cross_invariants_toml.as_deref(),
        )
        .await
        .with_context(|| format!("Failed to persist spec catalog for {tenant} in Turso"))?;
    if let Some(policy_text) = loaded.cedar_policy_text.as_deref() {
        turso
            .save_policy(tenant, "primary", policy_text, "system")
            .await
            .with_context(|| format!("Failed to persist Cedar policy for {tenant} in Turso"))?;
    }
    Ok(())
}

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
        assert_eq!(redact_connection_url("no-scheme-here"), "no-scheme-here");
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
