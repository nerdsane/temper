//! Regression: another app's closure can hold the dependency version rollback needs.

use super::*;

pub(super) fn cache(platform: &PlatformState, tenant: &str, app: &str, version: &str) -> PathBuf {
    genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant,
            source: InstallSource::Registry("default"),
        },
        &format!("acme/{app}@{version}"),
    )
}

pub(super) fn record(root: &Path, tenant: &str, app: &str, version: &str) -> InstalledAppRecord {
    InstalledAppRecord {
        tenant: tenant.to_string(),
        app_ref: format!("acme/{app}@{version}"),
        version_hash: version.to_string(),
        registry_tenant: "default".to_string(),
        registry_url: "http://127.0.0.1:9".to_string(),
        status: "installed".to_string(),
        ..recorded(root, app)
    }
}

/// Register every app `root`'s list names for `tenant`, as an install does.
fn register_listed(tenant: &str, root: &Path) {
    let apps = written::read(root).expect("the list");
    register_written_apps(tenant, root.to_path_buf(), apps);
}

#[tokio::test]
async fn rollback_recovers_a_dependency_from_another_installed_closure() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-rollback-other-closure");
    let (v1, v2, v3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let notes_cache = cache(&platform, &tenant, "notes", &v1);
    write_app_needing(&notes_cache, "notes", "notes v1", &["tags"]);
    write_app(&notes_cache, "tags", "tags v1");
    let other_cache = cache(&platform, &tenant, "other", &v2);
    write_app_needing(&other_cache, "other", "other v2", &["tags"]);
    write_app(&other_cache, "tags", "tags v2");
    let broken_cache = cache(&platform, &tenant, "notes", &v3);
    write_app_needing(&broken_cache, "notes", "notes v3", &["tags"]);
    write_app(&broken_cache, "tags", "broken tags v3");
    let notes = record(&notes_cache, &tenant, "notes", &v1);
    let tags = record(&other_cache, &tenant, "tags", &v2);
    let other = record(&other_cache, &tenant, "other", &v2);
    // The failed install has already replaced the selected bundles and records.
    for installed in [
        other,
        record(&broken_cache, &tenant, "notes", &v3),
        record(&broken_cache, &tenant, "tags", &v3),
    ] {
        ps.record_installed_app_metadata(&installed)
            .await
            .expect("record install");
    }
    register_listed(&tenant, &broken_cache);
    assert!(!cache(&platform, &tenant, "tags", &v2).exists());
    crate::genesis_install::rollback_to_prior(
        &platform,
        ps.as_ref(),
        &tenant,
        &notes,
        &[notes.clone(), tags.clone()],
    )
    .await
    .expect("rollback recovers both recorded bundles offline");
    for (app, expected) in [("notes", "notes v1"), ("tags", "tags v2")] {
        let guide = crate::os_apps::tenant_app_guide(&tenant, app).expect("resolved app");
        assert!(guide.contains(expected), "{guide}");
    }
    assert_eq!(
        crate::os_apps::tenant_os_app_bundle_digest(&tenant, "tags")
            .expect("tags digest")
            .bundle_digest,
        tags.bundle_digest
    );
}

#[tokio::test]
async fn rollback_fails_when_the_recorded_dependency_cannot_be_recovered() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-rollback-missing");
    let (v1, v2, v3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let notes_cache = cache(&platform, &tenant, "notes", &v1);
    write_app_needing(&notes_cache, "notes", "notes v1", &["tags"]);
    write_app(&notes_cache, "tags", "tags v1");
    let lost = tempfile::tempdir().expect("lost files");
    write_app(lost.path(), "tags", "tags v2");
    let notes = record(&notes_cache, &tenant, "notes", &v1);
    let tags = record(lost.path(), &tenant, "tags", &v2);
    let broken = cache(&platform, &tenant, "notes", &v3);
    write_app_needing(&broken, "notes", "notes v3", &["tags"]);
    write_app(&broken, "tags", "broken tags v3");
    let failed_notes = record(&broken, &tenant, "notes", &v3);
    ps.record_installed_app_metadata(&failed_notes)
        .await
        .expect("record failed root");
    ps.record_installed_app_metadata(&record(&broken, &tenant, "tags", &v3))
        .await
        .expect("record failed dependency");
    register_listed(&tenant, &broken);
    // The only matching cache belongs to another tenant and must not be used.
    let foreign = cache(&platform, "another-tenant", "other", &v2);
    write_app(&foreign, "other", "other v2");
    write_app(&foreign, "tags", "tags v2");
    ps.record_installed_app_metadata(&record(&foreign, "another-tenant", "other", &v2))
        .await
        .expect("record foreign install");

    let error = crate::genesis_install::rollback_to_prior(
        &platform,
        ps.as_ref(),
        &tenant,
        &notes,
        &[notes.clone(), tags],
    )
    .await
    .expect_err("an unavailable prior dependency must fail rollback");
    assert!(error.contains("tags"), "{error}");
    assert_eq!(
        ps.get_installed_app(&tenant, "notes")
            .await
            .expect("read root")
            .expect("root record")
            .app_ref,
        failed_notes.app_ref
    );
}

#[tokio::test]
async fn rollback_fetches_the_recorded_dependency_and_rejects_changed_bytes() {
    use base64::Engine as _;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    for changed in [false, true] {
        let registry = MockServer::start().await;
        let data_dir = tempfile::tempdir().expect("data dir");
        let ps = Arc::new(store());
        let platform = server_on(data_dir.path(), &ps);
        let tenant = format!("{TENANT}-rollback-fetch-{changed}");
        let (v1, v2) = ("1".repeat(40), "2".repeat(40));
        let notes_cache = cache(&platform, &tenant, "notes", &v1);
        write_app_needing(&notes_cache, "notes", "notes v1", &["tags"]);
        write_app(&notes_cache, "tags", "tags v1");
        let remote = tempfile::tempdir().expect("registry files");
        write_app(remote.path(), "tags", "tags v2");
        let notes = record(&notes_cache, &tenant, "notes", &v1);
        let tags = InstalledAppRecord {
            registry_url: registry.uri(),
            ..record(remote.path(), &tenant, "tags", &v2)
        };
        if changed {
            write_app(remote.path(), "tags", "changed tags v2");
        }
        let files: Vec<_> = ["app.toml", "APP.md"]
            .into_iter()
            .map(|name| GenesisRegistryBundleFile {
                path: name.to_string(),
                content_base64: base64::engine::general_purpose::STANDARD.encode(
                    std::fs::read(remote.path().join("tags").join(name)).expect("bundle file"),
                ),
            })
            .collect();
        let bundle = GenesisRegistryBundleResponse {
            app_ref: tags.app_ref.clone(),
            registry_tenant: "default".to_string(),
            apps: vec![GenesisRegistryBundleApp {
                owner: "acme".to_string(),
                name: "tags".to_string(),
                version_hash: v2.clone(),
                files,
            }],
        };
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/genesis/apps/acme/tags/versions/{v2}/bundle"
            )))
            .and(header("X-Tenant-Id", "default"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&bundle))
            .expect(1)
            .mount(&registry)
            .await;
        let result = crate::genesis_install::rollback_to_prior(
            &platform,
            ps.as_ref(),
            &tenant,
            &notes,
            &[notes.clone(), tags.clone()],
        )
        .await;
        if changed {
            let error = result.expect_err("changed registry bytes must fail rollback");
            assert!(error.contains("recorded bundle"), "{error}");
            assert!(crate::os_apps::tenant_app_guide(&tenant, "tags").is_none());
        } else {
            result.expect("the recorded dependency is refetched");
            assert_eq!(
                crate::os_apps::tenant_os_app_bundle_digest(&tenant, "tags")
                    .expect("tags digest")
                    .bundle_digest,
                tags.bundle_digest
            );
        }
        registry.verify().await;
    }
}

#[tokio::test]
async fn rollback_checks_dependency_runtime_readiness() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-rollback-unready");
    let v1 = "1".repeat(40);
    let root = cache(&platform, &tenant, "notes", &v1);
    write_app_needing(&root, "notes", "notes v1", &["tags", "labels"]);
    write_app(&root, "labels", "labels v1");
    write_app(&root, "tags", "tags v1");
    std::fs::write(
        root.join("tags/app.toml"),
        r#"name = "tags"
[[wasm_modules]]
name = "broken"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .expect("manifest");
    std::fs::create_dir_all(root.join("tags/wasm/broken")).expect("wasm dir");
    std::fs::write(root.join("tags/wasm/broken/broken.wasm"), b"not wasm").expect("broken wasm");
    let tags_files = ["app.toml", "APP.md", "wasm/broken/broken.wasm"];
    written::keep_app(&root, "tags", &tags_files);
    let notes = record(&root, &tenant, "notes", &v1);
    let tags = record(&root, &tenant, "tags", &v1);
    let labels = record(&root, &tenant, "labels", &v1);
    let error = crate::genesis_install::rollback_to_prior(
        &platform,
        ps.as_ref(),
        &tenant,
        &notes,
        &[notes.clone(), tags.clone(), labels.clone()],
    )
    .await
    .expect_err("a ready root cannot hide an unready dependency");
    assert!(error.contains("runtime-ready"), "{error}");
    let failed_tags = InstalledAppRecord {
        status: "failed".to_string(),
        ..tags
    };
    assert_eq!(
        ps.get_installed_app(&tenant, "tags")
            .await
            .unwrap()
            .unwrap(),
        failed_tags,
        "an unready dependency keeps recovery coordinates without claiming readiness"
    );
    assert_eq!(
        ps.get_installed_app(&tenant, "labels")
            .await
            .unwrap()
            .unwrap(),
        labels,
        "an unready sibling does not prevent a healthy dependency from recovering"
    );
}

#[tokio::test]
async fn partial_rollback_preserves_each_dependency_for_retry_and_boot_recovery() {
    use base64::Engine as _;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    for seed in 0..4 {
        let registry = MockServer::start().await;
        let data_dir = tempfile::tempdir().expect("data dir");
        let ps = Arc::new(SimPlatformStore::new(seed, SimPlatformFaultConfig::none()));
        let platform = server_on(data_dir.path(), &ps);
        let tenant = format!("{TENANT}-partial-rollback-{seed}");
        let (v1, v2) = ("1".repeat(40), "2".repeat(40));
        let root = cache(&platform, &tenant, "notes", &v1);
        write_app_needing(
            &root,
            "notes",
            "notes v1",
            &["missing", "cached", "bundled"],
        );
        write_app(&root, "bundled", "bundled v1");
        let own = cache(&platform, &tenant, "cached", &v1);
        write_app(&own, "cached", "cached v1");
        let missing_files = tempfile::tempdir().expect("missing files");
        write_app(missing_files.path(), "missing", "missing v1");
        let notes = record(&root, &tenant, "notes", &v1);
        let bundled = record(&root, &tenant, "bundled", &v1);
        let cached = record(&own, &tenant, "cached", &v1);
        let missing = InstalledAppRecord {
            registry_url: registry.uri(),
            ..record(missing_files.path(), &tenant, "missing", &v1)
        };
        // Exercise both dependency orderings and failure to recover the root itself.
        if seed & 1 != 0 {
            std::fs::remove_dir_all(root.join("notes")).expect("evict root");
        }
        let mut dependencies = vec![missing.clone(), cached.clone(), bundled.clone()];
        if seed & 2 != 0 {
            dependencies.reverse();
        }
        let mut prior_records = vec![notes.clone()];
        prior_records.extend(dependencies.clone());
        let broken = cache(&platform, &tenant, "notes", &v2);
        for app in ["notes", "missing", "cached", "bundled"] {
            write_app(&broken, app, "failed upgrade");
            // Reconcile clears Genesis provenance before the upgrade fails verification.
            let local = InstalledAppRecord {
                tenant: tenant.clone(),
                status: "installed".to_string(),
                source_kind: "local".to_string(),
                bundle_digest: recorded(&broken, app).bundle_digest,
                app_name: app.to_string(),
                ..Default::default()
            };
            ps.record_installed_app_metadata(&local)
                .await
                .expect("partial upgrade");
        }
        register_listed(&tenant, &broken);
        let error = crate::genesis_install::rollback_to_prior(
            &platform,
            ps.as_ref(),
            &tenant,
            &notes,
            &prior_records,
        )
        .await
        .expect_err("incomplete recovery still fails");
        assert!(error.contains("rollback"), "seed {seed}: {error}");
        for prior in &dependencies {
            let stored = ps
                .get_installed_app(&tenant, &prior.app_name)
                .await
                .expect("read dependency")
                .expect("dependency record");
            let mut expected = prior.clone();
            if prior.app_name == "missing" {
                expected.status = "failed".to_string();
            }
            assert_eq!(
                stored, expected,
                "seed {seed}: {} keeps its recovery coordinates",
                prior.app_name
            );
        }
        // The registry returns after rollback fails. Boot must fetch using the saved coordinates.
        let files = ["app.toml", "APP.md"]
            .into_iter()
            .map(|name| GenesisRegistryBundleFile {
                path: name.to_string(),
                content_base64: base64::engine::general_purpose::STANDARD.encode(
                    std::fs::read(missing_files.path().join("missing").join(name)).unwrap(),
                ),
            })
            .collect();
        let bundle = GenesisRegistryBundleResponse {
            app_ref: missing.app_ref.clone(),
            registry_tenant: "default".to_string(),
            apps: vec![GenesisRegistryBundleApp {
                owner: "acme".to_string(),
                name: "missing".to_string(),
                version_hash: v1.clone(),
                files,
            }],
        };
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/genesis/apps/acme/missing/versions/{v1}/bundle"
            )))
            .and(header("X-Tenant-Id", "default"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&bundle))
            .expect(1)
            .mount(&registry)
            .await;
        write_app_needing(
            &root,
            "notes",
            "notes v1",
            &["missing", "cached", "bundled"],
        );
        let rebooted = server_on(data_dir.path(), &ps);
        let recovery = start_genesis_registry_cache_recovery(&rebooted).await;
        assert_eq!(
            (
                recovery.restored,
                recovery.fetching.finished().await.fetched
            ),
            (1, 1),
            "seed {seed}: boot restores the cached dependency and fetches the missing one"
        );
        crate::genesis_install::rollback_to_prior(
            &rebooted,
            ps.as_ref(),
            &tenant,
            &notes,
            &prior_records,
        )
        .await
        .expect("retry completes after missing files return");
        registry.verify().await;
        for prior in &prior_records {
            assert_eq!(
                ps.get_installed_app(&tenant, &prior.app_name)
                    .await
                    .unwrap()
                    .unwrap(),
                *prior,
                "seed {seed}: retry restores {}",
                prior.app_name
            );
        }
    }
}
