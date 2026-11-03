//! The install's review of its app's modules ([`crate::module_review`]). It starts when the install starts, so Jev
//! reads beside the install's own proofs and writes, and it goes on after the install answers, since Jev takes seconds
//! and an app's change may not. Advisory: what it says is kept for the app (`PlatformStore::save_module_review`), and it
//! never refuses or holds an install. A caller that answers with all of it waits (`InstallResult::with_reviews_read`).
//!
//! A module is read again when its code changed or when a spec that runs it changed, since Jev reads a module against
//! the specs that run it: a new guard or a split state can answer a finding, or make one.

use std::path::{Component, Path};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use serde_json::json;
use temper_runtime::tenant::TenantId;
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;

use super::reconcile;
use super::types::{AppBundle, InstallWarning, WasmModuleManifest};
use crate::module_review::{
    BundleReview, Finding, ModuleSource, ReviewSaveFailure, SPEC_BYTES, SpecSource, said_left_out,
};
use crate::state::PlatformState;

/// Source past this is not read. The largest app module seen was 1,175 lines, about 60 KB (census of Rita's apps, 26
/// Oct 2026); each 160 lines is a paid call, so a file many times that is not a module to read line by line.
const MAX_SOURCE_BYTES: u64 = 512 * 1024;

/// The file in an app's folder that says which commit it is, when whoever made it says one.
const COMMIT_FILE: &str = "COMMIT";

/// What the review said, and which modules Jev read (with or without findings).
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ReviewOutcome {
    pub(super) warnings: Vec<InstallWarning>,
    pub(super) reviewed: Vec<String>,
}

/// A review an install left running: `GET /api/os-apps/{app}/review/{version}` says what became of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewAsked {
    /// The app reviewed.
    pub app: String,
    /// The version to ask about: the app's commit, else its bundle's digest.
    pub version: String,
    /// The modules that changed.
    pub modules: Vec<String>,
}

/// What a review left running comes to say, for a caller that waits for it.
#[derive(Debug, Clone)]
pub(crate) struct ReviewAnswer {
    said: watch::Receiver<Option<ReviewOutcome>>,
    asked: Vec<String>,
    publication: Arc<ReviewPublication>,
}

/// The final successful boundary owns publication. Dropping all copies before
/// that boundary also stops paid work when a closure fails after reconciliation.
#[derive(Debug)]
struct ReviewPublication {
    installed: Mutex<Option<oneshot::Sender<()>>>,
    abort: AbortHandle,
}

impl Drop for ReviewPublication {
    fn drop(&mut self) {
        if self
            .installed
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            self.abort.abort();
        }
    }
}

impl ReviewAnswer {
    pub(super) fn publish(&self) {
        if let Some(installed) = self
            .publication
            .installed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = installed.send(());
        }
    }

    /// Never fails: a review that stopped before it answered is said as one note.
    pub(super) async fn read(mut self) -> ReviewOutcome {
        match self.said.wait_for(Option::is_some).await {
            Ok(said) => said.clone().unwrap_or_default(),
            Err(_) => ReviewOutcome {
                warnings: vec![note(
                    "Modules were not reviewed: the review stopped.".to_string(),
                    json!({ "modules": self.asked }),
                )],
                reviewed: Vec::new(),
            },
        }
    }
}

/// A review under way beside the install.
#[derive(Default)]
pub(super) struct PendingReview {
    left: Option<(ReviewAsked, ReviewAnswer)>,
}

impl PendingReview {
    /// Pass the review to the caller that decides whether the whole install succeeded.
    pub(super) fn left_running(self) -> (Vec<ReviewAsked>, Vec<ReviewAnswer>) {
        self.left.into_iter().unzip()
    }
}

/// Start reviewing the modules of `bundle` whose code or running specs differ from what `tenant` has, when the
/// install puts modules (`wasm`).
pub(super) async fn start(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    app_dir: &Path,
    bundle: &AppBundle,
    wasm: bool,
) -> PendingReview {
    if !wasm || bundle.wasm_modules.is_empty() {
        return PendingReview::default();
    }
    let specs: Vec<SpecSource> = bundle
        .specs
        .iter()
        .map(|(entity_type, source)| SpecSource::new(entity_type, source))
        .collect();
    let changed = changed_modules(state, tenant, bundle, &specs).await;
    if changed.is_empty() {
        return PendingReview::default();
    }
    let reviewer = state.module_reviewer.get().cloned();
    let mut said = Vec::new();
    let mut modules = Vec::with_capacity(changed.len());
    if reviewer.is_none() {
        said.push(note(
            "Modules were not reviewed: no Jev is set up on this server.".to_string(),
            json!({ "modules": changed }),
        ));
    } else {
        for module in &changed {
            match read_source(app_dir, module, bundle.wasm_module_configs.get(module)) {
                Ok(Some(source)) => modules.push(source),
                // A built module with no source beside it (one the platform adds to every app): nothing to read.
                Ok(None) => {
                    tracing::info!(module = %module, "module review: no source in the bundle")
                }
                Err(why) => said.push(note(
                    format!("Module {module} was not reviewed: {why}."),
                    json!({ "module": module }),
                )),
            }
        }
    }
    if modules.is_empty() && said.is_empty() {
        return PendingReview::default();
    }
    let asked: Vec<String> = modules.iter().map(|module| module.module.clone()).collect();
    let version = version_of(app_dir, tenant, app_name, bundle);
    let store = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone());
    let (tell, answer) = watch::channel(None);
    let (install_succeeded, installed) = oneshot::channel();
    let review = {
        let (tenant, app_name, version, asked) = (
            tenant.to_string(),
            app_name.to_string(),
            version.clone(),
            asked.clone(),
        );
        async move {
            let review = match reviewer {
                Some(reviewer) if !modules.is_empty() => reviewer.review(&modules, &specs).await,
                _ => BundleReview::default(),
            };
            let outcome = outcome_of(review, said, &asked);
            // Reviewing overlaps the install, but publishing must not replace
            // saved findings if any later install write or readiness check fails.
            if installed.await.is_err() {
                return Ok(());
            }
            let kept =
                json!({ "warnings": outcome.warnings, "modules_reviewed": outcome.reviewed });
            // Startup and Genesis wait for Jev, never for the store's write gate.
            // HTTP callers may already have dropped their receivers.
            let _ = tell.send(Some(outcome));
            let saved = match store {
                Some(store) => {
                    store
                        .save_module_review(&tenant, &app_name, &version, &kept.to_string())
                        .await
                }
                None => Err("No platform store is configured for module reviews.".to_string()),
            };
            saved.map_err(|error| {
                tracing::warn!(error = %error, app = %app_name, "module review was not kept");
                ReviewSaveFailure {
                    result: kept,
                    error,
                }
            })
        }
    };
    let task = state
        .module_reviews
        .start(tenant, app_name, &version, review);
    let publication = Arc::new(ReviewPublication {
        installed: Mutex::new(Some(install_succeeded)),
        abort: task.abort_handle(),
    });
    PendingReview {
        left: Some((
            ReviewAsked {
                app: app_name.to_string(),
                version,
                modules: changed,
            },
            ReviewAnswer {
                said: answer,
                asked,
                publication,
            },
        )),
    }
}

/// What names a version for its review: the commit its files say, else its bundle's digest.
fn version_of(app_dir: &Path, tenant: &str, app_name: &str, bundle: &AppBundle) -> String {
    std::fs::read_to_string(app_dir.join(COMMIT_FILE))
        .ok()
        .map(|commit| commit.trim().to_string())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| {
            reconcile::digest_app_bundle(Some(tenant), app_name, bundle).bundle_digest
        })
}

/// The modules whose code differs from the stored module, or that a changed spec runs (every module, when a changed
/// spec runs none by name).
async fn changed_modules(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
    specs: &[SpecSource],
) -> Vec<String> {
    let stored = state
        .server
        .load_wasm_module_sources(tenant)
        .await
        .unwrap_or_default();
    let tenant_id = TenantId::new(tenant);
    let changed_specs: Vec<&SpecSource> = {
        let registry = state
            .registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        specs
            .iter()
            .filter(|spec| {
                registry
                    .get_spec(&tenant_id, &spec.entity_type)
                    .is_none_or(|existing| {
                        temper_store_turso::spec_content_hash(&existing.ioa_source)
                            != temper_store_turso::spec_content_hash(&spec.source)
                    })
            })
            .collect()
    };
    bundle
        .wasm_modules
        .iter()
        .filter(|(module, bytes)| {
            let hash = temper_wasm::WasmEngine::hash_module(bytes);
            let code_changed = stored
                .get(*module)
                .is_none_or(|existing| existing.sha256_hash != hash);
            let runs_it =
                |spec: &&SpecSource| spec.module_triggers.iter().any(|(_, runs)| runs == *module);
            let named = specs.iter().any(|spec| runs_it(&spec));
            code_changed
                || changed_specs
                    .iter()
                    .any(|spec| runs_it(spec) || (!named && spec.module_triggers.is_empty()))
        })
        .map(|(module, _)| module.clone())
        .collect()
}

/// A module's source, read only from inside the app's folder: a bundle comes from anyone who may publish one, and
/// whatever file it names is sent to Jev, so a path out of the folder (`..`, an absolute path, a link) would send
/// the server's own files to an outside service. `None` when the manifest names no source and none is at
/// `wasm/<module>/src/lib.rs`.
fn read_source(
    app_dir: &Path,
    module: &str,
    config: Option<&WasmModuleManifest>,
) -> Result<Option<ModuleSource>, String> {
    let named = config.and_then(|config| config.source.clone());
    let path = named
        .clone()
        .unwrap_or_else(|| format!("wasm/{module}/src/lib.rs"));
    if named.is_none() && !app_dir.join(&path).exists() {
        return Ok(None);
    }
    let relative = Path::new(&path);
    let inside = relative
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !inside {
        return Err(format!("its source {path} is not a path inside the app"));
    }
    let (Ok(file), Ok(root)) = (
        app_dir.join(relative).canonicalize(),
        app_dir.canonicalize(),
    ) else {
        return Err(format!("its source {path} is not in the bundle"));
    };
    if !file.starts_with(&root) {
        return Err(format!("its source {path} leads out of the app"));
    }
    let size = std::fs::metadata(&file)
        .map_err(|_| format!("its source {path} could not be read"))?
        .len();
    if size > MAX_SOURCE_BYTES {
        return Err(format!(
            "its source {path} is {} KB, past the {} KB read",
            size / 1024,
            MAX_SOURCE_BYTES / 1024
        ));
    }
    let source =
        std::fs::read_to_string(&file).map_err(|_| format!("its source {path} is not text"))?;
    Ok(Some(ModuleSource {
        module: module.to_string(),
        path,
        source,
    }))
}

/// What a finished review says, after what was known before it asked (`said`).
fn outcome_of(review: BundleReview, said: Vec<InstallWarning>, asked: &[String]) -> ReviewOutcome {
    let mut outcome = ReviewOutcome {
        warnings: said,
        reviewed: Vec::new(),
    };
    let mut findings = 0;
    let mut input_tokens = 0;
    for module in &review.reviews {
        outcome.reviewed.push(module.module.clone());
        input_tokens += module.input_tokens;
        for finding in &module.findings {
            findings += 1;
            // A warning, as it is to the install: telemetry that keeps only warnings counts findings by item.
            tracing::warn!(module = %module.module, item = finding.item, kind = %finding.kind, rating = finding.rating, "module review finding");
            outcome.warnings.push(finding_note(&module.module, finding));
        }
        if !module.left_out.is_empty() {
            tracing::warn!(module = %module.module, left_out = module.left_out.len(), "module review read part of its spec");
            outcome.warnings.push(note(
                format!(
                    "Module {} was reviewed against part of its spec, the {} KB a review reads: {} left out. What it \
                     found stands; what it did not find may be in the part left out.",
                    module.module,
                    SPEC_BYTES / 1024,
                    said_left_out(&module.left_out)
                ),
                json!({ "module": module.module, "partial": true, "left_out": module.left_out }),
            ));
        }
    }
    if let Some(reason) = &review.unavailable {
        let unreviewed: Vec<&String> = asked
            .iter()
            .filter(|module| !outcome.reviewed.contains(module))
            .collect();
        tracing::warn!(reason = %reason, modules = unreviewed.len(), "module review unavailable");
        outcome.warnings.push(note(
            reason.words().to_string(),
            json!({ "modules": unreviewed, "reason": reason.to_string() }),
        ));
    }
    tracing::info!(
        reviewed = outcome.reviewed.len(),
        findings,
        input_tokens,
        "module review"
    );
    outcome
}

fn note(message: String, detail: serde_json::Value) -> InstallWarning {
    InstallWarning::module_review(message, detail)
}

fn finding_note(module: &str, finding: &Finding) -> InstallWarning {
    note(
        format!(
            "{} in module {} ({}): {} {}",
            finding.title, module, finding.location, finding.why, finding.instead
        ),
        json!({
            "module": module,
            "item": finding.item,
            "kind": finding.kind,
            "title": finding.title,
            "why": finding.why,
            "where": finding.location,
            "line": finding.line,
            "instead": finding.instead,
            "rating": finding.rating,
        }),
    )
}
