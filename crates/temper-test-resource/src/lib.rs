//! One rule for tests that need an outside resource, such as a database URL,
//! a binary or a built module: the test either runs, or fails in CI when the
//! resource is missing. Locally it skips and says so on stderr. In CI it never
//! passes without running.
//!
//! "In CI" means the `CI` variable is set, as GitHub Actions sets it. A CI job
//! that runs a gated test provides its resource; a job that does not fails
//! here, naming what was missing.
//!
//! Test-only: a dev-dependency, never part of a production build.

/// Reads `name`, a variable that points a test at an outside resource.
///
/// Returns its value when set. When unset, panics in CI; otherwise prints a
/// skip line and returns `None`, and the caller returns early:
///
/// ```no_run
/// let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
///     return;
/// };
/// # let _ = url;
/// ```
pub fn env_var(name: &str) -> Option<String> {
    assert!(!name.is_empty(), "a resource variable has a name");
    match std::env::var(name) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => {
            missing(&format!("{name} is not set"));
            None
        }
        Err(std::env::VarError::NotUnicode(_)) => panic!("{name} is not valid Unicode"),
    }
}

/// Reports a missing resource: panics in CI, prints a skip line otherwise.
///
/// `what` says what is missing, for example "DATABASE_URL is not set" or the
/// path of a module that was not built. A caller that gets control back
/// returns early.
pub fn missing(what: &str) {
    assert!(!what.is_empty(), "a missing resource is named");
    assert!(
        std::env::var_os("CI").is_none(),
        "a test resource is missing in CI: {what}"
    );
    eprintln!("skipped: {what}");
}
