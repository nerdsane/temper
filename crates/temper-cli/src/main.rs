//! temper-cli: Command-line interface for Temper.
//!
//! Provides commands for parsing specifications, generating code,
//! running model checks, and managing Temper projects.

mod codegen;
mod decide;
mod init;
mod install;
mod mcp;
mod serve;
mod verify;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Debug, ValueEnum, PartialEq, Eq)]
pub(crate) enum StorageBackend {
    Postgres,
    Turso,
    Redis,
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
    /// Generate Rust code from specifications
    Codegen {
        /// Path to the specs directory
        #[arg(short, long, default_value = "specs")]
        specs_dir: String,
        /// Output directory for generated code
        #[arg(short, long, default_value = "generated")]
        output_dir: String,
    },
    /// Run the verification cascade
    Verify {
        /// Path to the specs directory
        #[arg(short, long, default_value = "specs")]
        specs_dir: String,
    },
    /// Install the Temper App Builder skill into Claude Code (global)
    Install,
    /// Approve or deny pending governance decisions from the terminal
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
        /// If omitted, startup preserves legacy behavior:
        /// - use Postgres when `DATABASE_URL` is set
        /// - otherwise run in-memory only
        #[arg(long, value_enum, default_value = "turso")]
        storage: StorageBackend,
        /// Load an app: --app name=specs-dir (repeatable)
        #[arg(long)]
        app: Vec<String>,
        /// Skip the Observe UI (Next.js dev server in observe/)
        #[arg(long)]
        no_observe: bool,
        /// Directory containing IOA TOML and CSDL specs to load at startup (legacy, use --app)
        #[arg(long)]
        specs_dir: Option<String>,
        /// Tenant name (used with --specs-dir to load user specs)
        #[arg(long, default_value = "default")]
        tenant: String,
    },
    /// Start the stdio MCP server for Code Mode
    Mcp {
        /// Port where Temper HTTP server is running (omit for self-contained mode).
        /// Mutually exclusive with --url.
        #[arg(short, long, conflicts_with = "url")]
        port: Option<u16>,
        /// Full URL of a remote Temper server (e.g. https://temper.railway.app).
        /// Mutually exclusive with --port.
        #[arg(long, conflicts_with = "port")]
        url: Option<String>,
        /// Load an app: --app name=specs-dir (repeatable)
        #[arg(long)]
        app: Vec<String>,
        /// Agent identity for Cedar authorization and trajectory logging (default: "mcp-agent")
        #[arg(long)]
        agent_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file from project root (silently ignored if missing).
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { name } => init::run(&name)?,
        Commands::Install => install::run()?,
        Commands::Decide { port, tenant } => decide::run(port, &tenant).await?,
        Commands::Codegen {
            specs_dir,
            output_dir,
        } => codegen::run(&specs_dir, &output_dir)?,
        Commands::Verify { specs_dir } => verify::run(&specs_dir)?,
        Commands::Serve {
            port,
            storage,
            app,
            no_observe,
            specs_dir,
            tenant,
        } => {
            let storage_explicit =
                std::env::args().any(|arg| arg == "--storage" || arg.starts_with("--storage="));
            // Build app list from --app flags, fall back to --specs-dir/--tenant
            let mut apps: Vec<(String, String)> = Vec::new();
            for entry in &app {
                if let Some((name, path)) = entry.split_once('=') {
                    apps.push((name.to_string(), path.to_string()));
                } else {
                    anyhow::bail!("Invalid --app format: '{entry}'. Expected name=specs-dir");
                }
            }
            if apps.is_empty()
                && let Some(ref dir) = specs_dir
            {
                apps.push((tenant.clone(), dir.clone()));
            }
            serve::run(port, apps, storage, storage_explicit, !no_observe).await?
        }
        Commands::Mcp {
            port,
            url,
            app,
            agent_id,
        } => {
            let mut apps: Vec<(String, String)> = Vec::new();
            for entry in &app {
                if let Some((name, path)) = entry.split_once('=') {
                    apps.push((name.to_string(), path.to_string()));
                } else {
                    anyhow::bail!("Invalid --app format: '{entry}'. Expected name=specs-dir");
                }
            }
            mcp::run(port, url, apps, agent_id).await?
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
    fn test_cli_parse_install() {
        let cli = Cli::parse_from(["temper", "install"]);
        match cli.command {
            Commands::Install => {}
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
    fn test_cli_parse_mcp_with_port_and_apps() {
        let cli = Cli::parse_from([
            "temper",
            "mcp",
            "--port",
            "3001",
            "--app",
            "haku-ops=apps/haku-ops/specs",
        ]);
        match cli.command {
            Commands::Mcp {
                port,
                url,
                app,
                agent_id,
            } => {
                assert_eq!(port, Some(3001));
                assert_eq!(url, None);
                assert_eq!(app, vec!["haku-ops=apps/haku-ops/specs"]);
                assert_eq!(agent_id, None);
            }
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_with_agent_id() {
        let cli = Cli::parse_from([
            "temper",
            "mcp",
            "--port",
            "3001",
            "--agent-id",
            "haku",
            "--app",
            "haku-ops=apps/haku-ops/specs",
        ]);
        match cli.command {
            Commands::Mcp {
                port,
                url,
                app,
                agent_id,
            } => {
                assert_eq!(port, Some(3001));
                assert_eq!(url, None);
                assert_eq!(app, vec!["haku-ops=apps/haku-ops/specs"]);
                assert_eq!(agent_id, Some("haku".to_string()));
            }
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_cli_parse_mcp_with_url() {
        let cli = Cli::parse_from([
            "temper",
            "mcp",
            "--url",
            "https://temper.railway.app",
            "--app",
            "demo=specs/demo",
        ]);
        match cli.command {
            Commands::Mcp {
                port,
                url,
                app,
                agent_id,
            } => {
                assert_eq!(port, None);
                assert_eq!(url, Some("https://temper.railway.app".to_string()));
                assert_eq!(app, vec!["demo=specs/demo"]);
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
