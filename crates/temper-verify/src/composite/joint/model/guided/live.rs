//! Phase two for a liveness property: a settled state where the condition
//! is false is stuck only where a complete walk of the moves the liveness
//! check counts on from it finishes and [`settle`] finds a state there that
//! cannot reach the condition.

use std::collections::{BTreeMap, HashMap, VecDeque};

use temper_spec::predicate::{Expr, Truth, eval};

use super::super::super::liveness::{settle, settle_bytes};
use super::super::super::pack::{Packed, Packer};
use super::super::super::room::{Bound, Held, block};
use super::super::super::{JointState, LivenessOutcome, StuckCase};
use super::super::JointModel;
use super::super::held::Tables;
use super::super::step::Certainty;
use super::super::walk::{Edge, Explored, Made, Node};
use super::{Budget, Engine, Look, could_not_start, plain, print, reads, seeds};

/// What a confirming walk found.
enum Confirm {
    /// Every state it reached reaches the condition: their fingerprints.
    Reaches(Vec<u64>),
    /// It finished, and some state it reached cannot.
    Stuck(LivenessOutcome),
    /// It did not finish, or cannot be trusted to have seen every way out.
    Open,
}
impl Engine<'_, '_> {
    /// Walk every move the liveness check counts on from `start`, to the
    /// last state they reach, and decide each state it reaches as
    /// [`settle`] does.
    fn confirm(&mut self, owner: &str, goal: &Expr, start: &JointState, cap: usize) -> Confirm {
        let model = self.model;
        let mut sub = Explored {
            nodes: Vec::new(),
            packer: Packer::default(),
            made: Vec::new(),
            edges: Vec::new(),
            stopped: None,
            held_back: false,
            outside: BTreeMap::new(),
            held: Held::new(self.bound),
        };
        let mut seen: HashMap<Packed, u32> = HashMap::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        let mut tables = Tables::default();
        let mut steps = 0;
        let walked = (|| -> Result<bool, String> {
            let packed = sub.packer.pack(start);
            tables.reserve(&mut sub, &mut seen, &mut queue, true, 1, 0)?;
            tables.states += block(size_of::<[usize; 2]>() + 4 * packed.len());
            seen.insert(packed.clone(), 0);
            queue.push_back(0);
            sub.nodes.push(Node {
                packed,
                parent: None,
                sure: false,
                queued: false,
            });
            while let Some(index) = queue.pop_front() {
                if steps == cap || self.left == 0 {
                    return Ok(false);
                }
                steps += 1;
                self.left -= 1;
                self.stats.steps += 1;
                let state = sub.state(index);
                tables.make_room(&mut sub, &mut seen, &mut queue, true)?;
                let before = [sub.nodes.len(), sub.made.len(), sub.edges.len()];
                let failing = eval(goal, &model.env(owner, &state)) != Truth::True;
                let (moves, room) =
                    model.within(&mut sub.held, &mut tables, || model.moves(&state))?;
                for mv in &moves {
                    let relied = model.relies(mv);
                    // A reaction from outside the scope is kept too: `settle`
                    // does not call a state stuck when one may lead out of it.
                    if !(mv.by_system() || mv.is_outside_reaction() || relied && failing) {
                        continue;
                    }
                    let ((nexts, held), built) =
                        model.within(&mut sub.held, &mut tables, || model.next(&state, mv))?;
                    sub.held_back |= held;
                    tables.reserve(&mut sub, &mut seen, &mut queue, true, 0, nexts.len())?;
                    sub.made.push(Made {
                        from: index as u32,
                        by_system: mv.by_system(),
                        relied,
                        outside: mv.is_outside_reaction(),
                        covered: !held
                            && !nexts.is_empty()
                            && nexts.iter().all(|n| n.certainty != Certainty::Unsure),
                        held,
                    });
                    let made = (sub.made.len() - 1) as u32;
                    for next in nexts {
                        let packed = sub.packer.pack(&next.state);
                        let to = match seen.get(&packed) {
                            Some(&to) => to,
                            None => {
                                tables.reserve(&mut sub, &mut seen, &mut queue, false, 1, 0)?;
                                let to = sub.nodes.len() as u32;
                                let label = sub.packer.id(&next.label);
                                tables.states += block(size_of::<[usize; 2]>() + 4 * packed.len());
                                seen.insert(packed.clone(), to);
                                sub.nodes.push(Node {
                                    packed,
                                    parent: Some((index as u32, label)),
                                    sure: false,
                                    queued: false,
                                });
                                queue.push_back(to as usize);
                                to
                            }
                        };
                        sub.edges.push(Edge {
                            made,
                            to,
                            sure: next.certainty == Certainty::Sure,
                        });
                    }
                    tables.give(&mut sub.held, built);
                    tables.count(&mut sub, &seen, &queue)?;
                }
                tables.give(&mut sub.held, room);
                let after = [sub.nodes.len(), sub.made.len(), sub.edges.len()];
                tables.saw([0, 1, 2].map(|i| after[i] - before[i]));
            }
            drop(seen);
            drop(queue);
            sub.held.add(settle_bytes(&sub))?;
            Ok(true)
        })();
        match walked {
            Ok(true) => {}
            Ok(false) | Err(_) => return Confirm::Open,
        }
        match settle(model, owner, goal, &sub) {
            (LivenessOutcome::Holds, _) => {
                Confirm::Reaches((0..sub.len()).map(|i| print(&sub.state(i))).collect())
            }
            (stuck @ LivenessOutcome::Stuck { .. }, _) => Confirm::Stuck(stuck),
            _ => Confirm::Open,
        }
    }
}

impl JointModel {
    /// Phase two for the liveness property `goal` of `owner`, after the
    /// breadth-first walk `explored` stopped (`why`): NEVER SETTLES when a
    /// guided walk reaches a settled state where the goal is false and the
    /// complete walk of the system's moves from it finds a state that cannot
    /// reach it; otherwise NOT CHECKED, saying how far the walks got.
    pub(in crate::composite::joint) fn guided_liveness(
        &self,
        owner: &str,
        goal: &Expr,
        explored: Explored<'_>,
        why: String,
        bound: Bound<'_>,
    ) -> LivenessOutcome {
        let budget = Budget::INSTALL;
        let order = &mut plain;
        if explored.len() == 0 {
            return LivenessOutcome::Incomplete { reason: why };
        }
        let mut engine = Engine::new(self, bound, reads(owner, &[goal]), budget.steps);
        let seeds = seeds(&explored, &mut engine.packer);
        drop(explored);
        let mut stuck: Option<LivenessOutcome> = None;
        // The condition was false where the walk went by a move it cannot be
        // sure of.
        let mut maybe = false;
        let ran = engine.run(&seeds, order, &mut |engine, trace, state| {
            if !state.settled() || eval(goal, &engine.model.env(owner, state)) == Truth::True {
                return Look::Go;
            }
            // A stuck verdict refuses an install, so the way here is sure too.
            // Left untried, a sure way here later still gets its walk.
            if !trace.last().is_some_and(|step| step.2) {
                maybe = true;
                return Look::Go;
            }
            let key = print(state);
            if engine.good.contains_key(&key) {
                return Look::Go;
            }
            match Engine::mark(&mut engine.held, &mut engine.tried, key) {
                Ok(true) => {}
                Ok(false) | Err(_) => return Look::Go,
            }
            engine.stats.candidates += 1;
            let cap = budget.confirm;
            match engine.confirm(owner, goal, state, cap) {
                Confirm::Reaches(prints) => {
                    for key in prints {
                        if Engine::mark(&mut engine.held, &mut engine.good, key).is_err() {
                            break;
                        }
                    }
                    Look::Go
                }
                Confirm::Open => {
                    engine.stats.unconfirmed += 1;
                    Look::Go
                }
                Confirm::Stuck(LivenessOutcome::Stuck {
                    stuck: count,
                    cases,
                }) => {
                    let Ok(prefix) = engine.replay(trace) else {
                        engine.stats.unconfirmed += 1;
                        return Look::Go;
                    };
                    engine.stats.found = Some((engine.stats.steps, engine.seen.len()));
                    let cases = cases
                        .into_iter()
                        .map(|case| StuckCase {
                            path: prefix
                                .iter()
                                .cloned()
                                .chain(case.path.into_iter().skip(1))
                                .collect(),
                            moves: case.moves,
                        })
                        .collect();
                    stuck = Some(LivenessOutcome::Stuck {
                        stuck: count,
                        cases,
                    });
                    Look::Stop
                }
                Confirm::Stuck(_) => Look::Go,
            }
        });
        if let Err(reason) = ran {
            engine.stats.stopped = Some(reason);
        }
        let stats = engine.stats.clone();
        match stuck {
            Some(outcome) => outcome,
            None if stats.walks == 0 => LivenessOutcome::Incomplete {
                reason: could_not_start(&why, stats.stopped.as_deref().unwrap_or("no seeds")),
            },
            None => {
                let mut reason = stats.describe(&why);
                if maybe {
                    reason.push_str(
                        "; the condition was false on a way the runtime may never take, so it is not reported",
                    );
                }
                LivenessOutcome::Incomplete { reason }
            }
        }
    }
}
