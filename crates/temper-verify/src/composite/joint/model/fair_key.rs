//! Which move a walked move is, so a fairness check can match "possible here"
//! with "taken there" across states.

use std::fmt;

use super::super::JointState;
use super::JointModel;
use super::step::Move;

/// Who makes a move, in the terms of what the runtime owes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) enum MoveKind {
    /// A request from someone outside the kernel.
    Outside,
    /// A reaction from a type outside the scope.
    OutsideReaction,
    /// A state timer.
    Timer,
    /// A module call ending: its answer, its failure, or no answer at all.
    Call,
    /// A module call lost to a restart.
    Lost,
    /// A dispatch read on the dispatch path, or taken or refused by its actor.
    Delivery,
}

/// One move, across the states it can be made in.
///
/// The instance leaves out a dispatch's parameters and a call's asked fields:
/// a walk renames symbols state by state, so their values do not name one
/// dispatch across states. Two dispatches or calls differing only there share a
/// key, so the ending of one counts for the other. A BROKEN verdict may rest on
/// that. HOLDS does not: each stays possible until it ends, so sharing a key
/// never makes a constraint harder to satisfy.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) struct FairKey {
    pub(in crate::composite::joint) kind: MoveKind,
    /// The entity type the move acts on.
    pub(in crate::composite::joint) entity: String,
    /// The action it sends; empty for a call that fails with no `on_failure`,
    /// or that ends with no answer.
    pub(in crate::composite::joint) action: String,
    /// Which call or dispatch, for those kinds; which state, for a timer.
    pub(in crate::composite::joint) instance: String,
    /// The action of the timer this move starts again, in the state it is made
    /// in: its `reset_on`.
    pub(in crate::composite::joint) resets: Option<(String, String)>,
}

impl JointModel {
    /// Which move `mv` is, as made in `state`.
    pub(in crate::composite::joint) fn fair_key(&self, state: &JointState, mv: &Move) -> FairKey {
        let mut key = mv.fair_key();
        let status = state
            .records
            .get(key.entity.as_str())
            .map(|record| record.status.as_str());
        let timeout = status.and_then(|status| self.timeout(&key.entity, status));
        if key.kind == MoveKind::Timer {
            key.instance = status.unwrap_or_default().to_string();
        } else if !matches!(mv, Move::Read(_))
            && let Some(timeout) = timeout
            && timeout.reset_on.contains(&key.action)
        {
            key.resets = Some((timeout.action.clone(), timeout.state.clone()));
        }
        key
    }
}

impl Move {
    /// Which move this is, whichever state it is made in.
    fn fair_key(&self) -> FairKey {
        let key = |kind, entity: &str, action: &str, instance: String| FairKey {
            kind,
            entity: entity.to_string(),
            action: action.to_string(),
            instance,
            resets: None,
        };
        let call_instance = |call: &super::super::Call| {
            format!(
                "{} call answered by {:?} or {:?} as {}",
                call.module, call.on_success, call.on_failure, call.by
            )
        };
        let dispatch_instance = |step: &str, dispatch: &super::super::Dispatch| {
            format!("{step} via {:?} as {}", dispatch.via, dispatch.by)
        };
        match self {
            Move::Take(entity, action, _) | Move::Send(entity, action, _) => {
                key(MoveKind::Outside, entity, &action.name, String::new())
            }
            Move::Reaction(entity, action, _) => key(
                MoveKind::OutsideReaction,
                entity,
                &action.name,
                String::new(),
            ),
            Move::Timer(entity, action) => key(MoveKind::Timer, entity, action, String::new()),
            Move::Answer(call, action) => {
                key(MoveKind::Call, &call.to, &action.name, call_instance(call))
            }
            Move::Fail(call) => key(
                MoveKind::Call,
                &call.to,
                call.on_failure.as_deref().unwrap_or_default(),
                call_instance(call),
            ),
            Move::Silent(call) => key(MoveKind::Call, &call.to, "", call_instance(call)),
            Move::Lost(call) => key(
                MoveKind::Lost,
                &call.to,
                call.on_failure.as_deref().unwrap_or_default(),
                call_instance(call),
            ),
            Move::Read(dispatch) => key(
                MoveKind::Delivery,
                &dispatch.to,
                &dispatch.action,
                dispatch_instance("read", dispatch),
            ),
            Move::Arrive(dispatch) => key(
                MoveKind::Delivery,
                &dispatch.to,
                &dispatch.action,
                dispatch_instance("arrive", dispatch),
            ),
        }
    }
}

impl fmt::Display for FairKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let by = match self.kind {
            MoveKind::Outside => "a request",
            MoveKind::OutsideReaction => "a reaction from outside the rule's scope",
            MoveKind::Timer => "a timer",
            MoveKind::Call => "a module call ending",
            MoveKind::Lost => "a restart",
            MoveKind::Delivery => "a delivery",
        };
        write!(f, "{}.{} ({by})", self.entity, self.action)
    }
}
