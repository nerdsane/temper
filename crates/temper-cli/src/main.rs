//! temper-cli: Command-line interface for Temper.
//!
//! Provides commands for parsing specifications, generating code,
//! running model checks, and managing Temper projects.

/// Use jemalloc to aggressively return freed pages to the OS via MADV_DONTNEED,
/// preventing the RSS bloat caused by glibc malloc on Debian bookworm (Railway).
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod codegen;
mod decide;
mod init;
mod install;
mod local;
mod local_args;
mod mcp;
mod migrate_turso_to_postgres;
mod module_sdk;
mod serve;
mod util;
mod verify;
mod verify_ioa;
mod verify_remote;

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

const DEFAULT_TOKIO_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, ValueEnum, PartialEq, Eq)]
pub(crate) enum StorageBackend {
    Postgres,
    Turso,
    Redis,
}

#[derive(Clone, Debug, ValueEnum, PartialEq, Eq)]
pub(crate) enum ActorRuntimeBackend {
    Legacy,
    Postgres,
}

#[derive(Parser)]
#[command(name = "temper", about = "Temper framework CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new Temper project
    Init { name: String },
    /// Run a persistent, self-contained local Temper instance.
    Up(local_args::UpArgs),
    /// Manage immutable local application bundles.
    App(local_args::AppArgs),
    /// Watch a local app and promote each verified immutable revision.
    Dev(local_args::DevArgs),
    /// Generate Rust code from specifications
    Codegen {
        /// Path to the specs directory
        #[arg(short, long, default_value = "specs")]
        specs_dir: String,
        /// Output directory for generated code
        #[arg(short, long, default_value = "generated")]
        output_dir: String,
    },
    /// Generate and bind typed Rust SDKs for local application modules.
    ModuleSdk {
        #[command(subcommand)]
        command: module_sdk::Command,
    },
    /// Run the verification cascade
    Verify {
        /// Path to the specs directory
        #[arg(short, long, default_value = "specs")]
        specs_dir: String,
    },
    /// Lint specs locally, then run the verification cascade on a remote Temper server
    VerifyRemote {
        /// Path to the specs directory
        #[arg(short, long, default_value = "specs")]
        specs_dir: String,
        /// Base URL for the Temper server.
        #[arg(long, default_value = "http://127.0.0.1:3000")]
        url: String,
        /// Remote simulation seed budget.
        #[arg(long, default_value_t = 5)]
        sim_seeds: u64,
        /// Remote property-test case budget.
        #[arg(long, default_value_t = 100)]
        prop_test_cases: u32,
    },
    /// Install Claude/Codex helper skills, or install a Genesis app ref into Temper
    Install {
        /// Genesis app ref to install, for example `owner/app@hash`. Omit to install Claude/Codex helper skills.
        app_ref: Option<String>,
        /// Target tenant for Genesis app installation.
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Tenant that hosts the Genesis registry App row.
        #[arg(long, default_value = "default")]
        registry_tenant: String,
        /// Base URL for the Temper/Genesis server.
        #[arg(long, default_value = "http://127.0.0.1:3000")]
        url: String,
        /// Installer identity recorded on the AppInstallation row.
        #[arg(long, default_value = "temper-cli")]
        installer: String,
        /// Genesis install follow policy: pinned or follow_latest.
        #[arg(long, default_value = "pinned")]
        follow_policy: String,
    },
    /// Approve or deny pending governance decisions using `TEMPER_API_KEY`
    Decide {
        /// Port where Temper HTTP server is running
        #[arg(short, long, default_value = "3000")]
        port: u16,
        /// Tenant name to watch for decisions
        #[arg(short, long, default_value = "default")]
        tenant: String,
    },
    /// Start the platform server
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value = "3000")]
        port: u16,
        /// Storage backend (`postgres`, `turso`, `redis`).
        ///
        /// If omitted, `TEMPER_EVENT_STORE` can select the backend. When neither
        /// is set, startup preserves today's default Turso/libSQL backend.
        #[arg(long, value_enum, default_value = "turso")]
        storage: StorageBackend,
        /// Actor runtime (`legacy`, `postgres`).
        ///
        /// If omitted, `TEMPER_ACTOR_RUNTIME` can select the runtime. The
        /// Postgres runtime requires Postgres storage and DATABASE_URL.
        #[arg(long, value_enum, default_value = "legacy")]
        actor_runtime: ActorRuntimeBackend,
        /// Entity type to route through the selected actor runtime.
        ///
        /// Repeatable. Comma-separated values are accepted. Use `tenant:Type`
        /// to canary a type for one tenant. If omitted while
        /// `--actor-runtime postgres` is selected, all registered entity types
        /// must pass the actor-runtime compatibility gate.
        #[arg(long)]
        actor_backed_type: Vec<String>,
        /// Install or load an app at startup. Repeatable. Two formats:
        ///   --app NAME           install a built-in OS app from the embedded catalog
        ///                        (e.g. --app project-management)
        ///   --app NAME=DIR       load a user app's specs from a directory
        ///                        (e.g. --app ecommerce=reference-apps/ecommerce/specs)
        ///
        /// `--os-app` is accepted as an alias for backward compatibility.
        #[arg(long, visible_alias = "os-app")]
        app: Vec<String>,
        /// Skip the Observe UI (Next.js dev server in observe/)
        #[arg(long)]
        no_observe: bool,
        /// Directory containing IOA TOML and CSDL specs to load at startup (legacy, use --app NAME=DIR)
        #[arg(long)]
        specs_dir: Option<String>,
        /// Tenant name (used with --specs-dir to load user specs)
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Run spec verification in an isolated subprocess (panics/hangs won't crash the server).
        ///
        /// Each entity's IOA source is written to stdin of `temper verify-ioa`;
        /// the result is read from stdout as JSON. A 30-second timeout is applied
        /// per entity. Exit 0 = pass; non-zero or timeout = failure.
        #[arg(long)]
        verify_subprocess: bool,
        /// Discord bot token for channel transport (enables Discord Gateway).
        /// Falls back to DISCORD_BOT_TOKEN env var if not provided.
        #[arg(long)]
        discord_bot_token: Option<String>,
    },
    /// Run the verification cascade on IOA TOML source read from stdin.
    ///
    /// Reads the full IOA TOML source from stdin, runs the verification cascade,
    /// and writes a JSON-encoded `CascadeResult` to stdout.
    /// Exits 0 if all levels pass; exits 1 if any level fails or an error occurs.
    ///
    /// This subcommand is used internally by `temper serve --verify-subprocess`.
    VerifyIoa,
    /// Copy Turso/libSQL data into a PostgreSQL store.
    MigrateTursoToPostgres {
        /// Tenant to migrate, or `all` to discover tenants from source data.
        #[arg(long)]
        tenant: String,
        /// Read and verify the source without writing to Postgres.
        #[arg(long)]
        dry_run: bool,
        /// Compare copied row counts and checksums after the migration.
        #[arg(long)]
        verify: bool,
        /// Copy entity snapshots in addition to event journals.
        #[arg(long)]
        from_snapshot: bool,
        /// Path where the migration manifest JSON will be written.
        #[arg(long, default_value = "migration_manifest.json")]
        manifest: PathBuf,
        /// Source Turso/libSQL URL. Falls back to TURSO_URL.
        #[arg(long)]
        turso_url: Option<String>,
        /// Source Turso Cloud auth token. Falls back to TURSO_AUTH_TOKEN.
        #[arg(long)]
        turso_auth_token: Option<String>,
        /// Target PostgreSQL URL. Falls back to DATABASE_URL.
        #[arg(long)]
        database_url: Option<String>,
    },
    /// Start the stdio MCP server for Code Mode
    Mcp {
        /// Port where Temper HTTP server is running.
        /// Mutually exclusive with --url.
        #[arg(short, long, conflicts_with = "url")]
        port: Option<u16>,
        /// Full URL of a remote Temper server (e.g. https://temper.railway.app).
        /// Mutually exclusive with --port.
        #[arg(long, conflicts_with = "port")]
        url: Option<String>,
        /// Override auto-derived agent identity (deprecated: identity is now auto-derived)
        #[arg(long, hide = true)]
        agent_id: Option<String>,
    },
}

fn resolve_storage_backend(
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

fn resolve_actor_runtime_backend(
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

fn resolve_tokio_thread_stack_bytes(env_value: Option<&str>) -> Result<usize, String> {
    let Some(value) = env_value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_TOKIO_THREAD_STACK_BYTES);
    };

    value
        .parse::<usize>()
        .map_err(|_| format!("TEMPER_TOKIO_THREAD_STACK_BYTES must be an integer; got {value:?}"))
        .and_then(|bytes| {
            if bytes < 2 * 1024 * 1024 {
                Err(format!(
                    "TEMPER_TOKIO_THREAD_STACK_BYTES must be at least 2097152; got {bytes}"
                ))
            } else {
                Ok(bytes)
            }
        })
}

fn main() -> anyhow::Result<()> {
    // Load .env file from project root (silently ignored if missing).
    dotenvy::dotenv().ok();

    let tokio_thread_stack_bytes = resolve_tokio_thread_stack_bytes(
        std::env::var("TEMPER_TOKIO_THREAD_STACK_BYTES")
            .ok()
            .as_deref(),
    )
    .map_err(|err| anyhow::anyhow!(err))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("temper-cli")
        .thread_stack_size(tokio_thread_stack_bytes)
        .build()?;

    runtime.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { name } => init::run(&name)?,
        Commands::Up(args) => local::run_up(args).await?,
        Commands::App(args) => local::run_app(args).await?,
        Commands::Dev(args) => local::run_dev(args).await?,
        Commands::Install {
            app_ref,
            tenant,
            registry_tenant,
            url,
            installer,
            follow_policy,
        } => match app_ref {
            Some(app_ref) => {
                install::run_genesis_app(
                    &url,
                    &registry_tenant,
                    &tenant,
                    &app_ref,
                    &installer,
                    &follow_policy,
                )
                .await?
            }
            None => install::run()?,
        },
        Commands::Decide { port, tenant } => decide::run(port, &tenant).await?,
        Commands::Codegen {
            specs_dir,
            output_dir,
        } => codegen::run(&specs_dir, &output_dir)?,
        Commands::ModuleSdk { command } => module_sdk::run(command)?,
        Commands::Verify { specs_dir } => verify::run(&specs_dir)?,
        Commands::VerifyRemote {
            specs_dir,
            url,
            sim_seeds,
            prop_test_cases,
        } => verify_remote::run(&specs_dir, &url, sim_seeds, prop_test_cases).await?,
        Commands::VerifyIoa => verify_ioa::run()?,
        Commands::Serve {
            port,
            storage,
            actor_runtime,
            actor_backed_type,
            app,
            no_observe,
            specs_dir,
            tenant,
            verify_subprocess,
            discord_bot_token,
        } => {
            let storage_explicit =
                std::env::args().any(|arg| arg == "--storage" || arg.starts_with("--storage="));
            let storage_env = std::env::var("TEMPER_EVENT_STORE").ok();
            let storage_config_explicit = storage_explicit
                || storage_env
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
            let storage =
                resolve_storage_backend(storage, storage_explicit, storage_env.as_deref())
                    .map_err(|err| anyhow::anyhow!(err))?;
            let actor_runtime_explicit = std::env::args()
                .any(|arg| arg == "--actor-runtime" || arg.starts_with("--actor-runtime="));
            let actor_runtime_env = std::env::var("TEMPER_ACTOR_RUNTIME").ok();
            let actor_runtime = resolve_actor_runtime_backend(
                actor_runtime,
                actor_runtime_explicit,
                actor_runtime_env.as_deref(),
            )
            .map_err(|err| anyhow::anyhow!(err))?;
            let actor_backed_type = if actor_backed_type.is_empty() {
                std::env::var("TEMPER_ACTOR_BACKED_TYPES")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .into_iter()
                    .collect()
            } else {
                actor_backed_type
            };
            // Split --app entries by format:
            //   NAME=DIR  → load user app's specs from directory
            //   NAME      → install a built-in OS app from the embedded catalog
            let mut apps: Vec<(String, String)> = Vec::new();
            let mut os_app_installs: Vec<String> = Vec::new();
            for entry in &app {
                if let Some((name, path)) = entry.split_once('=') {
                    apps.push((name.to_string(), path.to_string()));
                } else {
                    os_app_installs.push(entry.clone());
                }
            }
            if apps.is_empty()
                && let Some(ref dir) = specs_dir
            {
                apps.push((tenant.clone(), dir.clone()));
            }
            let discord_token =
                discord_bot_token.or_else(|| std::env::var("DISCORD_BOT_TOKEN").ok()); // determinism-ok: read once at startup
            serve::run(
                port,
                apps,
                os_app_installs,
                storage,
                storage_config_explicit,
                actor_runtime,
                actor_backed_type,
                !no_observe,
                verify_subprocess,
                discord_token,
                tenant,
                None,
                "0.0.0.0".to_string(),
                None,
                None,
            )
            .await?
        }
        Commands::Mcp {
            port,
            url,
            agent_id,
        } => mcp::run(port, url, agent_id).await?,
        Commands::MigrateTursoToPostgres {
            tenant,
            dry_run,
            verify,
            from_snapshot,
            manifest,
            turso_url,
            turso_auth_token,
            database_url,
        } => {
            migrate_turso_to_postgres::run(migrate_turso_to_postgres::MigrationOptions {
                tenant,
                dry_run,
                verify,
                from_snapshot,
                manifest_path: manifest,
                turso_url,
                turso_auth_token,
                database_url,
            })
            .await?
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn test_cli_parse_init() {
        let cli = Cli::parse_from(["temper", "init", "my-project"]);
        match cli.command {
            Commands::Init { name } => assert_eq!(name, "my-project"),
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_cli_parse_codegen_defaults() {
        let cli = Cli::parse_from(["temper", "codegen"]);
        match cli.command {
            Commands::Codegen {
                specs_dir,
                output_dir,
            } => {
                assert_eq!(specs_dir, "specs");
                assert_eq!(output_dir, "generated");
            }
            _ => panic!("expected Codegen command"),
        }
    }

    #[test]
    fn test_cli_parse_codegen_custom() {
        let cli = Cli::parse_from([
            "temper",
            "codegen",
            "--specs-dir",
            "my-specs",
            "--output-dir",
            "my-out",
        ]);
        match cli.command {
            Commands::Codegen {
                specs_dir,
                output_dir,
            } => {
                assert_eq!(specs_dir, "my-specs");
                assert_eq!(output_dir, "my-out");
            }
            _ => panic!("expected Codegen command"),
        }
    }

    #[test]
    fn test_cli_parse_verify() {
        let cli = Cli::parse_from(["temper", "verify", "--specs-dir", "custom-specs"]);
        match cli.command {
            Commands::Verify { specs_dir } => assert_eq!(specs_dir, "custom-specs"),
            _ => panic!("expected Verify command"),
        }
    }

    #[test]
    fn test_cli_parse_verify_remote() {
        let cli = Cli::parse_from([
            "temper",
            "verify-remote",
            "--specs-dir",
            "custom-specs",
            "--url",
            "https://temper.example",
            "--sim-seeds",
            "3",
            "--prop-test-cases",
            "25",
        ]);
        match cli.command {
            Commands::VerifyRemote {
                specs_dir,
                url,
                sim_seeds,
                prop_test_cases,
            } => {
                assert_eq!(specs_dir, "custom-specs");
                assert_eq!(url, "https://temper.example");
                assert_eq!(sim_seeds, 3);
                assert_eq!(prop_test_cases, 25);
            }
            _ => panic!("expected VerifyRemote command"),
        }
    }

    #[test]
    fn test_cli_parse_install() {
        let cli = Cli::parse_from(["temper", "install"]);
        match cli.command {
            Commands::Install { app_ref, .. } => assert!(app_ref.is_none()),
            _ => panic!("expected Install command"),
        }
    }

    #[test]
    fn test_cli_parse_genesis_app_install() {
        let cli = Cli::parse_from([
            "temper",
            "install",
            "acme/notes@abc123",
            "--tenant",
            "tenant-a",
            "--registry-tenant",
            "genesis",
            "--url",
            "https://genesis.example",
            "--installer",
            "temperpaw",
            "--follow-policy",
            "follow_latest",
        ]);
        match cli.command {
            Commands::Install {
                app_ref,
                tenant,
                registry_tenant,
                url,
                installer,
                follow_policy,
            } => {
                assert_eq!(app_ref.as_deref(), Some("acme/notes@abc123"));
                assert_eq!(tenant, "tenant-a");
                assert_eq!(registry_tenant, "genesis");
                assert_eq!(url, "https://genesis.example");
                assert_eq!(installer, "temperpaw");
                assert_eq!(follow_policy, "follow_latest");
            }
            _ => panic!("expected Install command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_default_port() {
        let cli = Cli::parse_from(["temper", "serve"]);
        match cli.command {
            Commands::Serve { port, .. } => {
                assert_eq!(port, 3000);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_custom_port() {
        let cli = Cli::parse_from(["temper", "serve", "--port", "8080"]);
        match cli.command {
            Commands::Serve { port, .. } => assert_eq!(port, 8080),
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_with_specs() {
        let cli = Cli::parse_from([
            "temper",
            "serve",
            "--specs-dir",
            "my-specs",
            "--tenant",
            "my-app",
        ]);
        match cli.command {
            Commands::Serve {
                specs_dir, tenant, ..
            } => {
                assert_eq!(specs_dir, Some("my-specs".into()));
                assert_eq!(tenant, "my-app");
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_with_app_flags() {
        let cli = Cli::parse_from([
            "temper",
            "serve",
            "--app",
            "ecommerce=specs/ecommerce",
            "--app",
            "linear=specs/linear",
        ]);
        match cli.command {
            Commands::Serve { app, .. } => {
                assert_eq!(app.len(), 2);
                assert_eq!(app[0], "ecommerce=specs/ecommerce");
                assert_eq!(app[1], "linear=specs/linear");
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_with_storage() {
        let cli = Cli::parse_from(["temper", "serve", "--storage", "turso"]);
        match cli.command {
            Commands::Serve {
                storage: StorageBackend::Turso,
                ..
            } => {}
            _ => panic!("expected Serve command with turso storage"),
        }
    }

    #[test]
    fn test_cli_parse_serve_with_postgres_actor_runtime() {
        let cli = Cli::parse_from([
            "temper",
            "serve",
            "--actor-runtime",
            "postgres",
            "--actor-backed-type",
            "Order",
            "--actor-backed-type",
            "Invoice,Customer",
        ]);
        match cli.command {
            Commands::Serve {
                actor_runtime: ActorRuntimeBackend::Postgres,
                actor_backed_type,
                ..
            } => {
                assert_eq!(actor_backed_type, vec!["Order", "Invoice,Customer"]);
            }
            _ => panic!("expected Serve command with postgres actor runtime"),
        }
    }

    #[test]
    fn test_cli_parse_serve_default_storage() {
        let cli = Cli::parse_from(["temper", "serve"]);
        match cli.command {
            Commands::Serve {
                storage: StorageBackend::Turso,
                ..
            } => {}
            _ => panic!("expected Serve command with default turso storage"),
        }
    }

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
    fn tokio_thread_stack_defaults_and_accepts_override() {
        assert_eq!(
            resolve_tokio_thread_stack_bytes(None).expect("default stack size should resolve"),
            DEFAULT_TOKIO_THREAD_STACK_BYTES
        );
        assert_eq!(
            resolve_tokio_thread_stack_bytes(Some("8388608"))
                .expect("valid stack override should parse"),
            8 * 1024 * 1024
        );
    }

    #[test]
    fn tokio_thread_stack_rejects_tiny_or_invalid_override() {
        assert!(resolve_tokio_thread_stack_bytes(Some("abc")).is_err());
        assert!(resolve_tokio_thread_stack_bytes(Some("1048576")).is_err());
    }

    #[test]
    fn railway_start_command_does_not_pin_turso_storage() {
        let railway_toml = include_str!("../../../railway.toml");
        assert!(
            !railway_toml.contains("--storage turso"),
            "Railway must allow TEMPER_EVENT_STORE=postgres to select the backend"
        );
    }

    #[test]
    fn test_cli_parse_migrate_turso_to_postgres() {
        let cli = Cli::parse_from([
            "temper",
            "migrate-turso-to-postgres",
            "--tenant",
            "all",
            "--dry-run",
            "--verify",
            "--from-snapshot",
            "--manifest",
            "migration_manifest.json",
            "--turso-url",
            "file:temper.db",
            "--database-url",
            "postgres://temper:temper_dev@localhost:5432/temper",
        ]);
        match cli.command {
            Commands::MigrateTursoToPostgres {
                tenant,
                dry_run,
                verify,
                from_snapshot,
                manifest,
                turso_url,
                database_url,
                ..
            } => {
                assert_eq!(tenant, "all");
                assert!(dry_run);
                assert!(verify);
                assert!(from_snapshot);
                assert_eq!(
                    manifest,
                    std::path::PathBuf::from("migration_manifest.json")
                );
                assert_eq!(turso_url, Some("file:temper.db".to_string()));
                assert_eq!(
                    database_url,
                    Some("postgres://temper:temper_dev@localhost:5432/temper".to_string())
                );
            }
            _ => panic!("expected MigrateTursoToPostgres command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_with_app_install() {
        // Bare `--app NAME` (no =) installs a built-in OS app.
        let cli = Cli::parse_from(["temper", "serve", "--app", "project-management"]);
        match cli.command {
            Commands::Serve { app, .. } => {
                assert_eq!(app.len(), 1);
                assert_eq!(app[0], "project-management");
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_with_os_app_alias() {
        // --os-app is also a backward-compatible alias for --app.
        let cli = Cli::parse_from(["temper", "serve", "--os-app", "project-management"]);
        match cli.command {
            Commands::Serve { app, .. } => {
                assert_eq!(app.len(), 1);
                assert_eq!(app[0], "project-management");
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_with_multiple_apps() {
        // Multiple --app flags accumulate; both formats can mix.
        let cli = Cli::parse_from([
            "temper",
            "serve",
            "--app",
            "project-management",
            "--app",
            "crm=specs/crm",
        ]);
        match cli.command {
            Commands::Serve { app, .. } => {
                assert_eq!(app.len(), 2);
                assert_eq!(app[0], "project-management");
                assert_eq!(app[1], "crm=specs/crm");
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_no_port() {
        let cli = Cli::parse_from(["temper", "mcp"]);
        match cli.command {
            Commands::Mcp {
                port,
                url,
                agent_id,
                ..
            } => {
                assert_eq!(port, None);
                assert_eq!(url, None);
                assert_eq!(agent_id, None);
            }
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_with_port() {
        let cli = Cli::parse_from(["temper", "mcp", "--port", "3001"]);
        match cli.command {
            Commands::Mcp {
                port,
                url,
                agent_id,
            } => {
                assert_eq!(port, Some(3001));
                assert_eq!(url, None);
                assert_eq!(agent_id, None);
            }
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_with_agent_id() {
        let cli = Cli::parse_from(["temper", "mcp", "--port", "3001", "--agent-id", "haku"]);
        match cli.command {
            Commands::Mcp {
                port,
                url,
                agent_id,
            } => {
                assert_eq!(port, Some(3001));
                assert_eq!(url, None);
                assert_eq!(agent_id, Some("haku".to_string()));
            }
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_with_url() {
        let cli = Cli::parse_from(["temper", "mcp", "--url", "https://temper.railway.app"]);
        match cli.command {
            Commands::Mcp {
                port,
                url,
                agent_id,
            } => {
                assert_eq!(port, None);
                assert_eq!(url, Some("https://temper.railway.app".to_string()));
                assert_eq!(agent_id, None);
            }
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_url_and_port_conflict() {
        let result = Cli::try_parse_from([
            "temper",
            "mcp",
            "--port",
            "3001",
            "--url",
            "https://temper.railway.app",
        ]);
        assert!(
            result.is_err(),
            "--port and --url should be mutually exclusive"
        );
    }
}
