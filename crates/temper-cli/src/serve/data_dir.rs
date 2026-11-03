//! The folder a server keeps what it writes to its own disk in.
//!
//! `TEMPER_DATA_DIR` gives a server a folder of its own: its files and its
//! local databases default into it, so it never opens the database every
//! other server on the machine shares by default. Without it every path is
//! the one it always was, since a server restarted onto other paths would
//! lose sight of its databases.

use std::path::PathBuf;

/// The variable that gives a server its own folder.
const DATA_DIR: &str = "TEMPER_DATA_DIR";

/// `TEMPER_DATA_DIR`, or `$HOME/.local/share/temper` (under `/tmp` without a
/// HOME). The server's files (blobs, caches, the spec registry) live here,
/// and so does its one local database unless `TURSO_URL` names another.
pub(super) fn data_dir() -> PathBuf {
    match own_data_dir(&env) {
        Some(dir) => dir,
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local/share/temper")
        }
    }
}

/// Where routed tenants' local databases live: `TURSO_LOCAL_BASE_DIR`, else
/// `tenants` in the server's own folder, else `$HOME/.local/share/temper/tenants`.
/// `None` without any of them, which leaves the router's own default.
/// `var` reads one environment variable.
pub(super) fn routed_local_base_dir(var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(dir) = var("TURSO_LOCAL_BASE_DIR") {
        return Some(dir);
    }
    if let Some(dir) = own_data_dir(var) {
        return Some(dir.join("tenants").display().to_string());
    }
    let home = var("HOME")?;
    Some(format!("{home}/.local/share/temper/tenants"))
}

/// This process's environment, as [`routed_local_base_dir`] reads it.
pub(super) fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn own_data_dir(var: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    var(DATA_DIR)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::routed_local_base_dir;

    fn with(vars: &[(&str, &str)]) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        routed_local_base_dir(&|name| {
            vars.iter()
                .find(|(var, _)| var == name)
                .map(|(_, value)| value.clone())
        })
    }

    /// Without its own folder a routed server keeps the paths main gave it:
    /// moving them would lose every tenant database at the next restart.
    #[test]
    fn without_a_data_dir_routed_tenants_stay_where_they_were() {
        assert_eq!(
            with(&[]),
            None,
            "no HOME leaves the router's .temper/tenants"
        );
        assert_eq!(
            with(&[("HOME", "/home/temper")]).as_deref(),
            Some("/home/temper/.local/share/temper/tenants")
        );
        assert_eq!(
            with(&[
                ("HOME", "/home/temper"),
                ("TURSO_LOCAL_BASE_DIR", "/v/tenants")
            ])
            .as_deref(),
            Some("/v/tenants")
        );
        assert_eq!(
            with(&[("TEMPER_DATA_DIR", "")]),
            None,
            "an empty data dir is none"
        );
    }

    #[test]
    fn a_data_dir_holds_routed_tenants_unless_a_base_dir_is_named() {
        assert_eq!(
            with(&[("HOME", "/home/temper"), ("TEMPER_DATA_DIR", "/srv/one")]).as_deref(),
            Some("/srv/one/tenants")
        );
        assert_eq!(
            with(&[("TEMPER_DATA_DIR", "/srv/one")]).as_deref(),
            Some("/srv/one/tenants")
        );
        assert_eq!(
            with(&[
                ("TEMPER_DATA_DIR", "/srv/one"),
                ("TURSO_LOCAL_BASE_DIR", "/v/tenants")
            ])
            .as_deref(),
            Some("/v/tenants")
        );
    }
}
