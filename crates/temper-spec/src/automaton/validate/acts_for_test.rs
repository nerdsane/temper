//! A reaction's `as` names a field only its user's own identity writes, or
//! the spec is refused: anyone else who could write the field could choose
//! whose identity the reaction acts with.

use crate::automaton::parser::{LivenessEnforcement, parse_automaton_with_liveness};

/// A pusher record: `Claim` puts its caller's own identity in `person`, and a
/// hook's `Pushed` sets off `follow`, made for whoever `person` names.
/// `claim` adds to `Claim`'s declaration, `trigger` to the trigger's.
fn spec(strict: bool, claim: &str, trigger: &str) -> String {
    format!(
        r#"
[automaton]
name = "Pusher"
states = ["Unclaimed", "Claimed", "Followed"]
initial = "Unclaimed"
strict_action_params = {strict}

[[state]]
name = "person"
type = "string"
initial = ""

[[state]]
name = "note"
type = "string"
initial = ""

[[action]]
name = "Claim"
kind = "input"
from = ["Unclaimed"]
to = "Claimed"
{claim}

[[action]]
name = "Pushed"
kind = "input"
from = ["Unclaimed", "Claimed"]

[[action.triggers]]
name = "follow"
kind = "entity"
target_entity = "Pusher"
target_action = "Follow"
resolve_target = {{ kind = "same_id" }}
{trigger}

[[action]]
name = "Follow"
kind = "input"
from = ["Claimed"]
to = "Followed"
"#
    )
}

const SUBJECT: &str =
    r#"params = [{ name = "person", type = "string", source = "authenticated_subject" }]"#;

fn parse(source: &str) -> Result<(), String> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[track_caller]
fn refused(source: &str, needle: &str) {
    match parse(source) {
        Ok(()) => panic!("loaded a spec that should be refused with `{needle}`:\n{source}"),
        Err(error) => assert!(error.contains(needle), "`{needle}` not in: {error}"),
    }
}

#[test]
fn a_field_only_its_users_identity_writes_loads() {
    parse(&spec(true, SUBJECT, r#"as = "person""#)).expect("loads");
    // An effect may clear it: '' names no one.
    let clears = format!(
        "{SUBJECT}\n\n[[action]]\nname = \"Unclaim\"\nkind = \"input\"\nfrom = [\"Claimed\"]\nto = \"Unclaimed\"\neffect = [\"person = ''\"]"
    );
    parse(&spec(true, &clears, r#"as = "person""#)).expect("clearing loads");
}

#[test]
fn a_field_the_caller_supplies_is_refused() {
    let plain = r#"params = [{ name = "person", type = "string" }]"#;
    refused(
        &spec(true, plain, r#"as = "person""#),
        "takes 'person' from its caller",
    );
}

#[test]
fn a_field_an_effect_copies_into_is_refused() {
    let copies = r#"params = [{ name = "who", type = "string" }]
effect = ["person = params.who"]"#;
    refused(
        &spec(true, copies, r#"as = "person""#),
        "writes 'person' in an effect",
    );
    let from_field = r#"effect = ["person = note"]"#;
    refused(
        &spec(true, from_field, r#"as = "person""#),
        "writes 'person' in an effect",
    );
}

#[test]
fn a_spec_whose_bodies_write_any_field_is_refused() {
    refused(
        &spec(false, SUBJECT, r#"as = "person""#),
        "needs `strict_action_params = true`",
    );
}

#[test]
fn as_beside_principal_or_on_a_field_it_lacks_is_refused() {
    refused(
        &spec(true, SUBJECT, "as = \"person\"\nprincipal = \"line\""),
        "`as` and `principal` both say",
    );
    refused(
        &spec(true, SUBJECT, r#"as = "nobody""#),
        "not a string field",
    );
}
