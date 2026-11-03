//! Who sends each move, as the kernel sends it, and whether Cedar lets them.
//!
//! - An action sent from outside: whoever sent it, someone outside the kernel.
//! - A reaction with a declared principal: that service, with the trigger's
//!   source in its context (`resolve_trigger_principal` in the trigger
//!   dispatcher). Without one: whoever sent its source action.
//! - A module's answer, or its `on_failure`: whoever sent the move that
//!   called the module, whether the call ran inline or in the background
//!   (`dispatch_wasm_callback` in `invocation_artifacts.rs`).
//! - A state timer: whoever armed it, the sender of the move that entered its
//!   state or last reset it (`timer_agent` in `state_timeouts.rs`).
//!
//! Cedar decides two of these: a request from outside, at its entry point,
//! and a reaction, as its sender (the trigger dispatcher). A timer's move and
//! a module's answer are dispatched without it (`dispatch` keeps
//! authorization at the entry points), so their sender matters for the
//! reactions they set off. Without policies every sender may send everything.
//!
//! Each is decided with the cause the kernel gives it (`cause_context`): a
//! request is `via == "request"`, a reaction names its rule and the move that
//! set it off, and is `via == "timer"` in a timer's chain (a timer's fire
//! starts one, and the reactions and module answers after it carry it on),
//! `"reaction"` otherwise. A sender's number says which chain it is in, as
//! the kernel's security context does, when some policy can tell the two
//! apart ([`SendAnalysis::tells_timer_chains`]); otherwise every chain is
//! taken as a request's, and states that differ only in it are one.
//!
//! What each sender may send is read from the policies once. A reaction
//! whose answer depends on the request is decided again when it arrives,
//! with what the joint state knows of it: its source's id, and its record's
//! id and tracked fields. A reaction that finds its record through a field
//! (`resolve_target` `field`) needs this, since only the state says whether
//! that record is one its source may act on.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use serde_json::Value;
use temper_authz::{Asker, Attr, Cause, Hop, May, SendAnalysis};
use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};

use super::env::ID;
use super::room::Heap;
use super::state::JointState;
use crate::model::ModelString;

/// Someone outside the kernel, in a request's chain.
pub(super) const OUTSIDE: u32 = 0;

/// The senders a model knows, by number, and what Cedar lets each send.
///
/// A sender's number is its asker's place, doubled, plus one in a timer's
/// chain.
pub(super) struct Senders {
    askers: Vec<Asker>,
    /// Each asker's place with resource-relative IDs forgotten after a
    /// different-ID hop.
    detached: Vec<u32>,
    /// Whether some policy tells a timer's chain from a request's.
    timer_chains: bool,
    /// Entity, then action: what Cedar lets someone outside request.
    requests: Option<BTreeMap<String, BTreeMap<String, May>>>,
    /// Entity, then action, then each sender's answer as a reaction's.
    may: Option<BTreeMap<String, BTreeMap<String, Vec<May>>>>,
    /// The policies, to decide again a move whose answer depends on the
    /// request.
    policies: Option<SendAnalysis>,
    /// The answers decided again so far: a walk asks the same few questions
    /// over and over, and reading the policies takes far longer than a step.
    decided: RefCell<HashMap<Question, May>>,
    /// The bytes `decided` holds, as a walk counts them against its room.
    decided_bytes: std::cell::Cell<usize>,
    /// The policies that may apply to a move whose answer depends on the
    /// request ([`SendAnalysis::applicable`]): all a walk reads of `policies`
    /// when it decides one again.
    reread: BTreeSet<String>,
}

/// What a walk reads of its senders, which a model's digest
/// ([`JointProof::digest`](super::JointProof::digest)) is of: who sends,
/// what Cedar lets each send, and the policies it may read again. Policies
/// no walk can read, and the answers decided so far, are left out.
impl fmt::Debug for Senders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Senders")
            .field("askers", &self.askers)
            .field("detached", &self.detached)
            .field("timer_chains", &self.timer_chains)
            .field("requests", &self.requests)
            .field("may", &self.may)
            .field("reread", &self.reread)
            .finish()
    }
}

/// A move decided again: its sender, record type and action, the rule that
/// made it, the cause's and inherited service's source IDs, and the record's
/// tracked strings and booleans.
type Question = (
    u32,
    String,
    String,
    String,
    Option<ModelString>,
    Option<ModelString>,
    BTreeMap<String, ModelString>,
    BTreeMap<String, bool>,
);

impl Senders {
    /// The senders of the moves among `entities`, and what `policies` lets
    /// each send.
    pub(super) fn new<'a>(
        by_name: &BTreeMap<&str, &Automaton>,
        entities: impl IntoIterator<Item = &'a String> + Clone,
        policies: Option<&SendAnalysis>,
    ) -> Self {
        let mut askers = vec![Asker::Outside];
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
        for mut asker in askers.clone() {
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
        let none = BTreeMap::new();
        let requests = policies.map(|policies| {
            each(by_name, entities.clone(), &|entity, action| {
                let request = Cause::request();
                policies.may(&Asker::Outside, action, entity, &none, Some(&request))
            })
        });
        let may = policies.map(|policies| {
            each(by_name, entities.clone(), &|entity, action| {
                let answers = |timed: bool| {
                    let cause = Cause::reaction(timed, Hop::unknown());
                    askers
                        .iter()
                        .map(|asker| policies.may(asker, action, entity, &none, Some(&cause)))
                        .collect::<Vec<_>>()
                };
                let request_chain = answers(false);
                let timer_chain = if timer_chains {
                    answers(true)
                } else {
                    request_chain.clone()
                };
                request_chain
                    .into_iter()
                    .zip(timer_chain)
                    .flat_map(|(r, t)| [r, t])
                    .collect()
            })
        });
        let reread = reread(policies, &askers, may.as_ref());
        Self {
            askers,
            detached,
            timer_chains,
            requests,
            may,
            policies: policies.cloned(),
            decided: RefCell::default(),
            decided_bytes: std::cell::Cell::default(),
            reread,
        }
    }

    /// Forget what Cedar lets each sender send of `moves` (entity, action),
    /// which the model no longer makes: neither a walk nor its digest reads
    /// them.
    pub(super) fn forget(&mut self, moves: &BTreeSet<(String, String)>) {
        let (Some(may), Some(requests)) = (&mut self.may, &mut self.requests) else {
            return;
        };
        for (entity, action) in moves {
            if let Some(actions) = may.get_mut(entity) {
                actions.remove(action);
            }
            if let Some(actions) = requests.get_mut(entity) {
                actions.remove(action);
            }
        }
        self.reread = reread(self.policies.as_ref(), &self.askers, self.may.as_ref());
    }

    /// Whether Cedar lets someone outside request `action` on an `entity`
    /// record.
    pub(super) fn request(&self, entity: &str, action: &str) -> May {
        let Some(requests) = &self.requests else {
            return May::Yes;
        };
        requests
            .get(entity)
            .and_then(|actions| actions.get(action))
            .copied()
            .unwrap_or(May::No)
    }

    /// Whether Cedar lets sender `by` send `action` to an `entity` record as
    /// a reaction, whatever its rule.
    pub(super) fn may(&self, by: u32, entity: &str, action: &str) -> May {
        let Some(may) = &self.may else {
            return May::Yes;
        };
        may.get(entity)
            .and_then(|actions| actions.get(action))
            .and_then(|answers| answers.get(by as usize))
            .copied()
            .unwrap_or(May::No)
    }

    /// Whether Cedar lets sender `by` send `action` to the `entity` record
    /// of `state` as the reaction `rule` makes it: as the policies alone
    /// answer, or, when that depends on the request, as they answer given
    /// the rule, its source's id and the record's id and tracked fields in
    /// `state`.
    pub(super) fn may_in(
        &self,
        by: u32,
        rule: &Rule<'_>,
        entity: &str,
        action: &str,
        state: &JointState,
    ) -> May {
        let may = self.may(by, entity, action);
        let (May::Maybe, Some(policies), Some(asker), Some(target)) = (
            may,
            &self.policies,
            self.askers.get(by as usize / 2),
            state.records.get(entity),
        ) else {
            return may;
        };
        // The current cause names the record whose move set this reaction off.
        // The service keeps the source recorded when it was declared.
        let source_id = state
            .records
            .get(rule.from)
            .and_then(|record| record.strings.get(ID));
        let service_source_id = match asker {
            Asker::Service { context, .. } => match context.get("triggerSourceEntityType") {
                Some(Attr::Value(Value::String(source))) => state
                    .records
                    .get(source)
                    .and_then(|record| record.strings.get(ID)),
                _ => None,
            },
            Asker::Outside => None,
        };
        let name = rule.to_string();
        let question = (
            by,
            entity.to_string(),
            action.to_string(),
            name.clone(),
            source_id.cloned(),
            service_source_id.cloned(),
            target.strings.clone(),
            target.booleans.clone(),
        );
        if let Some(may) = self.decided.borrow().get(&question) {
            return *may;
        }
        // A value the model made up is none of the literals it holds.
        let literals: BTreeSet<String> = source_id
            .into_iter()
            .chain(service_source_id)
            .chain(target.strings.values())
            .filter_map(|value| match value {
                ModelString::Literal(text) if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect();
        let mut asker = asker.clone();
        if let (Asker::Service { context, .. }, Some(id)) = (&mut asker, service_source_id) {
            context.insert("triggerSourceEntityId".to_string(), symbol(id, &literals));
        }
        let text = |value: &str| Attr::Value(Value::String(value.to_string()));
        let hop = Hop {
            rule: text(&name),
            action: text(rule.action),
            entity_type: text(rule.from),
            entity_id: source_id.map_or(Attr::Unknown, |id| symbol(id, &literals)),
        };
        let cause = Cause::reaction(by % 2 == 1, hop);
        let strings = target.strings.iter().map(|(field, value)| {
            let field = if field == ID { "id" } else { field.as_str() };
            (field.to_string(), symbol(value, &literals))
        });
        let booleans = target
            .booleans
            .iter()
            .map(|(field, value)| (field.clone(), Attr::Value(Value::Bool(*value))));
        let record = strings.chain(booleans).collect();
        let may = policies.may(&asker, action, entity, &record, Some(&cause));
        let entry = super::room::map_bytes::<Question, May>(1)
            + question.1.heap()
            + question.2.heap()
            + question.3.heap()
            + question.4.heap()
            + question.5.heap()
            + question.6.heap()
            + question.7.heap();
        self.decided_bytes.set(self.decided_bytes.get() + entry);
        self.decided.borrow_mut().insert(question, may);
        may
    }

    /// The bytes the answers decided again so far hold.
    pub(super) fn held(&self) -> usize {
        self.decided_bytes.get()
    }

    /// The number of the sender a reaction runs as: its declared principal,
    /// or `invoker` without one, in `invoker`'s chain. An inherited
    /// resource-relative ID remains known only when this hop preserves the
    /// receiver ID.
    pub(super) fn reaction(&self, declared_as: Option<&Asker>, invoker: u32, same_id: bool) -> u32 {
        let asker = declared_as
            .and_then(|asker| self.askers.iter().position(|a| a == asker))
            .map_or_else(
                || {
                    let invoker = invoker / 2;
                    if same_id {
                        invoker
                    } else {
                        self.detached[invoker as usize]
                    }
                },
                |at| at as u32,
            );
        asker * 2 + invoker % 2
    }

    /// The sender a timer armed by `by` fires as: the same asker, starting
    /// a timer's chain.
    pub(super) fn timer(&self, by: u32) -> u32 {
        if self.timer_chains { by | 1 } else { by }
    }

    /// Who armed a timer when `by` did, without the chain `by` is in: the
    /// timer's fire starts its own.
    pub(super) fn armer(by: u32) -> u32 {
        by & !1
    }

    /// Someone outside, in `by`'s chain: who stands for every sender of a
    /// move whose reactions all declare theirs.
    pub(super) fn anyone(by: u32) -> u32 {
        OUTSIDE | (by % 2)
    }

    /// Who sender `by` is, in words.
    pub(super) fn name(&self, by: u32) -> String {
        match self.askers.get(by as usize / 2) {
            Some(Asker::Outside) => "an outside caller".to_string(),
            Some(Asker::Service { name, .. }) => format!("service:{name}"),
            None => format!("sender {by}"),
        }
    }
}

/// `answer` for each action of each of `entities`, by entity, then action.
fn each<'a, T>(
    by_name: &BTreeMap<&str, &Automaton>,
    entities: impl IntoIterator<Item = &'a String>,
    answer: &dyn Fn(&str, &str) -> T,
) -> BTreeMap<String, BTreeMap<String, T>> {
    entities
        .into_iter()
        .map(|entity| {
            let actions = by_name[entity.as_str()]
                .actions
                .iter()
                .map(|action| (action.name.clone(), answer(entity, &action.name)))
                .collect();
            (entity.clone(), actions)
        })
        .collect()
}

/// The policies that may apply to a move whose answer `may` leaves to the
/// request: all a walk reads of `policies` when it decides one again.
fn reread(
    policies: Option<&SendAnalysis>,
    askers: &[Asker],
    may: Option<&BTreeMap<String, BTreeMap<String, Vec<May>>>>,
) -> BTreeSet<String> {
    let mut reread = BTreeSet::new();
    if let (Some(policies), Some(may)) = (policies, may) {
        for (entity, actions) in may {
            for (action, answers) in actions {
                for (by, answer) in answers.iter().enumerate() {
                    if *answer == May::Maybe {
                        reread.extend(policies.applicable(&askers[by / 2], action, entity));
                    }
                }
            }
        }
    }
    reread
}

/// The sender a reaction declares, as the trigger dispatcher builds it.
pub(super) fn declared(
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

/// A tracked string as Cedar may read it: a value the model made up is
/// known only by which others it equals, and is none of `literals`; a
/// literal is itself.
fn symbol(value: &ModelString, literals: &BTreeSet<String>) -> Attr {
    match value {
        ModelString::Symbol(_) | ModelString::BlankSymbol(_) => Attr::Same {
            name: value.to_string(),
            unlike: literals.clone(),
        },
        ModelString::Literal(text) => Attr::Value(Value::String(text.clone())),
    }
}

/// The reaction rule a move is, as the kernel names it:
/// `<from>:<action>:<trigger>`.
pub(super) struct Rule<'a> {
    /// The type whose move set it off.
    pub(super) from: &'a str,
    /// That move.
    pub(super) action: &'a str,
    /// The trigger's name.
    pub(super) trigger: &'a str,
}

impl fmt::Display for Rule<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.from, self.action, self.trigger)
    }
}

fn service(name: &str, context: BTreeMap<String, Attr>) -> Asker {
    Asker::Service {
        name: name.to_string(),
        context,
    }
}

#[cfg(test)]
#[path = "senders_test.rs"]
mod tests;
