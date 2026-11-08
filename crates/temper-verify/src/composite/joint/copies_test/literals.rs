//! Which literals a field is offered: those a read names, one of each kind.

use super::*;

fn kinds() -> Model {
    model!("literals", "Kind", "Board")
}

/// `kinds` with `Spoil` closed off, offering each of two inputs `choices`:
/// nothing but the list reads what they hold.
fn offering(choices: &str) -> Model {
    let closed = fixed(&kinds(), 0, "kind == 'c'", "kind == 'z'");
    let all = "['a', 'b', 'c']";
    let model = fixed(
        &closed,
        0,
        &format!("params.kind in {all}"),
        &format!("params.kind in {choices}"),
    );
    fixed(
        &model,
        0,
        &format!("params.other in {all}"),
        &format!("params.other in {choices}"),
    )
}

#[test]
fn literals_no_read_tells_apart_are_walked_as_one() {
    let three = check(&offering("['a', 'b', 'c']"), 1);
    let one = check(&offering("['a']"), 1);
    assert_eq!(three.states, one.states, "{three} {one}");
}

#[test]
fn a_literal_a_guard_names_stays_its_own() {
    let moves = moves(&check(&kinds(), 1));
    assert!(moves.iter().any(|m| m.contains("kind='c'")), "{moves:?}");
}

#[test]
fn two_fields_compared_keep_their_literals() {
    // Taken as one value, the two would always be equal and `Spoil` closed.
    let compared = fixed(&kinds(), 0, "kind == 'c'", "kind != other");
    let moves = moves(&check(&compared, 1));
    assert!(moves.iter().any(|m| m.contains("Kind.Spoil")), "{moves:?}");
}

#[test]
fn a_policy_that_reads_the_field_keeps_its_literals() {
    // `Spoil` comes as a reaction, which Cedar decides on the record. Taken as
    // one value, the first would stand for all, and Cedar refuse every one.
    let spoil = "[[action]]\nname = \"Spoil\"";
    let reacts = format!(
        "[[action.triggers]]\nname = \"spoil\"\nkind = \"entity\"\n\
         target_entity = \"Kind\"\ntarget_action = \"Spoil\"\n\
         [action.triggers.resolve_target]\nkind = \"same_id\"\n\n{spoil}"
    );
    let any = fixed(&kinds(), 0, "kind == 'c'", "kind in ['a', 'b', 'c']");
    let (sources, _) = fixed(&any, 0, spoil, &reacts);
    let policies = r#"permit(principal, action == Action::"Choose", resource);
        permit(principal, action == Action::"Spoil", resource)
        when { context has via && context.via != "request"
            && resource has kind && resource.kind == "c" };"#;
    let moves = moves(&check(&(sources, policies), 1));
    assert!(moves.iter().any(|m| m.contains("kind='c'")), "{moves:?}");
}

#[test]
fn a_list_keeps_its_literals() {
    let moves = moves(&check(&model!("literals", "Items", "Board"), 1));
    assert!(moves.iter().any(|m| m.contains("item='c'")), "{moves:?}");
}

#[test]
fn a_literal_no_read_names_is_never_offered() {
    // Only Spoil's effect writes the literal, and every read of `note` asks
    // only whether it is empty: a made-up value answers as it does.
    let noted = fixed(
        &kinds(),
        0,
        "[[state]]\nname = \"board\"",
        "[[state]]\nname = \"note\"\ntype = \"string\"\ninitial = \"\"\n\n[[state]]\nname = \"board\"",
    );
    let noted = fixed(
        &noted,
        0,
        "\"other\", \"board\"]",
        "\"other\", \"board\", \"note\"]",
    );
    let noted = fixed(
        &noted,
        0,
        "guard = \"kind == 'c'\"",
        "guard = \"kind == 'c' && note != ''\"\neffect = [\"note = 'a note nothing reads'\"]",
    );
    let noted = fixed(
        &noted,
        0,
        "status == 'Spoiled' && kind != '' && other != '' => Board[board].status == 'Never'",
        "status == 'Spoiled' && note != '' => Board[board].status == 'Open'",
    );
    let moves = moves(&check(&noted, 1));
    assert!(moves.iter().any(|m| m.contains("Kind.Spoil")), "{moves:?}");
    assert!(
        !moves.iter().any(|m| m.contains("a note nothing reads")),
        "{moves:?}"
    );
}
