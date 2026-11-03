//! What the liveness check decides from the joint state rather than from the
//! specs alone: Cedar on a reaction that finds its record through a field,
//! and a count a reaction carries that its target must equal.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::super::room::{Bytes, Progress};

use super::*;

/// An app its owner deletes, naming its install; the delete purges the
/// install it names, as the service `delete`.
const APP: &str = r#"
[automaton]
name = "App"
states = ["Live", "Deleting"]
initial = "Live"

[[state]]
name = "delete_install"
type = "string"
initial = ""

[[action]]
name = "OwnerDelete"
kind = "input"
from = ["Live"]
to = "Deleting"
params = [{ name = "delete_install", type = "string" }]

[[action.constraints]]
kind = "param_nonempty"
param = "delete_install"

[[action.triggers]]
name = "purge"
kind = "entity"
target_entity = "Install"
target_action = "Purge"
principal = "delete"

[action.triggers.resolve_target]
kind = "field"
id_field = "delete_install"
"#;

/// An install of one app, purged only for that app's delete.
const INSTALL: &str = r#"
[automaton]
name = "Install"
states = ["Unmade", "Running", "Purged"]
initial = "Unmade"

[[state]]
name = "app_id"
type = "string"
initial = ""

[[action]]
name = "Install"
kind = "input"
from = ["Unmade"]
to = "Running"
params = [{ name = "app_id", type = "string" }]

[[action.constraints]]
kind = "param_nonempty"
param = "app_id"

[[action]]
name = "Purge"
kind = "input"
from = ["Running"]
to = "Purged"

[[liveness]]
name = "purged_only_for_its_app"
eventually = "status == 'Purged' => App[app_id].status == 'Deleting'"
"#;

/// Anyone outside may delete and install; `purge` is the condition under
/// which Cedar lets the delete service purge.
fn policies(purge: &str) -> String {
    format!(
        r#"
permit(principal, action in [Action::"OwnerDelete", Action::"Install"], resource);
permit(principal == Agent::"service:delete", action == Action::"Purge", resource is Install)
when {{ {purge} }};
"#
    )
}

fn check(specs: [&str; 2], policy_text: &str) -> JointLivenessResult {
    let automatons: Vec<Automaton> = specs
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policy_text).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// The record statuses each stuck case comes to rest in, in words.
fn resting(result: &JointLivenessResult) -> Vec<String> {
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("expected a stuck outcome, got {result}");
    };
    cases
        .iter()
        .map(|case| case.path.last().expect("a path has a start").state.clone())
        .collect()
}

#[test]
fn a_purge_cedar_allows_only_for_its_own_app_settles() {
    let own_app = "context has triggerSourceEntityId && resource has app_id && \
                   context.triggerSourceEntityId == resource.app_id";
    let result = check([APP, INSTALL], &policies(own_app));
    assert!(result.holds(), "{result}");
}

#[test]
fn a_purge_cedar_allows_for_any_app_purges_another_apps_install() {
    let result = check([APP, INSTALL], &policies("true"));
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Install=Purged")),
        "{result}"
    );
}

/// An app that asks its probe once, and is answered with the count it asked
/// with, which it takes only when it equals its own.
const ASKING_APP: &str = r#"
[automaton]
name = "App"
states = ["Idle", "Waiting", "Done"]
initial = "Idle"

[[state]]
name = "asked"
type = "counter"
initial = 0

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Waiting"
effect = ["asked += 1"]

[[action.triggers]]
name = "look"
kind = "entity"
target_entity = "Probe"
target_action = "Look"
principal = "app"

[action.triggers.args]
asked = "asked"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Answered"
kind = "input"
from = ["Waiting"]
to = "Done"
params = [{ name = "asked", type = "counter" }]

[[action.constraints]]
kind = "param_equals_field"
param = "asked"
field = "asked"
"#;

/// The probe: it keeps the count it was asked with (or, with `keeps` false,
/// does not) and answers with the count it holds. Its app is not left
/// waiting: the probe is never ahead of its app, so the condition holds in
/// Waiting for no state, and it reads both counts, so the model keeps them.
fn probe(keeps: bool) -> String {
    let effect = if keeps {
        r#"effect = ["asked = params.asked"]"#
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Probe"
states = ["Idle", "Looked"]
initial = "Idle"

[[state]]
name = "asked"
type = "counter"
initial = 0

[[action]]
name = "Look"
kind = "input"
from = ["Idle"]
to = "Looked"
params = [{{ name = "asked", type = "counter" }}]
{effect}

[[action.triggers]]
name = "answer"
kind = "entity"
target_entity = "App"
target_action = "Answered"
principal = "probe"

[action.triggers.args]
asked = "asked"

[action.triggers.resolve_target]
kind = "same_id"

[[liveness]]
name = "answered"
eventually = "App[Id].status == 'Waiting' => App[Id].asked < asked"
"#
    )
}

/// Anyone outside may ask; only the app's service looks, and only the
/// probe's answers.
const SERVICES: &str = r#"
permit(principal, action == Action::"Ask", resource);
permit(principal == Agent::"service:app", action == Action::"Look", resource);
permit(principal == Agent::"service:probe", action == Action::"Answered", resource);
"#;

#[test]
fn an_answer_carrying_the_count_asked_is_counted_on() {
    let result = check([ASKING_APP, &probe(true)], SERVICES);
    assert!(result.holds(), "{result}");
}

#[test]
fn an_answer_carrying_another_count_is_refused() {
    let result = check([ASKING_APP, &probe(false)], SERVICES);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("App=Waiting")),
        "{result}"
    );
}

/// An app whose ask has its probe look, naming itself by `name` (a quoted
/// literal), which the probe requires not empty and does not keep.
fn naming_app(name: &str) -> String {
    format!(
        r#"
[automaton]
name = "App"
states = ["Idle", "Asked"]
initial = "Idle"

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "look"
kind = "entity"
target_entity = "Probe"
target_action = "Look"
principal = "app"

[action.triggers.args]
by = "{name}"

[action.triggers.resolve_target]
kind = "same_id"
"#
    )
}

/// The probe, which its app is not left waiting on once it asked.
const NAMED_PROBE: &str = r#"
[automaton]
name = "Probe"
states = ["Idle", "Looked"]
initial = "Idle"

[[state]]
name = "by"
type = "string"
initial = ""

[[action]]
name = "Look"
kind = "input"
from = ["Idle"]
to = "Looked"
params = [{ name = "by", type = "string" }]

[[action.constraints]]
kind = "param_nonempty"
param = "by"

[[liveness]]
name = "looked"
eventually = "App[Id].status == 'Asked' => status == 'Looked'"
"#;

/// Anyone outside may ask; only the app's service looks.
const ASK_AND_LOOK: &str = r#"
permit(principal, action == Action::"Ask", resource);
permit(principal == Agent::"service:app", action == Action::"Look", resource);
"#;

#[test]
fn a_reaction_naming_its_sender_is_counted_on() {
    let result = check([&naming_app("'app'"), NAMED_PROBE], ASK_AND_LOOK);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_reaction_sending_an_empty_name_is_refused() {
    let result = check([&naming_app("''"), NAMED_PROBE], ASK_AND_LOOK);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Probe=Idle")),
        "{result}"
    );
}
