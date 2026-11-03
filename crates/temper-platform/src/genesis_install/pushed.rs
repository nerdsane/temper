//! A second source for the one installer: an app's files, posted to this
//! server (`POST /api/os-apps/{name}/install`).
//!
//! A push only materializes its files into a cache directory of its own
//! ([`InstallSource::Pushed`], named by the digest of the files); from there
//! it is installed like a registry closure
//! ([`closure_install::install_materialized_closure`]): parse, register,
//! reconcile, verify, then commit or roll back. Its install record says
//! `pushed` and names the version as `pushed/<app>@<digest>`, so boot restores
//! it from that directory and a later failed install rolls back to it, as for
//! a registry install.

use std::time::{Duration, Instant};

use serde::Deserialize;

use super::closure_install::{ClosureRefused, ClosureSource, MaterializedApp, MaterializedClosure};
use super::*;
use crate::os_apps::{AppFileSource, read_app_guide, read_app_manifest, tenant_os_app_dir};

mod files;

/// The source kind a pushed install's records carry.
pub(super) const SOURCE_KIND: &str = "pushed";

/// One pushed file: its text, its bytes in base64, or the same bytes as the
/// file at its path in the version installed now, named by their SHA-256.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PushedFile {
    /// UTF-8 text.
    Text(String),
    /// Bytes, base64-encoded.
    Bytes(Base64File),
    /// The installed version's file at the same path, unchanged.
    Kept(KeptFile),
}

/// `{"base64": "..."}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Base64File {
    /// The file's bytes, standard base64.
    pub base64: String,
}

/// `{"keep": "<sha256 hex>"}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeptFile {
    /// The SHA-256 of the installed file, in hex.
    pub keep: String,
}

/// A pushed version that is installed.
#[derive(Debug)]
pub struct PushedInstall {
    /// The digest of its files, which names its version.
    pub digest: String,
    /// What the installer did.
    pub result: InstallResult,
}

/// Why a push was refused.
#[derive(Debug)]
pub enum PushRefused {
    /// The request is wrong: a name, a path, a kept file, or files that are
    /// not the app named. Nothing changed.
    Request(String),
    /// A spec does not parse; says which and why. Nothing changed.
    Unparsed(String),
    /// The installer refused the version, saying why; the version installed
    /// before it, if any, is installed again.
    Install(String),
    /// This server could not do its part, such as writing the files.
    Server(String),
}

/// Install `files` as `tenant`'s version of `app`, and say how long the push
/// waited for this host's install turn ([`temper_server::host_turn`]).
pub async fn install_pushed_app(
    platform: &PlatformState,
    tenant: &str,
    app: &str,
    files: BTreeMap<String, PushedFile>,
) -> (Duration, Result<PushedInstall, PushRefused>) {
    for (label, value) in [("tenant", tenant), ("app name", app)] {
        if let Err(error) = validate_identity_component(label, value) {
            return (Duration::ZERO, Err(PushRefused::Request(error)));
        }
    }
    let asked = Instant::now();
    let turn = temper_server::host_turn::take().await;
    let waited = asked.elapsed();
    let result = match turn {
        Ok(_turn) => install_in_turn(platform, tenant, app, files).await,
        Err(error) => Err(PushRefused::Server(error)),
    };
    (waited, result)
}

async fn install_in_turn(
    platform: &PlatformState,
    tenant: &str,
    app: &str,
    files: BTreeMap<String, PushedFile>,
) -> Result<PushedInstall, PushRefused> {
    let prior = recorded_digest(platform, tenant, app).await;
    let kept_from = prior
        .as_deref()
        .map(|digest| pushed_root(platform, tenant, app, digest).join(app));
    let files = files::resolve(files, kept_from.as_deref())?;
    let digest = files::digest(&files);
    let root = pushed_root(platform, tenant, app, &digest);
    // A directory's name does not prove its bytes or saved list still match.
    // Repair a damaged cache from the request before it can be installed.
    let created = !root.is_dir();
    if created || !files::matches(&root, app, &files) {
        files::materialize(&root, app, &files).map_err(|error| {
            PushRefused::Server(format!("the pushed files could not be written: {error}"))
        })?;
    }
    // Only discard a new version proven inactive. A repeated push can also
    // recreate a missing cache for the prior install; never delete that if
    // rollback selected it again.
    let discard = || {
        if created
            && tenant_os_app_dir(tenant, app).as_deref() != Some(root.join(app).as_path())
            && let Err(error) = std::fs::remove_dir_all(&root)
        {
            tracing::warn!(app, path = %root.display(), %error, "a refused pushed version could not be deleted");
        }
    };

    let app_dir = root.join(app);
    // The installer reads exactly these files, by name: a folder just renamed
    // into place can list short, and is never listed here.
    let written = AppFileSource::written(app_dir.clone(), files.keys().cloned());
    let named = read_app_manifest(&app_dir).is_some_and(|manifest| manifest.name == app);
    if !named || read_app_guide(&app_dir).is_none() {
        discard();
        return Err(PushRefused::Request(format!(
            "these files are not the app '{app}': it needs an app.toml naming '{app}' and an APP.md"
        )));
    }

    let app_ref = pushed_ref(app, &digest);
    let installed = closure_install::install_materialized_closure(
        platform,
        tenant,
        &root,
        closure(&app_ref, app, &digest, written),
    )
    .await;
    match installed {
        Ok(installed) => {
            let keep: Vec<&str> = [Some(digest.as_str()), prior.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            prune(platform, tenant, app, &keep);
            Ok(PushedInstall {
                digest,
                result: installed.install,
            })
        }
        Err(ClosureRefused::Unparsed(error)) => {
            discard();
            Err(PushRefused::Unparsed(error))
        }
        Err(ClosureRefused::Inactive(error)) => {
            discard();
            Err(PushRefused::Install(error))
        }
        Err(ClosureRefused::Failed(error)) => Err(PushRefused::Install(error)),
    }
}

/// At boot, make the version of `app` that `tenant` serves from its boot
/// catalog its first pushed version, when it has no pushed version recorded.
/// Returns the adopted digest, or `None` when there was nothing to adopt.
///
/// Boot calls this only after the version installed: one that did not install
/// was not served, so it is not adopted. Nothing goes empty that way: a boot
/// install that fails leaves the specs stored in the database serving, and
/// Katagami's control plane then sends that commit through the install route,
/// where a refusal changes nothing.
///
/// Why: an app running before pushes existed has only the catalog version it
/// was started with (for Katagami, the old checkout). Once adopted, the pushed
/// version is what every later boot restores, whatever the catalog holds then,
/// until the next push. Adoption runs no checks and refuses nothing: the
/// version is already serving, and one whose files fail today's checks must
/// keep serving until its owner pushes a new one. The whole app directory is
/// copied, so a `COMMIT` file in it (Katagami writes `<app>/COMMIT` into the
/// checkout it adopts) names the adopted version's commit.
///
/// This is a migration path for apps that predate pushes. Delete it, and its
/// call at boot in temper-cli `serve/bootstrap.rs`, once every production app
/// has a pushed root (tracked on the Katagami hub as "Remove adoption once
/// every prod app has a pushed root").
pub async fn adopt_catalog_app(
    platform: &PlatformState,
    tenant: &str,
    app: &str,
) -> Result<Option<String>, String> {
    validate_identity_component("tenant", tenant)?;
    validate_identity_component("app name", app)?;
    // Adoption replaces the catalog and its record, just like a new install;
    // a late recovery must see either side of that replacement, never half.
    let _turn = temper_server::host_turn::take().await?;
    if recorded_digest(platform, tenant, app).await.is_some() {
        return Ok(None);
    }
    let Some(app_dir) = tenant_os_app_dir(tenant, app) else {
        return Ok(None);
    };
    let tree = files::read_tree(&app_dir).map_err(|error| {
        format!(
            "the files of '{app}' at {} could not be read: {error}",
            app_dir.display()
        )
    })?;
    if tree.is_empty() {
        return Ok(None);
    }
    let digest = files::digest(&tree);
    let root = pushed_root(platform, tenant, app, &digest);
    if !root.is_dir() {
        files::materialize(&root, app, &tree)
            .map_err(|error| format!("the files of '{app}' could not be copied: {error}"))?;
    }
    let written = AppFileSource::written(root.join(app), tree.keys().cloned());
    register_written_apps(tenant, root, vec![(app.to_string(), written.clone())]);
    let app_ref = pushed_ref(app, &digest);
    let adopted = closure(&app_ref, app, &digest, written);
    closure_install::record_closure(platform, tenant, &adopted).await?;
    Ok(Some(digest))
}

/// Whether `tenant`'s install of `app` that this server stored is a pushed
/// version: boot then replays it as recorded rather than installing its
/// catalog files over it.
pub async fn pushed_install_recorded(platform: &PlatformState, tenant: &str, app: &str) -> bool {
    recorded_digest(platform, tenant, app).await.is_some()
}

/// The digest of the pushed version `tenant`'s stored install of `app` names.
async fn recorded_digest(platform: &PlatformState, tenant: &str, app: &str) -> Option<String> {
    let record = platform_store(platform)?
        .get_installed_app(tenant, app)
        .await
        .ok()??;
    if record.source_kind != SOURCE_KIND || record.status != "installed" {
        return None;
    }
    let (_, digest) = record.app_ref.rsplit_once('@')?;
    let is_digest = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
    is_digest.then(|| digest.to_string())
}

/// The ref a pushed version is recorded under, and its cache key.
fn pushed_ref(app: &str, digest: &str) -> String {
    format!("{SOURCE_KIND}/{app}@{digest}")
}

/// The files this push supplies: version `digest` of `app`, recorded as
/// pushed and pinned. The shared core resolves its complete dependency
/// closure over the tenant catalog before checking or mutating anything.
fn closure<'a>(
    app_ref: &'a str,
    app: &str,
    digest: &str,
    files: AppFileSource,
) -> MaterializedClosure<'a> {
    let version = RegistryAppRef {
        owner: SOURCE_KIND.to_string(),
        name: app.to_string(),
        version_hash: Some(digest.to_string()),
    };
    MaterializedClosure {
        app_ref,
        root: version.clone(),
        apps: vec![MaterializedApp { version, files }],
        source: ClosureSource {
            source_kind: SOURCE_KIND,
            registry_url: "",
            registry_tenant: "",
            follow_policy: "pinned",
        },
    }
}

/// Delete every pushed version of `app` for `tenant` but `keep`: the version
/// installed now and the one before it, which a failed install rolls back to.
/// Each version holds a whole copy of the app, so without this a server's
/// disk fills one push at a time.
fn prune(platform: &PlatformState, tenant: &str, app: &str, keep: &[&str]) {
    let kept: Vec<PathBuf> = keep
        .iter()
        .map(|digest| pushed_root(platform, tenant, app, digest))
        .collect();
    let Some(base) = kept.first().and_then(|root| root.parent()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    // A version's directory is `<sanitized ref>-<owner digest>`; a name is
    // taken as one only when it is exactly where that digest's version of
    // this app for this tenant lives.
    let prefix = format!("{}-", sanitize_fragment(&format!("{SOURCE_KIND}/{app}@")));
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(digest) = name
            .to_str()
            .and_then(|name| name.strip_prefix(&prefix))
            .and_then(|rest| rest.get(..64))
        else {
            continue;
        };
        let root = pushed_root(platform, tenant, app, digest);
        if root != entry.path() || kept.contains(&root) {
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&root) {
            tracing::warn!(app, path = %root.display(), %error, "an old pushed version could not be deleted");
        }
    }
}

/// The cache directory of `tenant`'s pushed version `digest` of `app`.
fn pushed_root(platform: &PlatformState, tenant: &str, app: &str, digest: &str) -> PathBuf {
    genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant,
            source: InstallSource::Pushed,
        },
        &pushed_ref(app, digest),
    )
}
