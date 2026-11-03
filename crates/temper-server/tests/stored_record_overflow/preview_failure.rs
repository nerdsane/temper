//! A transient read failure must not let an outside action bypass its checks.
use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use temper_server::secrets::vault::SecretsVault;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn comparison_ioa() -> String {
    let mut ioa = BATCH_IOA.replace(
        "name = \"Run\"\nkind = \"input\"\nfrom = [\"Draft\"]\nto = \"Running\"",
        r#"name = "Run"
kind = "input"
from = ["Draft"]
to = "Running"
params = ["expected_notes"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_notes"
field = "Notes""#,
    );
    ioa.push_str(
        r#"
[[state]]
name = "Notes"
type = "string"
initial = ""
"#,
    );
    ioa
}

#[tokio::test]
async fn an_unreadable_preview_stops_before_a_recovered_actor_can_store_an_unchecked_record() {
    use sha2::{Digest as _, Sha256};

    let mock = MockServer::start().await;
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let bytes = serde_json::to_vec(&json!("draft note")).unwrap();
    let key = format!("field-overflow/sha256/{:x}.json", Sha256::digest(&bytes));
    let response_bytes = bytes.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                // The preview cannot read this input. The very next read works,
                // without any intervening change to the entity's version.
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200).set_body_bytes(response_bytes.clone())
            }
        })
        .mount(&mock)
        .await;
    let ioa = comparison_ioa();
    let mut state = batch_state(&ioa);
    let temp = tempfile::tempdir().unwrap();
    state.data_dir = temp.path().join("data");
    let vault = SecretsVault::new(&[86; 32]);
    vault.cache_secret("default", "blob_endpoint", mock.uri());
    vault.cache_secret("default", "blob_bucket", "bucket".into());
    state.secrets_vault = Some(Arc::new(vault));
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Batch",
            "unreadable-preview",
            json!({"Notes": {
                "__temper_blob_ref": key,
                "__temper_blob_size": bytes.len(),
                "__temper_blob_encoding": "json"
            }}),
        )
        .await
        .unwrap();
    let before = state
        .get_tenant_entity_state(&tenant, "Batch", "unreadable-preview")
        .await
        .unwrap()
        .state;
    let (status, body) = act(
        &state,
        "unreadable-preview",
        "Run",
        json!({"expected_notes": "draft note"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.as_str().unwrap().contains("could not be read"),
        "{body}"
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "no actor retry after a failed preview"
    );
    let after = state
        .get_tenant_entity_state(&tenant, "Batch", "unreadable-preview")
        .await
        .unwrap()
        .state;
    assert_eq!(after.status, before.status);
    assert_eq!(after.fields, before.fields);
    assert_eq!(after.sequence_nr, before.sequence_nr);

    // Once the input is readable, the field invariant must still refuse a
    // running batch with no items. A caller can then supply the missing items.
    let (status, body) = act(
        &state,
        "unreadable-preview",
        "Run",
        json!({"expected_notes": "draft note"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.as_str().unwrap().contains("a running batch has items"),
        "{body}"
    );
    let (status, body) = act(
        &state,
        "unreadable-preview",
        "Fill",
        json!({"Items": ["ready"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = act(
        &state,
        "unreadable-preview",
        "Run",
        json!({"expected_notes": "draft note"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored(&state, "unreadable-preview").await.0, "Running");
}

#[tokio::test]
async fn unreadable_child_inputs_stop_the_parent_instead_of_predicting_a_refused_initializer() {
    use sha2::{Digest as _, Sha256};

    let mock = MockServer::start().await;
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let bytes = serde_json::to_vec(&json!("child note")).unwrap();
    let key = format!("field-overflow/sha256/{:x}.json", Sha256::digest(&bytes));
    let response_bytes = bytes.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200).set_body_bytes(response_bytes.clone())
            }
        })
        .mount(&mock)
        .await;
    let mut ioa = comparison_ioa();
    ioa.push_str(
        r#"
[[action]]
name = "Spawn"
kind = "input"
from = ["Draft"]
params = ["child_id", "expected_notes"]
effect = ["spawn('Batch', 'Run', ChildRef, params.child_id)"]
"#,
    );
    let mut state = batch_state(&ioa);
    let verified = state
        .registry
        .read()
        .unwrap()
        .get_verification_status(&TenantId::default(), "Batch")
        .cloned()
        .unwrap();
    // The parent may link only a child that stays in Draft. A failed read is
    // no evidence that the initializer will be refused and leave it there.
    state
        .registry
        .write()
        .unwrap()
        .register_tenant_with_constraints(
            "default",
            parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Batch", ioa.as_str())],
            Some(
                r#"
version = 1
[[invariant]]
name = "ChildStaysDraft"
kind = "hard"
on = "Batch.Spawn"
assert = 'related(Batch, ChildRef).status in ["Draft"]'
"#
                .into(),
            ),
        );
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Batch",
        verified,
    );
    let temp = tempfile::tempdir().unwrap();
    state.data_dir = temp.path().join("data");
    let vault = SecretsVault::new(&[87; 32]);
    vault.cache_secret("default", "blob_endpoint", mock.uri());
    vault.cache_secret("default", "blob_bucket", "bucket".into());
    state.secrets_vault = Some(Arc::new(vault));
    let tenant = TenantId::default();
    for (id, fields) in [
        ("parent", json!({})),
        (
            "child",
            json!({"Items": ["ready"], "Notes": {
                "__temper_blob_ref": key,
                "__temper_blob_size": bytes.len(),
                "__temper_blob_encoding": "json"
            }}),
        ),
    ] {
        state
            .get_or_create_tenant_entity(&tenant, "Batch", id, fields)
            .await
            .unwrap();
    }
    let before = stored(&state, "parent").await;
    let (status, body) = act(
        &state,
        "parent",
        "Spawn",
        json!({
            "child_id": "child", "expected_notes": "child note"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.as_str().unwrap().contains("could not be read"),
        "{body}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(stored(&state, "parent").await, before);
    assert_eq!(stored(&state, "child").await.0, "Draft");

    // Recovery predicts the actual initializer, which would change the child
    // to Running, and refuses the parent's cross-invariant before dispatch.
    let (status, body) = act(
        &state,
        "parent",
        "Spawn",
        json!({
            "child_id": "child", "expected_notes": "child note"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.as_str().unwrap().contains("has status='Running'"),
        "{body}"
    );
    assert_eq!(stored(&state, "parent").await, before);
    assert_eq!(stored(&state, "child").await.0, "Draft");
}
