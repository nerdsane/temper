//! `POST /os-apps/{name}/install` as a caller sees it: who may push, what a
//! push may name, and what a refused or installed push leaves behind. A push
//! is the installer's second source, so these also hold the shared install:
//! checks before anything changes, rollback to the version before, records,
//! boot restore and adoption.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{
    InstalledAppRecord, PlatformStore, SimPlatformFaultConfig, SimPlatformStore,
};
use tower::ServiceExt;

use crate::genesis_install::{adopt_catalog_app, restore_genesis_registry_cache_roots};
use crate::os_apps::{app_files_seam, tenant_os_app};
use crate::state::PlatformState;
use crate::tenant_api::tenant_api_router;

#[path = "pushed_test/module_review_test.rs"]
mod module_review;

#[path = "pushed_test/review_regressions_test.rs"]
mod review_regressions;

#[path = "pushed_test/source_and_cache_test.rs"]
mod source_and_cache;

const APP: &str = "evolution";
const SPEC: &str = "sentinel_monitor.ioa.toml";

/// A server whose files live in `data_dir` and whose install records live in
/// `ps`, where `tenant` lets any of its agents install apps and read its
/// installs.
fn server_on(tenant: &str, data_dir: &Path, ps: &Arc<SimPlatformStore>) -> PlatformState {
    let mut state = PlatformState::new(None);
    state.server.data_dir = data_dir.to_path_buf();
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_sim(
            temper_store_sim::SimEventStore::no_faults(0),
            Some(ps.clone()),
        ));
    state
        .server
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal, action == Action::"install_app", resource is App);
permit(principal, action == Action::"read_app_installs", resource is AppInstall);"#,
        )
        .expect("install policy should parse");
    state
}

struct Server {
    state: PlatformState,
    ps: Arc<SimPlatformStore>,
    data_dir: tempfile::TempDir,
}

fn server(tenant: &str) -> Server {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(SimPlatformStore::new(0, SimPlatformFaultConfig::none()));
    let state = server_on(tenant, data_dir.path(), &ps);
    Server {
        state,
        ps,
        data_dir,
    }
}

fn unique_tenant(label: &str) -> String {
    format!("push-{label}-{}", uuid::Uuid::new_v4().simple())
}

/// Every file of the workspace's `evolution` app, as a push sends it.
fn app_files() -> BTreeMap<String, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../os-apps")
        .join(APP);
    let mut files = BTreeMap::new();
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read app dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let relative = path.strip_prefix(&root).expect("inside app");
            let text = std::fs::read_to_string(&path).expect("app files are text");
            files.insert(relative.to_string_lossy().into_owned(), text);
        }
    }
    files
}

fn push(tenant: &str, body: serde_json::Value) -> Request<Body> {
    let mut request = Request::post(format!("/os-apps/{APP}/install"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("agent-1", "operator", None),
        ));
    request
}

async fn send(state: &PlatformState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = tenant_api_router()
        .with_state(state.clone())
        .oneshot(request)
        .await
        .expect("request should run");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// `GET /genesis/apps/installs` as `tenant`'s operator: the status and the
/// body as the bytes a script reads.
async fn installs(state: &PlatformState, tenant: &str) -> (StatusCode, String) {
    let mut request = Request::get("/genesis/apps/installs")
        .body(Body::empty())
        .expect("request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("agent-1", "operator", None),
        ));
    let response = tenant_api_router()
        .with_state(state.clone())
        .oneshot(request)
        .await
        .expect("request should run");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .expect("body");
    (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

/// What the listing says the tenant serves for the app.
#[derive(Debug, PartialEq)]
struct Listed {
    source_kind: String,
    app_ref: String,
    version_hash: String,
    status: String,
    dir: String,
}

/// The app's install as the listing gives it. Read JSON as JSON so every
/// valid filesystem path survives escaping. None when the app is absent.
fn listed(body: &str) -> Option<Listed> {
    let value: serde_json::Value = serde_json::from_str(body).expect("json");
    let entry = value["value"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|entry| entry["app_name"] == APP)?;
    assert!(
        entry
            .as_object()
            .expect("an object")
            .values()
            .all(|field| field.is_string() || field.is_null()),
        "flat: {entry}"
    );
    let field = |name: &str| entry[name].as_str().unwrap_or_default().to_string();
    Some(Listed {
        source_kind: field("source_kind"),
        app_ref: field("app_ref"),
        version_hash: field("version_hash"),
        status: field("status"),
        dir: field("dir"),
    })
}

/// The listing's view of the app for `tenant`, which must answer 200.
async fn listed_for(state: &PlatformState, tenant: &str) -> Option<Listed> {
    let (status, body) = installs(state, tenant).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    listed(&body)
}

async fn record(ps: &SimPlatformStore, tenant: &str) -> Option<InstalledAppRecord> {
    ps.get_installed_app(tenant, APP)
        .await
        .expect("read record")
}

/// The text of the spec `tenant` resolves for the app now.
fn resolved_spec(tenant: &str) -> String {
    let bundle = tenant_os_app(tenant, APP).expect("the tenant resolves the app");
    bundle
        .specs
        .into_iter()
        .find(|(entity_type, _)| entity_type == "SentinelMonitor")
        .map(|(_, source)| source)
        .expect("the app has its sentinel spec")
}

/// The pushed versions on disk, by directory name.
fn pushed_versions(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("genesis-app-cache")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("pushed-"))
        .collect();
    names.sort();
    names
}

fn with_comment(comment: &str) -> BTreeMap<String, String> {
    let mut files = app_files();
    files
        .get_mut(SPEC)
        .expect("sentinel spec")
        .push_str(&format!("\n# {comment}\n"));
    files
}

#[tokio::test]
async fn a_push_needs_a_credential_and_a_permit_in_its_own_tenant() {
    let tenant = unique_tenant("auth");
    let server = server(&tenant);
    let body = serde_json::json!({ "files": app_files() });

    let anonymous = Request::post(format!("/os-apps/{APP}/install"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should build");
    assert_eq!(
        send(&server.state, anonymous).await.0,
        StatusCode::UNAUTHORIZED
    );

    // `tenant` permits installs; a credential of another tenant gets nothing
    // from that, and its push lands nowhere.
    let other = unique_tenant("other");
    assert_eq!(
        send(&server.state, push(&other, body)).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(pushed_versions(server.data_dir.path()).is_empty());

    // Reading the installs takes `read_app_installs`; with it, nothing is
    // installed yet.
    assert_eq!(
        installs(&server.state, &other).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(listed_for(&server.state, &tenant).await, None);
}

#[tokio::test]
async fn a_push_naming_a_path_outside_the_app_is_refused() {
    let tenant = unique_tenant("paths");
    let server = server(&tenant);
    for bad in ["../escape.txt", "/etc/escape", "specs//x.toml", "./APP.md"] {
        let mut files = app_files();
        files.insert(bad.to_string(), "x".to_string());
        let (status, body) = send(
            &server.state,
            push(&tenant, serde_json::json!({ "files": files })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert!(
            body["error"].as_str().is_some_and(|e| e.contains(bad)),
            "{body}"
        );
    }
    assert!(pushed_versions(server.data_dir.path()).is_empty());
    assert!(record(&server.ps, &tenant).await.is_none());
}

/// An install checks at main's level: each spec parses, and the installer's
/// joint proofs warn. A spec whose own invariant an unguarded move breaks
/// (an L1 counterexample, the shape of paw-fs's `UsageBelowQuota`) installs
/// with 200, as it does on main; the per-spec L0-L3 check is the caller's,
/// before it sends.
#[tokio::test]
async fn a_spec_with_a_model_check_counterexample_installs_at_mains_level() {
    let tenant = unique_tenant("counterexample");
    let server = server(&tenant);
    // Entering Triggering without setting has_alerts breaks the spec's own
    // invariant `Triggering => has_alerts`.
    let mut files = with_comment("breaks its own invariant");
    let spec = files.get_mut(SPEC).expect("sentinel spec");
    *spec = spec.replace(
        r#"effect = ["has_alerts = true", "alert_count += 1"]"#,
        r#"effect = ["alert_count += 1"]"#,
    );
    assert!(
        !spec.contains("has_alerts = true"),
        "the move no longer sets it"
    );
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.get("checks").is_none(),
        "no per-spec check runs: {body}"
    );
    let digest = body["digest"].as_str().expect("digest");
    let installed = record(&server.ps, &tenant).await.expect("recorded");
    assert_eq!(installed.app_ref, format!("pushed/{APP}@{digest}"));
    assert!(resolved_spec(&tenant).contains("# breaks its own invariant"));
}

/// A version whose checks pass but which the installer refuses goes back to
/// the version installed before it, through the same rollback a registry
/// install takes.
#[tokio::test]
async fn a_push_the_installer_refuses_rolls_back_to_the_version_before() {
    let tenant = unique_tenant("install-refused");
    let server = server(&tenant);
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = record(&server.ps, &tenant).await.expect("recorded");
    let spec_before = resolved_spec(&tenant);
    let listed_before = listed_for(&server.state, &tenant).await.expect("listed");

    // The specs parse; the installer cannot parse the policy.
    let mut files = with_comment("refused by the installer");
    files.insert(
        "policies/evolution.cedar".to_string(),
        "permit(principal, action, resource) when { this is not cedar };".to_string(),
    );
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("Rolled back")),
        "{body}"
    );

    let after = record(&server.ps, &tenant).await.expect("still recorded");
    assert_eq!(after.app_ref, installed.app_ref);
    assert_eq!(after.source_kind, "pushed");
    assert_eq!(resolved_spec(&tenant), spec_before);

    // The listing names the version before, served from its folder, still on
    // disk.
    let listed_after = listed_for(&server.state, &tenant)
        .await
        .expect("still listed");
    assert_eq!(listed_after, listed_before);
    assert!(Path::new(&listed_after.dir).join(SPEC).is_file());
}

#[tokio::test]
async fn a_good_push_is_installed_recorded_and_restored_at_boot() {
    let tenant = unique_tenant("installed");
    let server = server(&tenant);
    let files = with_comment("pushed to its own server");
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let digest = body["digest"].as_str().expect("digest").to_string();
    assert_eq!(body["installed"], APP);
    assert!(resolved_spec(&tenant).contains("# pushed to its own server"));
    let registered = server
        .state
        .registry
        .read()
        .expect("registry")
        .get_spec(&TenantId::new(&tenant), "SentinelMonitor")
        .map(|spec| spec.ioa_source.clone())
        .expect("the pushed spec is registered");
    assert!(registered.contains("# pushed to its own server"));
    let recorded = record(&server.ps, &tenant).await.expect("recorded");
    assert_eq!(recorded.source_kind, "pushed");
    assert_eq!(recorded.app_ref, format!("pushed/{APP}@{digest}"));

    // The listing names the pushed version and the folder it is served from,
    // its `dir` read from JSON.
    let served = listed_for(&server.state, &tenant).await.expect("listed");
    assert_eq!(
        served,
        Listed {
            source_kind: "pushed".to_string(),
            app_ref: format!("pushed/{APP}@{digest}"),
            version_hash: digest.clone(),
            status: "installed".to_string(),
            dir: served.dir.clone(),
        }
    );
    assert!(Path::new(&served.dir).is_absolute(), "{served:?}");
    assert!(
        std::fs::read_to_string(Path::new(&served.dir).join(SPEC))
            .expect("the served spec")
            .contains("# pushed to its own server")
    );

    // A fresh server over the same files and records finds the version again,
    // and lists it served from the same folder.
    let restarted = server_on(&tenant, server.data_dir.path(), &server.ps);
    assert!(restore_genesis_registry_cache_roots(&restarted).await >= 1);
    assert!(resolved_spec(&tenant).contains("# pushed to its own server"));
    assert_eq!(listed_for(&restarted, &tenant).await, Some(served));

    // The same files again keep the unchanged spec by its hash.
    let mut kept = serde_json::to_value(&files).expect("files");
    let hash = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(files[SPEC].as_bytes())
    );
    kept[SPEC] = serde_json::json!({ "keep": hash });
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": kept })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["digest"], digest.as_str());
}

/// Each version holds a whole copy of the app: a push keeps only the version
/// it installs and the one before it.
#[tokio::test]
async fn a_push_keeps_only_its_version_and_the_one_before() {
    let tenant = unique_tenant("prune");
    let server = server(&tenant);
    for comment in ["first", "second", "third"] {
        let (status, body) = send(
            &server.state,
            push(
                &tenant,
                serde_json::json!({ "files": with_comment(comment) }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{comment}: {body}");
    }
    assert_eq!(pushed_versions(server.data_dir.path()).len(), 2);
    assert!(resolved_spec(&tenant).contains("# third"));
}

/// A push waits while another Temper process on this host holds the install
/// turn, and its answer says how long.
#[tokio::test]
async fn a_push_waits_for_the_host_install_turn() {
    let tenant = unique_tenant("turn");
    let server = server(&tenant);
    let path = std::env::temp_dir().join("temper-installs.lock");
    let held = tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .expect("lock file");
        file.lock().expect("take the turn");
        file
    })
    .await
    .expect("lock thread");
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(held);
    });

    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    release.await.expect("the turn is given back");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["waited_ms"].as_u64().is_some_and(|ms| ms >= 400),
        "the push ran while the turn was held: {body}"
    );
}

/// Katagami's control plane reads these lines to know an app is served or
/// waiting; their words are fixed.
#[test]
fn the_install_and_waiting_lines_read_as_the_control_plane_reads_them() {
    let result = crate::os_apps::InstallResult {
        added: vec!["Order".to_string()],
        updated: vec!["Invoice".to_string()],
        skipped: vec!["Customer".to_string()],
        ..Default::default()
    };
    assert_eq!(
        crate::os_apps::installed_line("shop", "default", &result),
        "  App 'shop' installed for 'default': Order, Invoice, Customer"
    );
    assert_eq!(
        crate::os_apps::waiting_line("shop", "default"),
        "  App 'shop' waits for its first install for 'default'"
    );
    assert_eq!(
        crate::os_apps::stored_line("shop", "default"),
        "  App 'shop' serves its stored specs for 'default'; no files to install"
    );
}

/// An app served from the boot catalog is adopted as its first pushed
/// version; pushing the same files again is that version, and a changed push
/// replaces it.
#[tokio::test]
async fn an_adopted_catalog_version_is_the_version_a_push_builds_on() {
    let tenant = unique_tenant("adopted");
    let server = server(&tenant);
    let digest = adopt_catalog_app(&server.state, &tenant, APP)
        .await
        .expect("adopt")
        .expect("the catalog holds the app");
    let adopted = record(&server.ps, &tenant).await.expect("recorded");
    assert_eq!(adopted.source_kind, "pushed");
    assert_eq!(adopted.app_ref, format!("pushed/{APP}@{digest}"));
    assert_eq!(
        adopt_catalog_app(&server.state, &tenant, APP)
            .await
            .expect("adopt again"),
        None,
        "an app with a pushed version is not adopted again"
    );

    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["digest"], digest.as_str());

    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({ "files": with_comment("pushed after adoption") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(body["digest"], digest.as_str());
    assert!(resolved_spec(&tenant).contains("# pushed after adoption"));
}

/// The app's files with one spec in the old syntax this kernel refuses: a
/// bool written as a string, as TemperPaw's paw-patrol has it.
/// The sentinel spec `tenant` has registered with the server.
fn registered_spec(state: &PlatformState, tenant: &str) -> String {
    state
        .registry
        .read()
        .expect("registry")
        .get_spec(&TenantId::new(tenant), "SentinelMonitor")
        .map(|spec| spec.ioa_source.clone())
        .expect("the sentinel spec is registered")
}

/// The installer reads the files a push names, never a listing of the folder
/// the push just wrote. Here every folder under the server's data folder
/// lists empty, as a folder just renamed into place can on some drives, and
/// the push still parses every spec, installs, and serves them.
#[tokio::test]
async fn a_push_installs_the_files_it_named_when_its_folder_lists_empty() {
    let tenant = unique_tenant("lists-empty");
    let server = server(&tenant);
    app_files_seam::list_empty_under(server.data_dir.path());
    let files = with_comment("named, not listed");
    let specs = files
        .keys()
        .filter(|path| path.ends_with(".ioa.toml"))
        .count();
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = tenant_os_app(&tenant, APP).expect("the tenant resolves the app");
    assert_eq!(
        installed.specs.len(),
        specs,
        "every pushed spec is installed"
    );
    assert!(resolved_spec(&tenant).contains("# named, not listed"));
    assert!(registered_spec(&server.state, &tenant).contains("# named, not listed"));
    let digest = body["digest"].as_str().expect("digest");
    let recorded = record(&server.ps, &tenant).await.expect("recorded");
    assert_eq!(recorded.app_ref, format!("pushed/{APP}@{digest}"));
}

/// Boot restores a pushed version from the list of files its install kept,
/// never from a listing: here every folder under the data folder lists empty
/// at boot, and the restored app still serves its pushed specs. The catalog
/// outlives the restart in one test process, so the restore count is what
/// tells a list-driven restore from no restore at all.
#[tokio::test]
async fn a_boot_whose_folders_list_empty_restores_the_pushed_specs() {
    let tenant = unique_tenant("boot-lists-empty");
    let server = server(&tenant);
    let files = with_comment("restored from its list");
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let served = listed_for(&server.state, &tenant).await.expect("listed");

    app_files_seam::list_empty_under(server.data_dir.path());
    let restarted = server_on(&tenant, server.data_dir.path(), &server.ps);
    assert_eq!(restore_genesis_registry_cache_roots(&restarted).await, 1);
    let restored = tenant_os_app(&tenant, APP).expect("the tenant resolves the app");
    let pushed_specs = files.keys().filter(|path| path.ends_with(".ioa.toml"));
    assert_eq!(
        restored.specs.len(),
        pushed_specs.count(),
        "every pushed spec is restored"
    );
    assert!(resolved_spec(&tenant).contains("# restored from its list"));
    assert_eq!(listed_for(&restarted, &tenant).await, Some(served));
}

/// A rollback restores the version before from the list of files its install
/// kept, never from a listing: here every folder under the data folder lists
/// empty when the installer refuses the new version, and the version before
/// still serves its specs, recorded as it was.
#[tokio::test]
async fn a_rollback_whose_folders_list_empty_restores_the_prior_specs() {
    let tenant = unique_tenant("rollback-lists-empty");
    let server = server(&tenant);
    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({ "files": with_comment("the version before") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = record(&server.ps, &tenant).await.expect("recorded");
    let listed_before = listed_for(&server.state, &tenant).await.expect("listed");

    app_files_seam::list_empty_under(server.data_dir.path());
    // The specs parse; the installer cannot parse the policy.
    let mut files = with_comment("refused by the installer");
    files.insert(
        "policies/evolution.cedar".to_string(),
        "permit(principal, action, resource) when { this is not cedar };".to_string(),
    );
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("Rolled back")),
        "{body}"
    );

    let after = record(&server.ps, &tenant).await.expect("still recorded");
    assert_eq!(after.app_ref, installed.app_ref);
    assert_eq!(after.bundle_digest, installed.bundle_digest);
    assert!(resolved_spec(&tenant).contains("# the version before"));
    assert!(registered_spec(&server.state, &tenant).contains("# the version before"));
    let restored = tenant_os_app(&tenant, APP).expect("the tenant resolves the app");
    let specs = app_files()
        .keys()
        .filter(|path| path.ends_with(".ioa.toml"))
        .count();
    assert_eq!(restored.specs.len(), specs, "every prior spec is restored");
    assert_eq!(
        listed_for(&server.state, &tenant).await,
        Some(listed_before)
    );
}

/// A re-push whose folder lists empty serves its own version. Were the new
/// folder registered by listing it, it would hold no app, the version before
/// would stay what the tenant resolves, and that version would be installed
/// under the new digest.
#[tokio::test]
async fn a_re_push_whose_folder_lists_empty_serves_its_own_version() {
    let tenant = unique_tenant("re-push-lists-empty");
    let server = server(&tenant);
    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({ "files": with_comment("first") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    app_files_seam::list_empty_under(server.data_dir.path());
    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({ "files": with_comment("second") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let digest = body["digest"].as_str().expect("digest").to_string();

    let resolved = resolved_spec(&tenant);
    assert!(resolved.contains("# second") && !resolved.contains("# first"));
    let registered = registered_spec(&server.state, &tenant);
    assert!(registered.contains("# second") && !registered.contains("# first"));
    let recorded = record(&server.ps, &tenant).await.expect("recorded");
    assert_eq!(recorded.app_ref, format!("pushed/{APP}@{digest}"));
    let served = listed_for(&server.state, &tenant).await.expect("listed");
    assert_eq!(served.version_hash, digest);
    assert!(
        std::fs::read_to_string(Path::new(&served.dir).join(SPEC))
            .expect("the served spec")
            .contains("# second")
    );
}

/// A file the push names that cannot be read refuses the push with the
/// file's path, and the version before keeps serving.
#[tokio::test]
async fn a_named_file_that_cannot_be_read_refuses_the_push_naming_it() {
    let tenant = unique_tenant("unreadable");
    let server = server(&tenant);
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = record(&server.ps, &tenant).await.expect("recorded");
    let spec_before = resolved_spec(&tenant);
    let listed_before = listed_for(&server.state, &tenant).await.expect("listed");

    // Only the new version has this policy, so the version before still reads.
    app_files_seam::unreadable_under(server.data_dir.path(), "policies/unreadable.cedar");
    let mut files = with_comment("has a file that cannot be read");
    files.insert(
        "policies/unreadable.cedar".to_string(),
        "permit(principal, action, resource is SentinelMonitor);".to_string(),
    );
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": files })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("policies/unreadable.cedar could not be read"),
        "{body}"
    );

    let after = record(&server.ps, &tenant).await.expect("still recorded");
    assert_eq!(after.app_ref, installed.app_ref);
    assert_eq!(after.bundle_digest, installed.bundle_digest);
    assert_eq!(resolved_spec(&tenant), spec_before);
    assert_eq!(
        listed_for(&server.state, &tenant).await,
        Some(listed_before)
    );
}

fn old_syntax_files() -> BTreeMap<String, String> {
    let mut files = app_files();
    let spec = files.get_mut(SPEC).expect("sentinel spec");
    assert!(
        spec.contains("initial = false"),
        "the spec has a bool to spell the old way"
    );
    *spec = spec.replacen("initial = false", r#"initial = "false""#, 1);
    files
}

/// A spec the checker cannot parse is refused with what to fix, not a panic
/// or a 500, and nothing is installed.
#[tokio::test]
async fn a_push_with_an_old_syntax_spec_is_refused_with_its_problem() {
    let tenant = unique_tenant("old-syntax");
    let server = server(&tenant);
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({ "files": old_syntax_files() })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(SPEC)
            && error.contains("does not parse")
            && error.contains("migrate-predicates"),
        "{body}"
    );
    assert!(record(&server.ps, &tenant).await.is_none());
    assert!(pushed_versions(server.data_dir.path()).is_empty());
    assert!(listed_for(&server.state, &tenant).await.is_none());
}

/// A registry install takes the install a push takes, from the files its
/// bundle named. An old-syntax spec is refused with 422 naming the spec and
/// why, and installs nothing. A good version installs and serves its specs
/// even when every folder it wrote lists empty.
///
/// One test, because the registry is a process setting: two tests setting it
/// at once would each fetch from the other's registry.
#[tokio::test]
async fn a_registry_install_reads_the_files_its_bundle_named() {
    use base64::Engine as _;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const OLD: &str = "0123456789abcdef0123456789abcdef01234567";
    const GOOD: &str = "89abcdef0123456789abcdef0123456789abcdef";
    let registry = MockServer::start().await;
    // The parse refusal wins over the route's "not found" classification.
    let mut old = old_syntax_files();
    let unparsed = old.remove(SPEC).expect("old-syntax spec");
    old.insert(format!("not found {SPEC}"), unparsed);
    // Registry installs accept the same single-spec counterexample as pushes.
    let mut counterexample = with_comment("from the registry");
    let spec = counterexample.get_mut(SPEC).expect("sentinel spec");
    *spec = spec.replace(
        r#"effect = ["has_alerts = true", "alert_count += 1"]"#,
        r#"effect = ["alert_count += 1"]"#,
    );
    assert!(!spec.contains("has_alerts = true"));
    for (hash, files) in [(OLD, old), (GOOD, counterexample)] {
        let files: Vec<serde_json::Value> = files
            .into_iter()
            .map(|(path, text)| {
                serde_json::json!({
                    "path": path,
                    "content_base64": base64::engine::general_purpose::STANDARD.encode(text),
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/genesis/apps/acme/{APP}/versions/{hash}/bundle"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "app_ref": format!("acme/{APP}@{hash}"),
                "registry_tenant": "default",
                "apps": [{ "owner": "acme", "name": APP, "version_hash": hash, "files": files }],
            })))
            .mount(&registry)
            .await;
    }
    // SAFETY: the only test in this crate that sets the registry; the others
    // that read it pass with any value.
    unsafe { std::env::set_var("TEMPER_GENESIS_REGISTRY_URL", registry.uri()) };

    let tenant = unique_tenant("registry");
    let server = server(&tenant);
    let install = |hash: &str| {
        let mut request = Request::post("/genesis/apps/install")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "tenant": tenant,
                    "app_ref": format!("acme/{APP}@{hash}"),
                    "follow_policy": "pinned",
                })
                .to_string(),
            ))
            .expect("request should build");
        request
            .extensions_mut()
            .insert(AuthenticatedRequestContext::new(
                TenantId::new(&tenant),
                SecurityContext::from_resolved_identity("agent-1", "operator", None),
            ));
        request
    };

    let (status, body) = send(&server.state, install(OLD)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(SPEC)
            && error.contains("does not parse")
            && error.contains("migrate-predicates"),
        "{body}"
    );
    assert!(record(&server.ps, &tenant).await.is_none());
    assert!(listed_for(&server.state, &tenant).await.is_none());

    app_files_seam::list_empty_under(server.data_dir.path());
    let (status, body) = send(&server.state, install(GOOD)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(resolved_spec(&tenant).contains("# from the registry"));
    assert!(registered_spec(&server.state, &tenant).contains("# from the registry"));
    let recorded = record(&server.ps, &tenant).await.expect("recorded");
    assert_eq!(recorded.source_kind, "genesis");
    assert_eq!(recorded.app_ref, format!("acme/{APP}@{GOOD}"));
}
