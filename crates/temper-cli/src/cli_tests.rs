use clap::Parser;

use crate::cli::{Cli, Commands, StorageBackend};

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
fn test_cli_parse_verify() {
    let cli = Cli::parse_from(["temper", "verify", "--specs-dir", "custom-specs"]);
    match cli.command {
        Commands::Verify { specs_dir } => assert_eq!(specs_dir, "custom-specs"),
        _ => panic!("expected Verify command"),
    }
}

#[test]
fn test_cli_parse_genesis_app_install() {
    let cli = Cli::parse_from([
        "temper",
        "install",
        "acme/my-app@deadbeef",
        "--tenant",
        "my-tenant",
        "--registry-tenant",
        "genesis",
        "--url",
        "http://localhost:4000",
        "--installer",
        "ci-bot",
    ]);
    match cli.command {
        Commands::Install {
            app_ref,
            tenant,
            registry_tenant,
            url,
            installer,
        } => {
            assert_eq!(app_ref.as_deref(), Some("acme/my-app@deadbeef"));
            assert_eq!(tenant, "my-tenant");
            assert_eq!(registry_tenant, "genesis");
            assert_eq!(url, "http://localhost:4000");
            assert_eq!(installer, "ci-bot");
        }
        _ => panic!("expected Install command"),
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
