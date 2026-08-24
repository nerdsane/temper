use std::io::{self, Write};
use std::path::PathBuf;

use clap::Subcommand;
use temper_platform::module_sdk_build::{
    BindModuleSdkRequest, GenerateModuleSdkRequest, LocalModuleSdkInputs, bind_module_sdk,
    generate_module_sdk,
};

#[derive(Subcommand)]
pub(super) enum Command {
    /// Resolve local metadata, write its lock, and generate typed Rust source.
    Generate(GenerateArgs),
    /// Package compiled WASM and update its exact app-manifest binding.
    Bind(BindArgs),
}

#[derive(clap::Args)]
pub(super) struct CommonArgs {
    /// Root application directory; all conventional paths derive from here.
    #[arg(long)]
    app: PathBuf,
    /// Exact [[wasm_modules]] name to generate.
    #[arg(long)]
    module: String,
    /// Local directory containing dependency app directories; repeatable.
    #[arg(long)]
    dependency_root: Vec<PathBuf>,
    /// Override the conventional APP/app.toml path.
    #[arg(long)]
    app_manifest: Option<PathBuf>,
    /// Override APP/wasm/MODULE/src/temper_module_sdk.rs.
    #[arg(long)]
    source_out: Option<PathBuf>,
    /// Override APP/temper-module-sdk.lock.
    #[arg(long)]
    lock: Option<PathBuf>,
}

#[derive(clap::Args)]
pub(super) struct GenerateArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Fail on drift without rewriting generated files.
    #[arg(long)]
    check: bool,
}

#[derive(clap::Args)]
pub(super) struct BindArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Explicit unbound compiler output.
    #[arg(long)]
    wasm: PathBuf,
    /// Override APP/wasm/MODULE/MODULE.wasm.
    #[arg(long)]
    bound_wasm_out: Option<PathBuf>,
    /// Fail on drift without rewriting the artifact or manifest.
    #[arg(long)]
    check: bool,
}

pub(super) fn run(command: Command) -> anyhow::Result<()> {
    let report = match command {
        Command::Generate(args) => generate_module_sdk(GenerateModuleSdkRequest {
            inputs: inputs(args.common),
            check: args.check,
        }),
        Command::Bind(args) => bind_module_sdk(BindModuleSdkRequest {
            inputs: inputs(args.common),
            wasm: args.wasm,
            bound_wasm_out: args.bound_wasm_out,
            check: args.check,
        }),
    }
    .map_err(anyhow::Error::msg)?;
    writeln!(
        io::stdout().lock(),
        "{}",
        serde_json::to_string_pretty(&report)?
    )?;
    Ok(())
}

fn inputs(args: CommonArgs) -> LocalModuleSdkInputs {
    let dependency_roots = if args.dependency_root.is_empty() {
        args.app.parent().map(PathBuf::from).into_iter().collect()
    } else {
        args.dependency_root
    };
    LocalModuleSdkInputs {
        app: args.app,
        module: args.module,
        dependency_roots,
        app_manifest: args.app_manifest,
        source_out: args.source_out,
        lock: args.lock,
    }
}
