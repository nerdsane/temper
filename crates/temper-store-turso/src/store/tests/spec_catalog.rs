use super::*;

#[tokio::test]
async fn late_verifier_cannot_publish_newer_staged_bytes() {
    let url = sqlite_test_url("late-verifier-exact-catalog");
    let store = TursoEventStore::new(&url, None).await.expect("open store");
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let source_a = "[automaton]\nname = \"Item\"\n# candidate-a\n";
    let source_b = "[automaton]\nname = \"Item\"\n# candidate-b\n";
    let fingerprint_a = crate::spec_content_hash(source_a);
    let fingerprint_b = crate::spec_content_hash(source_b);
    store
        .upsert_spec("t", "Item", source_a, csdl, &fingerprint_a)
        .await
        .expect("stage candidate A");
    store
        .upsert_spec("t", "Item", source_b, csdl, &fingerprint_b)
        .await
        .expect("newer candidate B replaces staging");

    let error = store
        .persist_verified_spec_catalog_update(
            "t",
            &[("Item", fingerprint_a.as_str(), csdl)],
            &[],
            true,
            None,
        )
        .await
        .expect_err("candidate A verification must not publish candidate B");
    assert!(error.to_string().contains("fingerprint changed"));
    assert!(
        store
            .load_specs()
            .await
            .expect("load committed specs")
            .is_empty(),
        "no committed catalog may be created from mismatched staged bytes"
    );
}

#[tokio::test]
async fn concurrent_replica_replacements_commit_one_complete_catalog() {
    let url = sqlite_test_url("concurrent-spec-catalog-replacement");
    let store_a = TursoEventStore::new(&url, None)
        .await
        .expect("open first replica store");
    let store_b = TursoEventStore::new(&url, None)
        .await
        .expect("open second replica store");
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let source_a = "[automaton]\nname = \"ItemA\"\n";
    let source_b = "[automaton]\nname = \"ItemB\"\n";
    let fingerprint_a = crate::spec_content_hash(source_a);
    let fingerprint_b = crate::spec_content_hash(source_b);
    let specs_a = [("ItemA", source_a, fingerprint_a.as_str())];
    let specs_b = [("ItemB", source_b, fingerprint_b.as_str())];

    let (result_a, result_b) = tokio::join!(
        store_a.persist_spec_catalog_update("t", &specs_a, csdl, &[], true, None),
        store_b.persist_spec_catalog_update("t", &specs_b, csdl, &[], true, None),
    );
    result_a.expect("first replica replacement must commit");
    result_b.expect("second replica replacement must commit");

    drop(store_a);
    drop(store_b);
    let reopened = TursoEventStore::new(&url, None)
        .await
        .expect("reopen catalog after both replacements");
    let committed = reopened
        .load_specs()
        .await
        .expect("load committed catalog")
        .into_iter()
        .filter(|row| row.tenant == "t")
        .map(|row| row.entity_type)
        .collect::<Vec<_>>();
    assert!(
        committed == ["ItemA"] || committed == ["ItemB"],
        "the final durable catalog must be one serialized replacement, got {committed:?}"
    );
    assert_eq!(
        reopened
            .spec_replacement_entity_types("t")
            .await
            .expect("load present authority"),
        committed,
        "reopen must recover the same single authoritative catalog"
    );
}

#[tokio::test]
async fn merge_without_constraints_preserves_them_across_restart_and_replace_clears_them() {
    let url = sqlite_test_url("merge-preserves-spec-catalog-constraints");
    let store = TursoEventStore::new(&url, None).await.expect("open store");
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let source_a = "[automaton]\nname = \"ItemA\"\n";
    let source_b = "[automaton]\nname = \"ItemB\"\n";
    let fingerprint_a = crate::spec_content_hash(source_a);
    let fingerprint_b = crate::spec_content_hash(source_b);
    let specs_a = [("ItemA", source_a, fingerprint_a.as_str())];
    let specs_b = [("ItemB", source_b, fingerprint_b.as_str())];
    let constraints = r#"version = 1
default_delete_policy = "restrict"

[[invariant]]
name = "payment_must_be_captured"
kind = "hard"
on = "Order.Submit"
assert = 'related(Payment, payment_id).status in ["Captured"]'
"#;

    store
        .persist_spec_catalog_update("t", &specs_a, csdl, &[], true, Some(constraints))
        .await
        .expect("seed replacement with constraints");
    store
        .persist_spec_catalog_update("t", &specs_b, csdl, &[], false, None)
        .await
        .expect("merge without constraints");
    drop(store);

    let reopened = TursoEventStore::new(&url, None)
        .await
        .expect("reopen after merge");
    let persisted = reopened
        .load_tenant_constraints()
        .await
        .expect("load constraints after restart");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].tenant, "t");
    assert_eq!(persisted[0].cross_invariants_toml, constraints);

    reopened
        .persist_spec_catalog_update("t", &specs_a, csdl, &[], true, None)
        .await
        .expect("constraint-free replacement");
    drop(reopened);
    let final_store = TursoEventStore::new(&url, None)
        .await
        .expect("reopen after replacement");
    assert!(
        final_store
            .load_tenant_constraints()
            .await
            .expect("load cleared constraints")
            .is_empty()
    );
}
