//! Records that can never reach a place another record in the same place
//! still can.
//!
//! The model check's whole walk keeps its graph. From it, each state gets the
//! places it can still reach. A state loses a place when another state in the
//! same place can reach it and this one never can: a cup sent back to the line
//! that no barista may claim again, or an order that chose delivery and can no
//! longer be picked up. The first is a mistake and the second a choice; the
//! checker can't tell them apart, so each is named for the author to read and
//! nothing fails.
//!
//! A state with a step past a counter's bound somewhere ahead is not named:
//! what lies past the bound was not walked, so what it can reach is not known.
//! A move's loss is named at the first comparable record after it, not again
//! in each place the record passes through. An intermediate place without a
//! comparison peer, or a retry cycle, must not hide that move's loss.

mod origins;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::{TemperModel, TemperModelState};
use crate::reach::{Graph, NO_PLACE, Reach};

/// A record that can never reach places another record in its place can.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LostPlace {
    /// The place both records are in.
    pub place: String,
    /// The places the record can never reach.
    pub lost: Vec<String>,
    /// The record, at the shortest path to where its loss can be compared.
    pub record: TemperModelState,
    /// The moves to it from the start.
    pub path: Vec<String>,
    /// A record in the same place that can still reach some of them.
    pub other: TemperModelState,
    /// The moves to it from the start.
    pub other_path: Vec<String>,
    /// The places in `lost` the other record can reach.
    pub other_reaches: Vec<String>,
}

impl LostPlace {
    /// The finding in plain words: "a record in Waiting with makers = 1 can
    /// never reach BeingMade or Made, while one with makers = 0 can (the
    /// first after ..., the second after ...)".
    pub fn describe(&self) -> String {
        let (mine, theirs) = differences(&self.record, &self.other);
        let reaches = if self.other_reaches == self.lost {
            "can".to_string()
        } else {
            format!("can reach {}", either(&self.other_reaches))
        };
        format!(
            "a record in {} with {} can never reach {}, while one with {} {} (the first {}, the \
             second {})",
            self.place,
            mine,
            either(&self.lost),
            theirs,
            reaches,
            after(&self.path),
            after(&self.other_path),
        )
    }
}

/// Every record the walk in `graph` found that loses places, one per place
/// and set of places lost, shortest paths first. `reach` is what each of its
/// states can reach, and `found` maps each state to its number in `graph`.
pub(crate) fn lost_places(
    model: &TemperModel,
    graph: &Graph,
    reach: &Reach,
    found: &HashMap<TemperModelState, u32>,
) -> Vec<LostPlace> {
    let n = graph.len();
    let bits = reach.bits;
    let reach_of = |at: usize| reach.places(at);

    // What some record in each place can reach.
    let mut anyone = vec![0u64; model.states.len() * bits.words];
    for at in 0..n {
        let place = graph.place[at];
        if place != NO_PLACE {
            let start = place as usize * bits.words;
            for (w, word) in reach_of(at).iter().enumerate() {
                anyone[start + w] |= word;
            }
        }
    }

    // Each state, its whole future walked, that cannot reach what another
    // in its place can.
    let mut lost: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
    for at in 0..n {
        let place = graph.place[at];
        if place == NO_PLACE || reach.past_bound(at) {
            continue;
        }
        let row: Vec<u64> = bits
            .row(&anyone, place as usize)
            .iter()
            .zip(reach_of(at))
            .map(|(all, mine)| all & !mine)
            .collect();
        if row.iter().any(|w| *w != 0) {
            lost.insert(at, row);
        }
    }

    let made_here = origins::first_comparable(graph, reach, &lost);

    // One finding per place and set of places lost, with its shortest
    // record and the other record in its place that reaches the most of them.
    let mut groups: BTreeMap<(u32, Vec<u64>), usize> = BTreeMap::new();
    for &at in &made_here {
        let key = (graph.place[at], lost[&at].clone());
        let best = groups.entry(key).or_insert(at);
        if (graph.depth[at], at) < (graph.depth[*best], *best) {
            *best = at;
        }
    }
    let mut findings: Vec<(usize, usize, Vec<u64>)> = groups
        .into_iter()
        .map(|((place, row), at)| {
            let shared = |other: usize| -> u32 {
                reach_of(other)
                    .iter()
                    .zip(&row)
                    .map(|(r, l)| (r & l).count_ones())
                    .sum()
            };
            let other = (0..n)
                .filter(|&o| graph.place[o] == place && shared(o) > 0)
                .min_by_key(|&o| (std::cmp::Reverse(shared(o)), graph.depth[o], o))
                .expect("some record in the place reaches what this one lost");
            (at, other, row)
        })
        .collect();
    findings.sort_by_key(|(at, _, _)| (graph.depth[*at], *at));

    let wanted: BTreeSet<u32> = findings
        .iter()
        .flat_map(|(at, other, _)| [*at as u32, *other as u32])
        .collect();
    let states: BTreeMap<u32, &TemperModelState> = found
        .iter()
        .filter(|(_, at)| wanted.contains(at))
        .map(|(state, at)| (*at, state))
        .collect();
    findings
        .into_iter()
        .map(|(at, other, row)| {
            let other_reaches: Vec<u64> = reach_of(other)
                .iter()
                .zip(&row)
                .map(|(r, l)| r & l)
                .collect();
            LostPlace {
                place: model.states[graph.place[at] as usize].clone(),
                lost: bits.names(model, &row),
                record: states[&(at as u32)].clone(),
                path: graph.path(model, at),
                other: states[&(other as u32)].clone(),
                other_path: graph.path(model, other),
                other_reaches: bits.names(model, &other_reaches),
            }
        })
        .collect()
}

/// The fields in which `a` and `b` differ, as "makers = 1" for each.
fn differences(a: &TemperModelState, b: &TemperModelState) -> (String, String) {
    let mut mine = Vec::new();
    let mut theirs = Vec::new();
    let mut note = |name: &str, x: String, y: String| {
        if x != y {
            mine.push(format!("{name} = {x}"));
            theirs.push(format!("{name} = {y}"));
        }
    };
    for (name, x) in &a.counters {
        note(name, x.to_string(), field(b.counters.get(name)));
    }
    for (name, x) in &a.booleans {
        note(name, x.to_string(), field(b.booleans.get(name)));
    }
    for (name, x) in &a.lists {
        let y = b.lists.get(name).map(|y| format!("{y:?}"));
        note(name, format!("{x:?}"), y.unwrap_or_default());
    }
    for (name, x) in &a.strings {
        note(name, x.to_string(), field(b.strings.get(name)));
    }
    if mine.is_empty() {
        return ("the same fields".into(), "the same fields".into());
    }
    (mine.join(" and "), theirs.join(" and "))
}

fn field(value: Option<&impl ToString>) -> String {
    value.map(ToString::to_string).unwrap_or_default()
}

/// "A", "A or B", "A, B or C".
pub(crate) fn either(places: &[String]) -> String {
    match places {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// "after Add > Claim", with a run of the same moves written once with its
/// count: "after Start > (Call > Answer) x 100".
fn after(path: &[String]) -> String {
    if path.is_empty() {
        return "at the start".into();
    }
    let mut parts = Vec::new();
    let mut i = 0;
    while i < path.len() {
        // The repeated run that covers the most moves from here.
        let mut best = (1, 1);
        for period in 1..=4.min(path.len() - i) {
            let unit = &path[i..i + period];
            let mut times = 1;
            while path[i + times * period..].starts_with(unit) {
                times += 1;
            }
            if times > 1 && period * times > best.0 * best.1 {
                best = (period, times);
            }
        }
        let (period, times) = best;
        let unit = path[i..i + period].join(" > ");
        parts.push(match (period, times) {
            (_, 1) => unit,
            (1, _) => format!("{unit} x {times}"),
            _ => format!("({unit}) x {times}"),
        });
        i += period * times;
    }
    format!("after {}", parts.join(" > "))
}
