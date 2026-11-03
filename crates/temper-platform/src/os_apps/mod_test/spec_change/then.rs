//! A carry's `then` move: made after the carry as any move is, tried in
//! the dry run, and decided by the version's Cedar.

use super::*;

/// A carry's `then` move is made as any move is, with its triggers: carried
/// to Settled, p1 Leaves at once, and Leave's trigger closes its parent. The
/// dry run tries the `then` move, so one its guard refuses for p2 refuses the
/// install.
#[tokio::test]
async fn a_carry_then_move_is_made_with_its_triggers() {
    let places = Fixture::new("then").await;
    places.install(PLACE).await.expect("first version");
    places.open_and_closed_parents().await;
    places
        .placed_with("p1", serde_json::json!({ "parent_id": "open" }))
        .await;
    places
        .placed_with("p2", serde_json::json!({ "parent_id": "closed" }))
        .await;

    let invalid = places
        .install(&leaves_and_closes(
            "\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\nthen = \"Bring\"\n",
        ))
        .await
        .expect_err("Bring does not start from Settled");
    assert!(
        invalid.contains("does not start from 'Settled'"),
        "{invalid}"
    );

    let refused = places
        .install(&leaves_and_closes(
            "\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\nthen = \"Leave\"\n",
        ))
        .await
        .expect_err("p2's parent is closed, so it cannot Leave");
    assert!(
        refused.contains("then move Leave") && refused.contains("p2"),
        "{refused}"
    );
    assert!(!refused.contains("p1"), "p1's parent is open: {refused}");

    places
        .install(&leaves_and_closes(
            "\n[[carry]]\nfrom = \"Placed\"\nguard = \"parent_id == 'open'\"\nto = \"Settled\"\n\
             then = \"Leave\"\n\n[[carry]]\nfrom = \"Placed\"\nto = \"Unplaced\"\n",
        ))
        .await
        .expect("p1 Leaves under its open parent, p2 goes back to Unplaced");
    let report = places.carry().await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.carried, 2);
    assert_eq!(places.status("p1").await, "Unplaced");
    assert_eq!(places.status("p2").await, "Unplaced");
    assert_eq!(places.status_of("Parent", "open").await, "Closed");
}

/// A policy can keep a `then` move to the platform, which makes it with
/// `context.carryMove`. A record field of that name does not stand in for it,
/// so a caller is refused the same action.
#[tokio::test]
async fn a_policy_can_keep_a_then_move_to_the_platform() {
    let places = Fixture::new("then-cedar").await;
    places.install(PLACE).await.expect("first version");
    places.open_and_closed_parents().await;
    places
        .placed_with("p1", serde_json::json!({ "parent_id": "open" }))
        .await;
    fs::write(
        places.apps_root.join(APP).join("policies/leave.cedar"),
        "forbid(principal, action == Action::\"Leave\", resource) unless { context has carryMove };",
    )
    .expect("write the policy");

    places
        .install(&leaves_and_closes(
            "\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\nthen = \"Leave\"\n",
        ))
        .await
        .expect("p1 can Leave under its open parent");
    let report = places.carry().await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(places.status("p1").await, "Unplaced");

    let caller = AgentContext::for_service("caller")
        .security_ctx
        .expect("a service context carries its security context");
    let spoofed = std::collections::BTreeMap::from([(
        "carryMove".to_string(),
        serde_json::json!("carry:Placed:1"),
    )]);
    let decision = places.state.server.authorize_with_context(
        &caller,
        "Leave",
        "Place",
        &spoofed,
        places.tenant.as_str(),
    );
    assert!(
        decision.is_err(),
        "a record's carryMove field let a caller Leave"
    );
}
