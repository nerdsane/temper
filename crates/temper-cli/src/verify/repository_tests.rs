//! Verify source collections without claiming they are packaged applications.
//!
//! Several examples supply their policies or modules through their Rust host or
//! build scripts. The CLI requires the assembled files; this test keeps checking
//! their CSDL and IOA behavior before packaging, using the production cascade.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn specification_directories(root: &Path, found: &mut BTreeSet<std::path::PathBuf>) {
    let mut has_ioa = false;
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name != "target" && !name.starts_with('.') {
                specification_directories(&path, found);
            }
        } else if path.to_string_lossy().ends_with(".ioa.toml") {
            has_ioa = true;
        }
    }
    if has_ioa && root.join("model.csdl.xml").is_file() {
        found.insert(root.to_owned());
    }
}

// The kernel supplies two CSDL documents from one source directory. Read every
// document here, so both system and agent entities receive binding checks.
fn verify_source_collection(directory: &Path) -> Result<()> {
    let xml = fs::read_to_string(directory.join("model.csdl.xml"))?;
    input::validate_xml_document(&xml)?;
    let mut csdl = parse_csdl(&xml)?;
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".csdl.xml") && name != "model.csdl.xml")
        {
            let xml = fs::read_to_string(&path)?;
            input::validate_xml_document(&xml)?;
            csdl.schemas.extend(parse_csdl(&xml)?.schemas);
        }
    }
    anyhow::ensure!(!csdl.schemas.is_empty(), "CSDL must contain a Schema");
    let sources = input::read_ioa_files(&AppFiles::find(app_dir_of(directory)).ioa)?;
    anyhow::ensure!(!sources.is_empty(), "source contains no IOA specifications");
    input::validate_ioa_entities(&csdl, &sources)?;
    let model = build_spec_model(csdl, read_tla_sources(directory)?);
    anyhow::ensure!(model.validation.is_valid(), "{:?}", model.validation.errors);
    package::validate_policy_files(&AppFiles::find(directory).policies)?;
    verify_ioa_sources(&sources)
}

// The fixture catalog contains independent examples and alternative Process
// definitions. Its CSDL belongs to Order, whose complete application is checked
// by test_verify_reference_specs; other catalog IOAs have no shared CSDL contract.
fn verify_fixture_catalog(directory: &Path) -> Result<()> {
    let xml = fs::read_to_string(directory.join("model.csdl.xml"))?;
    input::validate_xml_document(&xml)?;
    let csdl = parse_csdl(&xml)?;
    let order = fs::read_to_string(directory.join("order.ioa.toml"))?;
    let name = temper_spec::automaton::parse_automaton(&order)?
        .automaton
        .name;
    input::validate_ioa_entities(&csdl, &BTreeMap::from([(name, order)]))?;
    let model = build_spec_model(csdl, read_tla_sources(directory)?);
    anyhow::ensure!(model.validation.is_valid(), "{:?}", model.validation.errors);
    package::validate_policy_files(&AppFiles::find(directory).policies)?;
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.to_string_lossy().ends_with(".ioa.toml") {
            let source = fs::read_to_string(&path)?;
            let name = temper_spec::automaton::parse_automaton(&source)?
                .automaton
                .name;
            verify_ioa_sources(&BTreeMap::from([(name, source)]))
                .with_context(|| format!("{}", path.display()))?;
        }
    }
    Ok(())
}

#[test]
fn repository_source_collections_pass_behavior_verification() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let mut directories = BTreeSet::new();
    if let Some(selected) = std::env::var_os("TEMPER_VERIFY_SOURCE_DIR") {
        directories.insert(
            Path::new(&selected)
                .canonicalize()
                .expect("source directory must exist"),
        );
    } else {
        specification_directories(&root, &mut directories);
    }
    assert!(!directories.is_empty());
    let mut failures = Vec::new();
    for directory in directories {
        let result = if directory == root.join("test-fixtures/specs") {
            verify_fixture_catalog(&directory)
        } else {
            verify_source_collection(&directory)
        };
        if let Err(error) = result {
            failures.push(format!("{}: {error:#}", directory.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn source_collections_require_csdl_bindings_before_packaging() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");
    let source = tempfile::tempdir().unwrap();
    fs::copy(
        fixture.join("order.ioa.toml"),
        source.path().join("order.ioa.toml"),
    )
    .unwrap();
    let xml = fs::read_to_string(fixture.join("model.csdl.xml")).unwrap();
    fs::write(source.path().join("model.csdl.xml"), &xml).unwrap();
    verify_source_collection(source.path()).expect("source is valid before policies are supplied");
    for (broken, message) in [
        (
            xml.replace("Name=\"Order\"", "Name=\"Other\""),
            "missing from CSDL",
        ),
        (xml.replace("EntitySet", "Unrelated"), "no CSDL entity set"),
    ] {
        fs::write(source.path().join("model.csdl.xml"), broken).unwrap();
        assert!(
            verify_source_collection(source.path())
                .unwrap_err()
                .to_string()
                .contains(message),
            "missing IOA binding must fail with {message}"
        );
    }
}
