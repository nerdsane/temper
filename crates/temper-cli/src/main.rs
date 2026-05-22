//! temper-cli: Command-line interface for Temper.
//!
//! Provides commands for parsing specifications, generating code,
//! running model checks, and managing Temper projects.

/// Use jemalloc to aggressively return freed pages to the OS via MADV_DONTNEED,
/// preventing the RSS bloat caused by glibc malloc on Debian bookworm (Railway).
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod cli;
mod codegen;
mod decide;
mod init;
mod install;
mod mcp;
mod serve;
mod util;
mod verify;
mod verify_ioa;

use clap::Parser;

use crate::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env file from project root (silently ignored if missing).
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { name } => init::run(&name)?,
        Commands::Install {
            app_ref,
            tenant,
            registry_tenant,
            url,
            installer,
        } => match app_ref {
            Some(app_ref) => {
                install::run_genesis_app(&url, &registry_tenant, &tenant, &app_ref, &installer)
                    .await?
            }
            None => install::run()?,
        },
        Commands::Decide { port, tenant } => decide::run(port, &tenant).await?,
        Commands::Codegen {
            specs_dir,
            output_dir,
        } => codegen::run(&specs_dir, &output_dir)?,
        Commands::Verify { specs_dir } => verify::run(&specs_dir)?,
        Commands::VerifyIoa => verify_ioa::run()?,
        Commands::Serve {
            port,
            storage,
            app,
            no_observe,
            specs_dir,
            tenant,
            os_app,
            verify_subprocess,
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
            serve::run(
                port,
                apps,
                os_app,
                storage,
                storage_explicit,
                !no_observe,
                verify_subprocess,
            )
            .await?
        }
        Commands::Mcp {
            port,
            url,
            agent_id,
        } => mcp::run(port, url, agent_id).await?,
    }

    Ok(())
}

#[cfg(test)]
mod cli_tests;
