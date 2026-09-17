//! Private-storage contract tests. Tokens below are test fixtures only.

use super::*;
use serde_json::json;
use std::os::unix::fs::{PermissionsExt, symlink};

struct PrivateDirectory(std::path::PathBuf);

impl PrivateDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("temper-setup-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> RequesterIdentity {
    RequesterIdentity {
        server: "https://genesis.example".into(),
        tenant: "default".into(),
        principal: format!("mcp-{}", "a".repeat(32)),
        token: format!("tmpr_{}", "b".repeat(64)),
    }
}

fn consent_fixture() -> SetupConsent {
    SetupConsent::from_client_response(&json!({"result": {
        "action": "accept", "content": {"setup": "configure_agent_identity"}
    }}))
    .unwrap()
}

#[test]
fn private_identity_survives_reconnect_but_cannot_cross_service_or_tenant() {
    let directory = PrivateDirectory::new();
    let path = directory.0.join("identity.json");
    let identity = fixture();
    save_identity(&consent_fixture(), &path, &identity).unwrap();
    let loaded = load_identity(&path, &identity.server, &identity.tenant).unwrap();
    assert_eq!(loaded.token, identity.token);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(load_identity(&path, "https://foresight.example", "default").is_err());
    assert!(load_identity(&path, &identity.server, "another-tenant").is_err());
    assert!(save_identity(&consent_fixture(), &path, &identity).is_err());
}

#[test]
fn public_or_symlinked_storage_is_rejected() {
    let directory = PrivateDirectory::new();
    let path = directory.0.join("identity.json");
    let identity = fixture();
    save_identity(&consent_fixture(), &path, &identity).unwrap();
    let alias = directory.0.join("alias.json");
    symlink(&path, &alias).unwrap();
    assert!(load_identity(&alias, &identity.server, &identity.tenant).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load_identity(&path, &identity.server, &identity.tenant).is_err());
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(save_identity(&consent_fixture(), &directory.0.join("new.json"), &identity).is_err());
}
