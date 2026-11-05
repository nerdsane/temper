//! Looking up what is keyed by two strings without allocating them.

use std::borrow::Borrow;
use std::cmp::Ordering;

/// Two strings, however they are held.
pub(super) trait Pair {
    fn parts(&self) -> (&str, &str);
}

impl Pair for (String, String) {
    fn parts(&self) -> (&str, &str) {
        (&self.0, &self.1)
    }
}

impl Pair for (&str, &str) {
    fn parts(&self) -> (&str, &str) {
        (self.0, self.1)
    }
}

impl PartialEq for dyn Pair + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for dyn Pair + '_ {}

impl PartialOrd for dyn Pair + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for dyn Pair + '_ {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts().cmp(&other.parts())
    }
}

impl<'a> Borrow<dyn Pair + 'a> for (String, String) {
    fn borrow(&self) -> &(dyn Pair + 'a) {
        self
    }
}

/// A key for what is keyed by `(String, String)`, for one lookup.
macro_rules! pair {
    ($a:expr, $b:expr) => {
        (&(AsRef::<str>::as_ref($a), AsRef::<str>::as_ref($b))
            as &dyn crate::composite::joint::pair::Pair)
    };
}
