//! Carries: a version that strands records is refused with the carry that
//! fixes it, and a carry is tried before the install and made after it.

use super::*;

/// What the check holds, from the refusing side: a version that would leave a
/// stored record in a state it drops is refused, nothing is swapped, and the
/// answer opens with the `[[carry]]` that would fix it.
#[tokio::test]
async fn a_dropped_state_that_holds_records_is_refused_with_the_carry_that_fixes_it() {
    let places = Fixture::new("dropped").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let refused = places
        .install(&settles())
        .await
        .expect_err("a version that drops Placed, where p1 is, with no carry is refused");
    let carry = refused
        .find("[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"")
        .unwrap_or_else(|| panic!("the refusal gives the carry to Settled: {refused}"));
    let record = refused
        .find("p1")
        .unwrap_or_else(|| panic!("the refusal names p1: {refused}"));
    assert!(carry < record, "the carry comes first: {refused}");

    // Nothing was swapped: p1 moves under the version it was stored with.
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

#[tokio::test]
async fn a_carry_moves_stored_records_out_of_a_dropped_state() {
    let places = Fixture::new("carried").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;
    places.create("p2").await;

    let installed = places
        .install(&settles_carried())
        .await
        .expect("a version that says where Placed records go is installed");
    let carrying: Vec<&str> = installed
        .warnings
        .iter()
        .map(|warning| warning.message.as_str())
        .collect();
    assert_eq!(
        carrying,
        [
            "Place: carry:Placed:1 carries 1 record once the install has answered; \
          until then they wait in their old state"
        ]
    );
    // The install answered before carrying: p1 waits where it was, in a state
    // the version dropped, and no caller move starts there.
    assert_eq!(places.status("p1").await, "Placed");
    let waiting = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(
        waiting,
        (false, "Placed".to_string()),
        "p1 waits for its carry"
    );

    let report = places.carry().await;
    assert_eq!((report.carried, report.failed), (1, Vec::new()));
    assert_eq!(places.status("p1").await, "Settled");
    assert_eq!(places.status("p2").await, "Unplaced", "p2 was never Placed");
    assert_eq!(
        places.carry().await.carried,
        0,
        "a carried record is not carried again"
    );
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

/// A record waiting for its carry keeps it: a later version that drops the
/// carry before the record has moved is refused, with the carry to keep.
#[tokio::test]
async fn a_version_that_drops_a_carry_its_records_still_wait_for_is_refused() {
    let places = Fixture::new("dropped-carry").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;
    places
        .install(&settles_carried())
        .await
        .expect("a version that says where Placed records go is installed");
    assert_eq!(places.status("p1").await, "Placed");

    let refused = places
        .install(&settles())
        .await
        .expect_err("p1 still waits in Placed for the carry this version drops");
    assert!(
        refused.contains("[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"")
            && refused.contains("p1"),
        "{refused}"
    );
    assert_eq!(
        places.carry().await.carried,
        1,
        "the installed carry still runs"
    );
    assert_eq!(places.status("p1").await, "Settled");
}

/// An install into a running server answers, then carries in the background.
#[tokio::test]
async fn an_install_into_a_running_server_carries_after_it_answers() {
    let places = Fixture::new("running").await;
    places.state.server.start_runtime().await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    places
        .install(&settles_carried())
        .await
        .expect("a version that says where Placed records go is installed");
    places.status_becomes("p1", "Settled").await;
}

/// A process that stopped before its carries were made leaves the records
/// where they were; the next start makes them, under the version it serves.
#[tokio::test]
async fn a_start_makes_the_carries_a_stopped_process_left() {
    let places = Fixture::new("restart").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;
    places
        .install(&settles_carried())
        .await
        .expect("a version that says where Placed records go is installed");
    assert_eq!(places.status("p1").await, "Placed");

    // The next process installs the same version before its runtime starts,
    // as `temper serve` does, then starts.
    let places = places.restarted().await;
    places
        .install(&settles_carried())
        .await
        .expect("the same version installs again");
    assert_eq!(places.status("p1").await, "Placed");
    places.state.server.start_runtime().await;
    places.status_becomes("p1", "Settled").await;
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

#[tokio::test]
async fn a_guard_that_leaves_records_no_move_is_refused_with_the_values_to_set() {
    let places = Fixture::new("guard").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let refused = places
        .install(&needs_ready(false))
        .await
        .expect_err("a version under which p1, Placed, can make no move is refused");
    for words in [
        "[[carry]]\nfrom = \"Placed\"\nset = { ready = true }",
        "p1",
        "Bring",
    ] {
        assert!(
            refused.contains(words),
            "the refusal has {words}: {refused}"
        );
    }
}

#[tokio::test]
async fn a_carry_that_sets_what_a_guard_wants_is_installed_and_the_record_moves() {
    let places = Fixture::new("set").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    places
        .install(&format!(
            "{}\n[[carry]]\nfrom = \"Placed\"\nset = {{ ready = true }}\n",
            needs_ready(false)
        ))
        .await
        .expect("a version that carries Placed records with ready set is installed");
    let waiting = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(
        waiting,
        (false, "Placed".to_string()),
        "p1 waits for its carry"
    );
    assert_eq!(places.carry().await.carried, 1);
    assert_eq!(places.status("p1").await, "Placed");
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

/// A version may narrow a move, so long as the records keep another: it is
/// installed and the install reports it.
#[tokio::test]
async fn a_narrowed_move_with_another_way_out_is_installed_and_reported() {
    let places = Fixture::new("narrowed").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let installed = places
        .install(&needs_ready(true))
        .await
        .expect("p1 can still be made Ready, so the version is installed");
    assert!(
        installed
            .warnings
            .iter()
            .any(|warning| warning.check == "spec_change"
                && warning.message.contains("Bring")
                && warning.message.contains("p1")),
        "{:?}",
        installed.warnings
    );
    let refused = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(refused, (false, "Placed".to_string()), "p1 waits for Ready");
    assert_eq!(
        places.act("p1", "Ready", serde_json::json!({})).await,
        (true, "Placed".to_string())
    );
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

#[tokio::test]
async fn a_carry_that_would_break_an_invariant_is_refused_and_nothing_is_written() {
    let places = Fixture::new("invariant").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    let refused = places
        .install(&format!(
            "{}\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\n",
            settles_once_brought()
        ))
        .await
        .expect_err("p1 was brought before brought was counted, so Settled would break the rule");
    for words in ["SettledWasBrought", "p1"] {
        assert!(
            refused.contains(words),
            "the refusal has {words}: {refused}"
        );
    }
    assert_eq!(places.status("p1").await, "Placed");
    let brought = places
        .act("p1", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

/// A carry is tried with the related records the new version's rules read,
/// resolved as a move resolves them: a Settled place may only Leave while its
/// parent is open, so a place under an open parent may be carried there and
/// one under a closed parent may not.
#[tokio::test]
async fn a_carry_is_tried_with_the_related_records_its_rules_read() {
    let places = Fixture::new("related").await;
    places.install(PLACE).await.expect("first version");
    places.open_and_closed_parents().await;
    places
        .placed_with("p1", serde_json::json!({ "parent_id": "open" }))
        .await;
    places
        .placed_with("p2", serde_json::json!({ "parent_id": "closed" }))
        .await;

    let refused = places
        .install(&leaves_while_open(
            "\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\n",
        ))
        .await
        .expect_err("p2's parent is closed, so in Settled it could not Leave");
    assert!(
        refused.contains("Leave") && refused.contains("p2"),
        "{refused}"
    );
    assert!(
        !refused.contains("p1"),
        "p1's parent is open, so in Settled it can Leave: {refused}"
    );

    places
        .install(&leaves_while_open(
            "\n[[carry]]\nfrom = \"Placed\"\nguard = \"parent_id == 'open'\"\nto = \"Settled\"\n\n\
             [[carry]]\nfrom = \"Placed\"\nto = \"Unplaced\"\n",
        ))
        .await
        .expect("p1 goes to Settled under its open parent, p2 back to Unplaced");
    assert_eq!(places.carry().await.carried, 2);
    assert_eq!(places.status("p1").await, "Settled");
    assert_eq!(places.status("p2").await, "Unplaced");
}

/// A carry that points a record at another related record is judged with
/// that one: p1 sits under a closed parent, and a carry that moves it under
/// the open one lets it Leave from Settled.
#[tokio::test]
async fn a_carry_that_repoints_a_record_is_judged_with_its_new_related_record() {
    let places = Fixture::new("repointed").await;
    places.install(PLACE).await.expect("first version");
    places.open_and_closed_parents().await;
    places
        .placed_with("p1", serde_json::json!({ "parent_id": "closed" }))
        .await;

    places
        .install(&leaves_while_open(
            "\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\nset = { parent_id = \"open\" }\n",
        ))
        .await
        .expect("carried under the open parent, p1 can Leave");
    assert_eq!(places.carry().await.carried, 1);
    assert_eq!(places.status("p1").await, "Settled");
    assert_eq!(
        places.act("p1", "Leave", serde_json::json!({})).await,
        (true, "Unplaced".to_string())
    );
}
