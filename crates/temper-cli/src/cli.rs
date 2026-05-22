use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Debug, ValueEnum, PartialEq, Eq)]
pub(crate) enum StorageBackend {
    Postgres,
    Turso,
    Redis,
}

#[derive(Parser)]
#[command(name = "temper", about = "Temper framework CLI")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    Init {
        name: String,
    },
    Codegen {
        #[arg(short, long, default_value = "specs")]
        specs_dir: String,
        #[arg(short, long, default_value = "generated")]
        output_dir: String,
    },
    Verify {
        #[arg(short, long, default_value = "specs")]
        specs_dir: String,
    },
    Install {
        app_ref: Option<String>,
        #[arg(long, default_value = "default")]
        tenant: String,
        #[arg(long, default_value = "default")]
        registry_tenant: String,
        #[arg(long, default_value = "http://127.0.0.1:3000")]
        url: String,
        #[arg(long, default_value = "temper-cli")]
        installer: String,
    },
    Decide {
        #[arg(short, long, default_value = "3000")]
        port: u16,
        #[arg(short, long, default_value = "default")]
        tenant: String,
    },
    Serve {
        #[arg(short, long, default_value = "3000")]
        port: u16,
        #[arg(long, value_enum, default_value = "turso")]
        storage: StorageBackend,
        #[arg(long)]
        app: Vec<String>,
        #[arg(long)]
        no_observe: bool,
        #[arg(long)]
        specs_dir: Option<String>,
        #[arg(long, default_value = "default")]
        tenant: String,
        #[arg(long)]
        os_app: Vec<String>,
        #[arg(long)]
        verify_subprocess: bool,
    },
    VerifyIoa,
    Mcp {
        #[arg(short, long, conflicts_with = "url")]
        port: Option<u16>,
        #[arg(long, conflicts_with = "port")]
        url: Option<String>,
        #[arg(long, hide = true)]
        agent_id: Option<String>,
    },
}
