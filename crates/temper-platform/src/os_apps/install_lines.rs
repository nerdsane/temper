//! The lines this server prints about an app's install. Katagami's control
//! plane reads them to know whether an app is served, so their words are
//! fixed; tests hold them.

use super::InstallResult;

/// Printed when `app` is installed for `tenant`, at boot or through an install
/// route: `  App '<app>' installed for '<tenant>': <types>`. The control plane
/// finds `App '<app>' installed for '<tenant>'` in it.
pub fn installed_line(app: &str, tenant: &str, result: &InstallResult) -> String {
    let types: Vec<&str> = result
        .added
        .iter()
        .chain(&result.updated)
        .chain(&result.skipped)
        .map(String::as_str)
        .collect();
    format!(
        "  App '{app}' installed for '{tenant}': {}",
        types.join(", ")
    )
}

/// Printed at boot for an `--app` this server serves nothing for: no files
/// for it anywhere and no install of it stored.
pub fn waiting_line(app: &str, tenant: &str) -> String {
    format!("  App '{app}' waits for its first install for '{tenant}'")
}

/// Printed at boot for an `--app` with no files anywhere whose install this
/// server stored: the stored specs serve, and nothing is installed until its
/// next install. Never the waiting line, which says nothing is served.
pub fn stored_line(app: &str, tenant: &str) -> String {
    format!("  App '{app}' serves its stored specs for '{tenant}'; no files to install")
}
