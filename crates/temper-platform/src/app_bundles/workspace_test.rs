use super::*;

fn write_minimal_app(path: &Path, name: &str, dependencies: &[&str]) {
    std::fs::create_dir_all(path.join("specs")).unwrap();
    let deps = dependencies
        .iter()
        .map(|dependency| format!("\"{dependency}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        path.join("app.toml"),
        format!("name = \"{name}\"\nversion = \"1.0.0\"\ndependencies = [{deps}]\n"),
    )
    .unwrap();
    std::fs::write(path.join("APP.md"), format!("# {name}\n")).unwrap();
}

#[test]
fn workspace_digest_is_stable_and_excludes_lock_paths() {
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("app");
    let dependency = root.path().join("dependency");
    write_minimal_app(&app, "root", &["dependency"]);
    write_minimal_app(&dependency, "dependency", &[]);
    std::fs::write(
        app.join(LOCK_FILE),
        "version = 1\n\n[[local]]\nname = \"dependency\"\npath = \"../dependency\"\n",
    )
    .unwrap();

    let first = build_workspace_bundle(&app, "default", false).unwrap();
    let second = build_workspace_bundle(&app, "default", false).unwrap();
    assert_eq!(
        first.request.manifest.bundle_digest,
        second.request.manifest.bundle_digest
    );
    assert_eq!(first.request.manifest.apps.len(), 2);
    assert!(first.request.manifest.apps.iter().all(|app| {
        app.files
            .iter()
            .all(|file| file.path != LOCK_FILE && !file.path.starts_with("target/"))
    }));
}

#[cfg(unix)]
#[test]
fn workspace_rejects_symlinks() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    write_minimal_app(root.path(), "root", &[]);
    symlink(root.path().join("APP.md"), root.path().join("linked.md")).unwrap();
    let error = build_workspace_bundle(root.path(), "default", false).unwrap_err();
    assert!(error.contains("symlink"), "unexpected error: {error}");
}

#[test]
fn locked_build_requires_resolved_lock() {
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("app");
    let dependency = root.path().join("dependency");
    write_minimal_app(&app, "root", &["dependency"]);
    write_minimal_app(&dependency, "dependency", &[]);
    let missing = build_workspace_bundle(&app, "default", true).unwrap_err();
    assert!(missing.contains("--locked requires"));

    std::fs::write(
        app.join(LOCK_FILE),
        "version = 1\n\n[[local]]\nname = \"dependency\"\npath = \"../dependency\"\n",
    )
    .unwrap();
    let unresolved = build_workspace_bundle(&app, "default", true).unwrap_err();
    assert!(unresolved.contains("has no resolved digest"));
}

#[test]
fn workspace_rejects_dependency_cycles() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    write_minimal_app(&first, "first", &["second"]);
    write_minimal_app(&second, "second", &["first"]);
    std::fs::write(
        first.join(LOCK_FILE),
        "version = 1\n\n[[local]]\nname = \"first\"\npath = \".\"\n\n[[local]]\nname = \"second\"\npath = \"../second\"\n",
    )
    .unwrap();
    let error = build_workspace_bundle(&first, "default", false).unwrap_err();
    assert!(error.contains("cyclic"), "unexpected error: {error}");
}
