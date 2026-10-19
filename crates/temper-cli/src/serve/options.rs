//! Resolution of `temper serve` options from CLI flags and environment
//! variables.
//!
//! Each option follows the same precedence: an explicit CLI flag wins, then
//! the environment variable, then the flag's default. A set-but-invalid
//! environment variable is an error rather than a silent fallback.

use std::net::Ipv4Addr;

use crate::{ActorRuntimeBackend, StorageBackend};

/// True when `flag` was passed on the command line (as `--flag` or
/// `--flag=value`), distinguishing an explicit value from the clap default.
pub(crate) fn flag_given(flag: &str) -> bool {
    let with_equals = format!("{flag}=");
    std::env::args().any(|arg| arg == flag || arg.starts_with(&with_equals))
}

/// The address `serve` binds: the `--bind` flag when given, else
/// `TEMPER_BIND_ADDR`, else the flag's default (all interfaces).
///
/// Only `0.0.0.0` and `127.0.0.1` are accepted. Temper's own components —
/// the Observe UI proxy, WASM host self-calls, the blob endpoint, the
/// Discord transport — reach the API at `http://127.0.0.1:{port}`, so any
/// other address would bind a server those callers cannot reach. A host
/// name is refused rather than resolved.
pub(crate) fn resolve_bind_addr(
    cli_bind: &str,
    bind_explicit: bool,
    env_bind: Option<&str>,
) -> Result<Ipv4Addr, String> {
    let parse = |value: &str, source: &str| match value.trim() {
        "0.0.0.0" => Ok(Ipv4Addr::UNSPECIFIED),
        "127.0.0.1" => Ok(Ipv4Addr::LOCALHOST),
        other => Err(format!(
            "{source} must be 0.0.0.0 (all interfaces) or 127.0.0.1 (loopback only); got {other:?}"
        )),
    };
    if bind_explicit {
        return parse(cli_bind, "--bind");
    }
    match env_bind.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => parse(value, "TEMPER_BIND_ADDR"),
        None => parse(cli_bind, "--bind"),
    }
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

    #[test]
    fn serve_binds_to_all_interfaces_unless_told_otherwise() {
        // Backward compatible: no flag, no env → 0.0.0.0, as before.
        assert_eq!(
            resolve_bind_addr("0.0.0.0", false, None).expect("default should resolve"),
            Ipv4Addr::UNSPECIFIED
        );
        // The env var selects the address when the flag is not given.
        assert_eq!(
            resolve_bind_addr("0.0.0.0", false, Some("127.0.0.1"))
                .expect("loopback env should parse"),
            Ipv4Addr::LOCALHOST
        );
        assert_eq!(
            resolve_bind_addr("0.0.0.0", false, Some(" 127.0.0.1 "))
                .expect("env value should be trimmed"),
            Ipv4Addr::LOCALHOST
        );
        // A set-but-blank env var falls back to the flag's default.
        assert_eq!(
            resolve_bind_addr("0.0.0.0", false, Some("  ")).expect("blank env should be ignored"),
            Ipv4Addr::UNSPECIFIED
        );
        // An explicit flag wins over the env var.
        assert_eq!(
            resolve_bind_addr("127.0.0.1", true, Some("0.0.0.0"))
                .expect("explicit --bind should win"),
            Ipv4Addr::LOCALHOST
        );
    }

    #[test]
    fn bind_addr_rejects_everything_but_the_two_known_addresses() {
        // Host names could resolve anywhere; other IPs would break Temper's
        // internal callers, which reach the API at 127.0.0.1.
        for value in ["localhost", "::1", "10.0.0.5", "not-an-ip"] {
            let error = resolve_bind_addr("0.0.0.0", false, Some(value))
                .expect_err("only 0.0.0.0 and 127.0.0.1 are accepted");
            assert!(error.contains("TEMPER_BIND_ADDR"), "{error}");
            assert!(error.contains(value), "{error}");
        }
        let error = resolve_bind_addr("192.168.1.10", true, None)
            .expect_err("explicit --bind is validated too");
        assert!(error.contains("--bind"), "{error}");
    }
}
