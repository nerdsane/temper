//! A principal-attribute refusal happens before app specs or rules are saved.

use super::*;

const STATUS: &str = r#"
[automaton]
name = "Order_roles"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
principal_record = true
"#;

const SET: &str = r#"
[automaton]
name = "Order"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]
principal_record = true
principal_record_set = "roles"

[[state]]
name = "roles"
type = "list"
initial = []

[[action]]
name = "Touch"
from = ["Active"]
"#;

#[tokio::test]
async fn principal_collision_refuses_an_app_before_persistent_writes() {
    let fx = Fixture::new("principal-collision").await;
    let tenant = "principal-collision-tenant";
    let original_rules = "version = 1\ndefault_delete_policy = \"restrict\"\n";
    fx.state
        .registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            tenant,
            parse_csdl(ORDER_CSDL).unwrap(),
            ORDER_CSDL.into(),
            &[("Order_roles", STATUS)],
            Some(original_rules.into()),
            false,
        )
        .unwrap();
    fx.store
        .upsert_spec(
            tenant,
            "Order_roles",
            STATUS,
            ORDER_CSDL,
            &temper_store_turso::spec_content_hash(STATUS),
        )
        .await
        .unwrap();
    fx.store.commit_specs(tenant).await.unwrap();
    fx.store
        .upsert_tenant_constraints(tenant, original_rules)
        .await
        .unwrap();

    let app = format!("refusal-principal-{}", uuid::Uuid::new_v4().simple());
    let new_rules = "version = 1\ndefault_delete_policy = \"cascade\"\n";
    fx.write_app(&app, ORDER_CSDL, Some(new_rules));
    fs::write(fx.apps_root.join(&app).join("specs/order.ioa.toml"), SET).unwrap();
    let error = fx
        .install(tenant, &app)
        .await
        .expect_err("attribute collision");
    assert!(error.contains("principal.Order_roles"), "{error}");
    let rows = fx.store.load_specs().await.unwrap();
    assert_eq!(rows.len(), 1, "refusal must not commit the app's specs");
    assert_eq!(rows[0].entity_type, "Order_roles");
    assert_eq!(rows[0].ioa_source, STATUS);
    let rules = fx.store.load_tenant_constraints().await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].cross_invariants_toml, original_rules);
    assert_eq!(rules[0].version, 1);
    assert!(!fx.store.is_app_installed(tenant, &app).await.unwrap());
    assert!(
        fx.state
            .registry
            .read()
            .unwrap()
            .get_spec(&TenantId::new(tenant), "Order")
            .is_none()
    );
}
