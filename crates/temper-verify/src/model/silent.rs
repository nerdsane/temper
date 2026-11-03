//! Moves that change nothing a level reads.

use temper_spec::predicate::Expr;

use super::{LivenessKind, TemperModel};

impl TemperModel {
    /// Whether transition `index` is silent: it stays in its place, has no
    /// guard, takes no parameters and writes nothing the model holds, no
    /// other transition has its action's name, and no entry rule guards a
    /// place it leaves.
    ///
    /// Firing one leaves the state as it was, so the levels find the same
    /// with or without it except through its places: whether one is
    /// reachable (a move no reachable state enables is dead), whether one
    /// is terminal (a move enabled there breaks `Terminal`), and whether it
    /// is the only way out of a reachable state (`NoDeadlock`). A spec with
    /// a `reaches` liveness has none: a move that stays in place is a loop
    /// a record can take forever, which breaks the rule, and its simulation
    /// counts walks that end short of the target.
    pub fn is_silent(&self, index: usize) -> bool {
        let t = &self.transitions[index];
        let stays = t.to_state.is_none()
            || t.from_states
                .iter()
                .all(|from| Some(from) == t.to_state.as_ref());
        !t.from_states.is_empty()
            && stays
            && t.guard == Expr::Const(true)
            && t.effects.is_empty()
            && t.params.is_empty()
            && t.string_params.is_empty()
            && self
                .transitions
                .iter()
                .filter(|other| other.name == t.name)
                .count()
                == 1
            && !self
                .entry_rules
                .iter()
                .any(|rule| t.from_states.contains(&rule.place))
            && !self.liveness.iter().any(|live| {
                matches!(&live.kind, LivenessKind::ReachesState { targets, .. } if !targets.is_empty())
            })
    }

    /// This model without its silent transitions.
    pub fn without_silent(&self) -> TemperModel {
        let mut model = self.clone();
        let mut index = 0;
        model.transitions.retain(|_| {
            index += 1;
            !self.is_silent(index - 1)
        });
        model
    }
}
