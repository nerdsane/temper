//! Byte reservations for the sender partition, not a work cap.

use super::*;

// The policy-read sets, scopes, type/literal catalogues, and their growth
// buffers each fit within the source JSON's full representation. Candidate
// indexes can hold every policy for every question.
pub(super) fn tests_room(policies: &[(Effect, Json)], questions: usize) -> usize {
    4 * policies
        .iter()
        .map(|(_, p)| size_of::<Json>() + json_heap(p))
        .sum::<usize>()
        + 2 * questions * (size_of::<Vec<usize>>() + policies.len() * size_of::<usize>())
}

pub(super) struct Memory<'a> {
    base: usize,
    node: usize,
    /// What a kind's form holds, while the partition remembers it.
    pub(super) form: usize,
    pub(super) branches: usize,
    reserve: &'a dyn Fn(usize) -> Result<(), String>,
}

impl<'a> Memory<'a> {
    pub(super) fn new(
        tests: &Tests,
        policies: &[(Effect, Json)],
        candidates: &[Vec<usize>],
        reserve: &'a dyn Fn(usize) -> Result<(), String>,
    ) -> Self {
        let questions = candidates.len();
        let metadata = tests_room(policies, questions);
        let reads: BTreeSet<&Test> = candidates
            .iter()
            .flatten()
            .flat_map(|&at| &tests.reads[at])
            .collect();
        let names = reads
            .iter()
            .filter(|t| matches!(t, Test::Named(..)))
            .count();
        let strings: usize = reads
            .iter()
            .map(|test| match test {
                Test::Kind => 0,
                Test::Named(_, name) => name.capacity(),
                Test::Like(_, name, pattern) => name.capacity() + pattern.capacity(),
            })
            .sum();
        let literals: usize = tests
            .literals
            .values()
            .map(|values| {
                values.capacity() * size_of::<Json>() + values.iter().map(json_heap).sum::<usize>()
            })
            .sum();
        let types = tests.kinds.iter().map(String::capacity).sum::<usize>()
            + tests.kinds.capacity() * size_of::<Json>();
        // A profile can hold every tested name and pattern, all excluded
        // literals, and stand-in names. Count both attribute/context trees
        // at their maximum and the undecided-test set at its maximum too.
        let profile = 2 * tree::<String, Fact>(names)
            + reads.iter().filter(|t| matches!(t, Test::Like(..))).count()
                * tree::<String, bool>(1)
            + tree::<String, ()>(reads.len())
            + 3 * strings
            + literals
            + types;
        let policy_bytes =
            |at: usize| size_of::<Json>() + json_heap(&policies[at].1) + json_text(&policies[at].1);
        // One policy's residual JSON and its text at once, for the one being
        // read: the forms keep only each distinct text ([`Forms`]).
        let residual = candidates
            .iter()
            .flatten()
            .map(|&at| policy_bytes(at))
            .max()
            .unwrap_or(0)
            * 2;
        let form = size_of::<Vec<u32>>() + (questions + 1) * size_of::<u32>();
        let node = size_of::<Node>()
            + profile
            + tree::<Test, ()>(reads.len())
            + strings
            + questions * size_of::<May>()
            + form;
        let branches = tests
            .literals
            .values()
            .map(|values| values.len() + 2)
            .chain([tests.kinds.len() + 1, 3])
            .max()
            .unwrap_or(3);
        Self {
            base: metadata + residual,
            node,
            form,
            branches,
            reserve,
        }
    }

    pub(super) fn reserve(&self, nodes: usize, held: usize) -> Result<(), String> {
        let bytes = self
            .node
            .checked_mul(nodes)
            .and_then(|n| n.checked_add(self.base))
            .and_then(|n| n.checked_add(held))
            .ok_or_else(|| {
                "sender-kind construction size overflows addressable memory".to_string()
            })?;
        (self.reserve)(bytes)
    }
}

/// The bytes the questions a split asked hold: each name, and each cause
/// with its keys at a map entry's worth apiece.
pub(super) fn asked_heap(asked: &super::Asked) -> usize {
    let entry = 2 * size_of::<(String, usize)>() + 4 * size_of::<usize>();
    asked
        .iter()
        .map(|(action, types)| {
            entry
                + action.capacity()
                + types
                    .iter()
                    .map(|(name, causes)| {
                        entry
                            + name.capacity()
                            + causes.capacity() * size_of::<Cause>()
                            + causes
                                .iter()
                                .map(|cause| cause.keys() * entry)
                                .sum::<usize>()
                    })
                    .sum::<usize>()
        })
        .sum()
}

impl Kind {
    /// The bytes this kind retains in its profiles, including their containers.
    pub fn bytes(&self) -> usize {
        // The questions asked are shared by the kinds of one split, which
        // reserves them once.
        (self.examples.capacity() + self.others.capacity()) * size_of::<Profile>()
            + self.every().map(profile_heap).sum::<usize>()
    }
}

fn profile_heap(profile: &Profile) -> usize {
    let facts = |facts: &BTreeMap<String, Fact>| {
        tree::<String, Fact>(facts.len())
            + facts
                .iter()
                .map(|(name, fact)| name.capacity() + fact_heap(fact))
                .sum::<usize>()
    };
    profile.kind.as_ref().map_or(0, fact_heap)
        + facts(&profile.attrs)
        + facts(&profile.context)
        + tree::<String, ()>(profile.stand_ins.len())
        + profile
            .stand_ins
            .iter()
            .map(String::capacity)
            .sum::<usize>()
}

pub(super) fn fact_heap(fact: &Fact) -> usize {
    match fact {
        Fact::Absent => 0,
        Fact::Is(value) => json_heap(value),
        Fact::Other { unlike, like, .. } => {
            unlike.capacity() * size_of::<Json>()
                + unlike.iter().map(json_heap).sum::<usize>()
                + tree::<String, bool>(like.len())
                + like.keys().map(String::capacity).sum::<usize>()
        }
    }
}

fn json_heap(value: &Json) -> usize {
    match value {
        Json::String(s) => s.capacity(),
        Json::Array(values) => {
            values.capacity() * size_of::<Json>() + values.iter().map(json_heap).sum::<usize>()
        }
        Json::Object(values) => {
            tree::<String, Json>(values.len())
                + values
                    .iter()
                    .map(|(k, v)| k.capacity() + json_heap(v))
                    .sum::<usize>()
        }
        _ => 0,
    }
}

// A residual's serialized strings may escape each input byte as six bytes.
fn json_text(value: &Json) -> usize {
    match value {
        Json::String(text) => 2 + 6 * text.len(),
        Json::Array(values) => 2 + values.iter().map(|v| 1 + json_text(v)).sum::<usize>(),
        Json::Object(values) => {
            2 + values
                .iter()
                .map(|(k, v)| 4 + 6 * k.len() + json_text(v))
                .sum::<usize>()
        }
        _ => 32,
    }
}

/// BTree nodes hold eleven entries and twelve child pointers; non-root
/// nodes hold at least five entries. Include allocation size-class slack.
fn tree<K, V>(len: usize) -> usize {
    let nodes = if len == 0 {
        0
    } else if len <= 11 {
        1
    } else {
        len.div_ceil(5)
    };
    nodes * (16 + 11 * size_of::<(K, V)>() + 12 * size_of::<usize>()).next_power_of_two()
}
