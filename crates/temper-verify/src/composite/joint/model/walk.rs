//! The breadth-first walk over joint states.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use super::super::JointOutcome;
use super::super::pack::{Packed, Packer};
use super::super::room::{Bound, Held, MB, block};
use super::super::state::render;
use super::super::{JointState, JointStep, premise};
use super::held::Tables;
use super::step::Certainty;
use super::{JointModel, Walk};
use temper_spec::predicate::{Expr, Name, Truth, eval};

/// One move made from a walked state.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Made {
    pub(in crate::composite::joint) from: u32,
    /// The system made it: a timer, a module, or the kernel's dispatches.
    pub(in crate::composite::joint) by_system: bool,
    /// It sends an action outside code is relied on to send.
    pub(in crate::composite::joint) relied: bool,
    /// It always ends in one of the states its edges reach: it leads
    /// somewhere, none of its states is unsure, and none was held back.
    pub(in crate::composite::joint) covered: bool,
}

/// A move's way from one walked state to another.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Edge {
    /// The move, an index into [`Explored::made`].
    pub(in crate::composite::joint) made: u32,
    pub(in crate::composite::joint) to: u32,
    /// The move can lead here.
    pub(in crate::composite::joint) sure: bool,
}

/// What a walk saw, and the memory it holds for it.
pub(in crate::composite::joint) struct Explored<'a> {
    /// Each state walked, packed, with the state and move (a name in
    /// `packer`) it was first reached by.
    pub(super) nodes: Vec<(Packed, Option<(u32, u32)>)>,
    pub(super) packer: Packer,
    /// Every move made from a walked state, when asked for.
    pub(in crate::composite::joint) made: Vec<Made>,
    /// Every way those moves lead between walked states.
    pub(in crate::composite::joint) edges: Vec<Edge>,
    /// Why the walk stopped before seeing every state.
    pub(in crate::composite::joint) stopped: Option<String>,
    /// Whether a move was held back for [`super::MAX_IN_FLIGHT`].
    pub(in crate::composite::joint) held_back: bool,
    /// The moves the runtime could make in a walked state that the walk
    /// never takes.
    unwalked: BTreeSet<Unwalked>,
    /// The memory its tables hold, given back when it is dropped.
    pub(in crate::composite::joint) held: Held<'a>,
}

/// A move the runtime could make that the walk never takes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) struct Unwalked {
    /// `Entity.Action`.
    pub(in crate::composite::joint) action: String,
    /// Who sends it in the runtime.
    pub(in crate::composite::joint) sender: String,
    /// The record's status where it could.
    pub(in crate::composite::joint) from: String,
}

impl Explored<'_> {
    /// Stop the walk: its room said no.
    fn stop_for_room(&mut self, reason: String) {
        self.stopped = Some(format!(
            "the walk does not fit in the memory it may hold: {reason} \
             (it held {} MB at {} states)",
            self.held.bytes / MB,
            self.len()
        ));
    }

    /// How many states the walk saw.
    pub(in crate::composite::joint) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// The state walked `index`th.
    pub(in crate::composite::joint) fn state(&self, index: usize) -> JointState {
        self.packer.unpack(&self.nodes[index].0)
    }

    /// The moves from the start to the state walked `index`th.
    pub(in crate::composite::joint) fn path(&self, mut index: usize) -> Vec<JointStep> {
        let mut steps = Vec::new();
        loop {
            let parent = self.nodes[index].1;
            steps.push(JointStep {
                action: parent.map(|(_, label)| self.packer.name(label).to_string()),
                state: render(&self.state(index)),
            });
            match parent {
                Some((from, _)) => index = from as usize,
                None => break,
            }
        }
        steps.reverse();
        steps
    }

    /// The moves the runtime could make that the walk never took, one line
    /// per sender: `who may take Entity.Action (from Status, ...), ...`.
    pub(in crate::composite::joint) fn unwalked(&self) -> Vec<String> {
        let mut by: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
        for found in &self.unwalked {
            by.entry(&found.sender)
                .or_default()
                .entry(&found.action)
                .or_default()
                .push(&found.from);
        }
        by.into_iter()
            .map(|(sender, actions)| {
                let actions: Vec<String> = actions
                    .into_iter()
                    .map(|(action, statuses)| format!("{action} (from {})", statuses.join(", ")))
                    .collect();
                format!("{sender} may take {}", actions.join(", "))
            })
            .collect()
    }
}

/// Why a walk that never took the moves `unwalked` proves nothing.
pub(in crate::composite::joint) fn unwalked_reason(unwalked: &[String]) -> String {
    format!(
        "the walk never takes moves the runtime can make, so it never goes where they lead: {}. Declare an action only a module answers with as internal, or name it in its trigger's on_success; the walk does not send a reaction from a type outside the rule's scope",
        unwalked.join("; ")
    )
}

impl JointModel {
    /// Whether `mv` sends an action outside code is relied on to send.
    pub(in crate::composite::joint) fn relies(&self, mv: &super::Move) -> bool {
        mv.outside_action().is_some_and(|(entity, action)| {
            self.relies_on
                .contains(&(entity.to_string(), action.to_string()))
        })
    }

    /// Walk every reachable joint state breadth first, so each state is
    /// first reached by a shortest path. `visit` sees each state as it is
    /// taken from the queue and returns true to stop the walk. With `edges`,
    /// every move between walked states is kept. The walk takes the memory
    /// it holds from `bound`'s room and stops when the room says no.
    pub(in crate::composite::joint) fn walk_with<'a>(
        &self,
        bound: Bound<'a>,
        edges: bool,
        mut visit: impl FnMut(usize, &JointState) -> bool,
    ) -> Explored<'a> {
        // The room bounds what the walk holds; a model's expansion work has
        // no count or clock of its own here.
        self.expansion.reset(usize::MAX, None);
        let mut explored = Explored {
            nodes: Vec::new(),
            packer: Packer::default(),
            made: Vec::new(),
            edges: Vec::new(),
            stopped: None,
            held_back: false,
            unwalked: BTreeSet::new(),
            held: Held::new(bound),
        };
        if let Some(reason) = &self.senders.problem {
            explored.stopped = Some(reason.clone());
            return explored;
        }
        // Check the sliced models before either caller can expand a move.
        let oversized: Vec<_> = self
            .models
            .iter()
            .filter_map(|(entity, model)| {
                crate::model::semantics::oversized_transition(model)
                    .map(|(action, _)| format!("{entity}.{action} parameter expansion"))
            })
            .collect();
        if !oversized.is_empty() {
            explored.stopped = Some(format!(
                "unsupported reads or expansion: {}",
                oversized.join(", ")
            ));
            return explored;
        }
        let mut seen: HashMap<Packed, u32> = HashMap::new();
        let mut queue = VecDeque::new();
        let mut tables = Tables::default();
        // One start per way the records' ids can be equal: many records start
        // in millions of ways, so the starts take their memory from the room
        // as the walk's states do.
        for init in self.inits() {
            if !self.expansion.take() {
                break;
            }
            let packed = explored.packer.pack(&init);
            if seen.contains_key(&packed) {
                continue;
            }
            if let Err(reason) = tables.make_room(&mut explored, &mut seen, &mut queue, edges) {
                explored.stop_for_room(reason);
                return explored;
            }
            tables.states += block(size_of::<[usize; 2]>() + 4 * packed.len());
            seen.insert(packed.clone(), explored.nodes.len() as u32);
            queue.push_back(explored.nodes.len());
            explored.nodes.push((packed, None));
        }
        while let Some(index) = queue.pop_front() {
            let state = explored.state(index);
            self.note_unwalked(&state, &mut explored.unwalked);
            if visit(index, &state) {
                break;
            }
            explored.held.walked(explored.nodes.len());
            if let Err(reason) = tables.make_room(&mut explored, &mut seen, &mut queue, edges) {
                explored.stop_for_room(reason);
                break;
            }
            let before = [
                explored.nodes.len(),
                explored.made.len(),
                explored.edges.len(),
            ];
            let moves = match self.within(&mut explored.held, &mut tables, || self.moves(&state)) {
                Ok(moves) => moves,
                Err(reason) => {
                    explored.stop_for_room(reason);
                    break;
                }
            };
            for mv in &moves.0 {
                if !self.expansion.take() {
                    break;
                }
                let ((nexts, held), room) =
                    match self.within(&mut explored.held, &mut tables, || self.next(&state, mv)) {
                        Ok(built) => built,
                        Err(reason) => {
                            explored.stop_for_room(reason);
                            break;
                        }
                    };
                explored.held_back |= held;
                tables.decided = self.senders.held();
                // Every outcome needs an edge; state slots are reserved only
                // when an unseen packed state is about to be kept.
                let reserved =
                    tables.reserve(&mut explored, &mut seen, &mut queue, edges, 0, nexts.len());
                if let Err(reason) = reserved {
                    tables.give(&mut explored.held, room);
                    explored.stop_for_room(reason);
                    break;
                }
                if edges {
                    explored.made.push(Made {
                        from: index as u32,
                        by_system: mv.by_system(),
                        relied: self.relies(mv),
                        covered: !held
                            && !nexts.is_empty()
                            && nexts.iter().all(|n| n.certainty != Certainty::Unsure),
                    });
                }
                let made = explored.made.len().saturating_sub(1) as u32;
                for next in nexts {
                    let packed = explored.packer.pack(&next.state);
                    let at = match seen.get(&packed) {
                        Some(&at) => at,
                        None => {
                            if let Err(reason) =
                                tables.reserve(&mut explored, &mut seen, &mut queue, false, 1, 0)
                            {
                                explored.stop_for_room(reason);
                                break;
                            }
                            let at = explored.nodes.len() as u32;
                            let label = explored.packer.id(&next.label);
                            tables.states += block(size_of::<[usize; 2]>() + 4 * packed.len());
                            seen.insert(packed.clone(), at);
                            explored.nodes.push((packed, Some((index as u32, label))));
                            queue.push_back(at as usize);
                            at
                        }
                    };
                    if edges {
                        explored.edges.push(Edge {
                            made,
                            to: at,
                            sure: next.certainty == Certainty::Sure,
                        });
                    }
                }
                tables.give(&mut explored.held, room);
                if let Err(reason) = tables.count(&mut explored, &seen, &queue) {
                    explored.stop_for_room(reason);
                    break;
                }
            }
            tables.give(&mut explored.held, moves.1);
            if explored.stopped.is_some() {
                break;
            }
            let after = [
                explored.nodes.len(),
                explored.made.len(),
                explored.edges.len(),
            ];
            tables.saw([0, 1, 2].map(|i| after[i] - before[i]));
        }
        if self.expansion.exhausted.get() {
            explored.stopped = Some("the joint expansion exhausted its work or time budget".into());
        }
        explored
    }
}

impl JointModel {
    /// Walk every reachable joint state breadth first, checking `invariants`
    /// of `owner` in each settled one (nothing in flight).
    pub(in crate::composite::joint) fn walk(
        &self,
        owner: &str,
        invariants: &[&Expr],
        bound: Bound<'_>,
    ) -> Walk {
        let mut unsupported = BTreeSet::new();
        let modeled = |entity: &str, field: &str| {
            self.models.get(entity).is_some_and(|model| {
                matches!(field, "Id" | "id")
                    || match model.var_kinds.get(field) {
                        Some(temper_spec::predicate::VarKind::Str) => {
                            model.tracked_strings.contains(field)
                        }
                        Some(
                            temper_spec::predicate::VarKind::Counter
                            | temper_spec::predicate::VarKind::Bool
                            | temper_spec::predicate::VarKind::List,
                        ) => true,
                        _ => false,
                    }
            })
        };
        for inv in invariants {
            inv.for_each_name(&mut |name| {
                let mut check = |entity: &str, field: &str| {
                    if !modeled(entity, field) {
                        unsupported.insert(format!("{entity}.{field}"));
                    }
                };
                match name {
                    Name::Var(field) => check(owner, field),
                    Name::CrossStatus { id_field, .. } => check(owner, id_field),
                    Name::CrossField {
                        entity_type,
                        id_field,
                        field,
                    } => {
                        check(owner, id_field);
                        check(entity_type, field);
                    }
                    Name::Status | Name::Param(_) => {}
                }
            });
        }
        if !unsupported.is_empty() {
            let reason = format!(
                "unsupported reads or expansion: {}",
                unsupported.into_iter().collect::<Vec<_>>().join(", ")
            );
            return Walk {
                states: 0,
                held_back: false,
                outcomes: invariants
                    .iter()
                    .map(|_| JointOutcome::Incomplete {
                        reason: reason.clone(),
                    })
                    .collect(),
            };
        }
        let mut unknown = vec![false; invariants.len()];
        let mut broken: Vec<Option<usize>> = vec![None; invariants.len()];
        let premises: Vec<Option<&Expr>> = invariants.iter().map(|inv| premise(inv)).collect();
        let mut met: Vec<bool> = premises.iter().map(Option::is_none).collect();
        let explored = self.walk_with(bound, false, |index, state| {
            if state.settled() {
                let env = self.env(owner, state);
                for (i, inv) in invariants.iter().enumerate() {
                    match eval(inv, &env) {
                        Truth::False if broken[i].is_none() => broken[i] = Some(index),
                        Truth::Unknown => unknown[i] = true,
                        _ => {}
                    }
                    if let (false, Some(premise)) = (met[i], premises[i]) {
                        met[i] = eval(premise, &env) != Truth::False;
                    }
                }
            }
            broken.iter().all(Option::is_some)
        });
        let unwalked = explored.unwalked();
        let outcomes = broken
            .iter()
            .enumerate()
            .map(|(i, found)| match found {
                Some(index) => JointOutcome::Broken {
                    path: explored.path(*index),
                },
                None if unknown[i] => JointOutcome::Incomplete {
                    reason: "an invariant read is unknown".into(),
                },
                None => match &explored.stopped {
                    Some(reason) => JointOutcome::Incomplete {
                        reason: reason.clone(),
                    },
                    None if !unwalked.is_empty() => JointOutcome::Incomplete {
                        reason: unwalked_reason(&unwalked),
                    },
                    None => match premises[i] {
                        Some(premise) if !met[i] => JointOutcome::Vacuous {
                            premise: premise.to_string(),
                        },
                        _ => JointOutcome::Holds,
                    },
                },
            })
            .collect();
        Walk {
            states: explored.len(),
            outcomes,
            held_back: explored.held_back,
        }
    }
}
