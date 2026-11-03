//! The kinds of outside sender a model keeps apart.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::{Asker, Cause, Hop, Kind, May, Question, SendAnalysis};
use temper_spec::automaton::Automaton;

/// The kinds of outside sender `policies` tell apart on what a chain from
/// outside asks: a request to one of `roots`, and the `inheriting` moves
/// as reactions, in either chain. Kinds that may request nothing there send
/// nothing, and are left out. None when the policies tell no two apart
/// on those reactions.
pub(super) fn outside(
    by_name: &BTreeMap<&str, &Automaton>,
    policies: &SendAnalysis,
    roots: &BTreeSet<String>,
    inheriting: &BTreeSet<(String, String)>,
    reserve: &dyn Fn(usize) -> Result<(), String>,
) -> Result<Option<Vec<Kind>>, String> {
    let mut requests = Vec::new();
    for entity in roots {
        for action in &by_name[entity.as_str()].actions {
            requests.push(Question {
                action: &action.name,
                resource_type: entity,
                cause: Cause::request(),
            });
        }
    }
    let mut questions = requests.clone();
    let chains = if policies.tells_timer_chains() {
        &[false, true][..]
    } else {
        &[false][..]
    };
    for (entity, action) in inheriting {
        for &timed in chains {
            questions.push(Question {
                action,
                resource_type: entity,
                cause: Cause::reaction(timed, Hop::unknown()),
            });
        }
    }
    if requests.is_empty() {
        return Ok(None);
    }
    let none = BTreeMap::new();
    let answers = |asker: &Asker, questions: &[Question<'_>]| -> Vec<May> {
        questions
            .iter()
            .map(|q| policies.may(asker, q.action, q.resource_type, &none, Some(&q.cause)))
            .collect()
    };
    // A chain asks a request once, and its reactions after it: kinds whose
    // reactions answer definitely alike behave alike after the request,
    // whichever of them sent it, so they are one sender.
    let reactions = &questions[requests.len()..];
    let mut kinds: Vec<(Vec<May>, Kind)> = Vec::new();
    for kind in policies.outside_kinds(&questions, reserve)? {
        let asker = Asker::OutsideAs(kind.clone());
        if answers(&asker, &requests).iter().all(|may| *may == May::No) {
            continue;
        }
        let chain = answers(&asker, reactions);
        // A Maybe still depends on the later rule or record. It cannot
        // establish that these senders' reactions will answer alike.
        match kinds
            .iter_mut()
            .find(|(known, _)| !chain.contains(&May::Maybe) && *known == chain)
        {
            Some((_, same)) => *same = same.clone().join(kind),
            None => kinds.push((chain, kind)),
        }
    }
    // One kind whose reactions answer definitely as anyone's adds nothing.
    let anyone = answers(&Asker::Outside, reactions);
    if let [(chain, _)] = &kinds[..]
        && !chain.contains(&May::Maybe)
        && *chain == anyone
    {
        return Ok(None);
    }
    Ok(Some(kinds.into_iter().map(|(_, kind)| kind).collect()))
}
