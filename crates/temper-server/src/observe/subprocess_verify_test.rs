use std::os::unix::fs::PermissionsExt as _;

use super::*;

const PARENT_ENV_SENTINEL: &str = "TEMPER_VERIFY_PARENT_SECRET";

struct ParentEnvSentinel;

impl ParentEnvSentinel {
    fn install() -> Self {
        // SAFETY: this test owns a unique Temper-specific environment key and
        // removes it before returning. The child must not inherit it because
        // `verify_in_subprocess` calls `env_clear()`.
        unsafe { std::env::set_var(PARENT_ENV_SENTINEL, "must-not-leak") };
        Self
    }
}

impl Drop for ParentEnvSentinel {
    fn drop(&mut self) {
        // SAFETY: see `install`; this is the matching cleanup for the same
        // unique test-only key.
        unsafe { std::env::remove_var(PARENT_ENV_SENTINEL) };
    }
}

#[tokio::test]
async fn verifier_subprocess_does_not_inherit_parent_environment() {
    let _sentinel = ParentEnvSentinel::install();
    assert!(
        std::env::var_os(PARENT_ENV_SENTINEL).is_some(),
        "test runner should provide the parent-only environment sentinel"
    );
    let directory = tempfile::tempdir().expect("create verifier test directory");
    let script = directory.path().join("verify-env.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ -n \"${{{PARENT_ENV_SENTINEL}+x}}\" ]; then echo inherited-parent-secret >&2; exit 42; fi\ncat >/dev/null\necho '{{}}'\n"
        ),
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
    assert!(
        error.contains("failed to parse verification subprocess output"),
        "unexpected subprocess verifier error: {error}"
    );
    assert!(!error.contains("inherited-parent-secret"));
}
