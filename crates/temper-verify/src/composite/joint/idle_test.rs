//! A reaction whose landing changes nothing: the walk neither holds it in
//! flight nor splits on a guard it cannot decide, since the records end the
//! same whether it fires or not.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// `Go` moves `Many` to Done and sets off twelve pokes, each guarded on
/// `Far`, a type outside the rule's scope, so the model cannot decide any
/// of them. A poke leaves `Many` as it was.
fn many_pokes(rule: &str) -> Vec<String> {
    let mut many = String::from(
        "[automaton]\nname = 'Many'\nstates = ['Start', 'Done']\ninitial = 'Start'\n\n\
         [[action]]\nname = 'Go'\nkind = 'input'\nfrom = ['Start']\nto = 'Done'\n",
    );
    for i in 0..12 {
        many.push_str(&format!(
            "\n[[action.triggers]]\nname = 'poke{i}'\nkind = 'entity'\n\
             target_entity = 'Many'\ntarget_action = 'Poke'\n\
             guard = \"Far[Id].status == 'Open'\"\n\
             [action.triggers.resolve_target]\nkind = 'same_id'\n"
        ));
    }
    many.push_str(&format!(
        "\n[[action]]\nname = 'Poke'\nkind = 'input'\nfrom = ['Done']\nto = 'Done'\n\
         \n[[invariant]]\nname = 'rule'\nassert = \"{rule}\"\n"
    ));
    let other = "[automaton]\nname = 'Other'\nstates = ['Open', 'Shut']\ninitial = 'Open'\n\n\
                 [[action]]\nname = 'Stay'\nkind = 'input'\nfrom = ['Open']\nto = 'Open'\n";
    let far = "[automaton]\nname = 'Far'\nstates = ['Open']\ninitial = 'Open'\n";
    vec![many, other.to_string(), far.to_string()]
}

fn check(sources: &[String]) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis =
        SendAnalysis::new("permit(principal, action, resource);").expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_break_past_reactions_that_change_nothing_is_sure() {
    // Whether each poke fires or not, Go leaves Many Done and Other Open:
    // the break rests on no guard the model cannot decide.
    let result = check(&many_pokes(
        "status == 'Start' || Other[Id].status == 'Shut'",
    ));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert_eq!(path.len(), 2, "{result}");
    assert!(!result.to_string().contains("BROKEN if"), "{result}");
}

#[test]
fn a_rule_reactions_that_change_nothing_cannot_break_holds() {
    let result = check(&many_pokes(
        "status == 'Start' || status == 'Done' || Other[Id].status == 'Open'",
    ));
    assert!(result.holds(), "{result}");
    assert!(result.states <= 4, "{result}");
}
