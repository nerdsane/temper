//! Making a walked state's moves, each from what the thread remembers of it
//! or afresh, and looking up where they lead.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::super::super::Dispatch;
use super::super::super::state::render;
use super::super::moves::dispatch_move;
use super::super::step::{Certainty, Move};
use super::checked::{BATCH, To, Walked, Walker, Walking, Way};
use super::levels::deeper;
use super::memo::{Holder, Slot, check_step};
use super::shared::{read, write};
use super::{Keep, Made, Via};

/// A move of a state, by where the thread remembers it.
#[derive(Clone, Copy)]
pub(super) enum Target<'a> {
    /// Move `at` of those a holder has at a place.
    Group(u32, Holder, usize),
    /// A dispatch's next move from a place, by the dispatch's number.
    Dispatch(u32, u32, &'a Arc<Dispatch>),
}

impl Target<'_> {
    /// The move.
    fn mv(self, walking: &Walking<'_>) -> Move {
        match self {
            Target::Group(place, holder, at) => walking.memo.group_moves(place, holder)[at].clone(),
            Target::Dispatch(_, _, dispatch) => dispatch_move(dispatch),
        }
    }

    /// Where the thread remembers it goes.
    fn slot(self, walking: &Walking<'_>) -> Slot {
        match self {
            Target::Group(place, holder, at) => walking.memo.group_slot(place, holder, at),
            Target::Dispatch(place, id, _) => walking.memo.dispatch_slot(place, id),
        }
    }
}

impl Walker {
    /// Make the moves `holder` has at `place` from the state. None when the
    /// walk ended first.
    pub(super) fn walk_moves(
        &self,
        place: u32,
        holder: Holder,
        walking: &mut Walking<'_>,
        jobs: &mut Vec<Walked>,
    ) -> Option<()> {
        if let Some(checked) = &mut walking.checked_moves {
            checked.extend_from_slice(walking.memo.group_moves(place, holder));
        }
        let count = walking.memo.move_count(place, holder).unwrap_or(0);
        for at in 0..count {
            self.walk_one(Target::Group(place, holder, at), walking, jobs)?;
        }
        Some(())
    }

    /// Make one move from the state. None when the walk ended first.
    pub(super) fn walk_one(
        &self,
        target: Target<'_>,
        walking: &mut Walking<'_>,
        jobs: &mut Vec<Walked>,
    ) -> Option<()> {
        let by = walking.by;
        walking.by += 1;
        let slot = target.slot(walking);
        if slot.known() {
            self.walk_remembered(target, slot, by, walking)?;
        } else {
            self.walk_afresh(target, by, walking)?;
        }
        if walking.batch.ways.len() >= BATCH {
            self.add(walking.job, &mut walking.batch, jobs);
        }
        Some(())
    }

    /// Make a move from what the thread remembers of it. None when the walk
    /// ended first.
    fn walk_remembered(
        &self,
        target: Target<'_>,
        slot: Slot,
        by: u32,
        walking: &mut Walking<'_>,
    ) -> Option<()> {
        let model = &*self.model;
        let state = walking.state;
        let unsure = match target {
            Target::Group(place, holder, at) => {
                model.unsure(state, &walking.memo.group_moves(place, holder)[at])
            }
            Target::Dispatch(_, _, dispatch) => model.overtakes(state, dispatch),
        };
        walking.memo.steps += 1;
        let check = check_step(walking.memo.steps);
        let mut made_here = Vec::new();
        let made = self.note_move(target, walking);
        let (mut held, mut covered, mut ways) = (false, true, 0);
        for way in slot.ways() {
            let (known, list) = walking.memo.way(way, state);
            let known = if unsure { Certainty::Unsure } else { known };
            if !model.settled_quietly(&mut *walking.memo, list, &self.shared.packer) {
                held = true;
                continue;
            }
            if check {
                made_here.push((walking.memo.scratch.clone(), known));
            }
            covered &= known != Certainty::Unsure;
            self.reached(known, by, ways, made, walking);
            ways += 1;
        }
        self.settle_move(made, held, covered && ways > 0, walking);
        if check {
            let mv = target.mv(walking);
            let (nexts, fresh_held) = self.built(|| model.next(state, &mv))?;
            assert!(
                held == fresh_held
                    && made_here.len() == nexts.len()
                    && made_here
                        .iter()
                        .zip(&nexts)
                        .all(|((s, k), next)| *s == next.state && *k == next.certainty),
                "a remembered move differs from the move made afresh: {mv:?} from {}",
                render(state)
            );
        }
        Some(())
    }

    /// Make a move now, and remember it unless it read what is in flight.
    /// None when the walk ended first.
    fn walk_afresh(&self, target: Target<'_>, by: u32, walking: &mut Walking<'_>) -> Option<()> {
        let model = &*self.model;
        let state = walking.state;
        let mv = target.mv(walking);
        model.expansion.flight_was_read();
        let outcomes = self.built(|| model.outcomes(state, &mv))?;
        let (nexts, held) = match outcomes {
            Some(outcomes) => {
                if !model.expansion.flight_was_read() {
                    match target {
                        Target::Group(place, holder, at) => {
                            walking.memo.keep_group_move(
                                (place, holder, at),
                                state,
                                &outcomes,
                                &self.shared.packer,
                            );
                        }
                        Target::Dispatch(place, id, _) => {
                            walking.memo.keep_dispatch(
                                (place, id),
                                state,
                                &outcomes,
                                &self.shared.packer,
                            );
                        }
                    }
                    self.remember(walking.memo.counted());
                }
                self.built(|| model.finished(state, &mv, outcomes.clone()))?
            }
            None => (Vec::new(), false),
        };
        let made = self.note_move(target, walking);
        let covered = !nexts.is_empty() && nexts.iter().all(|n| n.certainty != Certainty::Unsure);
        for (outcome, next) in nexts.iter().enumerate() {
            walking.memo.scratch.clone_from(&next.state);
            self.reached(next.certainty, by, outcome as u32, made, walking);
        }
        self.settle_move(made, held, covered, walking);
        Some(())
    }

    /// Note a move a state makes, for a walk that keeps its moves, and say
    /// which it is.
    fn note_move(&self, target: Target<'_>, walking: &mut Walking<'_>) -> u32 {
        if self.keep == Keep::Edges {
            let mv = target.mv(walking);
            walking.batch.made.push(Made {
                from: walking.job.at,
                by_system: mv.by_system(),
                relied: self.model.relies(&mv),
                outside: mv.is_outside_reaction(),
                covered: false,
                held: false,
            });
        }
        walking.batch.made.len().saturating_sub(1) as u32
    }

    /// Say what the move `made` came to.
    fn settle_move(&self, made: u32, held: bool, covered: bool, walking: &mut Walking<'_>) {
        if held {
            self.shared.held_back.store(true, Ordering::Relaxed);
        }
        if self.keep == Keep::Edges {
            let made = &mut walking.batch.made[made as usize];
            made.held = held;
            made.covered = !held && covered;
        }
    }

    /// A move led to the state in the thread's scratch state, by way
    /// `outcome` of it.
    fn reached(
        &self,
        known: Certainty,
        by: u32,
        outcome: u32,
        made: u32,
        walking: &mut Walking<'_>,
    ) {
        let shared = &*self.shared;
        let edges = self.keep == Keep::Edges;
        let memo = &mut *walking.memo;
        if !memo.mirror.known_into(&memo.scratch, &mut memo.packed) {
            if !read(&shared.packer).known_into(&memo.scratch, &mut memo.packed) {
                write(&shared.packer).pack_into(&memo.scratch, &mut memo.packed);
            }
            memo.mirror.learn(&memo.scratch, &memo.packed);
        }
        let packed = memo.packed.as_slice();
        let depth = deeper(walking.job.depth);
        let surely = walking.sure && known == Certainty::Sure;
        let found = shared.seen.get(packed).map(|seen| *seen);
        if let Some(seen) = found {
            shared.levels.found_again(seen.depth, depth);
        }
        let to = match found {
            Some(seen) if self.keep != Keep::Sure || !surely || seen.sure => {
                if !edges {
                    return;
                }
                To::Known(seen.at)
            }
            _ => To::Check {
                packed: packed.into(),
                via: Via { by, outcome },
                surely,
            },
        };
        walking.batch.ways.push(Way {
            made,
            to,
            sure: known == Certainty::Sure,
        });
    }
}
