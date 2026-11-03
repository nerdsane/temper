//! What a call is let do once its user's Member record is in one status or
//! another, in person and through an agent.

use axum::http::StatusCode;

use super::fixture::{
    ALICE, BOB, agent_for, app, in_person, list_notes, move_member, passivate_member, read_note,
    touch_note,
};

#[tokio::test]
async fn an_active_member_reads_and_a_user_with_no_record_does_not() {
    let state = app(1).await;
    assert_eq!(read_note(&state, &in_person(ALICE)).await, StatusCode::OK);
    assert_eq!(
        read_note(&state, &in_person(BOB)).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_removed_member_is_refused_on_the_next_call() {
    let state = app(2).await;
    let alice = in_person(ALICE);
    assert_eq!(read_note(&state, &alice).await, StatusCode::OK);
    assert_eq!(move_member(&state, ALICE, "Remove").await, "Removed");
    assert_eq!(
        read_note(&state, &alice).await,
        StatusCode::FORBIDDEN,
        "the same token, the call after Remove answered"
    );
    assert_eq!(move_member(&state, ALICE, "Rejoin").await, "Active");
    assert_eq!(read_note(&state, &alice).await, StatusCode::OK);
}

#[tokio::test]
async fn a_member_whose_actor_was_passivated_is_read_at_its_current_status() {
    let state = app(3).await;
    let alice = in_person(ALICE);
    assert_eq!(read_note(&state, &alice).await, StatusCode::OK);
    passivate_member(&state, ALICE).await;
    assert_eq!(read_note(&state, &alice).await, StatusCode::OK);
    assert_eq!(move_member(&state, ALICE, "Remove").await, "Removed");
    passivate_member(&state, ALICE).await;
    assert_eq!(
        read_note(&state, &alice).await,
        StatusCode::FORBIDDEN,
        "removed, then passivated"
    );
}

#[tokio::test]
async fn a_member_lists_the_notes_and_a_user_with_no_record_lists_none() {
    let state = app(4).await;
    assert_eq!(
        list_notes(&state, &in_person(ALICE)).await,
        (StatusCode::OK, vec!["n-1".to_string()])
    );
    assert_eq!(
        list_notes(&state, &in_person(BOB)).await,
        (StatusCode::OK, Vec::new())
    );
}

/// The note's fields are `Member = "Active"` and `actingForMember = "Active"`:
/// they reach Cedar as `context.*` and `resource.*`, never on the principal.
#[tokio::test]
async fn fields_named_like_the_attributes_admit_no_one() {
    let state = app(5).await;
    assert_eq!(
        read_note(&state, &in_person(BOB)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        touch_note(&state, &in_person(BOB)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        touch_note(&state, &agent_for(BOB)).await,
        StatusCode::FORBIDDEN
    );
}

/// An agent acting for a member passes the check that admits a member or
/// someone acting for one, and never the one that admits a member in person.
#[tokio::test]
async fn an_agent_acts_for_a_member_but_is_never_one() {
    let state = app(6).await;
    let agent = agent_for(ALICE);
    assert_eq!(touch_note(&state, &agent).await, StatusCode::OK);
    assert_eq!(
        read_note(&state, &agent).await,
        StatusCode::FORBIDDEN,
        "reading is for a member in person"
    );
    assert_eq!(move_member(&state, ALICE, "Remove").await, "Removed");
    assert_eq!(
        touch_note(&state, &agent).await,
        StatusCode::FORBIDDEN,
        "acting for a removed member"
    );
    assert_eq!(read_note(&state, &agent).await, StatusCode::FORBIDDEN);
}
