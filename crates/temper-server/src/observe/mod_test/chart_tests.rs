use super::*;

// -- What a statechart is drawn from: `says` and `[[group]]` in spec detail --

/// The Order fixture with two labelled actions and nested groups.
fn order_with_chart_keys() -> String {
    let mut spec = ORDER_IOA
        .replacen(
            "name = \"SubmitOrder\"\n",
            "name = \"SubmitOrder\"\nsays = \"send it in\"\n",
            1,
        )
        .replacen(
            "name = \"CancelOrder\"\n",
            "name = \"CancelOrder\"\nsays = \"call it off\"\n",
            1,
        );
    spec.push_str(
        r#"
[[group]]
name = "Live"
says = "while the order is live"
states = ["Open", "On its way"]

[[group]]
name = "Open"
states = ["Draft", "Submitted", "Confirmed"]

[[group]]
name = "On its way"
says = "the order is with the courier"
states = ["Processing", "Shipped", "Delivered"]
"#,
    );
    spec
}

#[tokio::test]
async fn spec_detail_carries_what_actions_say_and_the_groups() {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let spec = order_with_chart_keys();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", spec.as_str())],
    );
    let state = ServerState::from_registry(ActorSystem::new("test-chart"), registry);
    let app = Router::new()
        .nest("/observe", build_observe_router())
        .with_state(state);

    let raw = observe_json(app, "/observe/specs/Order").await;

    let action = |name: &str| {
        raw["actions"]
            .as_array()
            .expect("actions is a list")
            .iter()
            .find(|a| a["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no action {name}"))
    };
    assert_eq!(action("SubmitOrder")["says"], "send it in");
    assert_eq!(action("CancelOrder")["says"], "call it off");
    assert_eq!(
        action("ConfirmOrder").get("says"),
        Some(&serde_json::Value::Null),
        "an action that says nothing reads says: null"
    );

    assert_eq!(
        raw["groups"],
        serde_json::json!([
            {
                "name": "Live",
                "says": "while the order is live",
                "states": ["Open", "On its way"]
            },
            {
                "name": "Open",
                "says": null,
                "states": ["Draft", "Submitted", "Confirmed"]
            },
            {
                "name": "On its way",
                "says": "the order is with the courier",
                "states": ["Processing", "Shipped", "Delivered"]
            }
        ])
    );
    // Group names are not states: the state list is unchanged.
    assert_eq!(raw["states"].as_array().unwrap().len(), 10);
}

#[tokio::test]
async fn a_spec_without_the_keys_reads_says_null_and_no_groups() {
    let raw = observe_json(build_test_app(), "/observe/specs/Order").await;
    assert_eq!(raw["groups"], serde_json::json!([]));
    let keys = |value: &serde_json::Value| -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    // The only additions to the response are `groups` and each `says`.
    assert_eq!(
        keys(&raw),
        [
            "actions",
            "entity_type",
            "groups",
            "initial_state",
            "invariants",
            "spec_version",
            "state_variables",
            "states",
            "terminal_states",
        ]
    );
    let actions = raw["actions"].as_array().expect("actions is a list");
    assert!(!actions.is_empty());
    for action in actions {
        assert_eq!(
            keys(action),
            [
                "effects", "from", "guards", "hint", "kind", "name", "params", "says", "to"
            ]
        );
        assert_eq!(
            action.get("says"),
            Some(&serde_json::Value::Null),
            "every action reads says: null: {action}"
        );
    }
}
