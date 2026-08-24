use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use temper_spec::bundle::{IoaSourceInput, ScopedSpecBundle, ScopedSpecBundleInput};
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, merge_csdl, parse_csdl};

use crate::os_apps::AppManifest;

use super::metadata::{CandidateApp, read_candidate_app};
use super::paths::{
    canonical_directory, reject_path_aliases, resolve_manifest_path, resolve_output,
    scan_dependency_roots,
};
use super::types::{LocalLockedApp, LocalModuleSdkInputs, LocalModuleSdkLock};

pub(super) const LOCAL_MODULE_SDK_RESOLVER_VERSION: &str = "local-module-sdk/v1";
const CLOSURE_APP_BUDGET_V1: usize = 128;

pub(super) struct ResolvedCandidate {
    pub app: PathBuf,
    pub manifest_path: PathBuf,
    pub source_path: PathBuf,
    pub lock_path: PathBuf,
    pub manifest: AppManifest,
    pub csdl: CsdlDocument,
    pub lock: LocalModuleSdkLock,
    pub lock_source: String,
}

pub(super) fn resolve(inputs: &LocalModuleSdkInputs) -> Result<ResolvedCandidate, String> {
    if inputs.module.trim().is_empty() {
        return Err("module name must not be empty".into());
    }
    let app = canonical_directory(&inputs.app, "app root")?;
    let manifest_path = resolve_manifest_path(&app, inputs.app_manifest.as_deref())?;
    let root = read_candidate_app(&app, &manifest_path)?;
    let module = root
        .manifest
        .wasm_modules
        .iter()
        .find(|candidate| candidate.name == inputs.module)
        .ok_or_else(|| {
            format!(
                "module '{}' is not declared by '{}'",
                inputs.module,
                manifest_path.display()
            )
        })?;
    if module.data.is_none() {
        return Err(format!(
            "module '{}' has no [wasm_modules.data] grant",
            inputs.module
        ));
    }

    let dependency_candidates = scan_dependency_roots(&inputs.dependency_roots)?;
    let root_name = root.manifest.name.clone();
    let mut loaded = BTreeMap::from([(root_name.clone(), root)]);
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut order = Vec::new();
    visit(
        &root_name,
        &dependency_candidates,
        &mut loaded,
        &mut visiting,
        &mut visited,
        &mut order,
    )?;

    let (csdl, canonical_csdl, canonical_ioa) = compile_closure(&order, &loaded)?;
    let apps = order
        .iter()
        .map(|name| {
            let app = loaded
                .get(name)
                .ok_or_else(|| format!("resolved app '{name}' disappeared from the closure"))?;
            Ok(LocalLockedApp {
                name: name.clone(),
                version: app.manifest.version.clone(),
                metadata_digest: metadata_digest(app)?,
                dependencies: declared_dependencies(&app.manifest.dependencies),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let lock_digest = digest_serializable(&(
        LOCAL_MODULE_SDK_RESOLVER_VERSION,
        &root_name,
        &inputs.module,
        &apps,
        &canonical_csdl,
        &canonical_ioa,
    ))?;
    let lock = LocalModuleSdkLock {
        resolver_version: LOCAL_MODULE_SDK_RESOLVER_VERSION.into(),
        root: root_name,
        module: inputs.module.clone(),
        digest: lock_digest,
        apps,
    };
    let lock_source = toml::to_string_pretty(&lock)
        .map_err(|error| format!("failed to encode module SDK lock: {error}"))?;
    let source_path = resolve_output(
        &app,
        inputs.source_out.as_deref(),
        &PathBuf::from("wasm")
            .join(&inputs.module)
            .join("src")
            .join("temper_module_sdk.rs"),
        "generated source",
    )?;
    let lock_path = resolve_output(
        &app,
        inputs.lock.as_deref(),
        Path::new("temper-module-sdk.lock"),
        "module SDK lock",
    )?;
    reject_path_aliases(&[
        (&manifest_path, "app manifest"),
        (&source_path, "generated source"),
        (&lock_path, "module SDK lock"),
    ])?;

    let root = loaded
        .remove(&lock.root)
        .ok_or_else(|| "root app disappeared from the resolved closure".to_string())?;
    Ok(ResolvedCandidate {
        app,
        manifest_path,
        source_path,
        lock_path,
        manifest: root.manifest,
        csdl,
        lock,
        lock_source,
    })
}

fn visit(
    name: &str,
    candidates: &BTreeMap<String, Vec<PathBuf>>,
    loaded: &mut BTreeMap<String, CandidateApp>,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    if visited.contains(name) {
        return Ok(());
    }
    if visited.len() + visiting.len() >= CLOSURE_APP_BUDGET_V1 {
        return Err(format!(
            "local app closure budget exceeded: more than {CLOSURE_APP_BUDGET_V1} apps"
        ));
    }
    if !visiting.insert(name.to_string()) {
        return Err(format!("cyclic local app dependency at '{name}'"));
    }
    if !loaded.contains_key(name) {
        let dirs = candidates
            .get(name)
            .ok_or_else(|| format!("missing local dependency '{name}'"))?;
        if dirs.len() != 1 {
            return Err(format!(
                "dependency '{name}' is ambiguous between {}",
                dirs.iter()
                    .map(|path| format!("'{}'", path.display()))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ));
        }
        let dir = &dirs[0];
        let app = read_candidate_app(dir, &dir.join("app.toml"))?;
        if app.manifest.name != name {
            return Err(format!(
                "dependency manifest name '{}' does not match requested '{name}'",
                app.manifest.name
            ));
        }
        loaded.insert(name.to_string(), app);
    }
    let dependencies = dependency_names(&loaded[name].manifest.dependencies);
    for dependency in dependencies {
        visit(&dependency, candidates, loaded, visiting, visited, order)?;
    }
    visiting.remove(name);
    visited.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}

fn compile_closure(
    order: &[String],
    loaded: &BTreeMap<String, CandidateApp>,
) -> Result<(CsdlDocument, String, Vec<String>), String> {
    let mut merged = None;
    let mut symbol_hashes = BTreeMap::new();
    let mut ioa_sources = Vec::new();
    for name in order {
        let app = &loaded[name];
        if let Some(source) = &app.csdl_source {
            let document = parse_csdl(source)
                .map_err(|error| format!("invalid CSDL for app '{name}': {error}"))?;
            super::schema_conflicts::reject(name, &document, &mut symbol_hashes)?;
            merged = Some(match merged {
                Some(existing) => merge_csdl(&existing, &document),
                None => document,
            });
        }
        ioa_sources.extend(app.ioa_sources.iter().cloned());
    }
    let merged = merged.ok_or_else(|| "resolved app closure contains no CSDL".to_string())?;
    let canonical_csdl = emit_csdl_xml(&merged);
    let csdl_types = merged
        .schemas
        .iter()
        .flat_map(|schema| {
            schema.entity_types.iter().map(move |entity| {
                (
                    entity.name.clone(),
                    format!("{}.{}", schema.namespace, entity.name),
                )
            })
        })
        .collect::<Vec<_>>();
    let inputs = ioa_sources
        .iter()
        .map(|source| {
            let automaton = temper_spec::automaton::parse_automaton(source)
                .map_err(|error| format!("invalid closure IOA: {error}"))?;
            let matches = csdl_types
                .iter()
                .filter(|(short, _)| short == &automaton.automaton.name)
                .map(|(_, qualified)| qualified.clone())
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(format!(
                    "IOA type '{}' resolves to {} CSDL entity types",
                    automaton.automaton.name,
                    matches.len()
                ));
            }
            Ok(IoaSourceInput {
                entity_type: matches[0].clone(),
                source: source.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "local-module-sdk-candidate".into(),
        predecessor_digest: None,
        csdl_xml: canonical_csdl,
        ioa_sources: inputs,
        cedar_policies: Vec::new(),
        wasm_modules: Vec::new(),
        migration: None,
        budgets: Default::default(),
    })
    .map_err(|error| format!("invalid local app closure: {error}"))?;
    let canonical_ioa = compiled
        .ioa_specs()
        .iter()
        .map(|spec| spec.canonical_source.clone())
        .collect();
    let canonical_csdl = compiled.canonical_csdl().to_string();
    let csdl = parse_csdl(&canonical_csdl)
        .map_err(|error| format!("failed to parse canonical closure CSDL: {error}"))?;
    Ok((csdl, canonical_csdl, canonical_ioa))
}

fn metadata_digest(app: &CandidateApp) -> Result<String, String> {
    let mut manifest = app.manifest.clone();
    for module in &mut manifest.wasm_modules {
        module.data_binding = None;
    }
    let manifest = serde_json::to_vec(&manifest)
        .map_err(|error| format!("failed to encode app manifest: {error}"))?;
    let mut hasher = Sha256::new();
    push_hash_part(&mut hasher, b"manifest", &manifest);
    if let Some(csdl) = &app.csdl_source {
        push_hash_part(&mut hasher, b"csdl", csdl.as_bytes());
    }
    for source in &app.ioa_sources {
        push_hash_part(&mut hasher, b"ioa", source.as_bytes());
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn digest_serializable(value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("failed to encode deterministic digest input: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn push_hash_part(hasher: &mut Sha256, name: &[u8], bytes: &[u8]) {
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn dependency_names(dependencies: &[String]) -> Vec<String> {
    let mut result = dependencies
        .iter()
        .map(|dependency| {
            dependency
                .rsplit('/')
                .next()
                .unwrap_or(dependency)
                .split('@')
                .next()
                .unwrap_or(dependency)
                .to_string()
        })
        .collect::<Vec<_>>();
    result.sort();
    result.dedup();
    result
}

fn declared_dependencies(dependencies: &[String]) -> Vec<String> {
    let mut result = dependencies
        .iter()
        .map(|dependency| dependency.trim().to_string())
        .collect::<Vec<_>>();
    result.sort();
    result.dedup();
    result
}
