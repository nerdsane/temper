//! ABA-safe key-contract watermark regressions.

use super::*;

const DOC_IOA_WITHOUT_KEYS: &str = r#"
[automaton]
name = "Doc"
states = ["New", "Ready"]
initial = "New"

[[state]]
name = "WorkspaceId"
type = "string"
initial = ""

[[state]]
name = "Path"
type = "string"
initial = ""

[[action]]
name = "Create"
kind = "input"
from = ["New"]
to = "Ready"
params = ["WorkspaceId", "Path"]

[[action]]
name = "Rekey"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["WorkspaceId", "Path"]
"#;

/// A coverage watermark is tied to a monotonic key-contract revision, not only
/// the signature text. Cycling A -> no keys -> A must invalidate the original
/// A watermark, and a backfill that started before either live change cannot
/// publish after the corresponding revision has advanced.
#[tokio::test]
async fn key_contract_revision_fences_aba_spec_cycles() {
    let (_guard, _clock, _ids) = install_deterministic_context(243);
    let sim = SimEventStore::no_faults(243);
    let events = BoxedEventStore::new(sim);
    let keyed_table = TransitionTable::from_ioa_source(DOC_IOA);
    let signature_a = declared_key_set_signature(&keyed_table.keys);
    let table = Arc::new(RwLock::new(keyed_table));
    let system = ActorSystem::new("arn238-key-contract-aba");
    let actor = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "doc-aba",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-aba",
    );
    assert!(
        action(
            &actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/a"}),
        )
        .await
        .success
    );

    events
        .mark_key_index_backfilled("default", "Doc", &signature_a)
        .await
        .expect("mark A coverage");
    let revision_a = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read A revision");
    assert_eq!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read A coverage"),
        vec![("Doc".to_string(), signature_a.clone())]
    );

    let signature_without_keys =
        declared_key_set_signature(&TransitionTable::from_ioa_source(DOC_IOA_WITHOUT_KEYS).keys);
    let stale_no_key_repair_revision = events
        .begin_key_index_backfill("default", "Doc", &signature_without_keys)
        .await
        .expect("begin no-key repair");
    assert!(stale_no_key_repair_revision > revision_a);
    let concurrent_a = update(
        &actor,
        serde_json::json!({"Path": "/a-during-no-key-repair"}),
        false,
    )
    .await;
    assert!(concurrent_a.success, "concurrent A write failed");
    let revision_after_concurrent_a = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read post-race A revision");
    assert!(revision_after_concurrent_a > stale_no_key_repair_revision);
    assert!(
        !events
            .mark_key_index_backfilled_if_revision(
                "default",
                "Doc",
                &signature_without_keys,
                stale_no_key_repair_revision,
            )
            .await
            .expect("reject mixed-contract repair"),
        "a live A write during a no-key repair must fence publication"
    );

    let without_keys = TransitionTable::from_ioa_source(DOC_IOA_WITHOUT_KEYS);
    *table.write().expect("table lock") = without_keys;
    let no_key_write = update(&actor, serde_json::json!({"Path": "/while-unkeyed"}), false).await;
    assert!(no_key_write.success, "unkeyed write failed");
    let revision_without_keys = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read no-key revision");
    assert!(revision_without_keys > revision_after_concurrent_a);
    assert!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read invalidated coverage")
            .is_empty(),
        "a live no-key write must invalidate the A watermark"
    );
    assert!(
        !events
            .mark_key_index_backfilled_if_revision("default", "Doc", &signature_a, revision_a,)
            .await
            .expect("reject stale A backfill"),
        "a backfill captured under the first A contract must be fenced"
    );

    let restored_table = TransitionTable::from_ioa_source(DOC_IOA);
    assert_eq!(
        declared_key_set_signature(&restored_table.keys),
        signature_a,
        "the restored contract intentionally reuses the original signature"
    );
    *table.write().expect("table lock") = restored_table;
    assert!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read pre-write restored coverage")
            .is_empty(),
        "restoring the same signature in memory must not resurrect coverage"
    );
    let restored = update(&actor, serde_json::json!({"Path": "/restored-a"}), false).await;
    assert!(restored.success, "restored A write failed");
    let restored_revision = events
        .key_index_reconciliation_revision("default", "Doc")
        .await
        .expect("read restored A revision");
    assert!(restored_revision > revision_without_keys);
    assert!(
        !events
            .mark_key_index_backfilled_if_revision(
                "default",
                "Doc",
                &signature_without_keys,
                revision_without_keys,
            )
            .await
            .expect("reject stale no-key backfill"),
        "a backfill captured before A was restored must be fenced"
    );
    assert!(
        events
            .key_index_backfilled_types("default")
            .await
            .expect("read final coverage")
            .is_empty(),
        "the A -> no-key -> A cycle requires a fresh successful backfill"
    );
    assert_eq!(
        events
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws", "/restored-a"),)
            .await
            .expect("restored A key lookup"),
        Some("doc-aba".to_string())
    );
}
