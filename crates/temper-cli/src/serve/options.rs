//! Resolution of `temper serve` options from CLI flags and environment
//! variables.
//!
//! Each option follows the same precedence: an explicit CLI flag wins, then
//! the environment variable, then the flag's default. A set-but-invalid
//! environment variable is an error rather than a silent fallback.

use crate::{ActorRuntimeBackend, StorageBackend};

/// True when `flag` was passed on the command line (as `--flag` or
/// `--flag=value`), distinguishing an explicit value from the clap default.
pub(crate) fn flag_given(flag: &str) -> bool {
    let with_equals = format!("{flag}=");
    std::env::args().any(|arg| arg == flag || arg.starts_with(&with_equals))
}

/// The storage backend `serve` uses: the `--storage` flag when given, else
/// `TEMPER_EVENT_STORE`, else the flag's default.
pub(crate) fn resolve_storage_backend(
    cli_storage: StorageBackend,
    storage_explicit: bool,
    env_storage: Option<&str>,
) -> Result<StorageBackend, String> {
    if storage_explicit {
        return Ok(cli_storage);
    }

    let Some(value) = env_storage.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(cli_storage);
    };

    match value.to_ascii_lowercase().as_str() {
        "postgres" | "pg" => Ok(StorageBackend::Postgres),
        "turso" | "libsql" => Ok(StorageBackend::Turso),
        "redis" => Ok(StorageBackend::Redis),
        other => Err(format!(
            "TEMPER_EVENT_STORE must be one of postgres, turso, redis; got {other:?}"
        )),
    }
}

/// The actor runtime `serve` uses: the `--actor-runtime` flag when given,
/// else `TEMPER_ACTOR_RUNTIME`, else the flag's default.
pub(crate) fn resolve_actor_runtime_backend(
    cli_runtime: ActorRuntimeBackend,
    runtime_explicit: bool,
    env_runtime: Option<&str>,
) -> Result<ActorRuntimeBackend, String> {
    if runtime_explicit {
        return Ok(cli_runtime);
    }

    let Some(value) = env_runtime.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(cli_runtime);
    };

    match value.to_ascii_lowercase().as_str() {
        "legacy" | "in-memory" | "memory" => Ok(ActorRuntimeBackend::Legacy),
        "postgres" | "pg" => Ok(ActorRuntimeBackend::Postgres),
        other => Err(format!(
            "TEMPER_ACTOR_RUNTIME must be one of legacy, postgres; got {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_backend_env_is_used_when_cli_storage_is_not_explicit() {
        assert_eq!(
            resolve_storage_backend(StorageBackend::Turso, false, Some("postgres"))
                .expect("postgres storage env should parse"),
            StorageBackend::Postgres
        );
        assert_eq!(
            resolve_storage_backend(StorageBackend::Turso, false, Some("redis"))
                .expect("redis storage env should parse"),
            StorageBackend::Redis
        );
        assert_eq!(
            resolve_storage_backend(StorageBackend::Turso, true, Some("postgres"))
                .expect("explicit CLI storage should win"),
            StorageBackend::Turso
        );
    }

    #[test]
    fn storage_backend_env_rejects_unknown_values() {
        let error = resolve_storage_backend(StorageBackend::Turso, false, Some("sqlite"))
            .expect_err("unknown storage backend should fail");
        assert!(error.contains("TEMPER_EVENT_STORE"));
    }

    #[test]
    fn actor_runtime_env_is_used_when_cli_runtime_is_not_explicit() {
        assert_eq!(
            resolve_actor_runtime_backend(ActorRuntimeBackend::Legacy, false, Some("postgres"))
                .expect("postgres actor runtime env should parse"),
            ActorRuntimeBackend::Postgres
        );
        assert_eq!(
            resolve_actor_runtime_backend(ActorRuntimeBackend::Postgres, true, Some("legacy"))
                .expect("explicit CLI actor runtime should win"),
            ActorRuntimeBackend::Postgres
        );
    }

    #[test]
    fn actor_runtime_env_rejects_unknown_values() {
        let error =
            resolve_actor_runtime_backend(ActorRuntimeBackend::Legacy, false, Some("sqlite"))
                .expect_err("unknown actor runtime should fail");
        assert!(error.contains("TEMPER_ACTOR_RUNTIME"));
    }
}
