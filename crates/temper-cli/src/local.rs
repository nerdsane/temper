//! Local-first daemon credentials and app lifecycle commands.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use rand::RngCore as _;
use temper_platform::app_bundles::{
    InstallBundleRequest, InstallBundleResult, LocalDependencyLock, LocalDependencyLockEntry,
    build_workspace_bundle, write_workspace_lock,
};

use crate::local_args::{AppArgs, AppCommand, CacheCommand, DevArgs, UpArgs};
use crate::{ActorRuntimeBackend, StorageBackend, serve};

/// Start the persistent local runtime.
pub async fn run_up(args: UpArgs) -> Result<()> {
    let data_dir = args.data_dir.map_or_else(default_data_dir, Ok)?;
    let token = ensure_operator_credential(&data_dir)?;
    serve::run(
        args.port,
        Vec::new(),
        Vec::new(),
        StorageBackend::Turso,
        true,
        ActorRuntimeBackend::Legacy,
        Vec::new(),
        false,
        false,
        None,
        args.tenant,
        Some(data_dir),
        "127.0.0.1".to_string(),
        Some(token),
        Some(!args.no_open),
    )
    .await
}

/// Execute a local application bundle command.
pub async fn run_app(args: AppArgs) -> Result<()> {
    match args.command {
        AppCommand::Lock { path, locals } => lock_workspace(&path, &locals),
        AppCommand::Install {
            path,
            tenant,
            url,
            locked,
            data_dir,
        } => {
            let data_dir = data_dir.map_or_else(default_data_dir, Ok)?;
            install(&path, &tenant, &url, &data_dir, locked).await?;
            Ok(())
        }
        AppCommand::Cache {
            command:
                CacheCommand::Gc {
                    tenant,
                    url,
                    dry_run,
                    data_dir,
                },
        } => {
            let data_dir = data_dir.map_or_else(default_data_dir, Ok)?;
            garbage_collect(&tenant, &url, &data_dir, dry_run).await
        }
    }
}

/// Watch a workspace and promote each verified revision.
pub async fn run_dev(args: DevArgs) -> Result<()> {
    let data_dir = args.data_dir.map_or_else(default_data_dir, Ok)?;
    dev(&args.path, &args.tenant, &args.url, &data_dir).await
}

/// Return the established Temper local data directory.
pub fn default_data_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".local/share/temper"))
}

/// Load or create the local tenant operator credential.
pub fn ensure_operator_credential(data_dir: &Path) -> Result<String> {
    let credentials_dir = data_dir.join("credentials");
    std::fs::create_dir_all(&credentials_dir).with_context(|| {
        format!(
            "failed to create local credential directory {}",
            credentials_dir.display()
        )
    })?;
    protect_directory(&credentials_dir)?;
    let credential_path = credentials_dir.join("operator.token");
    match std::fs::symlink_metadata(&credential_path) {
        Ok(_) => {
            require_private_file(&credential_path)?;
            let token = std::fs::read_to_string(&credential_path)
                .with_context(|| format!("failed to read {}", credential_path.display()))?;
            let token = token.trim();
            anyhow::ensure!(!token.is_empty(), "{} is empty", credential_path.display());
            return Ok(token.to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {}", credential_path.display()));
        }
    }

    let mut random = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut random); // determinism-ok: local credential generation
    let token = format!(
        "temper-local-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
    );
    let mut temporary = tempfile::Builder::new()
        .prefix(".operator.token.")
        .tempfile_in(&credentials_dir)
        .context("failed to stage local operator credential")?;
    use std::io::Write as _;
    temporary
        .write_all(token.as_bytes())
        .context("failed to write local operator credential")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to flush local operator credential")?;
    match temporary.persist_noclobber(&credential_path) {
        Ok(_) => {}
        Err(_error) if credential_path.exists() => {
            require_private_file(&credential_path)?;
            return std::fs::read_to_string(&credential_path)
                .map(|value| value.trim().to_string())
                .with_context(|| format!("failed to read {}", credential_path.display()));
        }
        Err(error) => {
            return Err(error.error)
                .with_context(|| format!("failed to publish {}", credential_path.display()));
        }
    }
    Ok(token)
}

/// Install one local workspace through the governed bundle endpoint.
pub async fn install(
    workspace: &Path,
    tenant: &str,
    url: &str,
    data_dir: &Path,
    locked: bool,
) -> Result<InstallBundleResult> {
    let bundle = build_workspace_bundle(workspace, tenant, locked).map_err(anyhow::Error::msg)?;
    let result = install_request(bundle.request.clone(), url, data_dir).await?;
    if !locked {
        write_workspace_lock(&bundle).map_err(anyhow::Error::msg)?;
    }
    println!(
        "Installed {} at {} into tenant {}",
        result.app_name, result.bundle_digest, result.tenant
    );
    Ok(result)
}

/// Run reachability-based collection through the governed daemon boundary.
pub async fn garbage_collect(
    tenant: &str,
    url: &str,
    data_dir: &Path,
    dry_run: bool,
) -> Result<()> {
    let token = credential_for_url(url, data_dir)?;
    let endpoint = format!("{}/api/app-bundles/cache/gc", url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("X-Tenant-Id", tenant)
        .json(&serde_json::json!({ "tenant": tenant, "dry_run": dry_run }))
        .send()
        .await
        .with_context(|| format!("failed to call bundle cache GC at {endpoint}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "bundle cache GC failed ({status}): {body}"
    );
    println!("{body}");
    Ok(())
}

/// Add explicit local dependency mappings and resolve the workspace lock.
pub fn lock_workspace(workspace: &Path, locals: &[String]) -> Result<()> {
    let root = workspace
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace {}", workspace.display()))?;
    let path = root.join("temper.lock.toml");
    let original = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let mut lock = match original.as_deref() {
        Some(bytes) => toml::from_str::<LocalDependencyLock>(
            std::str::from_utf8(bytes).context("dependency lock is not UTF-8")?,
        )
        .with_context(|| format!("failed to parse {}", path.display()))?,
        None => LocalDependencyLock::default(),
    };
    for mapping in locals {
        let (name, source_path) = mapping
            .split_once('=')
            .with_context(|| format!("invalid --local '{mapping}', expected NAME=PATH"))?;
        anyhow::ensure!(
            !name.trim().is_empty(),
            "local dependency name must not be empty"
        );
        if let Some(entry) = lock.entries.iter_mut().find(|entry| entry.name == name) {
            entry.path = source_path.to_string();
            entry.digest.clear();
        } else {
            lock.entries.push(LocalDependencyLockEntry {
                name: name.to_string(),
                path: source_path.to_string(),
                digest: String::new(),
            });
        }
    }
    lock.entries
        .sort_by(|left, right| left.name.cmp(&right.name));
    let source = toml::to_string_pretty(&lock).context("failed to encode dependency lock")?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, source)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("failed to publish {}", path.display()))?;

    let bundle = match build_workspace_bundle(&root, "default", false) {
        Ok(bundle) => bundle,
        Err(error) => {
            restore_lock(&path, original.as_deref())?;
            return Err(anyhow::Error::msg(error));
        }
    };
    write_workspace_lock(&bundle).map_err(anyhow::Error::msg)?;
    println!("Locked local dependency closure for {}", root.display());
    Ok(())
}

fn restore_lock(path: &Path, original: Option<&[u8]>) -> Result<()> {
    match original {
        Some(bytes) => {
            let temporary = path.with_extension("toml.restore");
            std::fs::write(&temporary, bytes)
                .with_context(|| format!("failed to restore {}", path.display()))?;
            std::fs::rename(&temporary, path)
                .with_context(|| format!("failed to publish restored {}", path.display()))
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to remove {}", path.display()))
            }
        },
    }
}

/// Watch a workspace and promote each successfully built immutable revision.
pub async fn dev(workspace: &Path, tenant: &str, url: &str, data_dir: &Path) -> Result<()> {
    let daemon_is_ready = reqwest::get(format!("{}/healthz", url.trim_end_matches('/')))
        .await
        .is_ok_and(|response| response.status().is_success());
    let mut daemon = if !daemon_is_ready {
        let parsed_url =
            url::Url::parse(url).with_context(|| format!("invalid Temper URL '{url}'"))?;
        anyhow::ensure!(
            matches!(parsed_url.host_str(), Some("127.0.0.1" | "localhost")),
            "refusing to auto-start a daemon for non-loopback URL '{url}'"
        );
        let port = parsed_url
            .port_or_known_default()
            .context("local Temper URL has no port")?;
        let current = std::env::current_exe().context("failed to locate temper executable")?;
        let mut command = tokio::process::Command::new(current);
        command
            .arg("up")
            .arg("--port")
            .arg(port.to_string())
            .arg("--no-open")
            .arg("--tenant")
            .arg(tenant)
            .arg("--data-dir")
            .arg(data_dir)
            .kill_on_drop(true);
        let child = command
            .spawn()
            .context("failed to start local Temper daemon")?;
        wait_until_ready(url).await?;
        Some(child)
    } else {
        None
    };

    let mut active_digest = String::new();
    let mut rejected_digest = String::new();
    let mut retry_after = Instant::now();
    let mut last_error = String::new();
    println!("Watching {}", workspace.display());
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                match build_workspace_bundle(workspace, tenant, false) {
                    Ok(bundle)
                        if bundle.request.manifest.bundle_digest != active_digest
                            && (bundle.request.manifest.bundle_digest != rejected_digest
                                || Instant::now() >= retry_after) => {
                        let attempted_digest = bundle.request.manifest.bundle_digest.clone();
                        match install_request(bundle.request.clone(), url, data_dir).await {
                            Ok(result) => {
                                write_workspace_lock(&bundle).map_err(anyhow::Error::msg)?;
                                active_digest = result.bundle_digest;
                                rejected_digest.clear();
                                last_error.clear();
                                println!("Activated {}", active_digest);
                            }
                            Err(error) => {
                                rejected_digest = attempted_digest;
                                retry_after = Instant::now() + Duration::from_secs(5);
                                let message = error.to_string();
                                if message != last_error {
                                    eprintln!("Revision rejected; last good bundle remains active: {message}");
                                    last_error = message;
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(error) if error != last_error => {
                        eprintln!("Revision rejected; last good bundle remains active: {error}");
                        last_error = error;
                    }
                    Err(_) => {}
                }
            }
        }
    }
    if let Some(child) = daemon.as_mut() {
        let _ = child.kill().await;
    }
    Ok(())
}

async fn install_request(
    request: InstallBundleRequest,
    url: &str,
    data_dir: &Path,
) -> Result<InstallBundleResult> {
    let token = credential_for_url(url, data_dir)?;
    let endpoint = format!("{}/api/app-bundles/install", url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&endpoint)
        .header("Authorization", format!("Bearer {token}"))
        .header("X-Tenant-Id", &request.tenant)
        .json(&request)
        .send()
        .await
        .with_context(|| format!("failed to call local bundle install at {endpoint}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "local bundle install failed ({status}): {body}"
    );
    serde_json::from_str(&body).context("failed to decode local bundle install response")
}

fn credential_for_url(url: &str, data_dir: &Path) -> Result<String> {
    if let Ok(token) = std::env::var("TEMPER_API_KEY") {
        return Ok(token);
    }
    let parsed = url::Url::parse(url).with_context(|| format!("invalid Temper URL '{url}'"))?;
    if matches!(parsed.host_str(), Some("127.0.0.1" | "localhost")) {
        return ensure_operator_credential(data_dir);
    }
    anyhow::bail!("TEMPER_API_KEY is required for non-loopback Temper URL '{url}'")
}

async fn wait_until_ready(url: &str) -> Result<()> {
    let endpoint = format!("{}/healthz", url.trim_end_matches('/'));
    for _ in 0..100 {
        if reqwest::get(&endpoint)
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("local Temper daemon did not become ready at {endpoint}")
}

#[cfg(unix)]
fn protect_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect {}", path.display()))
}

#[cfg(not(unix))]
fn protect_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn require_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "{} must be a regular file",
        path.display()
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "{} must not be accessible by group or other users (mode {mode:o})",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn require_private_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "{} must be a regular file",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_credential_is_stable_and_private() {
        let root = tempfile::tempdir().unwrap();
        let first = ensure_operator_credential(root.path()).unwrap();
        let second = ensure_operator_credential(root.path()).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("temper-local-"));
        #[cfg(unix)]
        require_private_file(&root.path().join("credentials/operator.token")).unwrap();
    }

    #[test]
    fn concurrent_credential_creation_converges_on_one_token() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_path_buf();
        let first_path = path.clone();
        let first = std::thread::spawn(move || ensure_operator_credential(&first_path).unwrap());
        let second = std::thread::spawn(move || ensure_operator_credential(&path).unwrap());
        assert_eq!(first.join().unwrap(), second.join().unwrap());
    }
}
