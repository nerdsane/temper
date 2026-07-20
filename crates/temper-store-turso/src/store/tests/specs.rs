use super::*;

#[tokio::test]
async fn upsert_specs_and_commit_preserves_identical_spec_version() {
    let store = make_store("spec-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");
    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(r#"{"all_passed":true}"#),
            },
        )
        .await
        .expect("persist verification");

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("identical spec commit");

    let conn = store.connection().expect("connection");
    let mut rows = conn
        .query(
            "SELECT version, verified, verification_status, committed \
             FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant],
        )
        .await
        .expect("query spec");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let version: i64 = row.get(0).expect("version");
    let verified: i64 = row.get(1).expect("verified");
    let status: String = row.get(2).expect("verification status");
    let committed: i64 = row.get(3).expect("committed");

    assert_eq!(version, 1, "identical spec commit must not bump version");
    assert_eq!(
        verified, 1,
        "identical spec commit must preserve verification"
    );
    assert_eq!(status, "passed");
    assert_eq!(committed, 1);
}

#[tokio::test]
async fn upsert_specs_and_commit_bypasses_write_gate_for_identical_app_specs() {
    let mut store = make_store("spec-idempotent-bypasses-gate").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";
    let policy = r#"permit(principal, action, resource);"#;

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            Some(policy),
            "test-app",
        )
        .await
        .expect("initial spec commit");

    store.write_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let held_gate = store
        .write_gate
        .clone()
        .acquire_owned()
        .await
        .expect("hold gate");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            Some(policy),
            "test-app",
        ),
    )
    .await;
    drop(held_gate);

    result
        .expect("identical app spec commit should bypass the write gate")
        .expect("identical app spec commit should succeed");
}

#[tokio::test]
async fn persist_spec_verification_keeps_updated_at_for_identical_result() {
    let store = make_store("spec-verification-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";
    let result_json = r#"{"all_passed":true}"#;

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");
    let update = TursoSpecVerificationUpdate {
        status: "passed",
        verified: true,
        levels_passed: Some(1),
        levels_total: Some(1),
        verification_result_json: Some(result_json),
    };
    store
        .persist_spec_verification(&tenant, "Issue", update)
        .await
        .expect("persist verification");

    let conn = store.connection().expect("connection");
    conn.execute(
        "UPDATE specs SET updated_at = 'fixed-time' WHERE tenant = ?1 AND entity_type = 'Issue'",
        params![tenant.as_str()],
    )
    .await
    .expect("pin updated_at");

    store
        .persist_spec_verification(&tenant, "Issue", update)
        .await
        .expect("persist identical verification");

    let mut rows = conn
        .query(
            "SELECT updated_at FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant.as_str()],
        )
        .await
        .expect("query spec updated_at");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let updated_at: String = row.get(0).expect("updated_at");

    assert_eq!(
        updated_at, "fixed-time",
        "identical verification persistence must not rewrite the spec row"
    );
}

#[tokio::test]
async fn persist_spec_verification_ignores_verified_at_only_changes() {
    let store = make_store("spec-verification-verified-at-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";
    let first_result = r#"{"all_passed":true,"levels":[],"verified_at":"2026-04-28T17:00:00Z"}"#;
    let second_result = r#"{"all_passed":true,"levels":[],"verified_at":"2026-04-28T17:01:00Z"}"#;

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");
    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(first_result),
            },
        )
        .await
        .expect("persist first verification");

    let conn = store.connection().expect("connection");
    conn.execute(
        "UPDATE specs SET updated_at = 'fixed-time' WHERE tenant = ?1 AND entity_type = 'Issue'",
        params![tenant.as_str()],
    )
    .await
    .expect("pin updated_at");

    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(second_result),
            },
        )
        .await
        .expect("persist timestamp-only verification change");

    let mut rows = conn
        .query(
            "SELECT updated_at, verification_result FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant.as_str()],
        )
        .await
        .expect("query spec updated_at");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let updated_at: String = row.get(0).expect("updated_at");
    let verification_result: String = row.get(1).expect("verification_result");

    assert_eq!(
        updated_at, "fixed-time",
        "verified_at-only verification changes must not rewrite the spec row"
    );
    assert_eq!(
        verification_result, first_result,
        "stored verification_result should remain stable when only verified_at changes"
    );
}

#[tokio::test]
async fn commit_specs_keeps_updated_at_when_specs_are_already_committed() {
    let store = make_store("spec-commit-idempotent").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";

    store
        .upsert_specs_and_commit(
            &tenant,
            &[("Issue", ioa_source, csdl_xml, content_hash)],
            None,
            "test-app",
        )
        .await
        .expect("initial spec commit");

    let conn = store.connection().expect("connection");
    conn.execute(
        "UPDATE specs SET updated_at = 'fixed-time' WHERE tenant = ?1 AND entity_type = 'Issue'",
        params![tenant.as_str()],
    )
    .await
    .expect("pin updated_at");

    store
        .commit_specs(&tenant)
        .await
        .expect("commit already committed specs");

    let mut rows = conn
        .query(
            "SELECT updated_at FROM specs WHERE tenant = ?1 AND entity_type = 'Issue'",
            params![tenant.as_str()],
        )
        .await
        .expect("query spec updated_at");
    let row = rows
        .next()
        .await
        .expect("row result")
        .expect("spec row exists");
    let updated_at: String = row.get(0).expect("updated_at");

    assert_eq!(
        updated_at, "fixed-time",
        "committing already committed specs must not rewrite them"
    );
}

#[tokio::test]
async fn load_verification_cache_ignores_uncommitted_specs() {
    let store = make_store("verification-cache-committed-only").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ioa_source = "[automaton]\nname = \"Issue\"\n";
    let csdl_xml = "<Schema Namespace=\"Temper.Tests\" />";
    let content_hash = "sha256:issue-v1";

    store
        .upsert_spec(&tenant, "Issue", ioa_source, csdl_xml, content_hash)
        .await
        .expect("upsert uncommitted spec");
    store
        .persist_spec_verification(
            &tenant,
            "Issue",
            TursoSpecVerificationUpdate {
                status: "passed",
                verified: true,
                levels_passed: Some(1),
                levels_total: Some(1),
                verification_result_json: Some(r#"{"all_passed":true}"#),
            },
        )
        .await
        .expect("persist verification");

    let cache = store
        .load_verification_cache(&tenant)
        .await
        .expect("load verification cache");
    assert!(
        !cache.contains_key("Issue"),
        "uncommitted specs must not be used to skip bootstrap persistence"
    );

    store.commit_specs(&tenant).await.expect("commit spec");
    let cache = store
        .load_verification_cache(&tenant)
        .await
        .expect("load committed verification cache");
    assert_eq!(
        cache.get("Issue"),
        Some(&(content_hash.to_string(), true)),
        "committed verified specs should populate the verification cache"
    );
}
