//! Regression scenarios for publication and persistence of install reviews.

use super::*;

#[path = "module_review_storage_test.rs"]
mod storage;
use storage::ReviewStore;

#[tokio::test]
async fn a_failed_install_cannot_replace_saved_findings_even_when_no_jev_is_configured() {
    let tenant = unique_tenant("review-failed-install");
    let mut server = server(&tenant);
    let saved = serde_json::json!({"warnings": [], "modules_reviewed": ["previous"]});
    server
        .ps
        .save_module_review(&tenant, APP, "previous", &saved.to_string())
        .await
        .unwrap();
    let store = ReviewStore::attach(&mut server.state, server.ps.clone(), true, false);
    let state = server.state.clone();
    let installing_tenant = tenant.clone();
    let installing = tokio::spawn(async move {
        send(
            &state,
            push(
                &installing_tenant,
                serde_json::json!({
                    "files": app_with_module(Some("failed"), 1)
                }),
            ),
        )
        .await
    });
    store.spec_entered.notified().await;
    // With no Jev, the spawned review has no external work. Give it its turn
    // while the install is still paused at a fallible write.
    tokio::task::yield_now().await;
    let kept = server
        .ps
        .load_module_review(&tenant, APP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        kept.version, "previous",
        "an unfinished install published its review"
    );
    store.spec_release.add_permits(1);
    let (status, body) = installing.await.unwrap();
    assert_ne!(status, StatusCode::OK, "{body}");
    assert!(
        body.to_string().contains("injected spec write failure"),
        "{body}"
    );
    tokio::task::yield_now().await;
    assert_eq!(
        finished_review(&server.state, &tenant, "failed").await["state"],
        "none"
    );
    let kept = server
        .ps
        .load_module_review(&tenant, APP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.version, "previous");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&kept.result).unwrap(),
        saved
    );
}

#[tokio::test]
async fn waiting_callers_receive_findings_while_the_save_is_still_blocked() {
    let tenant = unique_tenant("review-slow-save");
    let mut server = server(&tenant);
    let store = ReviewStore::attach(&mut server.state, server.ps.clone(), false, false);
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::open())));
    let result = install_directly(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    store.save_entered.notified().await;
    assert_eq!(
        review_of(&server.state, &tenant, "c1").await["state"],
        "reading"
    );
    let result = tokio::time::timeout(Duration::from_secs(1), result.with_reviews_read())
        .await
        .expect("findings must not wait for the database write gate");
    assert_eq!(result.modules_reviewed, vec![MODULE.to_string()]);
    assert!(found_first_item(
        &serde_json::to_value(&result.warnings).unwrap()
    ));
    assert!(
        server
            .ps
            .load_module_review(&tenant, APP)
            .await
            .unwrap()
            .is_none()
    );
    store.save_release.add_permits(1);
    let done = finished_review(&server.state, &tenant, "c1").await;
    assert_eq!(done["state"], "done", "{done}");
    assert!(done.get("persistence_error").is_none(), "{done}");
    assert!(found_first_item(&done["warnings"]), "{done}");
}

#[tokio::test]
async fn failed_saves_leave_findings_and_the_failure_readable_until_a_newer_review() {
    let tenant = unique_tenant("review-failed-save");
    let mut server = server(&tenant);
    let store = ReviewStore::attach(&mut server.state, server.ps.clone(), false, true);
    server
        .state
        .set_module_reviewer(Arc::new(ModuleReviewer::new(HeldJudge::open())));
    // The HTTP answer drops its receiver; preserving the result cannot depend on it.
    let installed = install_over_http(&server.state, &tenant, app_with_module(Some("c1"), 1)).await;
    assert!(
        installed["result"]["modules_reviewed"].is_null(),
        "{installed}"
    );
    store.save_entered.notified().await;
    store.save_release.add_permits(1);
    let done = finished_review(&server.state, &tenant, "c1").await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(
        done["persistence_error"], "injected review save failure",
        "{done}"
    );
    assert_eq!(done["modules_reviewed"], serde_json::json!([MODULE]));
    assert!(found_first_item(&done["warnings"]), "{done}");
    assert_eq!(review_of(&server.state, &tenant, "c1").await, done);
    assert!(
        server
            .ps
            .load_module_review(&tenant, APP)
            .await
            .unwrap()
            .is_none()
    );
    let other = unique_tenant("review-failed-save-other");
    server
        .state
        .server
        .authz
        .reload_tenant_policies(
            &other,
            r#"permit(principal, action == Action::"install_app", resource is App);"#,
        )
        .unwrap();
    assert_eq!(
        review_of(&server.state, &other, "c1").await["state"],
        "none"
    );
    install_over_http(&server.state, &tenant, app_with_module(Some("c2"), 2)).await;
    assert_eq!(
        review_of(&server.state, &tenant, "c1").await["state"],
        "none"
    );
    store.save_release.add_permits(1);
    let newer = finished_review(&server.state, &tenant, "c2").await;
    assert_eq!(newer["state"], "done", "{newer}");
    assert_eq!(newer["persistence_error"], "injected review save failure");
}

#[tokio::test]
async fn a_failed_provenance_write_cannot_publish_a_successful_reconciles_review() {
    let tenant = unique_tenant("review-failed-provenance");
    let mut server = server(&tenant);
    server
        .ps
        .save_module_review(&tenant, APP, "previous", "{}")
        .await
        .unwrap();
    let store = ReviewStore::attach(&mut server.state, server.ps.clone(), false, false);
    store
        .fail_provenance
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // Let an early publication finish, so delaying until the inner install
    // returns would still erase the previous row.
    store.save_release.add_permits(1);
    let state = server.state.clone();
    let installing_tenant = tenant.clone();
    let installing = tokio::spawn(async move {
        send(
            &state,
            push(
                &installing_tenant,
                serde_json::json!({
                    "files": app_with_module(Some("failed"), 1)
                }),
            ),
        )
        .await
    });
    store.spec_entered.notified().await;
    tokio::task::yield_now().await;
    let kept = server
        .ps
        .load_module_review(&tenant, APP)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        kept.version, "previous",
        "published before provenance committed"
    );
    store.spec_release.add_permits(1);
    let (status, body) = installing.await.unwrap();
    assert_ne!(status, StatusCode::OK, "{body}");
    assert!(
        body.to_string().contains("injected provenance failure"),
        "{body}"
    );
    assert_eq!(
        finished_review(&server.state, &tenant, "failed").await["state"],
        "none"
    );
    assert_eq!(
        server
            .ps
            .load_module_review(&tenant, APP)
            .await
            .unwrap()
            .unwrap()
            .version,
        "previous"
    );
}
