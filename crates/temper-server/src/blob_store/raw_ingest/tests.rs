use std::time::Duration;

use bytes::Bytes;
use temper_runtime::tenant::TenantId;

use super::{
    BlobByteStream, BlobIngestAdmissionError, BlobIngestBudget, BlobIngestProgressPolicy,
    BlobStageError, put_local_blob_stream,
};

fn policy() -> BlobIngestProgressPolicy {
    BlobIngestProgressPolicy::new(
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_millis(100),
        Duration::from_millis(100),
        1,
    )
}

#[test]
fn admission_is_tenant_fair_and_releases_on_drop() {
    let budget = BlobIngestBudget::with_limits(4, 1, 2, 1, policy());
    let tenant_a = TenantId::new("tenant-a");
    let tenant_b = TenantId::new("tenant-b");

    let permit_a = budget
        .try_reserve(&tenant_a, 4)
        .expect("first tenant admitted");
    assert_eq!(
        budget.try_reserve(&tenant_a, 1).err(),
        Some(BlobIngestAdmissionError::TenantBusy)
    );
    let permit_b = budget
        .try_reserve(&tenant_b, 4)
        .expect("different tenant retains a fair slot");

    drop(permit_a);
    budget
        .try_reserve(&tenant_a, 1)
        .expect("tenant slot released by RAII");
    drop(permit_b);
}

#[test]
fn staging_bytes_grow_with_progress_instead_of_declared_size() {
    let budget = BlobIngestBudget::with_limits(3, 1, 2, 1, policy());
    let tenant_a = TenantId::new("tenant-a");
    let tenant_b = TenantId::new("tenant-b");
    let mut permit_a = budget
        .try_reserve(&tenant_a, 3)
        .expect("large declaration holds only one staging unit");
    let mut permit_b = budget
        .try_reserve(&tenant_b, 3)
        .expect("another tenant is not starved by the declaration");

    permit_a
        .reserve_received_bytes(2)
        .expect("first upload grows to two actual bytes");
    assert_eq!(
        permit_b.reserve_received_bytes(2),
        Err(BlobStageError::StagingBudgetExhausted { received: 2 })
    );
    drop(permit_a);
    permit_b
        .reserve_received_bytes(2)
        .expect("actual staging capacity is released on cancellation");
}

#[test]
fn staging_capacity_rounds_down_to_complete_units() {
    let budget = BlobIngestBudget::with_limits(5, 4, 2, 1, policy());
    let first = budget
        .try_reserve(&TenantId::new("tenant-a"), 5)
        .expect("one complete unit is available");

    assert_eq!(
        budget.try_reserve(&TenantId::new("tenant-b"), 1).err(),
        Some(BlobIngestAdmissionError::BudgetExhausted)
    );
    drop(first);
}

#[tokio::test]
async fn local_stream_rejects_length_mismatch_before_publication() {
    let root = tempfile::tempdir().expect("blob root");
    let stream: BlobByteStream =
        Box::pin(futures_util::stream::iter([Ok(Bytes::from_static(b"abc"))]));

    let error = put_local_blob_stream(root.path(), "objects/value", stream, 2)
        .await
        .expect_err("oversized stream must fail");

    assert!(error.contains("exceeded declared length"));
    assert!(!root.path().join("objects/value").exists());
}
