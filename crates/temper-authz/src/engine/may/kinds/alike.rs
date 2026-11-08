//! The senders a kind stands for besides its example: those the partition
//! found answering as some node did and so did not split again.

use super::memory::fact_heap;
use super::split::facts_mut;
use super::*;

/// Facts by name: places in [`Alike::names`] and [`Alike::facts`], in the
/// order of the names.
type Facts = Vec<(u32, u32)>;

/// The place in [`Alike::facts`] of an unknown fact.
const UNKNOWN: u32 = 0;

/// The partition's nodes, and the children it dropped for answering as a
/// node it kept. Two that answer alike agree on every value a later split
/// reads (their forms show those values), so a dropped child's senders are
/// split as the node it answers as is. A child's example is its parent's
/// with one fact set, so a node keeps only that fact, and a dropped child
/// only its facts where its example and the node's differ.
pub(super) struct Alike {
    /// Each name a split tested: the type, or an attribute or context key.
    names: Vec<Test>,
    /// Each fact held here, once: an unknown one first.
    facts: Vec<Option<Fact>>,
    /// Each node kept: the node it was split from, and the name tested
    /// there and the fact it took (none for the root).
    nodes: Vec<(usize, Option<(u32, u32)>)>,
    /// Each child dropped: the node it was split from, the name tested, its
    /// facts where it and the node it answers as differ, and that node.
    dropped: Vec<(usize, u32, Facts, usize)>,
    /// What the names, facts and lists above hold on the heap.
    heap: usize,
    /// The most a fact here holds on the heap: a join holds no more.
    widest: usize,
}

impl Alike {
    /// The node every sender starts from, whose example knows nothing.
    pub(super) const ROOT: usize = 0;

    pub(super) fn new() -> Self {
        Self {
            names: Vec::new(),
            facts: vec![None],
            nodes: vec![(Self::ROOT, None)],
            dropped: Vec::new(),
            heap: 0,
            widest: 0,
        }
    }

    /// A node kept, split from `parent` on `test`.
    pub(super) fn node(&mut self, parent: usize, test: &Test, profile: &Profile) -> usize {
        let name = self.name(test);
        let fact = self.fact(fact_at(profile, &self.names[name as usize]));
        self.nodes.push((parent, Some((name, fact))));
        self.nodes.len() - 1
    }

    /// A child of `parent` on `test` dropped for answering as the node `like`.
    pub(super) fn drop(&mut self, parent: usize, test: &Test, profile: &Profile, like: usize) {
        let name = self.name(test);
        let differ: Vec<(u32, Option<Fact>)> = (0..self.names.len())
            .filter_map(|at| {
                let own = fact_at(profile, &self.names[at]);
                let at = place(at);
                let example = self.facts[self.example(like, at) as usize].as_ref();
                (own.as_ref() != example).then_some((at, own))
            })
            .collect();
        let differ: Facts = differ
            .into_iter()
            .map(|(at, own)| (at, self.fact(own)))
            .collect();
        self.heap += differ.capacity() * size_of::<(u32, u32)>();
        self.dropped.push((parent, name, differ, like));
    }

    /// The bytes this holds.
    pub(super) fn bytes(&self) -> usize {
        self.names.capacity() * size_of::<Test>()
            + self.facts.capacity() * size_of::<Option<Fact>>()
            + self.nodes.capacity() * size_of::<(usize, Option<(u32, u32)>)>()
            + self.dropped.capacity() * size_of::<(usize, u32, Facts, usize)>()
            + self.heap
    }

    /// What each node's other senders hold where they differ from its
    /// example. They come from the node it was split from, holding its
    /// value on the test, and from each child dropped for answering as it,
    /// as that child's example or as the senders of the node that child was
    /// split from. They agree with the node on every value its later splits
    /// read, so a split tells them apart as it does the example. A name some
    /// of them hold as the example does takes the example's fact too. A
    /// dropped child can answer as a node found before or after its parent,
    /// so this runs until no fact widens. Each pass joins what it finds with
    /// what the last found, so facts only widen, and it ends.
    /// `reserve` is asked for the most this and the lists can hold before
    /// each pass.
    pub(super) fn others(
        &mut self,
        reserve: &dyn Fn(usize) -> Result<(), String>,
    ) -> Result<Vec<Facts>, String> {
        let entry = size_of::<(u32, u32)>();
        // Each node's list of children dropped into it and its own list:
        // every list at its longest.
        let lists = self.nodes.len() * (size_of::<Vec<usize>>() + size_of::<Facts>())
            + (4 * self.nodes.len().min(self.dropped.len()) + 2 * self.dropped.len())
                * size_of::<usize>();
        let full = self.nodes.len() * self.names.len() * entry;
        reserve(self.bytes() + lists + full)?;
        let mut into: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (at, dropped) in self.dropped.iter().enumerate() {
            into[dropped.3].push(at);
        }
        // A node's sources while they are joined, with their names: its
        // parent's, its last, and two for each child dropped into it.
        let joining = (2 + 2 * into.iter().map(Vec::len).max().unwrap_or(0))
            * (size_of::<Facts>() + self.names.len() * (entry + size_of::<u32>()));
        let mut others: Vec<Facts> = vec![Vec::new(); self.nodes.len()];
        // The facts a join makes are reserved as the table fills, twice as
        // many each time.
        let fact = size_of::<Option<Fact>>() + self.widest;
        let mut room = 0;
        let mut changed = true;
        while changed {
            changed = false;
            for at in 0..self.nodes.len() {
                if self.facts.len() + self.names.len() > room {
                    room = 2 * (self.facts.len() + self.names.len());
                    let more = (room - self.facts.len()) * fact;
                    reserve(self.bytes() + lists + joining + full + more)?;
                }
                let mut sources: Vec<Facts> = Vec::new();
                if let (parent, Some((name, _))) = self.nodes[at] {
                    sources.push(without(&others[parent], name));
                }
                for &d in &into[at] {
                    let (from, name, differ, _) = &self.dropped[d];
                    sources.push(differ.clone());
                    sources.push(over(differ, &without(&others[*from], *name)));
                }
                sources.retain(|source| !source.is_empty());
                let mut found = self.join(at, &sources);
                drop(sources);
                if !others[at].is_empty() {
                    found = self.join(at, &[others[at].clone(), found]);
                }
                if found != others[at] {
                    others[at] = found;
                    changed = true;
                }
            }
        }
        Ok(others)
    }

    /// A profile of every sender `example` stands for but holding `others`
    /// where they differ from it, or none when it stands for no others.
    pub(super) fn profile(&self, example: &Profile, others: &Facts) -> Option<Profile> {
        if others.is_empty() {
            return None;
        }
        let mut profile = example.clone();
        for &(name, fact) in others {
            set_at(
                &mut profile,
                &self.names[name as usize],
                self.facts[fact as usize].clone(),
            );
        }
        Some(profile)
    }

    /// The facts that hold every sender of `sources`, each listing where it
    /// differs from the example of `at`.
    fn join(&mut self, at: usize, sources: &[Facts]) -> Facts {
        let mut names: Vec<u32> = sources.iter().flatten().map(|&(name, _)| name).collect();
        names.sort_unstable();
        names.dedup();
        let mut joined = Vec::with_capacity(names.len());
        for name in names {
            let mut fact = None;
            for source in sources {
                let own = match source.binary_search_by_key(&name, |&(name, _)| name) {
                    Ok(found) => source[found].1,
                    Err(_) => self.example(at, name),
                };
                fact = Some(match fact {
                    Some(fact) => self.either(fact, own),
                    None => own,
                });
            }
            joined.push((name, fact.unwrap_or(UNKNOWN)));
        }
        joined
    }

    /// The place of a fact holding every value the facts at `one` and
    /// `other` hold.
    fn either(&mut self, one: u32, other: u32) -> u32 {
        if one == other {
            return one;
        }
        let wide = match (&self.facts[one as usize], &self.facts[other as usize]) {
            (Some(one), Some(other)) => either(one, other),
            _ => None,
        };
        self.fact(wide)
    }

    /// The place of `fact` in [`Self::facts`], held once.
    fn fact(&mut self, fact: Option<Fact>) -> u32 {
        if let Some(at) = self.facts.iter().position(|known| *known == fact) {
            return place(at);
        }
        let heap = fact.as_ref().map_or(0, fact_heap);
        self.heap += heap;
        self.widest = self.widest.max(heap);
        self.facts.push(fact);
        place(self.facts.len() - 1)
    }

    /// The name `test` reads, as a place in [`Self::names`].
    fn name(&mut self, test: &Test) -> u32 {
        let test = match test {
            Test::Like(place, name, _) => Test::Named(*place, name.clone()),
            test => test.clone(),
        };
        match self.names.iter().position(|known| *known == test) {
            Some(at) => place(at),
            None => {
                if let Test::Named(_, name) = &test {
                    self.heap += name.capacity();
                }
                self.names.push(test);
                place(self.names.len() - 1)
            }
        }
    }

    /// The fact the example of node `at` holds for `name`: that its nearest
    /// split on the name set, or unknown.
    fn example(&self, mut at: usize, name: u32) -> u32 {
        while let (parent, Some((tested, fact))) = self.nodes[at] {
            if tested == name {
                return fact;
            }
            at = parent;
        }
        UNKNOWN
    }
}

/// A place in a list held here. The partition reserves room for what it
/// holds, so no list here comes near 2^32 entries.
fn place(at: usize) -> u32 {
    u32::try_from(at).expect("a list here holds fewer than 2^32 entries")
}

/// `facts` but for `name`.
fn without(facts: &Facts, name: u32) -> Facts {
    facts
        .iter()
        .filter(|&&(at, _)| at != name)
        .copied()
        .collect()
}

/// `under` with `over`'s facts where `over` names them.
fn over(under: &Facts, over: &Facts) -> Facts {
    let mut facts: Facts = under
        .iter()
        .filter(|(name, _)| over.binary_search_by_key(name, |&(name, _)| name).is_err())
        .chain(over)
        .copied()
        .collect();
    facts.sort_unstable();
    facts
}

/// A fact holding every value either holds, or None (unknown) when only
/// that does: a fact cannot say "absent, or some value".
fn either(one: &Fact, other: &Fact) -> Option<Fact> {
    let some = |unlike: Vec<Json>, like: BTreeMap<String, bool>, string| Fact::Other {
        unlike,
        like,
        string,
    };
    match (one, other) {
        _ if one == other => Some(one.clone()),
        (Fact::Absent, _) | (_, Fact::Absent) => None,
        (Fact::Is(value), Fact::Other { unlike, .. })
        | (Fact::Other { unlike, .. }, Fact::Is(value)) => Some(some(
            kept(unlike, |other| other != value),
            BTreeMap::new(),
            None,
        )),
        (Fact::Is(_), Fact::Is(_)) => Some(some(Vec::new(), BTreeMap::new(), None)),
        (
            Fact::Other {
                unlike,
                like,
                string,
            },
            Fact::Other {
                unlike: others,
                like: other_like,
                string: other_string,
            },
        ) => Some(some(
            kept(unlike, |value| others.contains(value)),
            like.iter()
                .filter(|(pattern, matches)| other_like.get(*pattern) == Some(matches))
                .map(|(pattern, matches)| (pattern.clone(), *matches))
                .collect(),
            if string == other_string {
                *string
            } else {
                None
            },
        )),
    }
}

/// The values of `unlike` that `keep` keeps, holding no more than it does.
fn kept(unlike: &[Json], keep: impl Fn(&Json) -> bool) -> Vec<Json> {
    let mut kept = Vec::with_capacity(unlike.len());
    kept.extend(unlike.iter().filter(|value| keep(value)).cloned());
    kept
}

/// What `profile` holds where `name` reads.
fn fact_at(profile: &Profile, name: &Test) -> Option<Fact> {
    match name {
        Test::Kind => profile.kind.clone(),
        Test::Named(place, name) | Test::Like(place, name, _) => {
            facts(profile, *place).get(name).cloned()
        }
    }
}

fn set_at(profile: &mut Profile, name: &Test, fact: Option<Fact>) {
    match (name, fact) {
        (Test::Kind, fact) => profile.kind = fact,
        (Test::Named(place, name) | Test::Like(place, name, _), Some(fact)) => {
            facts_mut(profile, *place).insert(name.clone(), fact);
        }
        (Test::Named(place, name) | Test::Like(place, name, _), None) => {
            facts_mut(profile, *place).remove(name);
        }
    }
}
