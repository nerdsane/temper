//! Production dispatch resolves reference defaults added after records existed.
mod common;
use serde_json::json;
use temper_runtime::tenant::TenantId;

const BASE: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Open", "Closed", "Submitted"]
initial = "Draft"
[[action]]
name = "Open"
from = ["Draft"]
to = "Open"
[[action]]
name = "Close"
from = ["Open"]
to = "Closed"
[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
"#;

#[tokio::test]
async fn invariant_reference_defaults_resolve_older_records_in_dispatch() {
    for (case, declaration) in [
        ("scalar", "type = \"string\"\ninitial = \"parent\""),
        ("list", "type = \"list\"\ninitial = [\"parent\"]"),
    ] {
        let (state, _) = common::build_single_tenant_state(
            0,
            &format!("reference-default-{case}"),
            "default",
            &[("Order", BASE)],
        );
        let tenant = TenantId::default();
        for id in ["parent", "old", "while-closed", "explicit-empty"] {
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Order",
                    id,
                    if id == "explicit-empty" {
                        json!({"parent_id": if case == "list" { json!([]) } else { json!("") }})
                    } else {
                        json!({})
                    },
                )
                .await
                .unwrap();
        }
        let spec = format!(
            r#"{BASE}
[[state]]
name = "parent_id"
{declaration}
[[invariant]]
name = "OpenParent"
assert = "status == 'Submitted' => Order[parent_id].status == 'Open'"
"#
        );
        {
            let mut registry = state.registry.write().unwrap();
            registry.register_tenant(
                "default",
                temper_spec::csdl::parse_csdl(common::CSDL_XML).unwrap(),
                common::CSDL_XML.to_owned(),
                &[("Order", spec.as_str())],
            );
        }
        let before = state
            .get_tenant_entity_state(&tenant, "Order", "old")
            .await
            .unwrap()
            .state;
        assert!(before.fields.get("parent_id").is_none());
        let refused = common::dispatch(&state, &tenant, "Order", "old", "Submit", json!({}))
            .await
            .unwrap();
        assert!(!refused.success);
        assert!(refused.error.unwrap().contains("OpenParent"));
        assert_eq!(
            serde_json::to_value(&refused.state).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert!(
            common::dispatch(&state, &tenant, "Order", "parent", "Open", json!({}))
                .await
                .unwrap()
                .success
        );
        let accepted = common::dispatch(&state, &tenant, "Order", "old", "Submit", json!({}))
            .await
            .unwrap();
        assert!(accepted.success, "{case}: {:?}", accepted.error);
        // A stored empty reference takes precedence over the new default.
        let empty = common::dispatch(
            &state,
            &tenant,
            "Order",
            "explicit-empty",
            "Submit",
            json!({}),
        )
        .await
        .unwrap();
        assert!(
            !empty.success,
            "{case}: explicit empty reference used the default"
        );
        assert!(
            common::dispatch(&state, &tenant, "Order", "parent", "Close", json!({}))
                .await
                .unwrap()
                .success
        );
        let closed = common::dispatch(
            &state,
            &tenant,
            "Order",
            "while-closed",
            "Submit",
            json!({}),
        )
        .await
        .unwrap();
        assert!(!closed.success);
        assert!(closed.error.unwrap().contains("OpenParent"));
    }
}
