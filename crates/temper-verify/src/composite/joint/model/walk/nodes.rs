//! A walk's states by number, added by its threads with no lock over them.
//!
//! A thread takes the next number and fills that number's place, in pages that
//! never move. A state is looked up by number only once it is in `seen`, and
//! it goes in there once its place is filled, so no thread waits for another
//! and none finds a place empty.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use super::super::super::room::block;
use super::{Node, Via};

/// A page holds 2^16 states, so a walk's 2^32 numbers take 2^16 pages.
const PAGE_BITS: usize = 16;
const PAGE: usize = 1 << PAGE_BITS;

type Page = Box<[OnceLock<Node>]>;

/// A state's first sure way to it, found after the state.
type SureWay = (u32, (u32, Via));

pub(super) struct Nodes {
    pages: Box<[OnceLock<Page>]>,
    count: AtomicUsize,
    /// Each state whose way to it changed: sure moves alone came to reach it.
    /// One state's changes once, and nothing reads it until the walk ends.
    sure_ways: Mutex<Vec<SureWay>>,
}

impl Nodes {
    /// The walk's states so far, `starts`, at the numbers they have.
    pub(super) fn new(starts: Vec<Node>) -> Self {
        let nodes = Self {
            pages: (0..PAGE).map(|_| OnceLock::new()).collect(),
            count: AtomicUsize::new(0),
            sure_ways: Mutex::new(Vec::new()),
        };
        for node in starts {
            nodes.add(node);
        }
        nodes
    }

    /// Give `node` the next number.
    pub(super) fn add(&self, node: Node) -> u32 {
        let at = self.count.fetch_add(1, Ordering::Relaxed);
        let number = u32::try_from(at).expect("fewer than 2^32 states");
        let page = self.pages[at >> PAGE_BITS]
            .get_or_init(|| (0..PAGE).map(|_| OnceLock::new()).collect());
        if page[at & (PAGE - 1)].set(node).is_err() {
            unreachable!("a number is taken once");
        }
        number
    }

    /// The state numbered `at`, which `seen` or a job named.
    pub(super) fn get(&self, at: u32) -> &Node {
        let at = at as usize;
        self.pages[at >> PAGE_BITS]
            .get()
            .and_then(|page| page[at & (PAGE - 1)].get())
            .expect("a number is named once its place is filled")
    }

    /// Note that `parent` is now the way to the state numbered `at`.
    pub(super) fn change_way(&self, at: u32, parent: (u32, Via)) {
        self.sure_ways
            .lock()
            .expect("a walk thread panicked")
            .push((at, parent));
    }

    /// How many states it has.
    pub(super) fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// The bytes it takes.
    pub(super) fn bytes(&self) -> usize {
        let pages = self.len().div_ceil(PAGE);
        block(PAGE * size_of::<OnceLock<Page>>())
            + pages * block(PAGE * size_of::<OnceLock<Node>>())
            + block(
                self.sure_ways
                    .lock()
                    .expect("a walk thread panicked")
                    .capacity()
                    * size_of::<SureWay>(),
            )
    }

    /// Its states in order, once the threads have ended.
    pub(super) fn into_nodes(self) -> Vec<Node> {
        let mut nodes = Vec::with_capacity(self.len());
        for page in self.pages.into_vec() {
            let Some(page) = page.into_inner() else {
                break;
            };
            nodes.extend(page.into_vec().into_iter().map_while(OnceLock::into_inner));
        }
        let sure_ways = self.sure_ways.into_inner().expect("a walk thread panicked");
        for (at, parent) in sure_ways {
            let node: &mut Node = &mut nodes[at as usize];
            node.parent = Some(parent);
        }
        nodes
    }
}
