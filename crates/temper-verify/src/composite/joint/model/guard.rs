//! A trigger's guard on related records in scope, read when its dispatch is
//! read: the runtime evaluates it after its source commits, on the related
//! records as they are then.

use temper_spec::predicate::{Truth, eval};

use super::super::super::{Dispatch, JointState};
use super::super::JointModel;
use super::super::commit::add;
use super::{Certainty, remove};

impl JointModel {
    /// The states reading `dispatch`'s trigger guard leads to: dropped when
    /// it does not hold, sent on when it does, and both when it may.
    pub(super) fn read_trigger_guard(
        &self,
        dispatch: &Dispatch,
        state: &JointState,
    ) -> Vec<(JointState, String, Certainty)> {
        let mut outcomes = Vec::new();
        let mut next = state.clone();
        remove(&mut next, dispatch);
        let holds = self.trigger_guard(dispatch, state);
        if holds != Truth::True {
            let text = format!("{dispatch}: its trigger's guard does not hold");
            let certainty = Certainty::unless(holds == Truth::False, Certainty::OneOf);
            outcomes.push((next.clone(), text, certainty));
        }
        if holds != Truth::False {
            let mut sent = dispatch.clone();
            sent.unread_guard = None;
            add(&mut next, sent);
            let text = format!("{dispatch}: its trigger's guard holds");
            let certainty = Certainty::unless(holds == Truth::True, Certainty::OneOf);
            outcomes.push((next, text, certainty));
        }
        outcomes
    }

    /// Whether the guard of the trigger that sent `dispatch` holds on the
    /// related records as `state` holds them, read on its source as it
    /// committed. The model holds one record of each type: a reference that
    /// names another record (one of its source's own type, or one its id
    /// field moved to) may find one in the runtime, so it reads as unknown.
    /// Before this read, every trigger guard on a related record went both
    /// ways; only the records the model holds decide one.
    fn trigger_guard(&self, dispatch: &Dispatch, state: &JointState) -> Truth {
        let (Some(committed), Some(index)) = (&dispatch.unread_guard, dispatch.via) else {
            return Truth::True;
        };
        let reaction = &self.reactions[index];
        let Some(guard) = &reaction.guard else {
            return Truth::True;
        };
        let own = committed.record();
        let mut env = self.env(&reaction.from, state);
        env.own = Some(&own);
        env.unheld_unknown = true;
        eval(guard, &env)
    }
}
