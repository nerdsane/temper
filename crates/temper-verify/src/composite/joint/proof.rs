//! One walk of the joint checks, and the digest its verdict is kept by.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, Invariant};
use temper_spec::predicate::Expr;

use super::liveness::{settle, settle_bytes};
use super::model::{self, JointModel, Sending};
use super::room::{Bound, Construction, MB, Room};
use super::{JointInvariantResult, JointLivenessResult, JointOutcome, LivenessOutcome};

mod widen;
use widen::Wider;

/// One walk of the joint checks: an entity's invariants that read related
/// records, or one liveness property. It holds all its walk reads, so its
/// verdict depends on nothing else.
#[derive(Debug)]
pub enum JointProof<'room> {
    Invariants(InvariantProof<'room>),
    Liveness(LivenessProof<'room>),
}

impl JointProof<'_> {
    /// What it proves, in words.
    pub fn name(&self) -> String {
        match self {
            Self::Invariants(proof) => format!("{}'s rules over several records", proof.entity),
            Self::Liveness(proof) => format!("{}.{}", proof.entity, proof.name),
        }
    }

    /// The digest of all its walk reads: the properties, and their joint
    /// model cut to what they read, with what Cedar lets each of its
    /// senders send. Two proofs with one digest walk the same model for the
    /// same properties, so a change to a spec or to Cedar that the cut
    /// leaves out leaves the digest as it was.
    pub fn digest(&self) -> String {
        // Every type a proof holds keeps its entries in order (no hash maps)
        // and prints all a walk reads of it, so this text is the same each
        // time. Two print less than they hold: a model's `Senders` leaves
        // out its cache and the policies no walk reads, and its `Expansion`
        // is the walk's own accounting. A kernel that changes those types
        // changes the text, and its proofs walk once again.
        let text = format!("{self:?}");
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }
}

/// Every walk the joint checks make for `automatons`: the invariants'
/// ([`invariant_proofs`]), then the liveness properties'
/// ([`liveness_proofs`]). Construction, retained sender profiles and answer
/// tables take their memory from `room`, returning it when each proof is dropped.
pub fn joint_proofs<'room>(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    room: &'room dyn Room,
) -> Vec<JointProof<'room>> {
    invariant_proofs(automatons, policies, room)
        .into_iter()
        .map(JointProof::Invariants)
        .chain(
            liveness_proofs(automatons, policies, room)
                .into_iter()
                .map(JointProof::Liveness),
        )
        .collect()
}

/// The walk that checks one entity's invariants that read related records,
/// with all it reads: those invariants and the joint model of their entity
/// and the types they name, cut to what they read. Its
/// [`JointProof::digest`] is of this value.
#[derive(Debug)]
pub struct InvariantProof<'room> {
    pub(super) entity: String,
    invariants: Vec<Invariant>,
    scope: BTreeSet<String>,
    /// The model, or why there is none: a type the invariants read that the
    /// app does not declare.
    model: Result<JointModel, String>,
    /// The model with the types reactions from outside the scope come from
    /// taken in too, built when there are any, to walk once more.
    wider: Option<Box<Wider<'room>>>,
    // Declared last so the model releases its allocations before its room.
    _memory: Construction<'room>,
}

impl InvariantProof<'_> {
    /// Walk the model, one result per invariant.
    pub fn check(&self, bound: Bound<'_>) -> Vec<JointInvariantResult> {
        let result = |inv: &Invariant, states, outcome| JointInvariantResult {
            entity: self.entity.clone(),
            invariant: inv.name.clone(),
            assert: inv.assert.to_string(),
            scope: self.scope.iter().cloned().collect(),
            states,
            in_flight_bound: None,
            outcome,
        };
        let model = match &self.model {
            Ok(model) => model,
            Err(reason) => {
                let reason = reason.clone();
                return self
                    .invariants
                    .iter()
                    .map(|inv| {
                        result(
                            inv,
                            0,
                            JointOutcome::Incomplete {
                                reason: reason.clone(),
                            },
                        )
                    })
                    .collect();
            }
        };
        let exprs: Vec<&Expr> = self.invariants.iter().map(|inv| &inv.assert).collect();
        let walk = model.walk(&self.entity, &exprs, bound);
        let mut results: Vec<JointInvariantResult> = self
            .invariants
            .iter()
            .zip(walk.outcomes)
            .map(|(inv, outcome)| {
                let mut found = result(inv, walk.states, outcome);
                found.in_flight_bound = walk.held_back.then_some(model::MAX_IN_FLIGHT);
                found
            })
            .collect();
        if let Some(wider) = &self.wider
            && walk.outside.iter().any(|from| !from.is_empty())
        {
            wider.walk(self, &walk.outside, &mut results, bound);
        }
        results
    }
}

/// The joint model that checks `invariants` of `name` with `scope`, cut to
/// what they read, its senders decided by `policies` and charged to
/// `memory`.
fn invariant_model(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    name: &str,
    invariants: &[Invariant],
    policies: Option<&SendAnalysis>,
    memory: &Construction<'_>,
) -> Result<JointModel, String> {
    let reserve = |bytes| memory.set(bytes);
    let exprs: Vec<&Expr> = invariants.iter().map(|inv| &inv.assert).collect();
    // Code outside the specs sends what a liveness property relies on,
    // whoever Cedar lets send it; a rule that must always hold has to hold
    // through those moves too.
    let relies_on = scope
        .iter()
        .flat_map(|entity| {
            by_name[entity.as_str()]
                .liveness
                .iter()
                .flat_map(move |live| {
                    live.relies_on
                        .iter()
                        .map(move |action| (entity.clone(), action.clone()))
                })
        })
        .collect();
    let sending = Sending {
        policies,
        reserve: Some(&reserve),
        kernel_moves: true,
        relies_on,
    };
    JointModel::new(by_name, scope, name, &exprs, sending, true)
        .without_silent(by_name)
        .built(memory)
}

/// The walks that check every `[[invariant]]` in `automatons` that reads a
/// related entity: one per entity, on the joint model of it and the types
/// its invariants name. `policies` decide each move as its sender, as for
/// liveness; without them every sender may send everything. `room` bounds
/// sender construction and retains its charge until each proof is dropped.
pub fn invariant_proofs<'room>(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    room: &'room dyn Room,
) -> Vec<InvariantProof<'room>> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut proofs = Vec::new();
    for (name, automaton) in &by_name {
        let invariants: Vec<Invariant> = automaton
            .invariants
            .iter()
            .filter(|inv| !inv.assert.related_types().is_empty())
            .cloned()
            .collect();
        if invariants.is_empty() {
            continue;
        }
        let mut scope: BTreeSet<String> = BTreeSet::from([name.to_string()]);
        let mut missing = Vec::new();
        for inv in &invariants {
            for (entity_type, _) in inv.assert.related_types() {
                if by_name.contains_key(entity_type.as_str()) {
                    scope.insert(entity_type);
                } else {
                    missing.push(entity_type);
                }
            }
        }
        let memory = Construction::new(room);
        let model = if missing.is_empty() {
            invariant_model(&by_name, &scope, name, &invariants, policies, &memory)
        } else {
            Err(format!(
                "it reads {}, which this app does not declare",
                missing.join(", ")
            ))
        };
        let wider = match &model {
            Ok(model) => Wider::new(
                &by_name,
                &scope,
                model.outside_types(),
                name,
                &invariants,
                policies,
                room,
            )
            .map(Box::new),
            Err(_) => None,
        };
        proofs.push(InvariantProof {
            _memory: memory,
            entity: name.to_string(),
            invariants,
            scope,
            model,
            wider,
        });
    }
    proofs
}

/// The walk that checks one `[[liveness]]` with `eventually`, with all it
/// reads: the property and the joint model of its entity and the types its
/// condition names, cut to what it reads. Its
/// [`JointProof::digest`] is of this value.
#[derive(Debug)]
pub struct LivenessProof<'room> {
    pub(super) entity: String,
    pub(super) name: String,
    goal: Expr,
    relies_on: Vec<String>,
    scope: BTreeSet<String>,
    /// The model, or why there is none: a type the condition reads that the
    /// app does not declare.
    model: Result<JointModel, String>,
    // Declared last so the model releases its allocations before its room.
    _memory: Construction<'room>,
}

impl LivenessProof<'_> {
    /// Walk the model and decide whether the records reach the condition.
    pub fn check(&self, bound: Bound<'_>) -> JointLivenessResult {
        let mut result = JointLivenessResult {
            entity: self.entity.clone(),
            name: self.name.clone(),
            eventually: self.goal.to_string(),
            relies_on: self.relies_on.clone(),
            scope: self.scope.iter().cloned().collect(),
            states: 0,
            in_flight_bound: None,
            outcome: LivenessOutcome::Holds,
            unwalked: Vec::new(),
        };
        let model = match &self.model {
            Ok(model) => model,
            Err(reason) => {
                result.outcome = LivenessOutcome::Incomplete {
                    reason: reason.clone(),
                };
                return result;
            }
        };
        let mut explored = model.walk_with(bound, model::Keep::Edges, |_, _, _| false);
        result.states = explored.len();
        result.in_flight_bound = explored.held_back.then_some(model::MAX_IN_FLIGHT);
        result.unwalked = explored.unwalked();
        let settling = settle_bytes(&explored);
        // Past where the walk stops, phase two (`model/guided.rs`).
        if let Some(reason) = explored.stopped.clone() {
            result.outcome =
                model.guided_liveness(&self.entity, &self.goal, explored, reason, bound);
            return result;
        }
        result.outcome = match explored.held.add(settling) {
            Ok(()) => match settle(model, &self.entity, &self.goal, &explored) {
                (LivenessOutcome::Holds, _) if !result.unwalked.is_empty() => {
                    LivenessOutcome::Incomplete {
                        reason: model::unwalked_reason(&result.unwalked),
                    }
                }
                (LivenessOutcome::Holds, false) => LivenessOutcome::Vacuous {
                    premise: super::premise(&self.goal)
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                },
                (outcome, _) => outcome,
            },
            Err(reason) => {
                let reason = format!(
                    "deciding the walk does not fit in the memory it may hold: {reason} \
                         (it held {} MB at {} states and needs {} MB more)",
                    explored.held.bytes / MB,
                    explored.len(),
                    settling / MB
                );
                model.guided_liveness(&self.entity, &self.goal, explored, reason, bound)
            }
        };
        result
    }
}

/// The walks that check every `[[liveness]]` with `eventually` in
/// `automatons`: one per property, on the joint model of its entity and the
/// types its condition names. `policies` decide each move as its sender;
/// without them every sender may send everything. `room` bounds sender
/// construction and retains its charge until each proof is dropped.
pub fn liveness_proofs<'room>(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    room: &'room dyn Room,
) -> Vec<LivenessProof<'room>> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut proofs = Vec::new();
    for (name, automaton) in &by_name {
        for live in &automaton.liveness {
            let Some(goal) = &live.eventually else {
                continue;
            };
            let mut scope = BTreeSet::from([name.to_string()]);
            let mut missing = Vec::new();
            for (entity_type, _) in goal.related_types() {
                if by_name.contains_key(entity_type.as_str()) {
                    scope.insert(entity_type);
                } else {
                    missing.push(entity_type);
                }
            }
            let memory = Construction::new(room);
            let reserve = |bytes| memory.set(bytes);
            let model = if missing.is_empty() {
                let sending = Sending {
                    policies,
                    reserve: Some(&reserve),
                    kernel_moves: true,
                    relies_on: live
                        .relies_on
                        .iter()
                        .map(|action| (name.to_string(), action.clone()))
                        .collect(),
                };
                JointModel::new(&by_name, &scope, name, &[goal], sending, true)
                    .without_silent(&by_name)
                    .built(&memory)
            } else {
                Err(format!(
                    "it reads {}, which this app does not declare",
                    missing.join(", ")
                ))
            };
            proofs.push(LivenessProof {
                _memory: memory,
                entity: name.to_string(),
                name: live.name.clone(),
                goal: goal.clone(),
                relies_on: live.relies_on.clone(),
                scope,
                model,
            });
        }
    }
    proofs
}
