//! temper-cli: Command-line interface for Temper.
//!
//! Provides commands for parsing specifications, generating code,
//! running model checks, and managing Temper projects.

mod codegen;
mod init;
mod serve;
mod verify;

use clap::{Parser, Subcommand};

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
    /// Start the development server
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value = "3000")]
        port: u16,
        /// Run in developer mode (interview + spec generation)
        #[arg(long)]
        dev: bool,
        /// Run in production mode (operate within specs)
        #[arg(long)]
        production: bool,
        /// Directory containing IOA TOML and CSDL specs (for production mode)
        #[arg(long)]
        specs_dir: Option<String>,
        /// Tenant name
        #[arg(long, default_value = "default")]
        tenant: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { name } => init::run(&name)?,
        Commands::Codegen {
            specs_dir,
            output_dir,
        } => codegen::run(&specs_dir, &output_dir)?,
        Commands::Verify { specs_dir } => verify::run(&specs_dir)?,
        Commands::Serve {
            port,
            dev,
            production,
            specs_dir,
            tenant,
        } => serve::run(port, dev, production, specs_dir, tenant).await?,
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
    fn test_cli_parse_serve_default_port() {
        let cli = Cli::parse_from(["temper", "serve"]);
        match cli.command {
            Commands::Serve { port, dev, production, .. } => {
                assert_eq!(port, 3000);
                assert!(!dev);
                assert!(!production);
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
    fn test_cli_parse_serve_dev_mode() {
        let cli = Cli::parse_from(["temper", "serve", "--dev"]);
        match cli.command {
            Commands::Serve { dev, .. } => assert!(dev),
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn test_cli_parse_serve_production_mode() {
        let cli = Cli::parse_from([
            "temper", "serve", "--production", "--specs-dir", "my-specs", "--tenant", "ecommerce",
        ]);
        match cli.command {
            Commands::Serve {
                production,
                specs_dir,
                tenant,
                ..
            } => {
                assert!(production);
                assert_eq!(specs_dir, Some("my-specs".into()));
                assert_eq!(tenant, "ecommerce");
            }
            _ => panic!("expected Serve command"),
        }
    }
}
