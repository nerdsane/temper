//! Shared selection must survive the install source's empty folder listings.
use super::*;
use crate::os_apps::{file_source::seam, load_app_bundle};

#[test]
fn written_selection_and_bundle_match_verify_when_folders_list_empty() {
    let root = tempfile::tempdir().expect("app root");
    let dir = root.path();
    let spec = r#"[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]
"#;
    let named = [
        (
            "app.toml",
            "name = \"selection-test\"\nmode = \"commons\"\n[[wasm_modules]]\nname = \"echo\"\n",
        ),
        ("APP.md", "# Selection test\n"),
        ("counter.ioa.toml", spec),
        ("specs/counter.ioa.toml", "invalid shadowed spec"),
        ("specs/model.csdl.xml", "<model/>"),
        ("csdl/ignored.csdl.xml", "<shadowed/>"),
        (
            "policies/guard.CEDAR",
            "permit(principal, action, resource);",
        ),
        (
            "policies/commons/shared.cedar",
            "permit(principal, action, resource);",
        ),
        ("wasm/echo/echo.wasm", "packaged-binary"),
        ("elsewhere/stray.ioa.toml", "not read"),
    ];
    for (path, content) in named {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create folder");
        std::fs::write(path, content).expect("write app file");
    }
    let verified = AppFiles::find(dir);
    let warnings = verified.unread_warnings(dir);
    assert_eq!(verified.ioa, [dir.join("counter.ioa.toml")]);
    assert_eq!(verified.policies.len(), 2);
    assert_eq!(verified.modules["echo"], dir.join("wasm/echo/echo.wasm"));
    // A file outside the recorded list must never become a served spec.
    std::fs::write(dir.join("unrecorded.ioa.toml"), "invalid unrecorded spec").expect("extra file");
    let source = AppFileSource::written(dir.to_path_buf(), named.map(|(path, _)| path.to_string()));
    seam::list_empty_under(dir);
    let installed = AppFiles::from_source(&source);
    assert_eq!(installed, verified);
    assert_eq!(installed.unread_warnings_from_source(&source), warnings);
    let bundle = load_app_bundle(&source).expect("load the exact recorded files");
    assert_eq!(bundle.specs.len(), 1);
    assert_eq!(bundle.specs[0].0, "Counter");
    assert_eq!(bundle.csdl.as_deref(), Some("<model/>"));
    assert_eq!(bundle.cedar_policy_sources.len(), 2);
    assert_eq!(bundle.wasm_modules["echo"], b"packaged-binary");
}

#[test]
fn an_unreadable_recorded_csdl_is_an_error_even_when_folders_list_empty() {
    let root = tempfile::tempdir().expect("app root");
    let dir = root.path();
    std::fs::write(dir.join("app.toml"), "name = \"unreadable-model\"\n").expect("manifest");
    std::fs::write(dir.join("model.csdl.xml"), "<model/>").expect("model");
    let source = AppFileSource::written(
        dir.to_path_buf(),
        ["app.toml".into(), "model.csdl.xml".into()],
    );
    seam::list_empty_under(dir);
    seam::unreadable_under(dir, "model.csdl.xml");
    let error = load_app_bundle(&source)
        .err()
        .expect("recorded model is required");
    assert!(
        error.contains("model.csdl.xml could not be read"),
        "{error}"
    );
}
