//! Construct senders and reserve their answer tables before allocation.

use super::standing::READ;
use super::table::each;
use super::*;

impl Senders {
    /// Whether an open answer to sender `by`'s `action` on an `entity` record
    /// can come out differently for different callers.
    pub(in crate::composite::joint) fn turns_on_who_asks(
        &self,
        by: u32,
        entity: &str,
        action: &str,
    ) -> bool {
        let (Some(policies), Some(asker)) = (&self.policies, self.askers.get(by as usize / 2))
        else {
            return false;
        };
        policies.turns_on_who_asks(asker, action, base(entity))
    }

    /// The senders of the moves among `entities`, and what `policies` lets
    /// each send. A request to one of `roots` starts a chain that runs as
    /// its sender through the `inheriting` moves (entity, action): those
    /// are what the kinds of outside sender are told apart on, with whether
    /// each may read a record of a `standing` type as its timer asks. A
    /// kind that may send a move outside `entities` that sets off a chain
    /// reaching one of `roots` is kept too.
    pub(in crate::composite::joint) fn new<'a>(
        by_name: &BTreeMap<&str, &Automaton>,
        entities: impl IntoIterator<Item = &'a String> + Clone,
        policies: Option<&SendAnalysis>,
        roots: &BTreeSet<String>,
        inheriting: &BTreeSet<(String, String)>,
        standing: &BTreeSet<String>,
        reserve: Option<&dyn Fn(usize) -> Result<(), String>>,
    ) -> Self {
        let unlimited = |_: usize| Ok(());
        let scope: BTreeSet<String> = entities.clone().into_iter().cloned().collect();
        let starts = super::super::model::starts(by_name, &scope, roots);
        let kinds = policies
            .map(|policies| {
                kinds::outside(
                    by_name,
                    policies,
                    roots,
                    &starts,
                    inheriting,
                    standing,
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
            Some(found) => (1..=found.kinds.len()).map(|at| at as u32 * 2).collect(),
        };
        let mut found = kinds.unwrap_or_else(|| kinds::Outside {
            kinds: Vec::new(),
            requests: HashMap::new(),
            reactions: HashMap::new(),
            reads: HashMap::new(),
        });
        let kind_count = found.kinds.len();
        let mut askers = Vec::with_capacity(kind_count + 1);
        askers.push(Asker::Outside);
        askers.extend(
            std::mem::take(&mut found.kinds)
                .into_iter()
                .map(Asker::OutsideAs),
        );
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
        let mut tables = 0;
        if policies.is_some() {
            let profiles: usize = askers
                .iter()
                .map(|asker| match asker {
                    Asker::OutsideAs(kind) => kind.bytes(),
                    _ => 0,
                })
                .sum::<usize>()
                + found.bytes();
            let room = table::room(by_name, entities.clone(), kind_count + 1, askers.len())
                .and_then(|room| {
                    room.checked_add(table::read_room(standing, askers.len()))
                        .ok_or_else(|| "sender table size overflows addressable memory".to_string())
                })
                .and_then(|room| {
                    profiles
                        .checked_add(room)
                        .ok_or_else(|| "sender table size overflows addressable memory".to_string())
                })
                .and_then(|room| {
                    tables = room;
                    reserve.unwrap_or(&unlimited)(room)
                });
            if let Err(reason) = room {
                return Self::failed(format!("sender answer tables do not fit: {reason}"));
            }
        }
        let none = BTreeMap::new();
        // A request asks once, and only what follows it depends on who sent
        // it, which the askers here answer alike, or stand for one sender.
        let requests = policies.map(|policies| {
            let beside_tables = |bytes: usize| {
                let total = tables
                    .checked_add(bytes)
                    .ok_or_else(|| "sender-kind size overflows addressable memory".to_string())?;
                reserve.unwrap_or(&unlimited)(total)
            };
            let requests = each(by_name, entities.clone(), &|entity, action| {
                let key = (entity.to_string(), action.to_string());
                let anyone = policies.may_request(&askers[0], action, entity, &beside_tables);
                match found.requests.get(&key) {
                    Some(kinds) => std::iter::once(anyone)
                        .chain(kinds.iter().copied())
                        .collect(),
                    None => std::iter::once(anyone)
                        .chain(askers[1..=kind_count].iter().map(|asker| {
                            policies.may_request(asker, action, entity, &beside_tables)
                        }))
                        .collect(),
                }
            });
            // Hand back what the last look took, which `tables` already holds.
            let _ = beside_tables(0);
            requests
        });
        let may = policies.map(|policies| {
            let mut may = each(by_name, entities.clone(), &|entity, action| {
                let mut answers = Vec::with_capacity(askers.len() * 2);
                let known = found
                    .reactions
                    .get(&(entity.to_string(), action.to_string()));
                for (by, asker) in askers.iter().enumerate() {
                    if let Some(known) = known
                        && (1..=kind_count).contains(&by)
                    {
                        answers.extend(known[by - 1]);
                        continue;
                    }
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
            });
            let cause = Cause::timer_fire();
            for entity in standing {
                let known = found.reads.get(entity);
                let answers = askers
                    .iter()
                    .enumerate()
                    .flat_map(|(by, asker)| {
                        let answer = match known {
                            Some(known) if (1..=kind_count).contains(&by) => known[by - 1],
                            _ => policies.may(asker, READ, entity, &none, Some(&cause)),
                        };
                        [answer, answer]
                    })
                    .collect();
                if let Some(actions) = may.get_mut(entity) {
                    actions.insert(READ.to_string(), answers);
                }
            }
            may
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
            decided: RwLock::default(),
            alike: RwLock::default(),
            asking: RwLock::default(),
            decided_bytes: std::sync::atomic::AtomicUsize::default(),
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
            decided: RwLock::default(),
            alike: RwLock::default(),
            asking: RwLock::default(),
            decided_bytes: std::sync::atomic::AtomicUsize::default(),
            reread: BTreeSet::new(),
            meets: BTreeMap::new(),
        }
    }
}

/// What a walk reads of its senders, which a model's digest
/// ([`JointProof::digest`](super::JointProof::digest)) is of: who sends,
/// what Cedar lets each send, and the policies it may read again. Policies
/// no walk can read, and the answers decided so far, are left out.
impl std::fmt::Debug for Senders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Senders")
            .field("askers", &self.askers)
            .field("outside", &self.outside)
            .field("detached", &self.detached)
            .field("timer_chains", &self.timer_chains)
            .field("requests", &self.requests)
            .field("may", &self.may)
            .field("reread", &self.reread)
            .field("meets", &self.meets)
            .finish()
    }
}

/// The sender a reaction declares, as the trigger dispatcher builds it.
pub(in crate::composite::joint) fn declared(
    trigger: &temper_spec::automaton::ActionTrigger,
    source_entity: &str,
    source_action: &str,
) -> Option<Asker> {
    if trigger.kind != TriggerKind::Entity {
        return None;
    }
    let name = trigger.principal.as_deref().filter(|p| !p.is_empty())?;
    let text = |value: &str| Attr::Value(Value::String(value.to_string()));
    // A same-id reaction acts on the record with its source's id.
    let source_id = match trigger.resolve_target {
        Some(TargetResolver::SameId) => Attr::ResourceId,
        _ => Attr::Unknown,
    };
    let context = BTreeMap::from([
        ("triggerRule".to_string(), text(&trigger.name)),
        ("triggerSourceEntityType".to_string(), text(source_entity)),
        ("triggerSourceEntityId".to_string(), source_id),
        ("triggerSourceAction".to_string(), text(source_action)),
        (
            "triggerDeclaredPrincipal".to_string(),
            Attr::Value(Value::Bool(true)),
        ),
    ]);
    Some(service(name, context))
}
