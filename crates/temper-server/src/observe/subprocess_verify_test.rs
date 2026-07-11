use std::os::unix::fs::PermissionsExt as _;

use super::*;

#[tokio::test]
async fn verifier_subprocess_does_not_inherit_parent_environment() {
    assert!(
        std::env::var_os("HOME").is_some(),
        "test runner should provide an inherited environment sentinel"
    );
    let directory = tempfile::tempdir().expect("create verifier test directory");
    let script = directory.path().join("verify-env.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nif [ -n \"${HOME+x}\" ]; then echo inherited-home >&2; exit 42; fi\necho '{}'\n",
    )
    .expect("write verifier test script");
    let mut permissions = std::fs::metadata(&script)
        .expect("read verifier script metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).expect("make verifier script executable");

    let error = verify_in_subprocess(&script, "[automaton]")
        .await
        .expect_err("empty JSON should not decode as a cascade result");
    assert!(error.contains("failed to parse verification subprocess output"));
    assert!(!error.contains("inherited-home"));
}
