//! A text literal reaches only the ids and fields that can meet it.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A note with `messages` actions that each write their own message, which
/// nothing compares with an id or with the box's fields (and which the walk
/// does not keep: anyone may also edit it), and that hands its box the
/// literal holder `'main'` when sent. The box's rule reads the note
/// the holder names, and is `rule` or else its tag is `'kept'`.
fn note_and_box(messages: usize, rule: &str) -> [String; 2] {
    let mut note = String::from(
        r#"
[automaton]
name = "Note"
states = ["Open", "Sent", "Gone"]
initial = "Open"

[[state]]
name = "message"
type = "string"
initial = ""

[[action]]
name = "Edit"
kind = "input"
from = ["Open"]
to = "Open"
params = [{ name = "message", type = "string" }]

[[action]]
name = "Send"
kind = "input"
from = ["Open"]
to = "Sent"

[[action.triggers]]
name = "hand"
kind = "entity"
target_entity = "Box"
target_action = "Hold"

[action.triggers.args]
holder = "'main'"

[action.triggers.resolve_target]
kind = "same_id"
"#,
    );
    for i in 0..messages {
        note.push_str(&format!(
            r#"
[[action]]
name = "Write{i}"
kind = "input"
from = ["Open"]
to = "Open"
guard = "message != 'note text number {i}, long enough to read as a message'"
effect = ["message = 'note text number {i}, long enough to read as a message'"]
"#
        ));
    }
    let boxed = format!(
        r#"
[automaton]
name = "Box"
states = ["Empty", "Held"]
initial = "Empty"

[[state]]
name = "holder"
type = "string"
initial = ""

[[state]]
name = "tag"
type = "string"
initial = ""

[[action]]
name = "Hold"
kind = "internal"
from = ["Empty"]
to = "Held"
params = [{{ name = "holder", type = "string" }}]

[[action]]
name = "Tag"
kind = "input"
from = ["Empty"]
to = "Empty"
params = [{{ name = "tag", type = "string" }}]

[[invariant]]
name = "rule"
assert = "{rule} || tag == 'kept'"
"#
    );
    [note, boxed]
}

/// Only a reaction holds a box: the note's `hand` is all that sets its holder.
const ONLY_REACTIONS_HOLD: &str = r#"
permit(principal, action, resource is Note);
permit(principal, action == Action::"Tag", resource is Box);
permit(principal, action == Action::"Hold", resource is Box) when { context.via == "reaction" };
"#;

/// The one result for `sources`, with `policies` (Cedar) deciding each move
/// as its sender.
fn check(sources: &[String], policies: Option<&str>) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = policies.map(|text| SendAnalysis::new(text).expect("fixture policies parse"));
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, analysis.as_ref(), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn literals_no_id_or_field_meets_do_not_multiply_the_walk() {
    // No action reaches Gone: the rule holds, and the whole walk is made.
    let rule = "Note[holder].status != 'Gone'";
    let few = check(&note_and_box(1, rule), None);
    let many = check(&note_and_box(12, rule), None);
    assert!(matches!(few.outcome, JointOutcome::Holds), "{few}");
    assert!(matches!(many.outcome, JointOutcome::Holds), "{many}");
    // The messages are neither ids nor the box's holder or tag: eleven more
    // of them start and walk no more states.
    assert_eq!(few.states, many.states, "{few}\n{many}");
}

#[test]
fn a_literal_that_names_a_record_still_reaches_its_id() {
    // The box holds 'main' once the note is sent, and nothing else: the
    // rule breaks only when the note's id is 'main', which the walk must
    // still start from.
    let rule = "Note[holder].status != 'Sent'";
    let result = check(&note_and_box(12, rule), Some(ONLY_REACTIONS_HOLD));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(
        path.iter()
            .any(|step| step.state.contains("Note=Sent<Id='main'")),
        "{result}"
    );
}

/// A note that, once sent, hands its box the holder `'main'` and then may
/// ping it to finish; a box tagged with any value while its note is open.
/// Cedar lets the ping finish a box only when its tag is `"main"`, which no
/// spec compares the tag with.
const PING_NOTE: &str = r#"
[automaton]
name = "Note"
states = ["Open", "Sent", "Gone"]
initial = "Open"

[[action]]
name = "Send"
kind = "input"
from = ["Open"]
to = "Sent"

[[action.triggers]]
name = "hand"
kind = "entity"
target_entity = "Box"
target_action = "Hold"

[action.triggers.args]
holder = "'main'"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Ping"
kind = "input"
from = ["Sent"]
to = "Sent"

[[action.triggers]]
name = "ping"
kind = "entity"
target_entity = "Box"
target_action = "Finish"

[action.triggers.resolve_target]
kind = "same_id"
"#;

const TAGGED_BOX: &str = r#"
[automaton]
name = "Box"
states = ["Empty", "Held", "Done"]
initial = "Empty"

[[state]]
name = "holder"
type = "string"
initial = ""

[[state]]
name = "tag"
type = "string"
initial = ""

[[action]]
name = "Tag"
kind = "input"
from = ["Empty"]
to = "Empty"
guard = "Note[Id].status == 'Open'"
params = [{ name = "tag", type = "string" }]

[[action]]
name = "Hold"
kind = "internal"
from = ["Empty"]
to = "Held"
params = [{ name = "holder", type = "string" }]

[[action]]
name = "Finish"
kind = "internal"
from = ["Held"]
to = "Done"

[[invariant]]
name = "rule"
assert = "status != 'Done' || tag == 'other' || holder == 'other' || Note[Id].status == 'Gone'"
"#;

const FINISH_WHEN_TAGGED_MAIN: &str = r#"
permit(principal, action in [Action::"Send", Action::"Ping", Action::"Tag"], resource);
permit(principal, action == Action::"Hold", resource is Box) when { context.via == "reaction" };
permit(principal, action == Action::"Finish", resource is Box) when {
    context.via == "reaction" && resource.tag == "main"
};
"#;

#[test]
fn cedar_may_take_a_made_up_value_for_a_literal_its_field_never_meets() {
    // Someone tags the box 'main' while its note is open, before anything
    // in the walk holds 'main'; the walk gives the tag a made-up value
    // instead. Once the box holds 'main' as its holder, that value must not
    // count as certainly not "main" to Cedar: the ping may finish the box.
    let sources = [PING_NOTE.to_string(), TAGGED_BOX.to_string()];
    let result = check(&sources, Some(FINISH_WHEN_TAGGED_MAIN));
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// Cedar lets the ping finish a box only when its tag equals its holder
/// and is not empty: a comparison of two of the record's fields.
const FINISH_WHEN_TAG_IS_HOLDER: &str = r#"
permit(principal, action in [Action::"Send", Action::"Ping", Action::"Tag", Action::"Label"], resource);
permit(principal, action == Action::"Hold", resource is Box) when { context.via == "reaction" };
permit(principal, action == Action::"Finish", resource is Box) when {
    context.via == "reaction" && resource.tag == resource.holder && resource.tag != ""
};
"#;

/// The ping note, handing its box as holder its own `label`, which anyone
/// may write while the note is open, instead of `'main'`.
fn labelled_note() -> String {
    PING_NOTE
        .replace("holder = \"'main'\"", "holder = \"label\"")
        .replace(
            "[[action]]\nname = \"Send\"",
            "[[state]]\nname = \"label\"\ntype = \"string\"\ninitial = \"\"\n\n\
             [[action]]\nname = \"Label\"\nkind = \"input\"\nfrom = [\"Open\"]\nto = \"Open\"\n\
             params = [{ name = \"label\", type = \"string\" }]\n\n\
             [[action]]\nname = \"Send\"",
        )
}

#[test]
fn cedar_comparing_two_fields_keeps_a_literal_one_holds_possible_for_the_other() {
    // The tag, set while the note is open, is a made-up value; the holder
    // is 'main'. The runtime may have tagged the box 'main' too, so the
    // policy's tag == holder may hold.
    let sources = [PING_NOTE.to_string(), TAGGED_BOX.to_string()];
    let result = check(&sources, Some(FINISH_WHEN_TAG_IS_HOLDER));
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

#[test]
fn cedar_comparing_two_made_up_values_from_different_classes_may_find_them_equal() {
    // The tag and the note's label are both made up, and nothing in the
    // specs compares them: one may still be written as the other, and the
    // walk must reach the box finished.
    let sources = [labelled_note(), TAGGED_BOX.to_string()];
    let result = check(&sources, Some(FINISH_WHEN_TAG_IS_HOLDER));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(
        path.iter().any(|step| step
            .action
            .as_deref()
            .is_some_and(|a| a.contains("Note.Label"))),
        "{result}"
    );
}
