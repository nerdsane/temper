//! Storage backend connection and persistence functions (Postgres, Turso).
//!
//! Postgres pool settings (read once at process start):
//!
//! - `TEMPER_PG_MAX_CONNECTIONS`, or `DATABASE_MAX_CONNECTIONS` — pool size.
//!   Default 32. A non-positive or non-numeric value keeps the default.
//! - `TEMPER_PG_STATEMENT_TIMEOUT_SECS`, or `DATABASE_STATEMENT_TIMEOUT_SECS` —
//!   per-statement timeout in seconds. Default 15. Applied as Postgres
//!   `statement_timeout` on the connection startup options and again with
//!   `set_config` when the pool opens each connection.
//! - Acquire timeout is 5 seconds. A checkout that waits longer fails the
//!   request instead of holding the caller until the pool is exhausted.
//!
//! Event size (any backend), the bytes of one event (one recorded change) as
//! JSON. Unset, empty or 0 sets no limit; anything but a whole number stops
//! the server.
//!
//! - `TEMPER_WARN_EVENT_BYTES` — a larger event is written and logged as a
//!   warning.
//! - `TEMPER_MAX_EVENT_BYTES` — a larger event is refused before it is written.
//!
//! Files: `TEMPER_FILES_NEED_OBJECT_STORE` (`1` or `true`) keeps a File's bytes
//! only in its tenant's remote object store, and refuses them for a tenant
//! without one, never keeping them on this disk and in the database.

use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use temper_evolution::PostgresRecordStore;
use temper_server::storage::{EventSizeLimits, StorageStack};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

use super::LoadedTenantSpecs;

const DEFAULT_MAX_CONNECTIONS: u32 = 32;
const DEFAULT_STATEMENT_TIMEOUT_SECS: u64 = 15;
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolved Postgres pool limits for `temper serve`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PostgresPoolConfig {
    /// Maximum checked-out connections.
    pub max_connections: u32,
    /// `statement_timeout` in milliseconds.
    pub statement_timeout_ms: u64,
}

impl PostgresPoolConfig {
    /// Read pool limits from the process environment.
    pub(super) fn from_env() -> Self {
        Self::from_values(&[
            (
                "TEMPER_PG_MAX_CONNECTIONS",
                std::env::var("TEMPER_PG_MAX_CONNECTIONS").ok(),
            ),
            (
                "DATABASE_MAX_CONNECTIONS",
                std::env::var("DATABASE_MAX_CONNECTIONS").ok(),
            ),
            (
                "TEMPER_PG_STATEMENT_TIMEOUT_SECS",
                std::env::var("TEMPER_PG_STATEMENT_TIMEOUT_SECS").ok(),
            ),
            (
                "DATABASE_STATEMENT_TIMEOUT_SECS",
                std::env::var("DATABASE_STATEMENT_TIMEOUT_SECS").ok(),
            ),
        ])
    }

    fn from_values(values: &[(&str, Option<String>)]) -> Self {
        let max_connections = positive_u64(
            values,
            &["TEMPER_PG_MAX_CONNECTIONS", "DATABASE_MAX_CONNECTIONS"],
            u64::from(DEFAULT_MAX_CONNECTIONS),
        )
        .min(u64::from(u32::MAX)) as u32;
        let statement_timeout_secs = positive_u64(
            values,
            &[
                "TEMPER_PG_STATEMENT_TIMEOUT_SECS",
                "DATABASE_STATEMENT_TIMEOUT_SECS",
            ],
            DEFAULT_STATEMENT_TIMEOUT_SECS,
        );
        Self {
            max_connections,
            statement_timeout_ms: statement_timeout_secs.saturating_mul(1000),
        }
    }
}

/// Read the event size limits from their variables' values, if set.
pub(super) fn event_size_limits(warn: Option<&str>, max: Option<&str>) -> Result<EventSizeLimits> {
    Ok(EventSizeLimits {
        warn_bytes: event_bytes("TEMPER_WARN_EVENT_BYTES", warn)?,
        max_bytes: event_bytes("TEMPER_MAX_EVENT_BYTES", max)?,
    })
}

/// One size variable's value: `None` is no limit.
fn event_bytes(name: &str, value: Option<&str>) -> Result<Option<usize>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let limit: usize = value
        .parse()
        .with_context(|| format!("{name} must be a whole number of bytes, not {value:?}"))?;
    Ok((limit > 0).then_some(limit))
}

fn positive_u64(values: &[(&str, Option<String>)], names: &[&str], default: u64) -> u64 {
    for name in names {
        let Some((_, Some(value))) = values.iter().find(|(key, _)| *key == *name) else {
            continue;
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        return trimmed
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .unwrap_or(default);
    }
    default
}

pub(super) async fn connect_postgres_store(
    database_url: &str,
) -> Result<(StorageStack, sqlx::PgPool)> {
    eprintln!("  Connecting to Postgres...");
    let config = PostgresPoolConfig::from_env();
    eprintln!(
        "  Postgres pool: max_connections={}, acquire_timeout={}s, statement_timeout={}ms",
        config.max_connections,
        ACQUIRE_TIMEOUT.as_secs(),
        config.statement_timeout_ms
    );
    let pool = connect_postgres_pool(database_url, config)
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
    let store = PostgresEventStore::new(pool.clone());
    store.start_first_event_backfill();
    Ok((StorageStack::from_postgres(store), pool))
}

async fn connect_postgres_pool(
    database_url: &str,
    config: PostgresPoolConfig,
) -> Result<sqlx::PgPool> {
    let mut options: PgConnectOptions = database_url
        .parse()
        .context("invalid Postgres connection URL")?;
    let timeout_setting = config.statement_timeout_ms.to_string();
    options = options.options([("statement_timeout", timeout_setting.as_str())]);
    let statement_timeout_ms = config.statement_timeout_ms;
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .after_connect(move |conn, _meta| {
            let timeout = statement_timeout_ms.to_string();
            Box::pin(async move {
                sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                    .bind(timeout)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .context("failed to open the Postgres pool")
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
    // Same write as Turso: a changed spec stays uncommitted until it verifies.
    let store = PostgresEventStore::new(pool.clone());
    for (entity_type, ioa_source) in &loaded.ioa_sources {
        let hash = temper_store_turso::spec_content_hash(ioa_source);
        store
            .upsert_spec(tenant, entity_type, ioa_source, &loaded.csdl_xml, &hash)
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

/// Upsert loaded specs to Turso (mirrors `upsert_loaded_specs_to_postgres`).
pub(super) async fn upsert_loaded_specs_to_turso(
    turso: &TursoEventStore,
    tenant: &str,
    loaded: &LoadedTenantSpecs,
) -> Result<()> {
    for (entity_type, ioa_source) in &loaded.ioa_sources {
        let hash = temper_store_turso::spec_content_hash(ioa_source);
        turso
            .upsert_spec(tenant, entity_type, ioa_source, &loaded.csdl_xml, &hash)
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
            .with_context(|| format!("Failed to clear tenant constraints for {tenant} in Turso"))?;
    }
    if let Some(policy_text) = loaded.cedar_policy_text.as_deref() {
        turso
            .save_policy(tenant, "primary", policy_text, "system")
            .await
            .with_context(|| format!("Failed to persist Cedar policy for {tenant} in Turso"))?;
    }
    Ok(())
}

/// Whether this server keeps a File's bytes only in an object store (`TEMPER_FILES_NEED_OBJECT_STORE`).
pub(super) fn files_need_object_store() -> bool {
    std::env::var("TEMPER_FILES_NEED_OBJECT_STORE") // determinism-ok: deployment config read
        .is_ok_and(|value| matches!(value.trim(), "1" | "true"))
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

    fn values<'a>(pairs: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, Option<String>)> {
        pairs
            .iter()
            .map(|(key, value)| (*key, Some((*value).to_string())))
            .collect()
    }

    #[test]
    fn event_size_limits_are_off_unless_a_positive_number_is_set() {
        assert_eq!(
            event_size_limits(None, None).unwrap(),
            EventSizeLimits::default()
        );
        assert_eq!(
            event_size_limits(Some(" "), Some("0")).unwrap(),
            EventSizeLimits::default()
        );
        assert_eq!(
            event_size_limits(Some("16384"), Some("131072")).unwrap(),
            EventSizeLimits {
                warn_bytes: Some(16_384),
                max_bytes: Some(131_072),
            }
        );
        let refused = event_size_limits(None, Some("128k")).unwrap_err();
        assert!(
            refused.to_string().contains("TEMPER_MAX_EVENT_BYTES"),
            "{refused}"
        );
        assert!(event_size_limits(Some("-1"), None).is_err());
    }

    #[test]
    fn pool_config_defaults_when_unset() {
        assert_eq!(
            PostgresPoolConfig::from_values(&[]),
            PostgresPoolConfig {
                max_connections: 32,
                statement_timeout_ms: 15_000,
            }
        );
    }

    #[test]
    fn pool_config_prefers_temper_names_and_rejects_non_positive() {
        let config = PostgresPoolConfig::from_values(&values(&[
            ("TEMPER_PG_MAX_CONNECTIONS", "8"),
            ("DATABASE_MAX_CONNECTIONS", "99"),
            ("TEMPER_PG_STATEMENT_TIMEOUT_SECS", "0"),
            ("DATABASE_STATEMENT_TIMEOUT_SECS", "4"),
        ]));
        assert_eq!(
            config,
            PostgresPoolConfig {
                max_connections: 8,
                statement_timeout_ms: 15_000,
            }
        );
    }

    #[test]
    fn pool_config_falls_back_to_database_names() {
        let config = PostgresPoolConfig::from_values(&values(&[
            ("DATABASE_MAX_CONNECTIONS", "4"),
            ("DATABASE_STATEMENT_TIMEOUT_SECS", "9"),
        ]));
        assert_eq!(
            config,
            PostgresPoolConfig {
                max_connections: 4,
                statement_timeout_ms: 9_000,
            }
        );
    }
}
