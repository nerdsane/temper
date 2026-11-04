//! Initial ID equality classes, including concrete strings and blank symbols.

use super::*;

impl JointModel {
    /// The starting states: one per way the records' ids can be equal or
    /// differ from one another and from the literals each can meet
    /// ([`super::classes`]).
    pub(super) fn inits(&self) -> Box<dyn Iterator<Item = JointState> + '_> {
        let blank = self.blank();
        if self.records == Records::One {
            return Box::new(std::iter::once(JointState {
                records: blank,
                inflight: Vec::new(),
                calls: Vec::new(),
                armed: BTreeMap::new(),
            }));
        }
        // Every type's id literals, in one order; each record may take only
        // its own type's.
        let literals: BTreeSet<&String> = self.id_literals.values().flatten().collect();
        let allowed: Vec<Vec<usize>> = blank
            .keys()
            .map(|entity| {
                literals
                    .iter()
                    .enumerate()
                    .filter(|(_, literal)| self.id_literals[&**entity].contains(**literal))
                    .map(|(at, _)| at)
                    .collect()
            })
            .collect();
        let literals: Vec<_> = literals
            .into_iter()
            .map(|l| ModelString::from(l.as_str()))
            .collect();
        Box::new(
            partitions(allowed, literals.len())
                .flat_map(move |blocks| id_values(blocks, literals.clone()))
                .map(move |ids| {
                    let mut records = blank.clone();
                    for (record, id) in records.values_mut().zip(ids) {
                        std::rc::Rc::make_mut(record)
                            .strings
                            .insert(ID.to_string(), id);
                    }
                    let mut state = JointState {
                        records,
                        inflight: Vec::new(),
                        calls: Vec::new(),
                        armed: BTreeMap::new(),
                    };
                    self.arm_at_start(&mut state);
                    self.order.normalize(&mut state);
                    canonical(&mut state);
                    state
                }),
        )
    }
}

/// Each fresh ID equality class may be nonblank or whitespace-only. Equal
/// IDs share that choice; literal classes retain their concrete values.
fn id_values(
    blocks: Vec<usize>,
    literals: Vec<ModelString>,
) -> impl Iterator<Item = Vec<ModelString>> {
    let symbols = blocks
        .iter()
        .max()
        .map_or(0, |last| (last + 1).saturating_sub(literals.len()));
    let mut blank = vec![false; symbols];
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let ids = blocks
            .iter()
            .map(|&block| {
                literals.get(block).cloned().unwrap_or_else(|| {
                    let symbol = block - literals.len();
                    if blank[symbol] {
                        ModelString::BlankSymbol(symbol + 1)
                    } else {
                        ModelString::Symbol(symbol + 1)
                    }
                })
            })
            .collect();
        if let Some(index) = blank.iter().rposition(|is_blank| !is_blank) {
            blank[index] = true;
            blank[index + 1..].fill(false);
        } else {
            done = true;
        }
        Some(ids)
    })
}

/// Every way to split items into blocks, as each item's block number, with
/// `fixed` distinguished literal blocks followed by fresh blocks numbered in
/// order of first use. Item `i` may take the literal blocks `allowed[i]`
/// (ascending, each below `fixed`) or any fresh block.
fn partitions(allowed: Vec<Vec<usize>>, fixed: usize) -> impl Iterator<Item = Vec<usize>> {
    // The smallest block item `i` may take: its first literal, or else the
    // first fresh block (new or already used).
    let least =
        move |i: usize, allowed: &[Vec<usize>]| allowed[i].first().copied().unwrap_or(fixed);
    // Restricted-growth strings, in lexicographic order, with O(n) storage.
    let mut blocks = Some(
        (0..allowed.len())
            .map(|i| least(i, &allowed))
            .collect::<Vec<_>>(),
    );
    std::iter::from_fn(move || {
        let current = blocks.take()?;
        let mut next = current.clone();
        for i in (0..allowed.len()).rev() {
            let newest = next[..i]
                .iter()
                .copied()
                .filter(|&b| b >= fixed)
                .max()
                .map_or(fixed, |b| b + 1);
            let bigger = allowed[i]
                .iter()
                .copied()
                .find(|&b| b > next[i])
                .or_else(|| (next[i] < newest).then(|| (next[i] + 1).max(fixed)));
            if let Some(b) = bigger {
                next[i] = b;
                for (j, block) in next.iter_mut().enumerate().skip(i + 1) {
                    *block = least(j, &allowed);
                }
                blocks = Some(next);
                break;
            }
        }
        Some(current)
    })
}

#[cfg(test)]
mod tests {
    use super::{ModelString, id_values, partitions};
    use std::collections::BTreeSet;

    #[test]
    fn equal_ids_share_blankness_and_distinct_ids_choose_it_independently() {
        use ModelString::{BlankSymbol as B, Symbol as S};
        assert_eq!(
            id_values(vec![0, 0], vec![]).collect::<Vec<_>>(),
            vec![vec![S(1), S(1)], vec![B(1), B(1)]],
        );
        assert_eq!(
            id_values(vec![0, 1], vec![]).collect::<Vec<_>>(),
            vec![
                vec![S(1), S(2)],
                vec![S(1), B(2)],
                vec![B(1), S(2)],
                vec![B(1), B(2)],
            ],
        );
    }

    #[test]
    fn literal_ids_keep_their_values_alongside_both_symbol_kinds() {
        let literals = vec![ModelString::from("known"), ModelString::from(" ")];
        assert_eq!(
            id_values(vec![0, 1, 2], literals.clone()).collect::<Vec<_>>(),
            vec![
                vec![
                    literals[0].clone(),
                    literals[1].clone(),
                    ModelString::Symbol(1)
                ],
                vec![
                    literals[0].clone(),
                    literals[1].clone(),
                    ModelString::BlankSymbol(1)
                ],
            ],
        );
        assert_eq!(
            id_values(vec![0, 1], literals.clone()).collect::<Vec<_>>(),
            vec![literals],
        );
        assert_eq!(id_values(vec![], vec![]).count(), 1);
    }

    #[test]
    fn literal_blocks_and_fresh_equalities_are_all_explored() {
        assert_eq!(
            partitions(vec![vec![0, 1]], 2).collect::<Vec<_>>(),
            vec![vec![0], vec![1], vec![2]]
        );
        assert_eq!(
            partitions(vec![vec![0]; 2], 1).collect::<Vec<_>>(),
            vec![vec![0, 0], vec![0, 1], vec![1, 0], vec![1, 1], vec![1, 2],]
        );
    }

    #[test]
    fn each_item_takes_only_its_own_literals() {
        assert_eq!(
            partitions(vec![vec![0], vec![1]], 2).collect::<Vec<_>>(),
            vec![vec![0, 1], vec![0, 2], vec![2, 1], vec![2, 2], vec![2, 3]]
        );
    }

    #[test]
    fn incremental_partitions_are_exhaustive_and_unique() {
        for (n, expected) in [1, 1, 2, 5, 15, 52].into_iter().enumerate() {
            let all: Vec<_> = partitions(vec![Vec::new(); n], 0).collect();
            assert_eq!(all.len(), expected);
            assert_eq!(all.into_iter().collect::<BTreeSet<_>>().len(), expected);
        }
    }
}
