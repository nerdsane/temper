//! Strings kept once.
//!
//! A verifier's states hold the same few names millions of times, and compare
//! and copy them on every step. A [`Sym`] is a pointer to the one copy of its
//! text, so copying one copies a word and comparing two for equality compares
//! a word.
//!
//! The copies stay for the life of the process: a walk's threads and whatever
//! a walk returns hold them with nothing to say when the last is gone. What
//! that costs is the text of the distinct names and literals in the specs the
//! process has read, which an install's specs already bound.

// A thin pointer: one word, not the two of `&'static str`, since a state holds
// many of these.
#![allow(clippy::borrowed_box)]

use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::{OnceLock, RwLock};

/// A string kept once. Equal strings are one pointer. It orders as its text
/// does, so what is sorted by these sorts as the strings would.
#[derive(Clone, Copy)]
pub struct Sym(&'static Box<str>);

type Table = RwLock<HashMap<&'static str, Sym>>;

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(Table::default)
}

impl Sym {
    /// The one copy of `text`.
    pub fn new(text: &str) -> Self {
        // A walk's threads name the same few things millions of times; each
        // keeps what it has looked up so they don't queue on the one lock.
        thread_local! {
            static SEEN: RefCell<FxHashMap<&'static str, Sym>> = RefCell::default();
        }
        SEEN.with(|seen| {
            if let Some(found) = seen.borrow().get(text) {
                return *found;
            }
            let found = Self::kept(text);
            seen.borrow_mut().insert(found.as_str(), found);
            found
        })
    }

    /// The one copy of `text`, from the table every thread shares.
    fn kept(text: &str) -> Self {
        let table = table();
        let found = table
            .read()
            .expect("the table of names is never left half written")
            .get(text)
            .copied();
        if let Some(found) = found {
            return found;
        }
        let mut table = table
            .write()
            .expect("the table of names is never left half written");
        if let Some(found) = table.get(text) {
            return *found;
        }
        let kept: &'static Box<str> = Box::leak(Box::new(text.into()));
        table.insert(kept, Self(kept));
        Self(kept)
    }

    /// Its text.
    pub fn as_str(self) -> &'static str {
        self.0
    }

    /// A number equal for equal strings, for those that index by it.
    pub fn address(self) -> usize {
        std::ptr::from_ref(self.0) as usize
    }
}

impl Deref for Sym {
    type Target = str;

    fn deref(&self) -> &str {
        self.0
    }
}

impl AsRef<str> for Sym {
    fn as_ref(&self) -> &str {
        self.0
    }
}

impl PartialEq for Sym {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for Sym {}

impl PartialOrd for Sym {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Sym {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self == other {
            std::cmp::Ordering::Equal
        } else {
            self.as_str().cmp(other.as_str())
        }
    }
}

impl Hash for Sym {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address().hash(state);
    }
}

impl PartialEq<str> for Sym {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Sym {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for Sym {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<Sym> for str {
    fn eq(&self, other: &Sym) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<Sym> for &str {
    fn eq(&self, other: &Sym) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<Sym> for String {
    fn eq(&self, other: &Sym) -> bool {
        self == other.as_str()
    }
}

impl From<&str> for Sym {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for Sym {
    fn from(text: String) -> Self {
        Self::new(&text)
    }
}

impl From<&String> for Sym {
    fn from(text: &String) -> Self {
        Self::new(text)
    }
}

impl From<Sym> for String {
    fn from(sym: Sym) -> Self {
        sym.as_str().to_string()
    }
}

impl fmt::Display for Sym {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

/// As its text, so what prints a model reads as it did with strings.
impl fmt::Debug for Sym {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

impl serde::Serialize for Sym {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Sym {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Ok(Self::new(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_are_one_and_order_as_text() {
        let (a, b) = (Sym::new("alpha"), Sym::from(String::from("alpha")));
        assert_eq!(a, b);
        assert_eq!(a.address(), b.address());
        assert!(Sym::new("alpha") < Sym::new("beta"));
        assert_eq!(a, "alpha");
        assert_eq!(format!("{a:?}"), format!("{:?}", "alpha"));
    }
}
