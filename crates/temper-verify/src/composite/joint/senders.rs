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
use temper_authz::{Asker, Attr, May, SendAnalysis};
use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};

use super::env::ID;
use super::state::JointState;
use crate::model::ModelString;

/// Someone outside the kernel.
pub(super) const OUTSIDE: u32 = 0;

/// The senders a model knows, by number, and what Cedar lets each send.
pub(super) struct Senders {
    askers: Vec<Asker>,
    /// Each sender with resource-relative IDs forgotten after a different-ID hop.
    detached: Vec<u32>,
    /// Entity, then action, then each sender's answer.
    may: Option<BTreeMap<String, BTreeMap<String, Vec<May>>>>,
    /// The policies, to decide again a move whose answer depends on the
    /// request.
    policies: Option<SendAnalysis>,
    /// The answers decided again so far: a walk asks the same few questions
    /// over and over, and reading the policies takes far longer than a step.
    decided: RefCell<HashMap<Question, May>>,
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
            .field("may", &self.may)
            .field("reread", &self.reread)
            .finish()
    }
}

/// A move decided again: its sender, record type and action, its source's
/// id, and the record's tracked strings and booleans.
type Question = (
    u32,
    String,
    String,
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
        let may: Option<BTreeMap<String, BTreeMap<String, Vec<May>>>> = policies.map(|policies| {
            entities
                .into_iter()
                .map(|entity| {
                    let actions = by_name[entity.as_str()]
                        .actions
                        .iter()
                        .map(|action| {
                            let answers = askers
                                .iter()
                                .map(|asker| {
                                    policies.may(asker, &action.name, entity, &BTreeMap::new())
                                })
                                .collect();
                            (action.name.clone(), answers)
                        })
                        .collect();
                    (entity.clone(), actions)
                })
                .collect()
        });
        let reread = reread(policies, &askers, may.as_ref());
        Self {
            askers,
            detached,
            may,
            policies: policies.cloned(),
            decided: RefCell::default(),
            reread,
        }
    }

    /// Forget what Cedar lets each sender send of `moves` (entity, action),
    /// which the model no longer makes: neither a walk nor its digest reads
    /// them.
    pub(super) fn forget(&mut self, moves: &BTreeSet<(String, String)>) {
        let Some(may) = &mut self.may else {
            return;
        };
        for (entity, action) in moves {
            if let Some(actions) = may.get_mut(entity) {
                actions.remove(action);
            }
        }
        self.reread = reread(self.policies.as_ref(), &self.askers, self.may.as_ref());
    }

    /// Whether Cedar lets sender `by` send `action` to an `entity` record.
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
    /// of `state`: as the policies alone answer, or, when that depends on
    /// the request, as they answer given the sender's source id and the
    /// record's id and tracked fields in `state`.
    pub(super) fn may_in(&self, by: u32, entity: &str, action: &str, state: &JointState) -> May {
        let may = self.may(by, entity, action);
        let (May::Maybe, Some(policies), Some(asker), Some(target)) = (
            may,
            &self.policies,
            self.askers.get(by as usize),
            state.records.get(entity),
        ) else {
            return may;
        };
        // A declared reaction names its source's type; the kernel adds the
        // source record's id.
        let source_id = match asker {
            Asker::Service { context, .. } => match context.get("triggerSourceEntityType") {
                Some(Attr::Value(Value::String(source))) => state
                    .records
                    .get(source)
                    .and_then(|record| record.strings.get(ID)),
                _ => None,
            },
            Asker::Outside => None,
        };
        let question = (
            by,
            entity.to_string(),
            action.to_string(),
            source_id.cloned(),
            target.strings.clone(),
            target.booleans.clone(),
        );
        if let Some(may) = self.decided.borrow().get(&question) {
            return *may;
        }
        // A value the model made up is none of the literals it holds.
        let literals: BTreeSet<String> = source_id
            .into_iter()
            .chain(target.strings.values())
            .filter_map(|value| match value {
                ModelString::Literal(text) if !text.is_empty() => Some(text.clone()),
                _ => None,
            })
            .collect();
        let asker = match (asker, source_id) {
            (Asker::Service { name, context }, Some(id)) => {
                let mut context = context.clone();
                context.insert("triggerSourceEntityId".to_string(), symbol(id, &literals));
                service(name, context)
            }
            (asker, _) => asker.clone(),
        };
        let strings = target.strings.iter().map(|(field, value)| {
            let field = if field == ID { "id" } else { field.as_str() };
            (field.to_string(), symbol(value, &literals))
        });
        let booleans = target
            .booleans
            .iter()
            .map(|(field, value)| (field.clone(), Attr::Value(Value::Bool(*value))));
        let may = policies.may(&asker, action, entity, &strings.chain(booleans).collect());
        self.decided.borrow_mut().insert(question, may);
        may
    }

    /// The number of the sender a reaction runs as: its declared principal,
    /// or `invoker` without one. An inherited resource-relative ID remains
    /// known only when this hop preserves the receiver ID.
    pub(super) fn reaction(&self, declared_as: Option<&Asker>, invoker: u32, same_id: bool) -> u32 {
        declared_as
            .and_then(|asker| self.askers.iter().position(|a| a == asker))
            .map_or_else(
                || {
                    if same_id {
                        invoker
                    } else {
                        self.detached[invoker as usize]
                    }
                },
                |at| at as u32,
            )
    }

    /// Who sender `by` is, in words.
    pub(super) fn name(&self, by: u32) -> String {
        match self.askers.get(by as usize) {
            Some(Asker::Outside) => "an outside caller".to_string(),
            Some(Asker::Service { name, .. }) => format!("service:{name}"),
            None => format!("sender {by}"),
        }
    }
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
                        reread.extend(policies.applicable(&askers[by], action, entity));
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

fn service(name: &str, context: BTreeMap<String, Attr>) -> Asker {
    Asker::Service {
        name: name.to_string(),
        context,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::automaton::parse_automaton;

    #[test]
    fn inherited_source_id_does_not_become_a_different_targets_id() {
        // A[id1] -> B[id1] declares the service. B then forwards the
        // principal to C[id2] through a field, retaining A's id1 at runtime.
        let a = parse_automaton(
            r#"
[automaton]
name = "A"
states = ["Ready", "Done"]
initial = "Ready"
[[action]]
name = "Start"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "first"
kind = "entity"
target_entity = "B"
target_action = "Forward"
principal = "chain"
[action.triggers.resolve_target]
kind = "same_id"
"#,
        )
        .unwrap();
        let policy = SendAnalysis::new(
            r#"
permit(principal == Agent::"service:chain", action, resource) when {
    context has triggerSourceEntityId && resource has id &&
    context.triggerSourceEntityId == resource.id
};
"#,
        )
        .unwrap();
        let entities = vec!["A".to_string()];
        let senders = Senders::new(&BTreeMap::from([("A", &a)]), &entities, Some(&policy));
        let principal = declared(&a.actions[0].triggers[0], "A", "Start").unwrap();
        let b = senders.reaction(Some(&principal), OUTSIDE, true);
        assert_eq!(senders.may(b, "A", "Start"), May::Yes);
        let c = senders.reaction(None, b, false);
        assert_eq!(senders.may(c, "A", "Start"), May::Maybe);
        // More same-ID hops cannot restore knowledge of the original ID.
        assert_eq!(senders.reaction(None, c, true), c);
        assert_eq!(senders.reaction(None, c, false), c);
        // Same-ID chains preserve knowledge; a declaration starts fresh.
        assert_eq!(senders.reaction(None, b, true), b);
        assert_eq!(senders.reaction(Some(&principal), c, true), b);
    }
}
