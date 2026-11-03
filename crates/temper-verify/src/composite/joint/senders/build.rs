//! Construct senders and reserve their answer tables before allocation.

use super::table::each;
use super::*;

impl Senders {
    /// The senders of the moves among `entities`, and what `policies` lets
    /// each send. A request to one of `roots` starts a chain that runs as
    /// its sender through the `inheriting` moves (entity, action): those
    /// are what the kinds of outside sender are told apart on.
    pub(in crate::composite::joint) fn new<'a>(
        by_name: &BTreeMap<&str, &Automaton>,
        entities: impl IntoIterator<Item = &'a String> + Clone,
        policies: Option<&SendAnalysis>,
        roots: &BTreeSet<String>,
        inheriting: &BTreeSet<(String, String)>,
        reserve: Option<&dyn Fn(usize) -> Result<(), String>>,
    ) -> Self {
        let unlimited = |_: usize| Ok(());
        let kinds = policies
            .map(|policies| {
                kinds::outside(
                    by_name,
                    policies,
                    roots,
                    inheriting,
                    reserve.unwrap_or(&unlimited),
                )
            })
            .transpose();
        let kinds = match kinds {
            Ok(kinds) => kinds.flatten(),
            Err(reason) => {
                return Self::failed(format!("sender-kind construction does not fit: {reason}"));
            }
        };
        let outside: Vec<u32> = match &kinds {
            None => vec![OUTSIDE],
            Some(kinds) => (1..=kinds.len()).map(|at| at as u32 * 2).collect(),
        };
        let kinds = kinds.unwrap_or_default();
        let kind_count = kinds.len();
        let mut askers = Vec::with_capacity(kind_count + 1);
        askers.push(Asker::Outside);
        askers.extend(kinds.into_iter().map(Asker::OutsideAs));
        for entity in entities.clone() {
            for action in &by_name[entity.as_str()].actions {
                for trigger in &action.triggers {
                    let Some(asker) = declared(trigger, entity, &action.name) else {
                        continue;
                    };
                    if !askers.contains(&asker) {
                        askers.push(asker);
                    }
                }
            }
        }
        // ResourceId is relative to the current receiver, not a stable ID.
        // Precompute a closed set of senders for chains that change receiver ID.
        let mut detached = Vec::new();
        for at in 0..askers.len() {
            let mut asker = askers[at].clone();
            if let Asker::Service { context, .. } = &mut asker {
                for attr in context.values_mut() {
                    if *attr == Attr::ResourceId {
                        *attr = Attr::Unknown;
                    }
                }
            }
            let at = askers.iter().position(|a| a == &asker).unwrap_or_else(|| {
                askers.push(asker);
                askers.len() - 1
            });
            detached.push(at as u32);
        }
        detached.extend((detached.len()..askers.len()).map(|at| at as u32));
        let timer_chains = policies.is_some_and(SendAnalysis::tells_timer_chains);
        // The partition's questions do not include every action these
        // tables cache. Reserve their full size before allocating a row,
        // with the finished profiles still held alongside them.
        if policies.is_some() {
            let profiles: usize = askers
                .iter()
                .map(|asker| match asker {
                    Asker::OutsideAs(kind) => kind.bytes(),
                    _ => 0,
                })
                .sum();
            let room = table::room(by_name, entities.clone(), kind_count + 1, askers.len())
                .and_then(|tables| {
                    profiles
                        .checked_add(tables)
                        .ok_or_else(|| "sender table size overflows addressable memory".to_string())
                })
                .and_then(reserve.unwrap_or(&unlimited));
            if let Err(reason) = room {
                return Self::failed(format!("sender answer tables do not fit: {reason}"));
            }
        }
        let none = BTreeMap::new();
        let requests = policies.map(|policies| {
            each(by_name, entities.clone(), &|entity, action| {
                let request = Cause::request();
                askers[..=kind_count]
                    .iter()
                    .map(|asker| policies.may(asker, action, entity, &none, Some(&request)))
                    .collect()
            })
        });
        let may = policies.map(|policies| {
            each(by_name, entities.clone(), &|entity, action| {
                let mut answers = Vec::with_capacity(askers.len() * 2);
                for asker in &askers {
                    let answer = |timed| {
                        policies.may(
                            asker,
                            action,
                            entity,
                            &none,
                            Some(&Cause::reaction(timed, Hop::unknown())),
                        )
                    };
                    let request_chain = answer(false);
                    let timer_chain = if timer_chains {
                        answer(true)
                    } else {
                        request_chain
                    };
                    answers.extend([request_chain, timer_chain]);
                }
                answers
            })
        });
        let reread = reread(policies, &askers, may.as_ref());
        let names = askers.iter().map(asker_name).collect();
        Self {
            askers,
            names,
            outside,
            problem: None,
            detached,
            timer_chains,
            requests,
            may,
            policies: policies.cloned(),
            decided: RefCell::default(),
            decided_bytes: std::cell::Cell::default(),
            reread,
            meets: BTreeMap::new(),
        }
    }

    /// A failed construction carries no partial table or sender cover.
    fn failed(reason: String) -> Self {
        Self {
            askers: vec![Asker::Outside],
            names: vec!["an outside caller".to_string()],
            outside: vec![OUTSIDE],
            problem: Some(reason),
            detached: vec![0],
            timer_chains: false,
            requests: None,
            may: None,
            policies: None,
            decided: RefCell::default(),
            decided_bytes: std::cell::Cell::default(),
            reread: BTreeSet::new(),
            meets: BTreeMap::new(),
        }
    }
}
