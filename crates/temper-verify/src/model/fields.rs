//! A record's fields, and anything else named: a small sorted list.
//!
//! A step copies the record it moves, and most of what a record holds does not
//! change in a step, so the list is shared until it is written.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

pub use temper_spec::predicate::Sym;

/// What finds an entry: a [`Sym`] by its pointer, any string by its text.
pub trait FieldKey {
    /// Where `entries`, sorted by name, hold this key, or where it would go.
    fn locate<V>(&self, entries: &[(Sym, V)]) -> Result<usize, usize>;
}

impl FieldKey for Sym {
    fn locate<V>(&self, entries: &[(Sym, V)]) -> Result<usize, usize> {
        // Lists are short and a pointer is quick to compare, so look for the
        // pointer first; only a name that is not here reads its text.
        match entries.iter().position(|(name, _)| name == self) {
            Some(at) => Ok(at),
            None => Err(entries.partition_point(|(name, _)| name < self)),
        }
    }
}

impl FieldKey for str {
    fn locate<V>(&self, entries: &[(Sym, V)]) -> Result<usize, usize> {
        entries.binary_search_by(|(name, _)| name.as_str().cmp(self))
    }
}

impl FieldKey for String {
    fn locate<V>(&self, entries: &[(Sym, V)]) -> Result<usize, usize> {
        self.as_str().locate(entries)
    }
}

impl<K: FieldKey + ?Sized> FieldKey for &K {
    fn locate<V>(&self, entries: &[(Sym, V)]) -> Result<usize, usize> {
        (**self).locate(entries)
    }
}

/// Named values sorted by name, as a `BTreeMap<String, V>` iterates them. What
/// reads them in order, renumbering a state's values or printing one, sees
/// what it would with the map.
pub struct Fields<V> {
    /// `None` holds nothing, with nothing to share or allocate.
    entries: Option<Arc<Vec<(Sym, V)>>>,
}

impl<V> Default for Fields<V> {
    fn default() -> Self {
        Self { entries: None }
    }
}

impl<V> Clone for Fields<V> {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

impl<V> Fields<V> {
    /// Nothing named.
    pub fn new() -> Self {
        Self::default()
    }

    fn slice(&self) -> &[(Sym, V)] {
        self.entries.as_ref().map_or(&[], |entries| &entries[..])
    }

    /// How many names it holds.
    pub fn len(&self) -> usize {
        self.slice().len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.slice().is_empty()
    }

    /// The value of `key`.
    pub fn get<K: FieldKey + ?Sized>(&self, key: &K) -> Option<&V> {
        let entries = self.slice();
        key.locate(entries).ok().map(|at| &entries[at].1)
    }

    /// Whether it holds `key`.
    pub fn contains_key<K: FieldKey + ?Sized>(&self, key: &K) -> bool {
        key.locate(self.slice()).is_ok()
    }

    /// Each name and value, by name.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Sym, &V)> + DoubleEndedIterator + Clone {
        self.slice().iter().map(|(name, value)| (name, value))
    }

    /// Each name, in order.
    /// Each entry, its value writable. Copies the entries if they are shared.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&Sym, &mut V)>
    where
        V: Clone,
    {
        self.entries
            .iter_mut()
            .flat_map(|entries| Arc::make_mut(entries).iter_mut())
            .map(|(name, value)| (&*name, value))
    }

    pub fn keys(&self) -> impl ExactSizeIterator<Item = &Sym> + DoubleEndedIterator + Clone {
        self.slice().iter().map(|(name, _)| name)
    }

    /// Each value, by name.
    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + DoubleEndedIterator + Clone {
        self.slice().iter().map(|(_, value)| value)
    }

    /// What `count` says of the list, when nothing else shares it; a list
    /// shared with others is theirs too.
    pub(crate) fn held(&self, count: impl FnOnce(&Vec<(Sym, V)>) -> usize) -> usize {
        match &self.entries {
            Some(entries) if Arc::strong_count(entries) == 1 => count(entries),
            _ => 0,
        }
    }

    /// Whether `self` and `other` share one list: nothing to compare.
    pub fn same(&self, other: &Self) -> bool {
        match (&self.entries, &other.entries) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl<V: Clone> Fields<V> {
    fn unshared(&mut self) -> &mut Vec<(Sym, V)> {
        Arc::make_mut(self.entries.get_or_insert_with(Default::default))
    }

    /// The value of `key`, to change. The list is copied here if it is shared.
    pub fn get_mut<K: FieldKey + ?Sized>(&mut self, key: &K) -> Option<&mut V> {
        let at = key.locate(self.slice()).ok()?;
        Some(&mut self.unshared()[at].1)
    }

    /// Set `name` to `value`, and say what it was.
    pub fn insert(&mut self, name: impl Into<Sym>, value: V) -> Option<V> {
        let name = name.into();
        match name.locate(self.slice()) {
            Ok(at) => Some(std::mem::replace(&mut self.unshared()[at].1, value)),
            Err(at) => {
                self.unshared().insert(at, (name, value));
                None
            }
        }
    }

    /// The value of `name`, set from `default` first if it has none.
    pub fn get_or_insert_with(
        &mut self,
        name: impl Into<Sym>,
        default: impl FnOnce() -> V,
    ) -> &mut V {
        let name = name.into();
        let at = match name.locate(self.slice()) {
            Ok(at) => at,
            Err(at) => {
                self.unshared().insert(at, (name, default()));
                at
            }
        };
        &mut self.unshared()[at].1
    }

    /// Take `key` out.
    pub fn remove<K: FieldKey + ?Sized>(&mut self, key: &K) -> Option<V> {
        let at = key.locate(self.slice()).ok()?;
        let (_, value) = self.unshared().remove(at);
        if self.is_empty() {
            self.entries = None;
        }
        Some(value)
    }

    /// Each value, by name, to change. The list is copied here if it is shared.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.entries
            .iter_mut()
            .flat_map(|entries| Arc::make_mut(entries).iter_mut())
            .map(|(_, value)| value)
    }

    /// Keep what `keep` says to.
    pub fn retain(&mut self, mut keep: impl FnMut(&Sym, &mut V) -> bool) {
        if !self.is_empty() {
            self.unshared()
                .retain_mut(|(name, value)| keep(name, value));
            if self.is_empty() {
                self.entries = None;
            }
        }
    }
}

impl<V: PartialEq> PartialEq for Fields<V> {
    fn eq(&self, other: &Self) -> bool {
        self.same(other) || self.slice() == other.slice()
    }
}

impl<V: Eq> Eq for Fields<V> {}

impl<V: Ord> PartialOrd for Fields<V> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// As the maps compared: entry by entry.
impl<V: Ord> Ord for Fields<V> {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.same(other) {
            return Ordering::Equal;
        }
        self.slice().cmp(other.slice())
    }
}

impl<V: std::hash::Hash> std::hash::Hash for Fields<V> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.slice().hash(state);
    }
}

/// As the map it stands for, so what prints a model reads as it did.
impl<V: fmt::Debug> fmt::Debug for Fields<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<K: Into<Sym>, V: Clone> FromIterator<(K, V)> for Fields<V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut fields = Self::new();
        fields.extend(iter);
        fields
    }
}

impl<K: Into<Sym>, V: Clone> Extend<(K, V)> for Fields<V> {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (name, value) in iter {
            self.insert(name, value);
        }
    }
}

impl<V: Clone> From<std::collections::BTreeMap<String, V>> for Fields<V> {
    fn from(map: std::collections::BTreeMap<String, V>) -> Self {
        map.into_iter().collect()
    }
}

impl<K: Into<Sym>, V: Clone, const N: usize> From<[(K, V); N]> for Fields<V> {
    fn from(entries: [(K, V); N]) -> Self {
        entries.into_iter().collect()
    }
}

impl<K: FieldKey + ?Sized, V> std::ops::Index<&K> for Fields<V> {
    type Output = V;

    fn index(&self, key: &K) -> &V {
        self.get(key).expect("no entry of that name")
    }
}

impl<'a, V> IntoIterator for &'a Fields<V> {
    type Item = (&'a Sym, &'a V);
    type IntoIter =
        std::iter::Map<std::slice::Iter<'a, (Sym, V)>, fn(&'a (Sym, V)) -> (&'a Sym, &'a V)>;

    fn into_iter(self) -> Self::IntoIter {
        fn pair<V>((name, value): &(Sym, V)) -> (&Sym, &V) {
            (name, value)
        }
        self.slice()
            .iter()
            .map(pair::<V> as fn(&'a (Sym, V)) -> (&'a Sym, &'a V))
    }
}

impl<V: serde::Serialize> serde::Serialize for Fields<V> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de, V: serde::Deserialize<'de> + Clone> serde::Deserialize<'de> for Fields<V> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = std::collections::BTreeMap::<String, V>::deserialize(deserializer)?;
        Ok(map.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn reads_as_the_map_it_stands_for() {
        let names = ["pear", "apple", "fig", "apple", "zucchini"];
        let mut fields = Fields::new();
        let mut map = BTreeMap::new();
        for (value, name) in names.iter().enumerate() {
            assert_eq!(
                fields.insert(*name, value),
                map.insert(name.to_string(), value)
            );
        }
        assert_eq!(fields.remove("fig"), map.remove("fig"));
        assert_eq!(fields.get("nothing"), None);
        let read: Vec<(String, usize)> = fields.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let expected: Vec<(String, usize)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(read, expected);
        assert_eq!(format!("{fields:?}"), format!("{map:?}"));
    }

    #[test]
    fn a_copy_changes_only_itself() {
        let first: Fields<usize> = [("a", 1), ("b", 2)].into();
        let mut second = first.clone();
        assert!(first.same(&second));
        *second.get_mut("a").expect("held") = 5;
        assert_eq!(first["a"], 1);
        assert_eq!(second["a"], 5);
        assert!(first < second);
    }
}
