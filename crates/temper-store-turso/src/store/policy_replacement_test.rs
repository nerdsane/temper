use super::*;

#[tokio::test]
async fn conditional_write_cannot_overwrite_newer_disabled_or_other_tenant_entry() {
    let store = TursoEventStore::new(":memory:", None).await.unwrap();
    store
        .save_policy("a", "legacy", "old", "fixture")
        .await
        .unwrap();
    store
        .save_policy("b", "legacy", "other", "fixture")
        .await
        .unwrap();
    let old_hash = compute_policy_hash("old");
    assert!(
        store
            .replace_policy_if_hash("a", "legacy", &old_hash, "new", "human")
            .await
            .unwrap()
    );
    assert!(
        !store
            .replace_policy_if_hash("a", "legacy", &old_hash, "stale", "human")
            .await
            .unwrap()
    );
    store
        .toggle_policy_enabled("a", "legacy", false)
        .await
        .unwrap();
    assert!(
        !store
            .replace_policy_if_hash(
                "a",
                "legacy",
                &compute_policy_hash("new"),
                "disabled",
                "human"
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .replace_policy_if_hash("a", "missing", &old_hash, "created", "human")
            .await
            .unwrap()
    );
    let rows = store.load_policies_for_tenant("a").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cedar_text, "new");
    assert!(!rows[0].enabled);
    assert_eq!(
        store.load_policies_for_tenant("b").await.unwrap()[0].cedar_text,
        "other"
    );
}
