//! Moves whose reactions nothing can tell apart send one set of reactions.
//!
//! Several moves of a type often send the same things to the same records,
//! and differ only in which move set them off. A dispatch keeps that move in
//! its chain and in the reaction it came through, so two states that will do
//! exactly the same from here on are walked twice, and a type with many such
//! moves walks every mix of them.
//!
//! What can read the move a dispatch came from:
//! - Its target's own reactions and module calls, through the chain: there are
//!   none when the target move is a dead end.
//! - The order of a commit's reactions, in `overtakes`: two moves are alike
//!   only with the same reactions in the same order. Two commits in flight
//!   then read as two of one move, as two commits of one move already do, so a
//!   later reaction of one landing before an earlier one of the other is
//!   unsure: that can only make a verdict less sure, never a HOLDS.
//! - How many a commit waits on, in `fits`: two commits can count as one, so
//!   the walk may keep more in flight, never less.
//! - Cedar, which names the source move and the rule:
//!   `Senders::reads_source_move` says whether a policy on the target move
//!   can read either.
//! - The drop check, which names the source move: it walks its own model,
//!   without this.
//!
//! When none can, each move sends its reactions as the first move like it.

use std::collections::BTreeMap;

use super::super::state::ReactionStep;
use super::JointModel;
use super::read::Reaction;

impl JointModel {
    /// This model, with each move nothing can tell from an earlier one
    /// sending its reactions as that one.
    pub(in crate::composite::joint) fn with_alike_as_one(mut self) -> Self {
        let mut moves: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
        for (index, reaction) in self.reactions.iter().enumerate() {
            moves
                .entry((&reaction.from, &reaction.source_action))
                .or_default()
                .push(index);
        }
        let mut moves: Vec<Vec<usize>> = moves.into_values().collect();
        moves.sort();
        let mut alike: Vec<usize> = (0..self.reactions.len()).collect();
        let mut firsts: Vec<&[usize]> = Vec::new();
        for sent in &moves {
            if !sent.iter().all(|&index| self.source_is_unread(index)) {
                continue;
            }
            let twin = firsts.iter().find(|first| {
                first.len() == sent.len()
                    && first
                        .iter()
                        .zip(sent)
                        .all(|(&a, &b)| same_but_source(&self.reactions[a], &self.reactions[b]))
            });
            match twin {
                Some(first) => {
                    for (&index, &at) in sent.iter().zip(first.iter()) {
                        alike[index] = at;
                    }
                }
                None => firsts.push(sent),
            }
        }
        self.alike = alike;
        self
    }

    /// Whether nothing past the commit can read which move sent reaction
    /// `index`.
    fn source_is_unread(&self, index: usize) -> bool {
        let reaction = &self.reactions[index];
        let goes_on =
            |r: &Reaction| r.from == reaction.to && r.source_action == reaction.target_action;
        let calls = (reaction.to.clone(), reaction.target_action.clone());
        reaction.spawn.is_none()
            && !self.reactions.iter().any(goes_on)
            && !self.kernel.calls_on.contains_key(&calls)
            && !self
                .senders
                .reads_source_move(&reaction.to, &reaction.target_action)
    }

    /// What reaction `index` goes out as, and its chain, which holds `step`
    /// after `chain`. Only a chain's first reaction has nothing before it but
    /// the move that sent it.
    pub(super) fn sent_as(
        &self,
        index: usize,
        chain: &[ReactionStep],
        step: ReactionStep,
    ) -> (usize, Vec<ReactionStep>) {
        let mut next = chain.to_vec();
        let twin = self.alike.get(index).copied().unwrap_or(index);
        let via = match next.as_mut_slice() {
            [root] => {
                root.action = self.reactions[twin].source_action.as_str().into();
                twin
            }
            _ => index,
        };
        next.push(step);
        (via, next)
    }
}

/// Whether two reactions do the same thing, whichever move set them off.
fn same_but_source(a: &Reaction, b: &Reaction) -> bool {
    a.from == b.from
        && a.to == b.to
        && a.target_action == b.target_action
        && a.guard == b.guard
        && a.args == b.args
        && a.resolve == b.resolve
        && a.principal == b.principal
        && a.may_drop == b.may_drop
}
