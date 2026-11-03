//! Safe cache paths and staged directory publication for Genesis apps.

use std::path::{Path, PathBuf};

use sha2::Digest as _;
use temper_server::state::ServerState;

const MAX_IDENTITY_COMPONENT_BYTES: usize = 128;

pub(super) fn validate_identity_component(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_IDENTITY_COMPONENT_BYTES
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "Genesis {label} must be a single ASCII identifier component (letters, digits, '.', '-', '_')"
        ));
    }
    Ok(())
}

pub(super) fn validate_git_object_id(value: &str) -> Result<&str, String> {
    let value = value.trim_start_matches('@');
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("Genesis version hash must be a 40-character lowercase Git SHA-1".to_string());
    }
    Ok(value)
}

pub(super) fn app_cache_dir(cache_root: &Path, app_name: &str) -> Result<PathBuf, String> {
    validate_identity_component("app name", app_name)?;
    Ok(cache_root.join(app_name))
}

pub(super) fn replace_directory(staged: PathBuf, destination: &Path) -> Result<(), String> {
    let Some(parent) = destination.parent() else {
        return Err(format!(
            "Genesis cache destination '{}' has no parent",
            destination.display()
        ));
    };
    if std::fs::symlink_metadata(destination).is_err() {
        return std::fs::rename(&staged, destination).map_err(|error| {
            let cleanup = cleanup_staged_directory(&staged);
            format!(
                "publish staged Genesis cache '{}' to '{}': {error}",
                staged.display(),
                destination.display()
            ) + &cleanup
        });
    }

    let backup = tempfile::Builder::new()
        .prefix(".genesis-backup-")
        .tempdir_in(parent)
        .map_err(|error| format!("create Genesis cache rollback directory: {error}"))?;
    let previous = backup.path().join("previous");
    std::fs::rename(destination, &previous).map_err(|error| {
        format!(
            "stage previous Genesis cache '{}' for replacement: {error}",
            destination.display()
        )
    })?;
    if let Err(publish_error) = std::fs::rename(&staged, destination) {
        let cleanup = cleanup_staged_directory(&staged);
        return match std::fs::rename(&previous, destination) {
            Ok(()) => Err(format!(
                "publish staged Genesis cache '{}' to '{}': {publish_error}; previous cache restored",
                staged.display(),
                destination.display()
            ) + &cleanup),
            Err(rollback_error) => {
                let recovery_path = backup.keep();
                Err(format!(
                    "publish staged Genesis cache '{}' to '{}': {publish_error}; rollback failed: {rollback_error}; previous cache retained at '{}'{}",
                    staged.display(),
                    destination.display(),
                    recovery_path.display(),
                    cleanup,
                ))
            }
        };
    }
    Ok(())
}

fn cleanup_staged_directory(staged: &Path) -> String {
    match std::fs::remove_dir_all(staged) {
        Ok(()) => String::new(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => format!("; failed to clean staged directory: {error}"),
    }
}

/// Whose files a Genesis cache directory holds.
#[derive(Debug, Clone, Copy)]
pub(super) enum CacheOwner<'a> {
    /// An install of an app closure into `tenant`.
    Install {
        tenant: &'a str,
        source: InstallSource<'a>,
    },
    /// A bundle this registry exports from `registry_tenant`'s Genesis rows.
    Export { registry_tenant: &'a str },
    /// A dependency read while resolving a closure in `tenant`.
    DependencyRead { tenant: &'a str },
}

/// Where an install's files come from.
#[derive(Debug, Clone, Copy)]
pub(super) enum InstallSource<'a> {
    /// The configured registry, as this registry tenant serves them.
    Registry(&'a str),
    /// This process's own Genesis rows in this tenant (`App.Install`).
    Local(&'a str),
    /// Files the caller posted to this server (`POST /api/os-apps/{name}/install`).
    Pushed,
}

impl CacheOwner<'_> {
    fn key(&self) -> String {
        match self {
            Self::Install {
                tenant,
                source: InstallSource::Registry(registry_tenant),
            } => format!("install\n{tenant}\nregistry\n{registry_tenant}"),
            Self::Install {
                tenant,
                source: InstallSource::Local(source_tenant),
            } => format!("install\n{tenant}\nlocal\n{source_tenant}"),
            Self::Install {
                tenant,
                source: InstallSource::Pushed,
            } => format!("install\n{tenant}\npushed"),
            Self::Export { registry_tenant } => format!("export\n{registry_tenant}"),
            Self::DependencyRead { tenant } => format!("dependency-read\n{tenant}"),
        }
    }
}

/// The cache directory for one owner's copy of `app_ref`.
///
/// Every owner gets its own directory: an install replaces its directory as a
/// whole, so a directory shared by two tenants' installs of one ref would
/// hand each tenant whatever the other installed last.
pub(super) fn genesis_cache_root(
    state: &ServerState,
    owner: CacheOwner<'_>,
    app_ref: &str,
) -> PathBuf {
    let root = genesis_cache_base(state);
    let key = format!("{}\n{app_ref}", owner.key());
    let digest = format!("{:x}", sha2::Sha256::digest(key.as_bytes()));
    root.join(format!("{}-{}", sanitize_fragment(app_ref), &digest[..32]))
}

/// The pre-tenant shared layout, used only for digest-verified recovery.
/// Delete with the pre-tenant cache migration after existing installs move.
pub(super) fn legacy_genesis_cache_root(state: &ServerState, app_ref: &str) -> PathBuf {
    let digest = format!("{:x}", sha2::Sha256::digest(app_ref.as_bytes()));
    genesis_cache_base(state).join(format!("{}-{}", sanitize_fragment(app_ref), &digest[..16]))
}

fn genesis_cache_base(state: &ServerState) -> PathBuf {
    if state.data_dir.as_os_str().is_empty() {
        std::env::temp_dir().join("temper-genesis-app-cache")
    } else {
        state.data_dir.join("genesis-app-cache")
    }
}

/// Lowercase `input` and turn each run of other characters into one dash.
pub(super) fn sanitize_fragment(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_cache_dir_rejects_path_components() {
        let root = Path::new("/safe/cache");
        for value in ["", ".", "..", "../target", "/tmp/target", "a/b", "a\\b"] {
            assert!(app_cache_dir(root, value).is_err(), "accepted {value:?}");
        }
        assert_eq!(
            app_cache_dir(root, "safe-app_1.0").expect("safe app name"),
            root.join("safe-app_1.0")
        );
    }

    #[test]
    fn git_object_ids_are_exact_lowercase_sha1() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(validate_git_object_id(hash), Ok(hash));
        assert_eq!(validate_git_object_id(&format!("@{hash}")), Ok(hash));
        for invalid in [
            "abc123",
            "../target",
            "-deadbeef",
            "ABCDEF0123456789ABCDEF0123456789ABCDEF01",
        ] {
            assert!(
                validate_git_object_id(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn failed_publish_restores_previous_directory() {
        let root = tempfile::tempdir().expect("cache root");
        let destination = root.path().join("app");
        std::fs::create_dir(&destination).expect("destination");
        std::fs::write(destination.join("marker"), b"previous").expect("marker");
        let missing_staged = root.path().join("missing-stage");

        let error = replace_directory(missing_staged, &destination)
            .expect_err("missing staged directory must fail");

        assert!(error.contains("previous cache restored"));
        assert_eq!(
            std::fs::read(destination.join("marker")).expect("restored marker"),
            b"previous"
        );
    }
}
