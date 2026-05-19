use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    install_os_app, list_startup_os_apps, os_app_bundle_digest, resolve_os_app_install_order,
};
use crate::state::PlatformState;

pub const OS_APP_CLOSURE_RESOLVER_VERSION: &str = "1.0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OsAppClosure {
    pub id: String,
    pub root: String,
    pub resolver_version: String,
    pub resolved: BTreeMap<String, String>,
    pub app_order: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClosureBootstrapResult {
    pub closure: OsAppClosure,
    pub installed_apps: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BootstrapManifest {
    closure: String,
}

pub fn parse_bootstrap_manifest_str(input: &str) -> Result<String, String> {
    let manifest: BootstrapManifest =
        toml::from_str(input).map_err(|e| format!("invalid bootstrap manifest TOML: {e}"))?;
    let closure = manifest.closure.trim().to_string();
    validate_closure_id(&closure)?;
    Ok(closure)
}

pub fn startup_os_app_closure() -> Result<OsAppClosure, String> {
    let roots = list_startup_os_apps();
    os_app_closure_for_roots(&roots)
}

pub fn os_app_closure_for_roots(app_names: &[String]) -> Result<OsAppClosure, String> {
    let app_order = resolve_os_app_install_order(app_names)?;
    os_app_closure_for_ordered_apps(app_names, app_order, |app_name| {
        os_app_bundle_digest(app_name)
            .map(|digest| digest.bundle_digest)
            .ok_or_else(|| format!("OS app '{app_name}' not found while resolving closure"))
    })
}

pub async fn bootstrap_closure_manifest(
    state: &PlatformState,
    tenant: &str,
    manifest_path: &Path,
) -> Result<ClosureBootstrapResult, String> {
    let manifest_source = std::fs::read_to_string(manifest_path).map_err(|e| {
        format!(
            "failed to read bootstrap manifest '{}': {e}",
            manifest_path.display()
        )
    })?;
    let pinned_closure = parse_bootstrap_manifest_str(&manifest_source)?;
    let closure = startup_os_app_closure()?;
    if pinned_closure != closure.id {
        return Err(format!(
            "bootstrap closure mismatch: manifest pins {pinned_closure}, current startup catalog resolves to {}",
            closure.id
        ));
    }

    let mut installed_apps = Vec::new();
    for app_name in &closure.app_order {
        install_os_app(state, tenant, app_name).await?;
        installed_apps.push(app_name.clone());
    }

    Ok(ClosureBootstrapResult {
        closure,
        installed_apps,
    })
}

fn validate_closure_id(closure: &str) -> Result<(), String> {
    let digest = closure
        .strip_prefix("cl-")
        .ok_or_else(|| "bootstrap closure must start with 'cl-'".to_string())?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("bootstrap closure must be 'cl-' plus a 64-character hex digest".to_string());
    }
    Ok(())
}

fn os_app_closure_for_ordered_apps(
    root_names: &[String],
    app_order: Vec<String>,
    digest_for: impl Fn(&str) -> Result<String, String>,
) -> Result<OsAppClosure, String> {
    if root_names.is_empty() {
        return Err("bootstrap closure requires at least one root OS app".to_string());
    }

    let mut resolved = BTreeMap::new();
    for app_name in &app_order {
        resolved.insert(app_name.clone(), digest_for(app_name)?);
    }

    let mut roots = root_names.to_vec();
    roots.sort();
    roots.dedup();
    let root = if roots.len() == 1 {
        let root_name = &roots[0];
        let digest = resolved
            .get(root_name)
            .ok_or_else(|| format!("root OS app '{root_name}' was not in resolved closure"))?;
        format!("{root_name}@{digest}")
    } else {
        let mut parts = Vec::with_capacity(roots.len());
        for root_name in &roots {
            let digest = resolved
                .get(root_name)
                .ok_or_else(|| format!("root OS app '{root_name}' was not in resolved closure"))?;
            parts.push(format!("{root_name}@{digest}"));
        }
        format!("os-apps:{}", parts.join(","))
    };

    let id = closure_id(&root, &resolved, OS_APP_CLOSURE_RESOLVER_VERSION);
    Ok(OsAppClosure {
        id,
        root,
        resolver_version: OS_APP_CLOSURE_RESOLVER_VERSION.to_string(),
        resolved,
        app_order,
    })
}

fn closure_id(root: &str, resolved: &BTreeMap<String, String>, resolver_version: &str) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"genesis-closure:v1\0");
    push_field(&mut bytes, "root", root);
    push_resolved(&mut bytes, resolved);
    push_field(&mut bytes, "resolver_version", resolver_version);

    let digest = Sha256::digest(&bytes);
    format!("cl-{}", hex_lower(&digest))
}

fn push_field(bytes: &mut Vec<u8>, name: &str, value: &str) {
    bytes.extend_from_slice(name.as_bytes());
    bytes.push(0);
    push_len_prefixed(bytes, value);
}

fn push_resolved(bytes: &mut Vec<u8>, resolved: &BTreeMap<String, String>) {
    bytes.extend_from_slice(b"resolved\0");
    bytes.extend_from_slice(resolved.len().to_string().as_bytes());
    bytes.push(0);
    for (name, hash) in resolved {
        push_len_prefixed(bytes, name);
        push_len_prefixed(bytes, hash);
    }
}

fn push_len_prefixed(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(value.len().to_string().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_for(app_name: &str) -> Result<String, String> {
        match app_name {
            "temper-git" => Ok("sha256:aaaaaaaaaaaaaaaa".to_string()),
            "paw-heal" => Ok("sha256:bbbbbbbbbbbbbbbb".to_string()),
            "paw-ui" => Ok("sha256:cccccccccccccccc".to_string()),
            other => Err(format!("unexpected app {other}")),
        }
    }

    #[test]
    fn parses_one_line_bootstrap_manifest() {
        let closure = "cl-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_bootstrap_manifest_str(&format!("closure = \"{closure}\"\n")).unwrap(),
            closure
        );
    }

    #[test]
    fn rejects_malformed_bootstrap_manifest_closure_ids() {
        let error = parse_bootstrap_manifest_str("closure = \"sha256:abc\"\n")
            .expect_err("non Closure IDs should be rejected");
        assert!(error.contains("cl-"));

        let error =
            parse_bootstrap_manifest_str("closure = \"cl-not-hex\"\n").expect_err("hex required");
        assert!(error.contains("64-character hex"));
    }

    #[test]
    fn os_app_closure_is_stable_for_root_ordering() {
        let first = os_app_closure_for_ordered_apps(
            &["paw-ui".to_string(), "paw-heal".to_string()],
            vec![
                "temper-git".to_string(),
                "paw-heal".to_string(),
                "paw-ui".to_string(),
            ],
            digest_for,
        )
        .expect("closure should resolve");
        let second = os_app_closure_for_ordered_apps(
            &["paw-heal".to_string(), "paw-ui".to_string()],
            vec![
                "temper-git".to_string(),
                "paw-heal".to_string(),
                "paw-ui".to_string(),
            ],
            digest_for,
        )
        .expect("closure should resolve");

        assert_eq!(first.id, second.id);
        assert_eq!(first.resolver_version, OS_APP_CLOSURE_RESOLVER_VERSION);
        assert_eq!(first.resolved["temper-git"], "sha256:aaaaaaaaaaaaaaaa");
        assert_eq!(
            first.root,
            "os-apps:paw-heal@sha256:bbbbbbbbbbbbbbbb,paw-ui@sha256:cccccccccccccccc"
        );
    }

    #[test]
    fn os_app_closure_changes_when_a_digest_changes() {
        let base = os_app_closure_for_ordered_apps(
            &["paw-heal".to_string()],
            vec!["temper-git".to_string(), "paw-heal".to_string()],
            digest_for,
        )
        .expect("base closure should resolve");
        let changed = os_app_closure_for_ordered_apps(
            &["paw-heal".to_string()],
            vec!["temper-git".to_string(), "paw-heal".to_string()],
            |app_name| {
                if app_name == "paw-heal" {
                    Ok("sha256:dddddddddddddddd".to_string())
                } else {
                    digest_for(app_name)
                }
            },
        )
        .expect("changed closure should resolve");

        assert_ne!(base.id, changed.id);
    }
}
