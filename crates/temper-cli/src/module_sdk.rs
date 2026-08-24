use std::path::PathBuf;

use temper_platform::module_sdk_build::{
    BindModuleSdkRequest, GenerateModuleSdkRequest, LocalModuleSdkInputs, bind_module_sdk,
    generate_module_sdk,
};

use crate::{ModuleSdkCommand, ModuleSdkCommonArgs};

pub(super) fn run(command: ModuleSdkCommand) -> anyhow::Result<()> {
    let report = match command {
        ModuleSdkCommand::Generate(args) => generate_module_sdk(GenerateModuleSdkRequest {
            inputs: inputs(args.common),
            check: args.check,
        }),
        ModuleSdkCommand::Bind(args) => bind_module_sdk(BindModuleSdkRequest {
            inputs: inputs(args.common),
            wasm: args.wasm,
            bound_wasm_out: args.bound_wasm_out,
            check: args.check,
        }),
    }
    .map_err(anyhow::Error::msg)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn inputs(args: ModuleSdkCommonArgs) -> LocalModuleSdkInputs {
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
