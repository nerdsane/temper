//! Preserve recoverable dependency sources and bound refused pushed caches.

use super::review_regressions::dependency_csdl;
use super::*;
use crate::genesis_install::{GenesisRegistryInstallRequest, install_genesis_app_from_registry};
use crate::os_apps::{AppFileSource, add_tenant_written_apps, tenant_os_app_dir};

const DEP: &str = "push-source";
const POLICY: &str = "permit(principal, action == Action::\"dependency_action\", resource);";

fn dependency_files() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.toml".into(), format!("name = {DEP:?}\n")),
        ("APP.md".into(), "# Recorded dependency".into()),
        ("model.csdl.xml".into(), dependency_csdl()),
        (
            SPEC.into(),
            app_files()[SPEC].replace("SentinelMonitor", "PushDependency"),
        ),
        ("policies/dependency.cedar".into(), POLICY.into()),
    ])
}

async fn install_dependency(server: &Server, tenant: &str, registry_source: bool) {
    if !registry_source {
        let mut request = push(tenant, serde_json::json!({"files": dependency_files()}));
        *request.uri_mut() = format!("/os-apps/{DEP}/install").parse().unwrap();
        let (status, body) = send(&server.state, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        return;
    }
    use base64::Engine as _;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
    let registry = MockServer::start().await;
    let files: Vec<_> = dependency_files().into_iter().map(|(path, text)| {
        serde_json::json!({"path": path, "content_base64": base64::engine::general_purpose::STANDARD.encode(text)})
    }).collect();
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/genesis/apps/acme/{DEP}/versions/{HASH}/bundle"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "app_ref": format!("acme/{DEP}@{HASH}"), "registry_tenant": "default",
            "apps": [{"owner": "acme", "name": DEP, "version_hash": HASH, "files": files}]
        })))
        .expect(1)
        .mount(&registry)
        .await;
    // SAFETY: these HTTP regressions run with nextest's process isolation,
    // as required by the existing registry-install tests.
    unsafe { std::env::set_var("TEMPER_GENESIS_REGISTRY_URL", registry.uri()) };
    install_genesis_app_from_registry(
        &server.state,
        GenesisRegistryInstallRequest {
            tenant: tenant.into(),
            app_ref: format!("acme/{DEP}@{HASH}"),
            registry_url: registry.uri(),
            registry_tenant: "default".into(),
            follow_policy: "pinned".into(),
        },
    )
    .await
    .unwrap();
    // It must recover offline, without fetching this source again.
}

async fn repair_preserves_source(registry_source: bool) {
    let tenant = unique_tenant("source-repair");
    let server = server(&tenant);
    install_dependency(&server, &tenant, registry_source).await;
    let prior = server
        .ps
        .get_installed_app(&tenant, DEP)
        .await
        .unwrap()
        .unwrap();
    let prior_dir = tenant_os_app_dir(&tenant, DEP).unwrap();
    // Drop only runtime policy state. Specs and bundle digests still match;
    // the push must reconcile this dependency instead of skipping it.
    server
        .state
        .server
        .tenant_policies
        .write()
        .unwrap()
        .remove(&tenant);
    server.state.server.authz.reload_tenant_policies(&tenant,
        "permit(principal, action == Action::\"install_app\", resource is App);\npermit(principal, action == Action::\"read_app_installs\", resource is AppInstall);"
    ).unwrap();
    assert!(
        !server
            .state
            .server
            .authz
            .get_tenant_policy_text(&tenant)
            .unwrap()
            .contains(POLICY)
    );
    let mut root = app_files();
    root.get_mut("app.toml")
        .unwrap()
        .push_str(&format!("\ndependencies = [{DEP:?}]\n"));
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({"files": root})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        server
            .state
            .server
            .authz
            .get_tenant_policy_text(&tenant)
            .unwrap()
            .contains(POLICY)
    );
    let repaired = server
        .ps
        .get_installed_app(&tenant, DEP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        repaired, prior,
        "repair must preserve all recovery coordinates"
    );

    // Replace the catalog entry, so restoring the recorded version really
    // has to select its cache rather than inheriting the test's catalog.
    let other = tempfile::tempdir().unwrap();
    let dir = other.path().join(DEP);
    std::fs::create_dir_all(&dir).unwrap();
    let mut changed = dependency_files();
    changed
        .get_mut(SPEC)
        .unwrap()
        .push_str("\n# wrong catalog version\n");
    for (path, text) in &changed {
        let dest = dir.join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, text).unwrap();
    }
    add_tenant_written_apps(
        &tenant,
        other.path().to_path_buf(),
        vec![(DEP.into(), AppFileSource::written(dir, changed.into_keys()))],
    );
    let restarted = server_on(&tenant, server.data_dir.path(), &server.ps);
    app_files_seam::list_empty_under(server.data_dir.path());
    assert_eq!(restore_genesis_registry_cache_roots(&restarted).await, 2);
    crate::os_apps::restore_os_app(&restarted, &tenant, DEP)
        .await
        .unwrap();
    assert_eq!(tenant_os_app_dir(&tenant, DEP).unwrap(), prior_dir);
    assert_eq!(
        server
            .ps
            .get_installed_app(&tenant, DEP)
            .await
            .unwrap()
            .unwrap(),
        prior
    );
    let expected = dependency_files()[SPEC].clone();
    let bundle = tenant_os_app(&tenant, DEP).unwrap();
    assert_eq!(
        bundle
            .specs
            .iter()
            .find(|(name, _)| name == "PushDependency")
            .map(|(_, spec)| spec),
        Some(&expected)
    );
    assert_eq!(
        restarted
            .registry
            .read()
            .unwrap()
            .get_spec(&TenantId::new(&tenant), "PushDependency")
            .unwrap()
            .ioa_source,
        expected
    );
}

#[tokio::test]
async fn repaired_pushed_dependency_keeps_its_source_through_restart() {
    repair_preserves_source(false).await;
}

#[tokio::test]
async fn repaired_registry_dependency_keeps_its_source_through_restart() {
    repair_preserves_source(true).await;
}

#[tokio::test]
async fn refused_missing_dependencies_do_not_accumulate_versions() {
    let tenant = unique_tenant("missing-dep-cache");
    let server = server(&tenant);
    for attempt in 0..3 {
        let mut files = with_comment(&format!("refused attempt {attempt}"));
        files
            .get_mut("app.toml")
            .unwrap()
            .push_str("\ndependencies = [\"push-missing-dependency\"]\n");
        let (status, body) = send(
            &server.state,
            push(&tenant, serde_json::json!({"files": files})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("push-missing-dependency"),
            "{body}"
        );
        assert!(record(&server.ps, &tenant).await.is_none());
        assert!(
            pushed_versions(server.data_dir.path()).is_empty(),
            "refused attempt {attempt} left a complete cache"
        );
    }
}

#[tokio::test]
async fn successful_rollback_discards_only_the_refused_version() {
    let tenant = unique_tenant("rollback-cache");
    let server = server(&tenant);
    for comment in ["first", "second"] {
        let (status, body) = send(
            &server.state,
            push(&tenant, serde_json::json!({"files": with_comment(comment)})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let before = pushed_versions(server.data_dir.path());
    let listed_before = listed_for(&server.state, &tenant).await;
    for attempt in 0..3 {
        let mut files = with_comment(&format!("refused policy {attempt}"));
        files.insert(
            "policies/evolution.cedar".into(),
            "this is not cedar".into(),
        );
        let (status, body) = send(
            &server.state,
            push(&tenant, serde_json::json!({"files": files})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("Rolled back"),
            "{body}"
        );
        assert_eq!(pushed_versions(server.data_dir.path()), before);
        assert_eq!(listed_for(&server.state, &tenant).await, listed_before);
    }
    let restarted = server_on(&tenant, server.data_dir.path(), &server.ps);
    assert_eq!(restore_genesis_registry_cache_roots(&restarted).await, 1);
    assert!(resolved_spec(&tenant).contains("# second"));
}

async fn repeated_push_repairs_cache(damage: &str) {
    let tenant = unique_tenant("cached-bytes");
    let server = server(&tenant);
    let files = with_comment("requested bytes");
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({"files": files})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let digest = body["digest"].clone();
    let served = listed_for(&server.state, &tenant).await.unwrap();
    let dir = Path::new(&served.dir);
    let list = dir.parent().unwrap().join(".written.json");
    match damage {
        "bytes" => std::fs::write(
            dir.join(SPEC),
            with_comment("damaged bytes")[SPEC].as_bytes(),
        )
        .unwrap(),
        "missing-list" => std::fs::remove_file(&list).unwrap(),
        "broken-list" => std::fs::write(&list, "not json").unwrap(),
        "short-list" => {
            std::fs::write(&list, format!("{{{APP:?}:[\"app.toml\",\"APP.md\"]}}")).unwrap()
        }
        _ => unreachable!(),
    }
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({"files": files})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{damage}: {body}");
    assert_eq!(body["digest"], digest);
    assert_eq!(
        std::fs::read_to_string(dir.join(SPEC)).unwrap(),
        files[SPEC]
    );
    assert_eq!(resolved_spec(&tenant), files[SPEC]);
    app_files_seam::list_empty_under(server.data_dir.path());
    let restarted = server_on(&tenant, server.data_dir.path(), &server.ps);
    assert_eq!(
        restore_genesis_registry_cache_roots(&restarted).await,
        1,
        "{damage}"
    );
}

#[tokio::test]
async fn repeated_push_repairs_damaged_bytes() {
    repeated_push_repairs_cache("bytes").await;
}

#[tokio::test]
async fn repeated_push_repairs_a_missing_list() {
    repeated_push_repairs_cache("missing-list").await;
}

#[tokio::test]
async fn repeated_push_repairs_a_broken_list() {
    repeated_push_repairs_cache("broken-list").await;
}

#[tokio::test]
async fn repeated_push_repairs_a_short_list() {
    repeated_push_repairs_cache("short-list").await;
}

#[tokio::test]
async fn failed_rollback_keeps_the_cache_that_may_still_be_selected() {
    let tenant = unique_tenant("failed-rollback-cache");
    let server = server(&tenant);
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({"files": app_files()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let prior = listed_for(&server.state, &tenant).await.unwrap();
    let prior_root = Path::new(&prior.dir).parent().unwrap();
    std::fs::write(prior_root.join(".written.json"), "broken prior list").unwrap();
    let mut files = with_comment("rollback cannot recover its prior cache");
    files.insert(
        "policies/evolution.cedar".into(),
        "this is not cedar".into(),
    );
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({"files": files})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("also failed"),
        "{body}"
    );
    assert_eq!(pushed_versions(server.data_dir.path()).len(), 2);
    let selected = tenant_os_app_dir(&tenant, APP).unwrap();
    assert!(selected.join(SPEC).is_file());
}

#[tokio::test]
async fn a_failed_first_install_keeps_its_registered_cache() {
    let tenant = unique_tenant("first-install-cache");
    let server = server(&tenant);
    let mut files = app_files();
    files.insert(
        "policies/evolution.cedar".into(),
        "this is not cedar".into(),
    );
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({"files": files})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(pushed_versions(server.data_dir.path()).len(), 1);
    assert!(
        tenant_os_app_dir(&tenant, APP)
            .unwrap()
            .join(SPEC)
            .is_file()
    );
}

#[tokio::test]
async fn changed_catalog_dependency_does_not_keep_an_obsolete_source() {
    let tenant = unique_tenant("changed-dependency");
    let server = server(&tenant);
    install_dependency(&server, &tenant, false).await;
    let prior = server
        .ps
        .get_installed_app(&tenant, DEP)
        .await
        .unwrap()
        .unwrap();
    let other = tempfile::tempdir().unwrap();
    let dir = other.path().join(DEP);
    let mut changed = dependency_files();
    changed
        .get_mut(SPEC)
        .unwrap()
        .push_str("\n# new local version\n");
    for (path, text) in &changed {
        let dest = dir.join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, text).unwrap();
    }
    add_tenant_written_apps(
        &tenant,
        other.path().to_path_buf(),
        vec![(DEP.into(), AppFileSource::written(dir, changed.into_keys()))],
    );
    let mut root = app_files();
    root.get_mut("app.toml")
        .unwrap()
        .push_str(&format!("\ndependencies = [{DEP:?}]\n"));
    let (status, body) = send(
        &server.state,
        push(&tenant, serde_json::json!({"files": root})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = server
        .ps
        .get_installed_app(&tenant, DEP)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(installed.bundle_digest, prior.bundle_digest);
    assert_eq!(installed.source_kind, "local");
    assert!(installed.app_ref.is_empty());
}
