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
//! Someone outside is one principal along all of a chain: its reactions,
//! module answers and timer fires run as it, with the same security context
//! (`resolve_trigger_principal` clones it, a timer keeps the arming one).
//! When some policy tells two outside principals apart on a move such a
//! chain makes, the model keeps one sender per kind of outside principal
//! the policies tell apart ([`SendAnalysis::outside_kinds`]): a request is
//! sent as each kind that may send it, and everything after it is decided
//! for that kind, so a chain is never let through for one principal and
//! refused for another. Together the kinds cover every outside principal.
//!
//! What each sender may send is read from the policies once. A reaction
//! whose answer depends on the request is decided again when it arrives,
//! with what the joint state knows of it: its source's id, and its record's
//! id and tracked fields. A reaction that finds its record through a field
//! (`resolve_target` `field`) needs this, since only the state says whether
//! that record is one its source may act on.

use super::model::copies::base;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;
use temper_authz::{Asker, Attr, Cause, Hop, May, SendAnalysis};
use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};

use super::env::ID;
use super::model::Classes;
use super::state::JointState;
use crate::model::ModelString;
use table::reread;

mod build;
pub(super) use build::declared;
mod kinds;
mod standing;
mod table;
pub(super) use standing::{Stake, policy_fields, standing};
#[path = "senders_unknown.rs"]
mod unknown;

/// Someone outside the kernel, of no known kind, in a request's chain.
pub(super) const OUTSIDE: u32 = 0;

/// Sender ids by entity, then by action.
type AlikeSenders = BTreeMap<String, BTreeMap<String, Arc<[u32]>>>;

/// The senders a model knows, by number, and what Cedar lets each send.
///
/// A sender's number is its asker's place, doubled, plus one in a timer's
/// chain. Someone outside of no known kind is first, then each kind of
/// outside sender, then the services.
pub(super) struct Senders {
    askers: Vec<Asker>,
    /// Each asker in a move's words, by place: a walk names a sender in
    /// every move's label, and a kind takes far longer to name than a step.
    names: Vec<String>,
    /// The senders an outside request is sent as: each kind of outside
    /// sender, or someone outside of no known kind when the policies tell
    /// none apart.
    outside: Vec<u32>,
    /// A failed partition cannot be walked as an unrestricted sender.
    pub(super) problem: Option<String>,
    /// Each asker's place with resource-relative IDs forgotten after a
    /// different-ID hop.
    detached: Vec<u32>,
    /// Whether some policy tells a timer's chain from a request's.
    timer_chains: bool,
    /// Entity, then action, then each outside asker's answer (of no known
    /// kind first): what Cedar lets them request.
    requests: Option<BTreeMap<String, BTreeMap<String, Vec<May>>>>,
    /// Entity, then action, then each sender's answer as a reaction's.
    may: Option<BTreeMap<String, BTreeMap<String, Vec<May>>>>,
    /// The policies, to decide again a move whose answer depends on the
    /// request.
    policies: Option<SendAnalysis>,
    /// The answers decided again so far: a walk asks the same few questions
    /// over and over, and reading the policies takes far longer than a step.
    decided: RwLock<HashMap<Question, May>>,
    /// The senders each (entity, action) is sent as from outside, of those
    /// that may, one for each way what follows can go.
    alike: RwLock<AlikeSenders>,
    /// Each sender in the words of a request it makes ([`Self::name_asking`]).
    asking: RwLock<Asking>,
    /// The bytes `decided` holds, as a walk counts them against its room.
    decided_bytes: AtomicUsize,
    /// The policies that may apply to a move whose answer depends on the
    /// request ([`SendAnalysis::applicable`]): all a walk reads of `policies`
    /// when it decides one again.
    reread: BTreeSet<String>,
    /// The literals each type's id and tracked fields can meet, by (type,
    /// field): a value the model made up in one may be any literal it never
    /// meets, which the model does not offer it.
    meets: BTreeMap<(String, String), BTreeSet<String>>,
}

/// Each sender's name on the requests named so far, by sender, entity and action.
type Asking = HashMap<(u32, String, String), String>;

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
    /// These senders, knowing which literals each id and tracked field of
    /// `models` can meet.
    pub(super) fn meeting(
        mut self,
        classes: &Classes,
        models: &BTreeMap<String, crate::model::TemperModel>,
    ) -> Self {
        for (entity, model) in models {
            for field in model.tracked_strings.iter().map(String::as_str).chain([ID]) {
                let key = (entity.clone(), field.to_string());
                self.meets.insert(key, classes.literals(entity, field));
            }
        }
        self
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
        self.alike.write().expect("a walk thread panicked").clear();
    }

    /// Bytes retained by the partition after its temporary work is dropped.
    pub(super) fn kind_bytes(&self) -> usize {
        self.askers
            .iter()
            .map(|asker| match asker {
                Asker::OutsideAs(kind) => kind.bytes(),
                _ => 0,
            })
            .sum()
    }

    /// Bytes retained by sender profiles and their complete answer tables.
    pub(super) fn construction_bytes(&self) -> usize {
        self.kind_bytes() + table::bytes(self.requests.as_ref()) + table::bytes(self.may.as_ref())
    }

    /// Whether Cedar lets someone outside, of no known kind, request
    /// `action` on an `entity` record.
    pub(super) fn request(&self, entity: &str, action: &str) -> May {
        self.request_as(OUTSIDE, entity, action)
    }

    /// Whether Cedar lets the outside sender `by` request `action` on an
    /// `entity` record.
    pub(super) fn request_as(&self, by: u32, entity: &str, action: &str) -> May {
        let entity = base(entity);
        let Some(requests) = &self.requests else {
            return May::Yes;
        };
        requests
            .get(entity)
            .and_then(|actions| actions.get(action))
            .and_then(|answers| answers.get(by as usize / 2))
            .copied()
            .unwrap_or(May::No)
    }

    /// The senders `entity`'s `action` is sent as from outside, once worked
    /// out ([`Self::note_alike`]).
    pub(super) fn alike(&self, entity: &str, action: &str) -> Option<Arc<[u32]>> {
        let entity = base(entity);
        self.alike
            .read()
            .expect("a walk thread panicked")
            .get(entity)?
            .get(action)
            .cloned()
    }

    /// Note the senders `entity`'s `action` is sent as from outside.
    pub(super) fn note_alike(&self, entity: &str, action: &str, senders: Arc<[u32]>) {
        let entity = base(entity);
        self.alike
            .write()
            .expect("a walk thread panicked")
            .entry(entity.to_string())
            .or_default()
            .insert(action.to_string(), senders);
    }

    /// The senders an outside request is sent as.
    pub(super) fn outside(&self) -> &[u32] {
        &self.outside
    }

    /// Whether Cedar lets sender `by` send `action` to an `entity` record as
    /// a reaction, whatever its rule.
    pub(super) fn may(&self, by: u32, entity: &str, action: &str) -> May {
        let entity = base(entity);
        let Some(may) = &self.may else {
            return May::Yes;
        };
        may.get(entity)
            .and_then(|actions| actions.get(action))
            .and_then(|answers| answers.get(by as usize))
            .copied()
            .unwrap_or_else(|| self.unknown_answer(by))
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
        // Cedar knows the type: which copy holds the record means nothing.
        let entity = base(entity);
        // The current cause names the record whose move set this reaction off.
        // The service keeps the source recorded when it was declared.
        let source_id = state
            .records
            .get(rule.from)
            .and_then(|record| record.strings.get(ID));
        let service_source = match asker {
            Asker::Service { context, .. } => match context.get("triggerSourceEntityType") {
                Some(Attr::Value(Value::String(source))) => Some(source.as_str()),
                _ => None,
            },
            Asker::Outside | Asker::OutsideAs(_) => None,
        };
        // A service's source is the record that set this off, when of its type.
        let service_source_id = service_source
            .and_then(|source| {
                let held = if base(rule.from) == source {
                    rule.from
                } else {
                    source
                };
                state.records.get(held)
            })
            .and_then(|record| record.strings.get(ID));
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
        if let Some(may) = self
            .decided
            .read()
            .expect("a walk thread panicked")
            .get(&question)
        {
            return *may;
        }
        let literals = standing::literals(
            source_id
                .into_iter()
                .chain(service_source_id)
                .chain(target.strings.values()),
        );
        let symbol = |value: &ModelString, entity: &str, field: &str| {
            self.symbol_in(value, entity, field, &literals)
        };
        let mut asker = asker.clone();
        if let (Asker::Service { context, .. }, Some(id), Some(source)) =
            (&mut asker, service_source_id, service_source)
        {
            context.insert("triggerSourceEntityId".to_string(), symbol(id, source, ID));
        }
        let text = |value: &str| Attr::Value(Value::String(value.to_string()));
        let hop = Hop {
            rule: text(&name),
            action: text(rule.action),
            entity_type: text(base(rule.from)),
            entity_id: source_id.map_or(Attr::Unknown, |id| symbol(id, rule.from, ID)),
        };
        let cause = Cause::reaction(by % 2 == 1, hop);
        let record = self.record_attrs(entity, target, &literals);
        let may = policies.may(&asker, action, entity, &record, Some(&cause));
        self.remember(question, may)
    }

    /// Whether a policy on `entity`'s `action` can read which move set a
    /// reaction off.
    pub(super) fn reads_source_move(&self, entity: &str, action: &str) -> bool {
        self.policies
            .as_ref()
            .is_some_and(|policies| policies.reads_source_move(action, base(entity)))
    }

    /// Whether a policy can read a value named `field`.
    pub(super) fn reads_field(&self, field: &str) -> bool {
        self.policies
            .as_ref()
            .is_some_and(|policies| policies.reads_attr(field))
    }

    /// The bytes the answers decided again so far hold.
    pub(super) fn held(&self) -> usize {
        self.decided_bytes.load(Ordering::Relaxed)
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
                    if same_id || invoker as usize >= self.detached.len() {
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
        match self.names.get(by as usize / 2) {
            Some(name) => name.clone(),
            None => self.unknown_name(by),
        }
    }
}

/// An asker in a move's words.
fn asker_name(asker: &Asker) -> String {
    match asker {
        Asker::Outside => "an outside caller".to_string(),
        Asker::OutsideAs(kind) => format!("an outside caller {kind}"),
        Asker::Service { name, .. } => format!("service:{name}"),
    }
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
