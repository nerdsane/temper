//! What Cedar lets each sender send, read from the policies once.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::{Asker, May, SendAnalysis};
use temper_spec::automaton::Automaton;

use super::super::room::{Heap, block};
use super::{Senders, asker_name};
use crate::model::heap::tree_bytes;

type Answers = BTreeMap<String, BTreeMap<String, Vec<May>>>;

impl Senders {
    /// Who sender `by` is, in words, on a request it makes of `action` on an
    /// `entity` record: one of its senders that may, for it may stand for
    /// others that may not.
    pub(in crate::composite::joint) fn name_asking(
        &self,
        by: u32,
        entity: &str,
        action: &str,
    ) -> String {
        let key = (by, entity.to_string(), action.to_string());
        if let Some(name) = self
            .asking
            .read()
            .expect("a walk thread panicked")
            .get(&key)
        {
            return name.clone();
        }
        let allowed = self
            .policies
            .as_ref()
            .zip(self.askers.get(by as usize / 2))
            .and_then(|(policies, asker)| policies.allowed(asker, action, entity));
        let name =
            allowed.map_or_else(|| self.name(by), |kind| asker_name(&Asker::OutsideAs(kind)));
        self.asking
            .write()
            .expect("a walk thread panicked")
            .insert(key, name.clone());
        name
    }
}

/// Room for both tables, including every key, tree node and answer vector.
/// Each row is built in place, so no second table-sized buffer coexists.
pub(super) fn room<'a>(
    by_name: &BTreeMap<&str, &Automaton>,
    entities: impl IntoIterator<Item = &'a String>,
    requests: usize,
    askers: usize,
) -> Result<usize, String> {
    let columns = block(requests * size_of::<May>()) + block(2 * askers * size_of::<May>());
    let mut bytes = 0usize;
    let mut count = 0;
    for entity in entities {
        count += 1;
        let actions = &by_name[entity.as_str()].actions;
        let nodes = 2 * tree_bytes::<String, Vec<May>>(actions.len());
        bytes = bytes
            .checked_add(nodes + 2 * entity.heap())
            .ok_or_else(overflow)?;
        for action in actions {
            bytes = bytes
                .checked_add(2 * action.name.heap() + columns)
                .ok_or_else(overflow)?;
        }
    }
    bytes
        .checked_add(2 * tree_bytes::<String, BTreeMap<String, Vec<May>>>(count))
        .ok_or_else(overflow)
}

/// Room for the `read` row each of the `standing` types adds to the
/// reactions' table: a key, a tree node and an answer for each sender in
/// either chain.
pub(super) fn read_room(standing: &BTreeSet<String>, askers: usize) -> usize {
    let row = tree_bytes::<String, Vec<May>>(1)
        + 2 * super::standing::READ.to_string().heap()
        + block(2 * askers * size_of::<May>());
    standing.len() * row
}

fn overflow() -> String {
    "sender table size overflows addressable memory".to_string()
}

/// The room the finished tables retain after unused actions are forgotten.
pub(super) fn bytes(tables: Option<&Answers>) -> usize {
    let Some(tables) = tables else {
        return 0;
    };
    tree_bytes::<String, BTreeMap<String, Vec<May>>>(tables.len())
        + tables
            .iter()
            .map(|(entity, actions)| {
                entity.heap()
                    + tree_bytes::<String, Vec<May>>(actions.len())
                    + actions
                        .iter()
                        .map(|(action, answers)| {
                            action.heap() + block(answers.capacity() * size_of::<May>())
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
}

/// `answer` for each action of each of `entities`, by entity, then action.
pub(super) fn each<'a, T>(
    by_name: &BTreeMap<&str, &Automaton>,
    entities: impl IntoIterator<Item = &'a String>,
    answer: &dyn Fn(&str, &str) -> T,
) -> BTreeMap<String, BTreeMap<String, T>> {
    let mut entities_table = BTreeMap::new();
    for entity in entities {
        let mut actions = BTreeMap::new();
        for action in &by_name[entity.as_str()].actions {
            actions.insert(action.name.clone(), answer(entity, &action.name));
        }
        entities_table.insert(entity.clone(), actions);
    }
    entities_table
}

/// The policies that may apply to a move whose answer `may` leaves to the
/// request: all a walk reads of `policies` when it decides one again.
pub(super) fn reread(
    policies: Option<&SendAnalysis>,
    askers: &[Asker],
    may: Option<&Answers>,
) -> BTreeSet<String> {
    let mut reread = BTreeSet::new();
    if let (Some(policies), Some(may)) = (policies, may) {
        for (entity, actions) in may {
            for (action, answers) in actions {
                // Every outside kind finds the same policies: they read no
                // profile, only who is outside.
                let mut outside_read = false;
                for (by, answer) in answers.iter().enumerate() {
                    let asker = &askers[by / 2];
                    let outside = !matches!(asker, Asker::Service { .. });
                    if *answer != May::Maybe || (outside && outside_read) {
                        continue;
                    }
                    outside_read |= outside;
                    reread.extend(policies.applicable(asker, action, entity));
                }
            }
        }
    }
    reread
}
