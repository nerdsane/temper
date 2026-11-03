//! A kernel-bound parameter retains the authenticated agent label.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_platform::state::PlatformState;
use temper_server::request_context::AgentContext;
use temper_store_sim::SimEventStore;

use super::fixture::{
    LABEL, PERMIT_ALL, PERSON, agent, agent_labelled, call, person, read, server_with, tenant,
    until_status,
};

/// `Publish` keeps who made the post. `Send` gets there through a reaction,
/// and `Edit` is a later move that leaves it alone.
const POST_IOA: &str = r#"
[automaton]
name = "Post"
states = ["Draft", "Sending", "Posted"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Sending", "Posted"]
strict_action_params = true

[[state]]
name = "author"
type = "string"
initial = ""

[[action]]
name = "Publish"
kind = "input"
from = ["Draft"]
to = "Posted"
params = [{ name = "author", type = "string", source = "authenticated_agent_label" }]

[[action]]
name = "Send"
kind = "input"
from = ["Draft"]
to = "Sending"
params = []

[[action.triggers]]
name = "land"
kind = "entity"
target_entity = "Post"
target_action = "Land"
resolve_target = { kind = "same_id" }

[[action]]
name = "Land"
kind = "input"
from = ["Sending"]
to = "Posted"
params = [{ name = "author", type = "string", source = "authenticated_agent_label" }]

[[action]]
name = "Edit"
kind = "input"
from = ["Posted"]
params = []
"#;

async fn post_server(seed: u64) -> PlatformState {
    server_with(
        &SimEventStore::no_faults(seed),
        3600,
        PERMIT_ALL,
        &[("Post", POST_IOA)],
    )
    .await
}

/// Make `action` on a Post with `body`, as `token`, and say how it answered.
async fn post_move(
    state: &PlatformState,
    id: &str,
    action: &str,
    token: &str,
    body: &str,
) -> (StatusCode, Value) {
    state
        .server
        .get_or_create_tenant_entity(&tenant(), "Post", id, json!({}))
        .await
        .expect("the record is created");
    let request = Request::post(format!("/tdata/Posts('{id}')/Temper.AgentLabel.{action}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    call(state, request).await
}

/// A field a move binds keeps who made the record, after later moves change
/// `last_event_label`.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_binds_its_agents_label_and_later_moves_leave_it() {
    let state = post_server(11).await;
    let (status, body) = post_move(&state, "p", "Publish", &agent(), "{}").await;
    assert!(status.is_success(), "{status}: {body}");
    let post = read(&state, "Posts", "p").await;
    assert_eq!(post["fields"]["author"], LABEL, "{post}");

    let (status, body) = post_move(&state, "p", "Edit", &person(), "{}").await;
    assert!(status.is_success(), "{status}: {body}");
    let post = read(&state, "Posts", "p").await;
    assert_eq!(post["fields"]["author"], LABEL, "{post}");
    assert_eq!(post["last_event_by"], PERSON, "{post}");
    assert_eq!(post["last_event_label"], Value::Null, "{post}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_binds_the_label_of_the_agent_that_set_it_going() {
    let state = post_server(12).await;
    let (status, body) = post_move(&state, "p", "Send", &agent(), "{}").await;
    assert!(status.is_success(), "{status}: {body}");
    let post = until_status(&state, "Posts", "p", "Posted").await;
    assert_eq!(post["fields"]["author"], LABEL, "{post}");
}

/// A person in person has no label, even with one on their token, and neither
/// has an agent whose issuer gave it none. The move still lands.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_with_no_label_binds_it_empty() {
    let state = post_server(13).await;
    for (id, token) in [("in-person", person()), ("unnamed", agent_labelled(""))] {
        let (status, body) = post_move(&state, id, "Publish", &token, "{}").await;
        assert!(status.is_success(), "{id}: {status}: {body}");
        let post = read(&state, "Posts", id).await;
        assert_eq!(post["status"], "Posted", "{post}");
        assert_eq!(post["fields"]["author"], "", "{post}");
    }
}

/// The caller's JSON never supplies it, even when it says the true value.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_cannot_supply_the_label() {
    let state = post_server(14).await;
    for (id, supplied) in [("forged", "someone-else"), ("true", LABEL)] {
        let body = json!({ "author": supplied }).to_string();
        let (status, answer) = post_move(&state, id, "Publish", &agent(), &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}: {answer}");
        assert_eq!(
            answer["error"]["code"], "AuthenticatedParameter",
            "{answer}"
        );
        let post = read(&state, "Posts", id).await;
        assert_eq!(post["status"], "Draft", "{post}");
        assert_eq!(post["fields"]["author"], "", "{post}");
    }
}

/// A move no one makes has no caller to name, as with `authenticated_subject`.
#[tokio::test(flavor = "multi_thread")]
async fn the_kernels_own_identity_cannot_bind_a_label() {
    let state = post_server(15).await;
    state
        .server
        .get_or_create_tenant_entity(&tenant(), "Post", "p", json!({}))
        .await
        .expect("the record is created");
    let refused = state
        .server
        .dispatch_tenant_action(
            &tenant(),
            "Post",
            "p",
            "Publish",
            json!({}),
            &AgentContext::system(),
        )
        .await;
    assert!(refused.is_err(), "the kernel names no caller");
    assert_eq!(read(&state, "Posts", "p").await["status"], "Draft");
}
