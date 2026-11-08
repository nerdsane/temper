use super::*;

/// A doc that waits, then a timer finishes it. Add makes it readable when
/// `opens` says so; Close makes it unreadable while it waits.
fn doc(opens: bool) -> String {
    format!(
        r#"
[automaton]
name = "Doc"
states = ["Unmade", "Open", "Waiting", "Done"]
initial = "Unmade"

[[state]]
name = "open"
type = "bool"
initial = false

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Open"
effect = ["open = {opens}"]

[[action]]
name = "Wait"
kind = "input"
from = ["Open"]
to = "Waiting"

[[action]]
name = "Close"
kind = "input"
from = ["Waiting"]
to = "Waiting"
effect = ["open = false"]

[[action]]
name = "Finish"
kind = "internal"
from = ["Waiting"]
to = "Done"

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "Finish"

[[liveness]]
name = "AWaitEnds"
eventually = "status != 'Waiting'"
"#
    )
}

/// Members make the moves `moves` names, and read a doc while it is open.
fn members(moves: &str) -> String {
    format!(
        r#"
permit(principal, action in [{moves}], resource is Doc)
when {{ principal has role && principal.role == "member" }};
permit(principal, action == Action::"read", resource is Doc)
when {{ principal has role && principal.role == "member" && resource.open }};
"#
    )
}

fn check(opens: bool, moves: &str) -> JointLivenessResult {
    let doc = parse_automaton(&doc(opens)).expect("fixture parses");
    let analysis = SendAnalysis::new(&members(moves)).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&[&doc], Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

const ALL: &str = r#"Action::"Add", Action::"Wait", Action::"Close""#;

#[test]
fn a_timer_whose_setter_may_no_longer_read_its_record_makes_no_move() {
    // The member waits while the doc is open, then closes it: when the
    // timer fires they may no longer read it, so Finish is withheld.
    let result = check(true, ALL);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Doc=Waiting")),
        "{result}"
    );
    let text = result.to_string();
    assert!(text.contains("withheld"), "{result}");
    assert!(text.contains("NEVER SETTLES"), "{result}");
}

#[test]
fn a_timer_whose_setter_keeps_their_standing_fires() {
    let result = check(true, r#"Action::"Add", Action::"Wait""#);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_timer_whose_setter_could_not_read_its_record_when_set_fires() {
    // Nobody may read a doc that never opened, so its setter had no
    // standing to lose.
    let result = check(false, ALL);
    assert!(result.holds(), "{result}");
}
