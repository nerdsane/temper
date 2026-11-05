//! How many in flight a proof's walk holds: as many as fit its room.

use super::JointOutcome;
use super::model::{self, JointModel};

/// The walk `walk` makes, with as many in flight as fit: while it held a
/// move back for [`JointModel::in_flight`], saw every state, and a rule
/// still holds, walked again with twice as many. Doubling keeps the walks
/// before the last under the last one's cost together, where one more at a
/// time walks once for each. A walk that does not fit keeps the last that
/// did, with the breaks it found sure; the bound is then the last one walked
/// whole.
pub(super) fn grown_walk(model: &JointModel, walk: impl Fn() -> model::Walk) -> model::Walk {
    let holds = |w: &model::Walk| {
        w.outcomes
            .iter()
            .any(|o| matches!(o, JointOutcome::Holds | JointOutcome::Vacuous { .. }))
    };
    let mut last = walk();
    while last.held_back && !last.stopped && holds(&last) {
        let fitted = model.in_flight();
        model.hold(fitted * 2);
        let next = walk();
        if !next.stopped {
            last = next;
            continue;
        }
        model.hold(fitted);
        for (kept, found) in last.outcomes.iter_mut().zip(next.outcomes) {
            if matches!(found, JointOutcome::Broken { .. })
                && matches!(kept, JointOutcome::Holds | JointOutcome::Vacuous { .. })
            {
                *kept = found;
            }
        }
        break;
    }
    last
}
