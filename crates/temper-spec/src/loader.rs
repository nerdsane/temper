//! Filesystem discovery of spec/app bundle pieces.
//!
//! Single shared implementation of the directory-scanning logic that was
//! previously duplicated (with drift) between the CLI serve loader and the
//! platform OS-app loader. Discovers:
//!
//! - IOA specs: `<dir>/*.ioa.toml` and `<dir>/specs/*.ioa.toml`
//! - CSDL model: `<dir>/model.csdl.xml`, then `<dir>/specs/model.csdl.xml`,
//!   then the lexically first `<dir>/csdl/*.csdl.xml`
//! - Cedar policies: `<dir>/policies/*.cedar`
//! - Cross invariants: `<dir>/cross-invariants.toml`, then
//!   `<dir>/specs/cross-invariants.toml`
//!
//! All scans are lexically sorted for deterministic results. Discovery is
//! load-time filesystem I/O performed by non-simulation-visible callers
//! (CLI startup, platform app install).

use std::path::{Path, PathBuf};

use crate::automaton::parse_automaton;
use crate::naming::to_pascal_case;

/// The CSDL document discovered in a spec directory.
#[derive(Debug, Clone)]
pub struct CsdlSource {
    /// Path the document was discovered at (for caller error messages).
    pub path: PathBuf,
    /// Raw CSDL XML content.
    pub content: String,
}

/// One Cedar policy file discovered in `<dir>/policies/`.
#[derive(Debug, Clone)]
pub struct CedarPolicySource {
    /// File stem (e.g. `order` for `order.cedar`). Callers that map policy
    /// files to entity types can PascalCase this stem.
    pub stem: String,
    /// Raw Cedar policy text.
    pub content: String,
}

/// Everything discovered in a spec/app directory.
#[derive(Debug, Clone)]
pub struct SpecDirBundle {
    /// `(entity_type, ioa_source)` pairs. The entity type is the parsed
    /// automaton's `name` field; if parsing fails the PascalCase of the file
    /// name is used as a fallback label so caller error messages stay useful.
    pub specs: Vec<(String, String)>,
    /// The CSDL model, if any was discovered.
    pub csdl: Option<CsdlSource>,
    /// Cedar policy files from `<dir>/policies/*.cedar`, lexically sorted
    /// by file name.
    pub cedar_policies: Vec<CedarPolicySource>,
    /// Raw `cross-invariants.toml` content, if present.
    pub cross_invariants_toml: Option<String>,
}

/// Load all spec bundle pieces from a directory.
///
/// Errors if `dir` is not a directory or any discovered file fails to read.
/// An existing directory with no spec files yields an empty bundle (apps may
/// ship only agents/skills/wasm with no entity specs).
pub fn load_spec_dir(dir: &Path) -> Result<SpecDirBundle, String> {
    if !dir.is_dir() {
        return Err(format!("Specs directory not found: {}", dir.display()));
    }
    Ok(SpecDirBundle {
        specs: load_ioa_specs(dir)?,
        csdl: load_csdl(dir)?,
        cedar_policies: load_cedar_policies(dir)?,
        cross_invariants_toml: load_cross_invariants_toml(dir)?,
    })
}

/// Discover and read all IOA specs in `<dir>/*.ioa.toml` and
/// `<dir>/specs/*.ioa.toml`.
///
/// Files are deduplicated by file name with the root directory taking
/// precedence over `specs/`. Within each directory, files are visited in
/// lexical file-name order (root files first, then `specs/` files).
///
/// The entity type of each spec is the parsed automaton's `name` field.
/// If the file does not parse as an automaton, the PascalCase of the file
/// name (minus `.ioa.toml`) is used as a fallback label so the caller's
/// error messages still carry a recognizable entity name.
pub fn load_ioa_specs(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen_names = std::collections::BTreeSet::new();

    // Root first (takes precedence for dedup), then specs/.
    scan_dir_for_ioa(dir, &mut paths, &mut seen_names)?;
    let specs_subdir = dir.join("specs");
    if specs_subdir.is_dir() {
        scan_dir_for_ioa(&specs_subdir, &mut paths, &mut seen_names)?;
    }

    let mut specs = Vec::new();
    for path in paths {
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read IOA file {}: {e}", path.display()))?;
        let entity_type = match parse_automaton(&source) {
            Ok(automaton) => automaton.automaton.name,
            Err(_) => fallback_entity_type(&path),
        };
        specs.push((entity_type, source));
    }
    Ok(specs)
}

/// PascalCase label derived from an IOA file name, used when the spec fails
/// to parse (`order_item.ioa.toml` -> `OrderItem`).
fn fallback_entity_type(path: &Path) -> String {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let stem = file_name.strip_suffix(".ioa.toml").unwrap_or(file_name);
    to_pascal_case(stem)
}

/// Scan a single directory for `*.ioa.toml` files, appending paths not
/// already seen (dedup key: file name) in lexical file-name order.
fn scan_dir_for_ioa(
    dir: &Path,
    paths: &mut Vec<PathBuf>,
    seen_names: &mut std::collections::BTreeSet<String>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("Failed to read specs directory {}: {e}", dir.display()))?;
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".ioa.toml"))
        .collect();
    files.sort_by_key(|e| e.file_name());

    for entry in files {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if seen_names.insert(file_name) {
            paths.push(entry.path());
        }
    }
    Ok(())
}

/// Find the CSDL model file in a spec directory.
///
/// Fallback order: `<dir>/model.csdl.xml`, then `<dir>/specs/model.csdl.xml`,
/// then the lexically first `*.csdl.xml` inside `<dir>/csdl/`.
pub fn find_csdl(dir: &Path) -> Option<PathBuf> {
    let root = dir.join("model.csdl.xml");
    if root.exists() {
        return Some(root);
    }
    let specs = dir.join("specs").join("model.csdl.xml");
    if specs.exists() {
        return Some(specs);
    }
    let csdl_dir = dir.join("csdl");
    if csdl_dir.is_dir() {
        let entries = std::fs::read_dir(&csdl_dir).ok()?;
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".csdl.xml"))
            .map(|e| e.path())
            .collect();
        files.sort();
        return files.into_iter().next();
    }
    None
}

/// Discover and read the CSDL model file (see [`find_csdl`] for the
/// fallback order). Returns `Ok(None)` when no CSDL file exists.
pub fn load_csdl(dir: &Path) -> Result<Option<CsdlSource>, String> {
    let Some(path) = find_csdl(dir) else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read CSDL file {}: {e}", path.display()))?;
    Ok(Some(CsdlSource { path, content }))
}

/// Discover and read all Cedar policy files in `<dir>/policies/*.cedar`,
/// lexically sorted by file name. The `.cedar` extension is matched
/// case-insensitively. Returns an empty list when the directory is absent.
pub fn load_cedar_policies(dir: &Path) -> Result<Vec<CedarPolicySource>, String> {
    let policies_dir = dir.join("policies");
    if !policies_dir.is_dir() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&policies_dir)
        .map_err(|e| format!("Failed to read {}: {e}", policies_dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("cedar"))
        })
        .collect();
    files.sort();

    let mut policies = Vec::new();
    for path in files {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read Cedar policy {}: {e}", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        policies.push(CedarPolicySource { stem, content });
    }
    Ok(policies)
}

/// Read the raw `cross-invariants.toml` source if present, checking
/// `<dir>/cross-invariants.toml` first, then `<dir>/specs/cross-invariants.toml`.
pub fn load_cross_invariants_toml(dir: &Path) -> Result<Option<String>, String> {
    for path in [
        dir.join("cross-invariants.toml"),
        dir.join("specs").join("cross-invariants.toml"),
    ] {
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
            return Ok(Some(content));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a fresh unique temp directory for a test.
    fn fresh_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("temper-spec-loader-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn ioa(name: &str) -> String {
        format!(
            r#"
[automaton]
name = "{name}"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
"#
        )
    }

    #[test]
    fn ioa_root_wins_dedup_and_lexical_order() {
        let dir = fresh_dir("dedup");
        let specs = dir.join("specs");
        std::fs::create_dir_all(&specs).unwrap();
        // Same file name in both locations: root wins.
        std::fs::write(dir.join("order.ioa.toml"), ioa("RootOrder")).unwrap();
        std::fs::write(specs.join("order.ioa.toml"), ioa("SpecsOrder")).unwrap();
        // Unique files in each location, named to exercise sorting.
        std::fs::write(dir.join("zeta.ioa.toml"), ioa("Zeta")).unwrap();
        std::fs::write(dir.join("alpha.ioa.toml"), ioa("Alpha")).unwrap();
        std::fs::write(specs.join("beta.ioa.toml"), ioa("Beta")).unwrap();

        let loaded = load_ioa_specs(&dir).expect("load");
        let names: Vec<&str> = loaded.iter().map(|(n, _)| n.as_str()).collect();
        // Root files in lexical order first, then non-duplicate specs/ files.
        assert_eq!(names, vec!["Alpha", "RootOrder", "Zeta", "Beta"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entity_type_comes_from_automaton_name_with_filename_fallback() {
        let dir = fresh_dir("naming");
        // Automaton name differs from file name: automaton name wins.
        std::fs::write(dir.join("order.ioa.toml"), ioa("PurchaseOrder")).unwrap();
        // Unparseable spec: PascalCase of file name is the fallback label.
        std::fs::write(dir.join("broken_thing.ioa.toml"), "not [valid toml").unwrap();

        let loaded = load_ioa_specs(&dir).expect("load");
        let names: Vec<&str> = loaded.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["BrokenThing", "PurchaseOrder"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn csdl_fallback_order_root_then_specs_then_csdl_dir() {
        let dir = fresh_dir("csdl");
        let specs = dir.join("specs");
        let csdl_dir = dir.join("csdl");
        std::fs::create_dir_all(&specs).unwrap();
        std::fs::create_dir_all(&csdl_dir).unwrap();
        std::fs::write(csdl_dir.join("b.csdl.xml"), "<csdl-dir-b/>").unwrap();
        std::fs::write(csdl_dir.join("a.csdl.xml"), "<csdl-dir-a/>").unwrap();

        // Only csdl/ present: lexically first file wins.
        let found = load_csdl(&dir).expect("load").expect("some");
        assert_eq!(found.content, "<csdl-dir-a/>");

        // specs/ beats csdl/.
        std::fs::write(specs.join("model.csdl.xml"), "<specs/>").unwrap();
        let found = load_csdl(&dir).expect("load").expect("some");
        assert_eq!(found.content, "<specs/>");
        assert_eq!(found.path, specs.join("model.csdl.xml"));

        // Root beats both.
        std::fs::write(dir.join("model.csdl.xml"), "<root/>").unwrap();
        let found = load_csdl(&dir).expect("load").expect("some");
        assert_eq!(found.content, "<root/>");
        assert_eq!(found.path, dir.join("model.csdl.xml"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cedar_policies_sorted_with_stems() {
        let dir = fresh_dir("cedar");
        let policies = dir.join("policies");
        std::fs::create_dir_all(&policies).unwrap();
        std::fs::write(policies.join("order.cedar"), "// order").unwrap();
        std::fs::write(policies.join("issue.cedar"), "// issue").unwrap();
        std::fs::write(policies.join("notes.txt"), "not a policy").unwrap();

        let loaded = load_cedar_policies(&dir).expect("load");
        let stems: Vec<&str> = loaded.iter().map(|p| p.stem.as_str()).collect();
        assert_eq!(stems, vec!["issue", "order"]);
        assert_eq!(loaded[0].content, "// issue");
        assert_eq!(loaded[1].content, "// order");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_invariants_root_beats_specs_subdir() {
        let dir = fresh_dir("xinv");
        let specs = dir.join("specs");
        std::fs::create_dir_all(&specs).unwrap();

        assert_eq!(load_cross_invariants_toml(&dir).expect("load"), None);

        std::fs::write(specs.join("cross-invariants.toml"), "version = 1").unwrap();
        assert_eq!(
            load_cross_invariants_toml(&dir).expect("load").as_deref(),
            Some("version = 1")
        );

        std::fs::write(dir.join("cross-invariants.toml"), "version = 2").unwrap();
        assert_eq!(
            load_cross_invariants_toml(&dir).expect("load").as_deref(),
            Some("version = 2")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_errors_and_empty_dir_yields_empty_bundle() {
        let missing =
            std::env::temp_dir().join(format!("temper-spec-loader-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let err = load_spec_dir(&missing).expect_err("missing dir must error");
        assert!(err.contains("Specs directory not found"), "got: {err}");

        let dir = fresh_dir("empty");
        let bundle = load_spec_dir(&dir).expect("empty dir loads");
        assert!(bundle.specs.is_empty());
        assert!(bundle.csdl.is_none());
        assert!(bundle.cedar_policies.is_empty());
        assert!(bundle.cross_invariants_toml.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
