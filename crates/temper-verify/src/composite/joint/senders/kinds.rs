//! The kinds of outside sender a model keeps apart.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use temper_authz::{Asker, Cause, Hop, Kind, May, Question, SendAnalysis};

/// The kinds of outside sender kept, and what the partition found each
/// answers, so the tables need not ask Cedar again.
pub(super) struct Outside {
    pub(super) kinds: Vec<Kind>,
    /// Each kind's answer to a request of a root's (entity, action), as
    /// [`SendAnalysis::may_request`] gives it.
    pub(super) requests: HashMap<(String, String), Vec<May>>,
    /// Each kind's answers to an inheriting (entity, action) as a reaction
    /// in a request's chain and in a timer's.
    pub(super) reactions: HashMap<(String, String), Vec<[May; 2]>>,
    /// Each kind's answer to a timer's read of a standing type.
    pub(super) reads: HashMap<String, Vec<May>>,
}

impl Outside {
    /// The bytes its answers hold, a hash table counted at twice its slots.
    pub(super) fn bytes(&self) -> usize {
        fn table<K, V>(map: &HashMap<K, Vec<V>>, key: impl Fn(&K) -> usize) -> usize {
            2 * map.capacity() * size_of::<(K, Vec<V>)>()
                + map
                    .iter()
                    .map(|(k, v)| key(k) + v.capacity() * size_of::<V>())
                    .sum::<usize>()
        }
        let pair = |(entity, action): &(String, String)| entity.capacity() + action.capacity();
        table(&self.requests, pair)
            + table(&self.reactions, pair)
            + table(&self.reads, String::capacity)
    }
}

/// One kind being kept: its answers to the requests, whether one of its
/// senders surely may make each, and its answers to the chain.
struct Kept {
    kind: Kind,
    requests: Vec<May>,
    surely: Vec<bool>,
    chain: Vec<May>,
}

/// The answer of senders of two kinds: the same, or open.
fn either(one: May, other: May) -> May {
    if one == other { one } else { May::Maybe }
}
use temper_spec::automaton::Automaton;

/// The kinds of outside sender `policies` tell apart on what a chain from
/// outside asks: a request to one of `roots`, and the `inheriting` moves
/// as reactions, in either chain, and whether each may read a record of a
/// `standing` type as its timer asks. Kinds that may request nothing there,
/// nor any of the `starts` (entity, action) that set off a chain reaching
/// them from outside the scope, send nothing, and are left out. None when
/// the policies tell no two apart on those reactions.
pub(super) fn outside(
    by_name: &BTreeMap<&str, &Automaton>,
    policies: &SendAnalysis,
    roots: &BTreeSet<String>,
    starts: &BTreeSet<(String, String)>,
    inheriting: &BTreeSet<(String, String)>,
    standing: &BTreeSet<String>,
    reserve: &dyn Fn(usize) -> Result<(), String>,
) -> Result<Option<Outside>, String> {
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
    for entity in standing {
        questions.push(Question {
            action: super::standing::READ,
            resource_type: entity,
            cause: Cause::timer_fire(),
        });
    }
    if requests.is_empty() {
        return Ok(None);
    }
    let starts: Vec<Question<'_>> = starts
        .iter()
        .map(|(entity, action)| Question {
            action,
            resource_type: entity,
            cause: Cause::request(),
        })
        .collect();
    // The kinds are told apart on the starts too: two that answer the
    // chain alike but only one of which may start it are not one kind.
    let told: Vec<Question<'_>> = questions.iter().chain(&starts).cloned().collect();
    // The partition's answers, in the order of `told`.
    let (asked, chained) = (requests.len(), questions.len());
    // A chain asks a request once, and its reactions after it: kinds whose
    // reactions answer definitely alike behave alike after the request,
    // whichever of them sent it, so they are one sender.
    let mut kinds: Vec<Kept> = Vec::new();
    for found in policies.outside_kinds(&told, reserve)? {
        let answers = found.answers;
        let sends = |answers: &[May]| answers.iter().any(|may| *may != May::No);
        if !sends(&answers[..asked]) && !sends(&answers[chained..]) {
            continue;
        }
        let chain = answers[asked..chained].to_vec();
        // A partition kind of several senders answers definitely, so each of
        // them answers as it does.
        let surely: Vec<bool> = answers[..asked]
            .iter()
            .map(|may| *may == May::Yes)
            .collect();
        // A Maybe still depends on the later rule or record. It cannot
        // establish that these senders' reactions will answer alike.
        match kinds
            .iter_mut()
            .find(|kept| !chain.contains(&May::Maybe) && kept.chain == chain)
        {
            Some(same) => {
                same.kind = same.kind.clone().join(found.kind);
                for (at, may) in answers[..asked].iter().enumerate() {
                    same.requests[at] = either(same.requests[at], *may);
                    same.surely[at] |= surely[at];
                }
            }
            None => kinds.push(Kept {
                kind: found.kind,
                requests: answers[..asked].to_vec(),
                surely,
                chain,
            }),
        }
    }
    // One kind whose reactions answer definitely as anyone's adds nothing.
    let none = BTreeMap::new();
    let anyone: Vec<May> = questions[asked..]
        .iter()
        .map(|q| {
            policies.may(
                &Asker::Outside,
                q.action,
                q.resource_type,
                &none,
                Some(&q.cause),
            )
        })
        .collect();
    if let [kept] = &kinds[..]
        && !kept.chain.contains(&May::Maybe)
        && kept.chain == anyone
    {
        return Ok(None);
    }
    let column = |at: usize| -> Vec<May> { kinds.iter().map(|kept| kept.chain[at]).collect() };
    let requests = questions[..asked]
        .iter()
        .enumerate()
        .map(|(at, q)| {
            let row = kinds
                .iter()
                .map(|kept| match kept.requests[at] {
                    May::Maybe if kept.surely[at] => May::Yes,
                    may => may,
                })
                .collect();
            ((q.resource_type.to_string(), q.action.to_string()), row)
        })
        .collect();
    let mut reactions = HashMap::new();
    for (at, (entity, action)) in inheriting.iter().enumerate() {
        let request_chain = column(at * chains.len());
        let timer_chain = column(at * chains.len() + chains.len() - 1);
        let row = request_chain
            .into_iter()
            .zip(timer_chain)
            .map(|(r, t)| [r, t])
            .collect();
        reactions.insert((entity.clone(), action.clone()), row);
    }
    let reads = standing
        .iter()
        .enumerate()
        .map(|(at, entity)| (entity.clone(), column(inheriting.len() * chains.len() + at)))
        .collect();
    Ok(Some(Outside {
        kinds: kinds.into_iter().map(|kept| kept.kind).collect(),
        requests,
        reactions,
        reads,
    }))
}
