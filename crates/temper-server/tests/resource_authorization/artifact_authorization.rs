use super::*;

fn artifact_vault(endpoint: &str) -> SecretsVault {
    let vault = SecretsVault::new(&[11_u8; 32]);
    for (key, value) in [
        ("published_blob_public_base_url", "https://public.example"),
        ("published_blob_endpoint", endpoint),
        ("published_blob_bucket", "published-bucket"),
    ] {
        vault
            .cache_secret(TENANT, key, value.to_string())
            .expect("cache artifact secret");
    }
    vault
}

fn artifact_body(namespace: &str, source_version: &str) -> serde_json::Value {
    serde_json::json!({
        "file_id": "file-a",
        "label": "latest",
        "owner_ref_type": "Document",
        "owner_ref_id": "doc-a",
        "source_file_version_id": source_version,
        "namespace": namespace,
    })
}

#[tokio::test]
async fn artifact_publish_requires_dedicated_source_permission_and_safe_segments() {
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&mock)
        .await;
    let (mut state, store, _temp) = state_with_turso("artifact-resource-auth").await;
    state.secrets_vault = Some(std::sync::Arc::new(artifact_vault(&mock.uri())));
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"
permit(
  principal == Customer::"publisher",
  action == Action::"publish_artifact",
  resource == File::"file-a"
);
permit(
  principal == Customer::"read-only",
  action == Action::"read",
  resource == File::"file-a"
);
permit(
  principal == Customer::"version-only",
  action == Action::"publish_artifact",
  resource == FileVersion::"version-a"
);
permit(
  principal == Customer::"publisher",
  action == Action::"publish_artifact",
  resource == FileVersion::"version-mismatch"
);
permit(
  principal == Customer::"forged-metadata",
  action == Action::"publish_artifact",
  resource == File::"file-a"
)
when {
  context.owner_ref_id == "doc-a"
};
"#,
        )
        .expect("artifact policy should parse");
    state
        .authz
        .reload_tenant_policies("other-tenant", "")
        .expect("other tenant should default-deny");
    seed_file(
        &store,
        "File",
        "file-a",
        "sha256:artifactcontent",
        b"artifact body",
        None,
    )
    .await;
    seed_file(
        &store,
        "FileVersion",
        "version-a",
        "sha256:artifactversion",
        b"artifact version",
        Some("file-a"),
    )
    .await;
    seed_file(
        &store,
        "FileVersion",
        "version-mismatch",
        "sha256:artifactmismatch",
        b"artifact mismatch",
        Some("file-b"),
    )
    .await;
    let app = build_router(state);

    for (tenant, principal, body, expected) in [
        (
            TENANT,
            "intruder",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "read-only",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "forged-metadata",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            "other-tenant",
            "publisher",
            artifact_body("artifacts", ""),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "publisher",
            artifact_body("artifacts", "version-a"),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "version-only",
            artifact_body("artifacts", "version-a"),
            StatusCode::FORBIDDEN,
        ),
        (
            TENANT,
            "publisher",
            artifact_body("artifacts", "version-mismatch"),
            StatusCode::BAD_REQUEST,
        ),
        (
            TENANT,
            "publisher",
            artifact_body("../escape", ""),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                "/api/files/publish-artifact",
                body,
                tenant,
                principal,
            ))
            .await
            .expect("rejected publish request should run");
        assert_eq!(
            response.status(),
            expected,
            "unexpected publish status for tenant {tenant} principal {principal}"
        );
    }

    let allowed = app
        .oneshot(json_request(
            Method::POST,
            "/api/files/publish-artifact",
            artifact_body("artifacts", ""),
            TENANT,
            "publisher",
        ))
        .await
        .expect("authorized publish request should run");
    assert_eq!(allowed.status(), StatusCode::OK);
    mock.verify().await;
}

#[tokio::test]
async fn local_artifact_publish_uses_direct_tenant_blob_store() {
    let (mut state, store, _temp) = state_with_turso("artifact-local-write").await;
    state.secrets_vault = Some(std::sync::Arc::new(artifact_vault(
        "http://127.0.0.1:9/_internal/blobs",
    )));
    state
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"permit(
  principal == Customer::"publisher",
  action == Action::"publish_artifact",
  resource == File::"file-a"
);"#,
        )
        .expect("artifact policy should parse");
    seed_file(
        &store,
        "File",
        "file-a",
        "sha256:localartifact",
        b"local artifact body",
        None,
    )
    .await;
    let state_for_read = state.clone();
    let app = build_router(state);

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/files/publish-artifact",
            artifact_body("artifacts", ""),
            TENANT,
            "publisher",
        ))
        .await
        .expect("local publish request should run");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read publish response");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("publish JSON");
    let storage_key = json["artifact"]["public_storage_key"]
        .as_str()
        .expect("public storage key");
    let stored = state_for_read
        .get_blob_with_legacy_fallback(
            &TenantId::default(),
            &format!("published-bucket/{storage_key}"),
        )
        .await
        .expect("read local artifact blob");
    assert_eq!(stored.as_deref(), Some(b"local artifact body".as_slice()));
}
