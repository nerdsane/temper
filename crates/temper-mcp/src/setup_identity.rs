//! Service-bound private requester identity storage for human-authorized setup.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::setup_consent::SetupConsent;

/// Private state retained across a partial setup and subsequent reconnects.
/// Deliberately does not implement Debug: the bearer must never be logged.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequesterIdentity {
    pub(crate) server: String,
    pub(crate) tenant: String,
    pub(crate) principal: String,
    pub(crate) token: String,
}

impl RequesterIdentity {
    pub(crate) fn validate_binding(&self, server: &str, tenant: &str) -> Result<()> {
        if self.server != server || self.tenant != tenant {
            bail!("Private requester identity belongs to a different server or tenant");
        }
        let principal = self.principal.strip_prefix("mcp-").unwrap_or("");
        let token = self.token.strip_prefix("tmpr_").unwrap_or("");
        if principal.len() != 32
            || token.len() != 64
            || !principal
                .bytes()
                .chain(token.bytes())
                .all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("Invalid private requester identity");
        }
        Ok(())
    }
}

/// Read a bounded private file; never follow a final symlink.
pub(crate) fn load_identity(path: &Path, server: &str, tenant: &str) -> Result<RequesterIdentity> {
    validate_parent(path)?;
    let metadata = fs::symlink_metadata(path).context("Cannot inspect private identity file")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 4096 {
        bail!("Identity file must be a small regular file");
    }
    check_private(&metadata)?;
    let mut file = OpenOptions::new().read(true).open(path)?;
    // Compare the opened file with the inspected file before reading secrets.
    check_same_file(&metadata, &file.metadata()?)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("Identity file exceeded its read budget");
    }
    let identity: RequesterIdentity =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid identity file"))?;
    identity.validate_binding(server, tenant)?;
    Ok(identity)
}

/// Persist a new identity only after human consent; never overwrite a file.
pub(crate) fn save_identity(
    _consent: &SetupConsent,
    path: &Path,
    identity: &RequesterIdentity,
) -> Result<()> {
    validate_parent(path)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    bail!("Private identity storage is currently supported only on Unix");
    let temporary = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let mut file = options
        .open(&temporary)
        .context("Cannot create private identity file")?;
    let bytes = serde_json::to_vec(identity)?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        // A hard link publishes complete bytes atomically and cannot replace an
        // identity another process has already published at this path.
        fs::hard_link(&temporary, path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn validate_parent(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("Identity file path must be absolute");
    }
    let parent = path
        .parent()
        .context("Identity file needs a private parent directory")?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Identity parent must be a real private directory");
    }
    check_private(&metadata)
}

#[cfg(unix)]
fn check_private(metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("Identity storage must not be accessible to group or other users");
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_metadata: &fs::Metadata) -> Result<()> {
    bail!("Private identity storage is currently supported only on Unix")
}

#[cfg(unix)]
fn check_same_file(before: &fs::Metadata, after: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if before.dev() != after.dev() || before.ino() != after.ino() {
        bail!("Identity file changed while opening it");
    }
    check_private(after)
}

#[cfg(not(unix))]
fn check_same_file(_before: &fs::Metadata, _after: &fs::Metadata) -> Result<()> {
    bail!("Private identity storage is currently supported only on Unix")
}

#[cfg(all(test, unix))]
#[path = "setup_identity_test.rs"]
mod tests;
