//! Carry a move's loss to its first comparable record, through places with
//! no comparison peer and through cycles. Suppress only later copies.

use std::collections::{BTreeMap, BTreeSet};

use crate::reach::{Graph, Reach};

/// Choose the first comparable records that can name each move's loss.
pub(super) fn first_comparable(
    graph: &Graph,
    reach: &Reach,
    lost: &BTreeMap<usize, Vec<u64>>,
) -> BTreeSet<usize> {
    let component = &reach.component;
    let first = &reach.first;
    let members = &reach.members;
    let bits = reach.bits;
    let reach = &reach.places;
    let mut pending = vec![0u64; reach.len()];
    for (&at, row) in lost {
        if graph.parent[at].is_none() {
            let start = component[at] as usize * bits.words;
            for (w, word) in row.iter().enumerate() {
                pending[start + w] |= word;
            }
        }
    }

    let mut made_here = BTreeSet::new();
    // Components are numbered after their successors. Walking them in
    // reverse carries unreported losses forward without circling forever.
    for c in (0..first.len() - 1).rev() {
        let mut row = bits.row(&pending, c).to_vec();
        // Members are in discovery order, so the shortest comparable
        // record in a cycle gets to name a loss before the others.
        for &at in &members[first[c]..first[c + 1]] {
            if let Some(loss) = lost.get(&at)
                && row.iter().zip(loss).any(|(r, l)| r & l != 0)
            {
                made_here.insert(at);
                for (word, named) in row.iter_mut().zip(loss) {
                    *word &= !named;
                }
            }
        }
        for &at in &members[first[c]..first[c + 1]] {
            for &to in graph.steps(at) {
                let d = component[to as usize] as usize;
                if d == c {
                    continue;
                }
                for (w, word) in row.iter().enumerate() {
                    // A move makes a loss only when its source can still
                    // reach the place and its destination never can.
                    let removed = bits.row(reach, c)[w] & !bits.row(reach, d)[w];
                    pending[d * bits.words + w] |= word | removed;
                }
            }
        }
    }
    made_here
}
