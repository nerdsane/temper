use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use temper_spec::csdl::parse_csdl;

use crate::os_apps::AppManifest;

const METADATA_DIRECTORY_ENTRY_BUDGET_V1: usize = 1_024;
const METADATA_FILE_BUDGET_V1: usize = 256;
const MANIFEST_BYTES_BUDGET_V1: u64 = 1024 * 1024;
const CSDL_BYTES_BUDGET_V1: u64 = 8 * 1024 * 1024;
const IOA_BYTES_BUDGET_V1: u64 = 2 * 1024 * 1024;
const APP_METADATA_BYTES_BUDGET_V1: u64 = 32 * 1024 * 1024;

pub(super) struct CandidateApp {
    pub manifest: AppManifest,
    pub csdl_source: Option<String>,
    pub ioa_sources: Vec<String>,
}

pub(super) fn read_candidate_app(dir: &Path, manifest_path: &Path) -> Result<CandidateApp, String> {
    let mut remaining_bytes = APP_METADATA_BYTES_BUDGET_V1;
    let manifest_source = read_scoped_file(
        dir,
        manifest_path,
        "app manifest",
        MANIFEST_BYTES_BUDGET_V1,
        &mut remaining_bytes,
    )?;
    let manifest: AppManifest = toml::from_str(&manifest_source).map_err(|error| {
        format!(
            "invalid app manifest '{}': {error}",
            manifest_path.display()
        )
    })?;
    manifest.validate_candidate().map_err(|error| {
        format!(
            "invalid app manifest '{}': {error}",
            manifest_path.display()
        )
    })?;
    let csdl_path = find_csdl(dir)?;
    let csdl_source = csdl_path
        .as_ref()
        .map(|path| {
            read_scoped_file(
                dir,
                path,
                "CSDL",
                CSDL_BYTES_BUDGET_V1,
                &mut remaining_bytes,
            )
        })
        .transpose()?;
    if let Some(source) = &csdl_source {
        parse_csdl(source)
            .map_err(|error| format!("invalid CSDL for app '{}': {error}", manifest.name))?;
    }
    let mut ioa_paths = Vec::new();
    collect_ioa(dir, &mut ioa_paths)?;
    let specs = dir.join("specs");
    if specs.is_dir() {
        collect_ioa(&specs, &mut ioa_paths)?;
    }
    ioa_paths.sort();
    let mut ioa_names = BTreeSet::new();
    for path in &ioa_paths {
        let name = path
            .file_name()
            .ok_or_else(|| format!("IOA path '{}' has no file name", path.display()))?;
        if !ioa_names.insert(name.to_os_string()) {
            return Err(format!(
                "app '{}' contains multiple IOA files named '{}'",
                manifest.name,
                name.to_string_lossy()
            ));
        }
    }
    let mut ioa_sources = Vec::with_capacity(ioa_paths.len());
    for path in &ioa_paths {
        let source = read_scoped_file(dir, path, "IOA", IOA_BYTES_BUDGET_V1, &mut remaining_bytes)?;
        temper_spec::automaton::parse_automaton(&source)
            .map_err(|error| format!("invalid IOA '{}': {error}", path.display()))?;
        ioa_sources.push(source);
    }
    Ok(CandidateApp {
        manifest,
        csdl_source,
        ioa_sources,
    })
}

fn read_scoped_file(
    root: &Path,
    path: &Path,
    label: &str,
    byte_budget: u64,
    remaining_bytes: &mut u64,
) -> Result<String, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("failed to resolve {label} '{}': {error}", path.display()))?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return Err(format!(
            "{label} '{}' escapes app root '{}'",
            path.display(),
            root.display()
        ));
    }
    let bytes = canonical
        .metadata()
        .map_err(|error| format!("failed to inspect {label} '{}': {error}", path.display()))?
        .len();
    if bytes > byte_budget {
        return Err(format!(
            "{label} byte budget exceeded for '{}': {bytes} > {byte_budget}",
            path.display()
        ));
    }
    if bytes > *remaining_bytes {
        return Err(format!(
            "app metadata byte budget exceeded while reading '{}': {bytes} > {} remaining",
            path.display(),
            *remaining_bytes
        ));
    }
    *remaining_bytes -= bytes;
    std::fs::read_to_string(&canonical)
        .map_err(|error| format!("failed to read {label} '{}': {error}", path.display()))
}

fn collect_ioa(dir: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries = bounded_directory_entries(dir, "app metadata")?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".ioa.toml"))
        {
            if output.len() >= METADATA_FILE_BUDGET_V1 {
                return Err(format!(
                    "app metadata file budget exceeded: more than {METADATA_FILE_BUDGET_V1} IOA files"
                ));
            }
            output.push(entry.path());
        }
    }
    Ok(())
}

fn find_csdl(dir: &Path) -> Result<Option<PathBuf>, String> {
    let mut paths = Vec::new();
    for path in [dir.join("model.csdl.xml"), dir.join("specs/model.csdl.xml")] {
        if path.is_file() {
            paths.push(path);
        }
    }
    let csdl_dir = dir.join("csdl");
    if csdl_dir.is_dir() {
        paths.extend(
            bounded_directory_entries(&csdl_dir, "CSDL")?
                .into_iter()
                .map(|entry| entry.path())
                .filter(|path| path.to_string_lossy().ends_with(".csdl.xml")),
        );
    }
    paths.sort();
    if paths.len() > 1 {
        return Err(format!(
            "app '{}' contains multiple CSDL candidates",
            dir.display()
        ));
    }
    Ok(paths.pop())
}

fn bounded_directory_entries(dir: &Path, label: &str) -> Result<Vec<std::fs::DirEntry>, String> {
    let directory = std::fs::read_dir(dir).map_err(|error| {
        format!(
            "failed to read {label} directory '{}': {error}",
            dir.display()
        )
    })?;
    let mut entries = Vec::new();
    for entry in directory {
        if entries.len() >= METADATA_DIRECTORY_ENTRY_BUDGET_V1 {
            return Err(format!(
                "metadata directory entry budget exceeded in '{}'",
                dir.display()
            ));
        }
        entries.push(entry.map_err(|error| {
            format!(
                "failed to read {label} directory '{}': {error}",
                dir.display()
            )
        })?);
    }
    Ok(entries)
}
