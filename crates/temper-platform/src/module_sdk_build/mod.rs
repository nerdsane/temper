//! Deterministic offline module SDK generation and artifact binding.

use std::io::Write;
use std::path::{Path, PathBuf};

mod manifest;
mod metadata;
mod paths;
mod resolver;
mod schema_conflicts;
mod types;

pub(crate) use resolver::resolve_materialized_module;

const WASM_ARTIFACT_BYTES_BUDGET_V1: u64 = 256 * 1024 * 1024;

pub use types::{
    BindModuleSdkRequest, GenerateModuleSdkRequest, LocalLockedApp, LocalModuleSdkInputs,
    LocalModuleSdkLock, ModuleSdkBuildReport,
};

/// Resolve local metadata, write its immutable lock, and generate typed Rust source.
pub fn generate_module_sdk(
    request: GenerateModuleSdkRequest,
) -> Result<ModuleSdkBuildReport, String> {
    let resolved = resolver::resolve(&request.inputs)?;
    let module = module_declaration(&resolved.manifest, &request.inputs.module)?;
    let grant = module
        .data
        .clone()
        .ok_or_else(|| format!("module '{}' has no data grant", request.inputs.module))?;
    let generated = temper_codegen::generate_module_sdk(
        &resolved.csdl,
        &request.inputs.module,
        &resolved.lock.digest,
        &resolved.lock.digest,
        "",
        grant,
    )
    .map_err(|error| format!("module SDK generation failed: {error}"))?;

    if request.check {
        check_file(
            &resolved.source_path,
            generated.source.as_bytes(),
            "generated source",
        )?;
        check_file(
            &resolved.lock_path,
            resolved.lock_source.as_bytes(),
            "module SDK lock",
        )?;
    } else {
        write_file(&resolved.source_path, generated.source.as_bytes())?;
        write_file(&resolved.lock_path, resolved.lock_source.as_bytes())?;
    }

    Ok(report(&resolved, None, request.check))
}

/// Package unbound compiler output with its exact SDK binding and update app.toml.
pub fn bind_module_sdk(request: BindModuleSdkRequest) -> Result<ModuleSdkBuildReport, String> {
    let resolved = resolver::resolve(&request.inputs)?;
    let module = module_declaration(&resolved.manifest, &request.inputs.module)?;
    let grant = module
        .data
        .clone()
        .ok_or_else(|| format!("module '{}' has no data grant", request.inputs.module))?;
    let generated = temper_codegen::generate_module_sdk(
        &resolved.csdl,
        &request.inputs.module,
        &resolved.lock.digest,
        &resolved.lock.digest,
        "",
        grant,
    )
    .map_err(|error| format!("module SDK generation failed: {error}"))?;
    check_file(
        &resolved.source_path,
        generated.source.as_bytes(),
        "generated source",
    )?;
    check_file(
        &resolved.lock_path,
        resolved.lock_source.as_bytes(),
        "module SDK lock",
    )?;

    let wasm_path = request.wasm.canonicalize().map_err(|error| {
        format!(
            "failed to resolve compiled WASM '{}': {error}",
            request.wasm.display()
        )
    })?;
    if !wasm_path.is_file() {
        return Err(format!(
            "compiled WASM '{}' is not a file",
            wasm_path.display()
        ));
    }
    let wasm = read_bounded_bytes(&wasm_path, "compiled WASM", WASM_ARTIFACT_BYTES_BUDGET_V1)?;
    let packaged = temper_codegen::package_generated_module_sdk(&wasm, generated)
        .map_err(|error| format!("module SDK binding failed: {error}"))?;
    if packaged.wasm.len() as u64 > WASM_ARTIFACT_BYTES_BUDGET_V1 {
        return Err(format!(
            "bound WASM byte budget exceeded: {} > {WASM_ARTIFACT_BYTES_BUDGET_V1}",
            packaged.wasm.len()
        ));
    }
    let default_output = PathBuf::from("wasm")
        .join(&request.inputs.module)
        .join(format!("{}.wasm", request.inputs.module));
    let bound_wasm = paths::resolve_output(
        &resolved.app,
        request.bound_wasm_out.as_deref(),
        &default_output,
        "bound WASM",
    )?;
    paths::reject_path_aliases(&[
        (&resolved.manifest_path, "app manifest"),
        (&resolved.source_path, "generated source"),
        (&resolved.lock_path, "module SDK lock"),
        (&wasm_path, "compiled WASM"),
        (&bound_wasm, "bound WASM"),
    ])?;
    let manifest_source = manifest::read(&resolved.manifest_path)?;
    let expected_manifest = manifest::with_module_binding(
        &manifest_source,
        &request.inputs.module,
        &packaged.manifest,
    )?;

    if request.check {
        check_file(&bound_wasm, &packaged.wasm, "bound WASM")?;
        check_file(
            &resolved.manifest_path,
            expected_manifest.as_bytes(),
            "app manifest binding",
        )?;
    } else {
        publish_binding(
            &bound_wasm,
            &packaged.wasm,
            &resolved.manifest_path,
            expected_manifest.as_bytes(),
        )?;
    }

    Ok(report(&resolved, Some(bound_wasm), request.check))
}

fn module_declaration<'a>(
    manifest: &'a crate::os_apps::AppManifest,
    module: &str,
) -> Result<&'a crate::os_apps::WasmModuleManifest, String> {
    manifest
        .wasm_modules
        .iter()
        .find(|candidate| candidate.name == module)
        .ok_or_else(|| format!("module '{module}' is absent from app manifest"))
}

fn check_file(path: &Path, expected: &[u8], label: &str) -> Result<(), String> {
    let metadata = path.metadata().map_err(|error| {
        format!(
            "{label} '{}' is missing or unreadable: {error}",
            path.display()
        )
    })?;
    if metadata.len() != expected.len() as u64 {
        return Err(format!("{label} drift detected at '{}'", path.display()));
    }
    let actual = std::fs::read(path)
        .map_err(|error| format!("failed to read {label} '{}': {error}", path.display()))?;
    if actual != expected {
        return Err(format!("{label} drift detected at '{}'", path.display()));
    }
    Ok(())
}

fn read_bounded_bytes(path: &Path, label: &str, byte_budget: u64) -> Result<Vec<u8>, String> {
    let bytes = path
        .metadata()
        .map_err(|error| format!("failed to inspect {label} '{}': {error}", path.display()))?
        .len();
    if bytes > byte_budget {
        return Err(format!(
            "{label} byte budget exceeded for '{}': {bytes} > {byte_budget}",
            path.display()
        ));
    }
    std::fs::read(path)
        .map_err(|error| format!("failed to read {label} '{}': {error}", path.display()))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    persist_staged(stage_file(path, bytes)?, path)
}

fn stage_file(path: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("output '{}' has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "failed to create output directory '{}': {error}",
            parent.display()
        )
    })?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        format!(
            "failed to stage output beside '{}': {error}",
            path.display()
        )
    })?;
    staged
        .write_all(bytes)
        .and_then(|()| staged.flush())
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|error| format!("failed to stage '{}': {error}", path.display()))?;
    Ok(staged)
}

fn persist_staged(staged: tempfile::NamedTempFile, path: &Path) -> Result<(), String> {
    staged.persist(path).map(|_| ()).map_err(|error| {
        format!(
            "failed to publish staged output '{}': {}",
            path.display(),
            error.error
        )
    })
}

fn publish_binding(
    artifact_path: &Path,
    artifact: &[u8],
    manifest_path: &Path,
    manifest: &[u8],
) -> Result<(), String> {
    publish_binding_with(
        artifact_path,
        artifact,
        manifest_path,
        manifest,
        persist_staged,
    )
}

fn publish_binding_with(
    artifact_path: &Path,
    artifact: &[u8],
    manifest_path: &Path,
    manifest: &[u8],
    mut publish: impl FnMut(tempfile::NamedTempFile, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let prior_artifact = match artifact_path.metadata() {
        Ok(_) => Some(read_bounded_bytes(
            artifact_path,
            "prior bound WASM",
            WASM_ARTIFACT_BYTES_BUDGET_V1,
        )?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "failed to preserve prior bound WASM '{}': {error}",
                artifact_path.display()
            ));
        }
    };
    let staged_artifact = stage_file(artifact_path, artifact)?;
    let staged_manifest = stage_file(manifest_path, manifest)?;
    publish(staged_artifact, artifact_path)?;
    if let Err(error) = publish(staged_manifest, manifest_path) {
        let rollback = match prior_artifact {
            Some(bytes) => write_file(artifact_path, &bytes),
            None => std::fs::remove_file(artifact_path).map_err(|rollback_error| {
                format!(
                    "failed to remove newly published artifact '{}': {rollback_error}",
                    artifact_path.display()
                )
            }),
        };
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!(
                "{error}; artifact rollback failed: {rollback_error}"
            )),
        };
    }
    Ok(())
}

fn report(
    resolved: &resolver::ResolvedCandidate,
    bound_wasm: Option<PathBuf>,
    checked: bool,
) -> ModuleSdkBuildReport {
    ModuleSdkBuildReport {
        app: resolved.app.clone(),
        module: resolved.lock.module.clone(),
        app_manifest: resolved.manifest_path.clone(),
        source: resolved.source_path.clone(),
        lock: resolved.lock_path.clone(),
        bound_wasm,
        closure_digest: resolved.lock.digest.clone(),
        checked,
    }
}

#[cfg(test)]
mod tests;
