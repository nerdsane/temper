use super::*;

/// The app as [`APP`], installed by a timer: Install waits, and the wait's
/// timer goes live and tells the place to bring.
const TIMED_APP: &str = r#"
[automaton]
name = "App"
states = ["Unmade", "Waiting", "Live"]
initial = "Unmade"

[[state]]
name = "commit"
type = "string"
initial = ""

[[action]]
name = "Install"
kind = "input"
from = ["Unmade"]
to = "Waiting"
params = [{ name = "commit", type = "string" }]

[[action.constraints]]
kind = "param_nonempty"
param = "commit"

[[action]]
name = "GoLive"
kind = "internal"
from = ["Waiting"]
to = "Live"

[[action.triggers]]
name = "place"
kind = "entity"
target_entity = "Place"
target_action = "Bring"
principal = "place"

[action.triggers.args]
commit = "commit"

[action.triggers.resolve_target]
kind = "same_id"

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "GoLive"
"#;

#[test]
fn a_reaction_cedar_lets_through_off_requests_is_counted_on() {
    let bring = r#"context has via && context.via != "request""#;
    let result = check(r#"["Waiting", "Stuck"]"#, bring);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_reaction_in_a_timers_chain_is_decided_as_one() {
    let place = place(r#"["Waiting", "Stuck"]"#);
    let result = check_specs(TIMED_APP, &place, r#"context.via == "timer""#);
    assert!(result.holds(), "{result}");
    let result = check_specs(TIMED_APP, &place, r#"context.via == "reaction""#);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Place=Unplaced")),
        "{result}"
    );
}
