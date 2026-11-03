//! Which app in a tenant owns each entity type and entity set.
//!
//! An app owns the entity types its CSDL declares and the specs that run
//! them. A tenant's CSDL is merged from every app it installs, and a type
//! runs one spec, so a spec its own app does not declare, or an entity set
//! two apps declare, leaves one app serving under the other's schema with
//! nothing saying so.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::csdl::{CsdlDocument, parse_csdl};

use super::app_catalog::catalog;
use super::app_files::AppFiles;
use super::types::{AppBundle, InstallWarning};
use crate::state::PlatformState;

/// What an install of `app_name`'s bundle would do to who owns what.
#[derive(Debug, Default)]
pub(super) struct Ownership {
    /// What the install would newly break, which a request refuses: each
    /// spec whose entity type its CSDL does not declare, and each entity set
    /// another app installed in `tenant` declares that is new to `app_name`
    /// (it is not installed there yet, or its installed version does not
    /// declare the set), named with that app.
    pub(super) refused: Vec<String>,
    /// Entity sets `app_name`'s installed version and another installed app
    /// both declare from before. The install goes ahead and says so, as a
    /// boot replay does: refusing it would take a running app away. When the
    /// installed version cannot be read, a shared set is kept here too, with
    /// a line saying why it could not be told apart.
    pub(super) standing: Vec<String>,
}

/// Who owns what once `app_name`'s bundle is installed in `tenant`.
pub(super) async fn ownership(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<Ownership, String> {
    let csdl = bundle_csdl(bundle, app_name)?;
    let declared = declared_types(csdl.as_ref());
    let mut ownership = Ownership {
        refused: bundle
            .specs
            .iter()
            .filter(|(entity_type, _)| !declared.contains(entity_type))
            .map(|(entity_type, _)| {
                format!("spec {entity_type} has no entity type {entity_type} in the app's CSDL")
            })
            .collect(),
        standing: Vec::new(),
    };
    let sets = entity_sets(csdl.as_ref());
    if sets.is_empty() {
        return Ok(ownership);
    }
    let installed = installed_entity_sets(state, tenant, app_name).await?;
    // The sets `app_name`'s installed version declares, read only when
    // another app shares one.
    let mut before: Option<Result<BTreeSet<String>, String>> = None;
    for (other, other_sets) in installed.others {
        for set in sets.keys().filter(|set| other_sets.contains(*set)) {
            if !installed.this_app {
                ownership.refused.push(format!(
                    "entity set {set} is already owned by app '{other}'"
                ));
                continue;
            }
            if before.is_none() {
                before = Some(installed_version_sets(state, tenant, app_name).await);
            }
            match before.as_ref() {
                Some(Ok(declared)) if !declared.contains(set) => ownership.refused.push(format!(
                    "entity set {set} is already owned by app '{other}', and the installed \
                     version of '{app_name}' does not declare it"
                )),
                Some(Err(why)) => {
                    ownership.standing.push(format!(
                        "entity set {set} is declared by both '{app_name}' and '{other}'"
                    ));
                    ownership.standing.push(format!(
                        "whether entity set {set} is new to '{app_name}' is not known, because \
                         its installed version cannot be read: {why}"
                    ));
                }
                _ => ownership.standing.push(format!(
                    "entity set {set} is declared by both '{app_name}' and '{other}'"
                )),
            }
        }
    }
    Ok(ownership)
}

/// The entity sets installed app `app_name`'s installed version declares, read
/// from that version's own files. Unreadable files are an error, never an
/// empty set: an empty set would refuse a pair that stands.
async fn installed_version_sets(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<BTreeSet<String>, String> {
    let source = crate::genesis_install::installed_version_files(state, tenant, app_name).await?;
    let csdl = read_source_csdl(&source)?;
    Ok(entity_sets(csdl.as_ref()).into_keys().collect())
}

/// Warnings for what the bundle declares but will not run: each CSDL entity
/// type with no spec in the app, which serves its schema with no state
/// machine, and each spec or CSDL file under `app_dir` the install does not
/// read, named with the file it reads in its place.
pub(super) fn unrun_declarations(
    source: &super::AppFileSource,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<Vec<InstallWarning>, String> {
    let csdl = bundle_csdl(bundle, app_name)?;
    let specs: BTreeSet<&str> = bundle.specs.iter().map(|(name, _)| name.as_str()).collect();
    let mut warnings: Vec<InstallWarning> = csdl
        .as_ref()
        .map(|csdl| unspecified_entity_types(csdl, &specs))
        .unwrap_or_default()
        .into_iter()
        .map(InstallWarning::entity_ownership)
        .collect();
    warnings.extend(
        AppFiles::from_source(source)
            .unread_warnings_from_source(source)
            .into_iter()
            .map(InstallWarning::entity_ownership),
    );
    Ok(warnings)
}

fn bundle_csdl(bundle: &AppBundle, app_name: &str) -> Result<Option<CsdlDocument>, String> {
    bundle
        .csdl
        .as_deref()
        .map(|text| {
            parse_csdl(text)
                .map_err(|e| format!("Failed to parse CSDL for os-app '{app_name}': {e}"))
        })
        .transpose()
}

/// One line for each entity type `csdl` gives a set and no spec in `specs`
/// runs: its records are served as schema with no state machine. Install
/// and `temper verify` both report these.
pub fn unspecified_entity_types(csdl: &CsdlDocument, specs: &BTreeSet<&str>) -> Vec<String> {
    entity_sets(Some(csdl))
        .into_iter()
        .filter(|(_, entity_type)| !specs.contains(entity_type.as_str()))
        .map(|(set, entity_type)| {
            format!(
                "entity type {entity_type} (set {set}) has no spec in the app, so it is \
                 served as schema with no state machine"
            )
        })
        .collect()
}

fn declared_types(csdl: Option<&CsdlDocument>) -> BTreeSet<String> {
    csdl.into_iter()
        .flat_map(|doc| &doc.schemas)
        .flat_map(|schema| &schema.entity_types)
        .map(|entity_type| entity_type.name.clone())
        .collect()
}

/// Entity set name to the unqualified name of its entity type.
fn entity_sets(csdl: Option<&CsdlDocument>) -> BTreeMap<String, String> {
    csdl.into_iter()
        .flat_map(|doc| &doc.schemas)
        .flat_map(|schema| &schema.entity_containers)
        .flat_map(|container| &container.entity_sets)
        .map(|set| {
            let entity_type = set
                .entity_type
                .rsplit('.')
                .next()
                .unwrap_or(&set.entity_type);
            (set.name.clone(), entity_type.to_string())
        })
        .collect()
}

/// The apps installed in a tenant, as the install of one of them sees them.
#[derive(Default)]
pub(super) struct Installed {
    /// Whether the installed apps could be listed: a tenant with no platform
    /// store cannot say which apps contribute to its schema.
    pub(super) listed: bool,
    /// Whether the app being installed is installed already.
    pub(super) this_app: bool,
    /// Each other installed app with its own CSDL.
    pub(super) others: Vec<InstalledApp>,
}

/// One installed app and what its files say it declares.
pub(super) struct InstalledApp {
    pub(super) name: String,
    /// The app's CSDL, `None` when the app has none, or why it cannot be read.
    pub(super) csdl: Result<Option<CsdlDocument>, String>,
}

/// The apps installed in `tenant` besides `app_name`, each with the CSDL its
/// files hold in the catalog. An app whose files the catalog no longer holds,
/// or whose CSDL does not read, is kept with the reason: callers decide what
/// that means for them.
pub(super) async fn installed_apps(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<Installed, String> {
    let Some(store) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Ok(Installed::default());
    };
    let installed = store
        .list_all_installed_apps()
        .await
        .map_err(|e| format!("Failed to list the apps installed in tenant '{tenant}': {e}"))?;
    let mut found = Installed {
        listed: true,
        ..Installed::default()
    };
    for (_, other) in installed
        .into_iter()
        .filter(|(row_tenant, _)| row_tenant == tenant)
    {
        if other == app_name {
            found.this_app = true;
            continue;
        }
        let csdl = read_app_csdl(tenant, &other);
        found.others.push(InstalledApp { name: other, csdl });
    }
    Ok(found)
}

fn read_app_csdl(tenant: &str, app: &str) -> Result<Option<CsdlDocument>, String> {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let source = cat.app_files(Some(tenant), app).cloned();
    drop(cat);
    let Some(source) = source else {
        return Err("the app is not in the catalog".to_string());
    };
    read_source_csdl(&source)
}

/// The CSDL the app files in `source` hold: `None` only for an app with no
/// specs, and an error for one whose files do not read.
fn read_source_csdl(source: &super::AppFileSource) -> Result<Option<CsdlDocument>, String> {
    // A boot or install that reads an app as declaring nothing rebuilds the
    // schema without it, so only an app whose files are on disk and hold no
    // specs may read as having no CSDL. A missing or half-written app is
    // unreadable, and callers keep the schema the tenant already has.
    source.read_text("app.toml")?;
    let files = AppFiles::from_source(source);
    let Some(path) = files.csdl else {
        if files.ioa.is_empty() {
            return Ok(None);
        }
        return Err("it has specs but no CSDL".to_string());
    };
    let text = source.read_text(&super::app_relative_path(source.dir(), &path))?;
    parse_csdl(&text)
        .map(Some)
        .map_err(|e| format!("its CSDL does not parse: {e}"))
}

/// The entity sets each other installed app declares. An app that cannot be
/// read cannot be compared, so it is logged and left out.
async fn installed_entity_sets(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<EntitySets, String> {
    let installed = installed_apps(state, tenant, app_name).await?;
    let mut found = EntitySets {
        this_app: installed.this_app,
        others: Vec::new(),
    };
    for other in installed.others {
        match other.csdl {
            Ok(csdl) => found
                .others
                .push((other.name, entity_sets(csdl.as_ref()).into_keys().collect())),
            Err(why) => {
                tracing::warn!(tenant, app = %other.name, "installed app's entity sets are not compared: {why}");
            }
        }
    }
    Ok(found)
}

/// The entity sets each other installed app declares.
struct EntitySets {
    this_app: bool,
    others: Vec<(String, BTreeSet<String>)>,
}
