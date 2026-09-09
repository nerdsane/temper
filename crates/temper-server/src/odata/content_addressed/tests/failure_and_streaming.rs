use super::*;

#[tokio::test]
async fn object_store_failure_creates_no_entity_and_cleans_staging() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let (mut state, data_dir) = test_state();
    let vault = SecretsVault::new(&[7u8; 32]);
    vault
        .cache_secret("default", "blob_endpoint", server.uri())
        .expect("cache endpoint");
    vault
        .cache_secret("default", "blob_bucket", "test-bucket".to_string())
        .expect("cache bucket");
    state.secrets_vault = Some(Arc::new(vault));

    let response = app(state.clone())
        .oneshot(ingest_request(Body::from("abc"), 3, &git_blob_id(b"abc")))
        .await
        .expect("backend failure response");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        state
            .list_entity_ids(&TenantId::default(), "Blob")
            .is_empty()
    );
    assert_eq!(
        staging_entry_count(&data_dir.path().join("blob-ingest-staging")).await,
        0
    );
}

#[tokio::test]
async fn stalled_upload_times_out_releases_admission_and_cleans_staging() {
    let (mut state, data_dir) = test_state();
    state.raw_blob_ingest_budget = BlobIngestBudget::with_limits(
        16,
        1,
        2,
        1,
        BlobIngestProgressPolicy::new(
            Duration::from_millis(25),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(50),
            1,
        ),
    );
    let router = app(state.clone());
    let stalled =
        Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
    let response = tokio::time::timeout(
        Duration::from_secs(1),
        router
            .clone()
            .oneshot(ingest_request(stalled, 3, &git_blob_id(b"abc"))),
    )
    .await
    .expect("stalled upload must be bounded")
    .expect("stalled upload response");
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        staging_entry_count(&data_dir.path().join("blobs/.ingest-staging")).await,
        0
    );

    let retry = router
        .oneshot(ingest_request(Body::from("abc"), 3, &git_blob_id(b"abc")))
        .await
        .expect("retry response");
    assert_eq!(retry.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn trickled_upload_fails_minimum_throughput_and_cleans_staging() {
    let (mut state, data_dir) = test_state();
    state.raw_blob_ingest_budget = BlobIngestBudget::with_limits(
        16,
        1,
        2,
        1,
        BlobIngestProgressPolicy::new(
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_millis(20),
            Duration::from_millis(20),
            10_000,
        ),
    );
    let body = Body::from_stream(async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"a"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"bc"));
    });
    let response = app(state.clone())
        .oneshot(ingest_request(body, 3, &git_blob_id(b"abc")))
        .await
        .expect("trickle response");
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        staging_entry_count(&data_dir.path().join("blobs/.ingest-staging")).await,
        0
    );
    assert!(
        state
            .list_entity_ids(&TenantId::default(), "Blob")
            .is_empty()
    );
}

#[tokio::test]
async fn large_blob_json_stays_descriptor_and_property_value_streams_exact_bytes() {
    let (state, _data_dir) = test_state();
    let body = vec![0x5au8; 256 * 1024];
    let object_id = git_blob_id(&body);
    let router = app(state.clone());
    let created = router
        .clone()
        .oneshot(ingest_request(
            Body::from(body.clone()),
            body.len(),
            &object_id,
        ))
        .await
        .expect("large ingest response");
    assert_eq!(created.status(), StatusCode::CREATED);

    let entity = router
        .clone()
        .oneshot(
            Request::get(format!("/tdata/Blobs('{object_id}')"))
                .body(Body::empty())
                .expect("entity request"),
        )
        .await
        .expect("entity response");
    assert_eq!(entity.status(), StatusCode::OK);
    let entity_bytes = axum::body::to_bytes(entity.into_body(), 2 * 1024 * 1024)
        .await
        .expect("entity body");
    let entity_json: serde_json::Value =
        serde_json::from_slice(&entity_bytes).expect("entity JSON");
    assert!(
        entity_json["fields"]["Content"]
            .get(FIELD_OVERFLOW_REF_KEY)
            .is_some(),
        "large JSON reads must retain a bounded media descriptor"
    );

    let content = router
        .clone()
        .oneshot(
            Request::get(format!("/tdata/Blobs('{object_id}')/Content/$value"))
                .body(Body::empty())
                .expect("content request"),
        )
        .await
        .expect("content response");
    assert_eq!(content.status(), StatusCode::OK);
    assert_eq!(content.headers()["content-length"], body.len().to_string());
    let content_bytes = axum::body::to_bytes(content.into_body(), body.len() + 1)
        .await
        .expect("streamed content");
    assert_eq!(content_bytes.as_ref(), body.as_slice());

    let canonical = router
        .oneshot(
            Request::get(format!("/tdata/Blobs('{object_id}')/CanonicalBytes/$value"))
                .body(Body::empty())
                .expect("canonical request"),
        )
        .await
        .expect("canonical response");
    assert_eq!(canonical.status(), StatusCode::OK);
    let canonical_bytes = axum::body::to_bytes(
        canonical.into_body(),
        body.len() + "blob 262144\0".len() + 1,
    )
    .await
    .expect("streamed canonical bytes");
    let mut expected_canonical = format!("blob {}\0", body.len()).into_bytes();
    expected_canonical.extend_from_slice(&body);
    assert_eq!(canonical_bytes.as_ref(), expected_canonical.as_slice());

    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action == Action::"create", resource is Blob);"#,
        )
        .expect("remove Blob read policy");
    let denied = app(state)
        .oneshot(
            Request::get(format!("/tdata/Blobs('{object_id}')/Content/$value"))
                .body(Body::empty())
                .expect("denied content request"),
        )
        .await
        .expect("denied content response");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}
