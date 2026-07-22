use super::*;

#[test]
fn idempotency_actor_key_matches_actor_persistence_id_shape() {
    let tenant = TenantId::new("acme");

    assert_eq!(
        idempotency_actor_key(&tenant, "WorkCycle", "wc-1"),
        "acme:WorkCycle:wc-1"
    );
}
