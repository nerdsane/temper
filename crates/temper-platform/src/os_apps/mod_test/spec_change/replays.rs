//! A record reads the same after a version that drops a state it went
//! through: replay restores each stored event's target and outcome, so a
//! state, and the moves into and out of it, can leave the spec without
//! changing a record that has already moved on.

use super::*;

/// The place without Bringing, or the moves into and out of it. Placing is
/// one move, and a place still Bringing is carried to Placed.
const PUTS: &str = r#"
[automaton]
name = "Place"
states = ["Unplaced", "Placed"]
initial = "Unplaced"
allow_indefinite_states = ["Unplaced", "Placed"]

[[state]]
name = "commit"
type = "string"
initial = ""

[[action]]
name = "Put"
kind = "input"
from = ["Unplaced", "Placed"]
to = "Placed"
params = [{ name = "commit", type = "string" }]

[[carry]]
from = "Bringing"
to = "Placed"
"#;

/// p1 went through Bringing to Placed under the first version, and p2 is
/// still Bringing. The next version drops Bringing, Bring and Placed, and
/// carries p2. Read again by a new process, which rebuilds each record from
/// its journal under the version it serves, p1 is what it was: the same
/// status, fields and moves.
#[tokio::test]
async fn a_record_that_went_through_a_dropped_state_reads_the_same_after_a_restart() {
    let places = Fixture::new("replays").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;
    places.create("p2").await;
    let brought = places
        .act("p2", "Bring", serde_json::json!({ "commit": "c2" }))
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
    let before = places.read("p1").await;
    assert_eq!(
        (before.status.as_str(), &before.fields["commit"]),
        ("Placed", &serde_json::json!("c1"))
    );

    places
        .install(PUTS)
        .await
        .expect("a version that drops Bringing and carries its records is installed");
    assert_eq!(places.carry().await.carried, 1);
    assert_eq!(places.status("p2").await, "Placed");

    let places = places.restarted().await;
    places
        .install(PUTS)
        .await
        .expect("the same version installs again");
    let after = places.read("p1").await;
    assert_eq!(after.status, before.status);
    assert_eq!(after.fields, before.fields);
    assert_eq!(after.counters, before.counters);
    assert_eq!(after.booleans, before.booleans);
    assert_eq!(moves(&after), moves(&before));
    assert_eq!(moves(&after), ["Created", "Bring", "Placed"]);
}

/// The moves a record's journal holds, in order.
fn moves(state: &temper_server::EntityState) -> Vec<String> {
    state
        .events
        .iter()
        .map(|event| event.action.clone())
        .collect()
}

impl Fixture {
    /// The place `id` as this process reads it.
    async fn read(&self, id: &str) -> temper_server::EntityState {
        self.state
            .server
            .get_tenant_entity_state(&self.tenant, "Place", id)
            .await
            .unwrap_or_else(|error| panic!("read Place {id}: {error}"))
            .state
    }
}
