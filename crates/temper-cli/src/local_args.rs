//! Command-line argument shapes for local-first operation.

use std::path::PathBuf;

use clap::{Args, Subcommand};

/// Arguments for the zero-configuration local daemon.
#[derive(Args)]
pub struct UpArgs {
    /// Port to listen on.
    #[arg(short, long, default_value = "3000")]
    pub port: u16,
    /// Local data directory override.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Default tenant.
    #[arg(long, default_value = "default")]
    pub tenant: String,
    /// Do not open Observe after startup.
    #[arg(long)]
    pub no_open: bool,
}

/// Arguments for immutable local application bundle management.
#[derive(Args)]
pub struct AppArgs {
    #[command(subcommand)]
    pub command: AppCommand,
}

#[derive(Subcommand)]
pub enum AppCommand {
    /// Resolve and pin explicit local dependencies.
    Lock {
        /// App workspace path.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Explicit local dependency mapping, repeatable as NAME=PATH.
        #[arg(long = "local")]
        locals: Vec<String>,
    },
    /// Build, verify, and install an immutable local app bundle.
    Install {
        /// App workspace path.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Target tenant.
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Temper server URL.
        #[arg(long, default_value = "http://127.0.0.1:3000")]
        url: String,
        /// Reject a missing or stale dependency lock.
        #[arg(long)]
        locked: bool,
        /// Local data directory used to discover credentials.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Maintain the local content-addressed bundle cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
}

#[derive(Subcommand)]
pub enum CacheCommand {
    /// Remove objects unreachable from durable installation records.
    Gc {
        /// Target tenant used to authorize cache maintenance.
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Local Temper URL.
        #[arg(long, default_value = "http://127.0.0.1:3000")]
        url: String,
        /// Report collectible objects without deleting them.
        #[arg(long)]
        dry_run: bool,
        /// Local data directory used to discover credentials.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

/// Arguments for watching and promoting local app revisions.
#[derive(Args)]
pub struct DevArgs {
    /// App workspace path.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Target tenant.
    #[arg(long, default_value = "default")]
    pub tenant: String,
    /// Local Temper URL.
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    pub url: String,
    /// Local data directory override.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}
