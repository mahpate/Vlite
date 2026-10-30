//! A compact, key-ordered property map.
//!
//! Behaves like `BTreeMap<String, PropertyValue>` (ordered iteration, last
//! insert wins, same equality) but stores entries in one exactly sized sorted
//! vector. A `BTreeMap` allocates a whole leaf node (about 630 bytes) even
//! for a single property, which dominated per-edge memory.

use super::model::PropertyValue;
use std::borrow::Borrow;
use std::fmt;

type Entry = (String, PropertyValue);

#[derive(Clone, Default, PartialEq)]
pub struct Properties {
    entries: Vec<Entry>,
}

impl Properties {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn position<Q>(&self, key: &Q) -> Result<usize, usize>
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.entries
            .binary_search_by(|(name, _)| name.borrow().cmp(key))
    }

    pub fn get<Q>(&self, key: &Q) -> Option<&PropertyValue>
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.position(key).ok().map(|at| &self.entries[at].1)
    }

    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut PropertyValue>
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.position(key).ok().map(|at| &mut self.entries[at].1)
    }

    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&String, &PropertyValue)>
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.position(key)
            .ok()
            .map(|at| (&self.entries[at].0, &self.entries[at].1))
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.position(key).is_ok()
    }

    pub fn insert(&mut self, key: String, value: PropertyValue) -> Option<PropertyValue> {
        match self.position(key.as_str()) {
            Ok(at) => Some(std::mem::replace(&mut self.entries[at].1, value)),
            Err(at) => {
                self.entries.reserve_exact(1);
                self.entries.insert(at, (key, value));
                None
            }
        }
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<PropertyValue>
    where
        String: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let at = self.position(key).ok()?;
        let (_, value) = self.entries.remove(at);
        self.entries.shrink_to_fit();
        Some(value)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&String, &mut PropertyValue) -> bool) {
        self.entries.retain_mut(|(key, value)| keep(key, value));
        self.entries.shrink_to_fit();
    }

    pub fn clear(&mut self) {
        self.entries = Vec::new();
    }

    pub fn first_key_value(&self) -> Option<(&String, &PropertyValue)> {
        self.entries.first().map(|(key, value)| (key, value))
    }

    pub fn last_key_value(&self) -> Option<(&String, &PropertyValue)> {
        self.entries.last().map(|(key, value)| (key, value))
    }

    pub fn iter(&self) -> Iter<'_> {
        Iter(self.entries.iter())
    }

    pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = (&String, &mut PropertyValue)> {
        self.entries.iter_mut().map(|(key, value)| (&*key, value))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &String> + ExactSizeIterator {
        self.entries.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &PropertyValue> + ExactSizeIterator {
        self.entries.iter().map(|(_, value)| value)
    }

    pub fn values_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut PropertyValue> {
        self.entries.iter_mut().map(|(_, value)| value)
    }

    pub fn into_keys(self) -> impl DoubleEndedIterator<Item = String> {
        self.entries.into_iter().map(|(key, _)| key)
    }

    pub fn into_values(self) -> impl DoubleEndedIterator<Item = PropertyValue> {
        self.entries.into_iter().map(|(_, value)| value)
    }

    /// Sorts by key, keeping the last value of duplicate keys (like
    /// repeated `BTreeMap::insert`), and drops spare capacity.
    fn from_unsorted(mut entries: Vec<Entry>) -> Self {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut sorted: Vec<Entry> = Vec::with_capacity(entries.len());
        for entry in entries {
            match sorted.last_mut() {
                Some(last) if last.0 == entry.0 => *last = entry,
                _ => sorted.push(entry),
            }
        }
        sorted.shrink_to_fit();
        Self { entries: sorted }
    }
}

impl fmt::Debug for Properties {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<Q> std::ops::Index<&Q> for Properties
where
    String: Borrow<Q>,
    Q: Ord + ?Sized,
{
    type Output = PropertyValue;

    fn index(&self, key: &Q) -> &PropertyValue {
        self.get(key).expect("no entry found for key")
    }
}

impl FromIterator<Entry> for Properties {
    fn from_iter<I: IntoIterator<Item = Entry>>(iter: I) -> Self {
        Self::from_unsorted(iter.into_iter().collect())
    }
}

impl<const N: usize> From<[Entry; N]> for Properties {
    fn from(entries: [Entry; N]) -> Self {
        Self::from_unsorted(Vec::from(entries))
    }
}

impl Extend<Entry> for Properties {
    fn extend<I: IntoIterator<Item = Entry>>(&mut self, iter: I) {
        let mut entries = std::mem::take(&mut self.entries);
        entries.extend(iter);
        *self = Self::from_unsorted(entries);
    }
}

impl IntoIterator for Properties {
    type Item = Entry;
    type IntoIter = std::vec::IntoIter<Entry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a Properties {
    type Item = (&'a String, &'a PropertyValue);
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Borrowing iterator over `(key, value)` pairs in key order.
#[derive(Clone)]
pub struct Iter<'a>(std::slice::Iter<'a, Entry>);

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a String, &'a PropertyValue);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(key, value)| (key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|(key, value)| (key, value))
    }
}

impl ExactSizeIterator for Iter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn int(value: i64) -> PropertyValue {
        PropertyValue::Integer(value)
    }

    #[test]
    fn matches_btree_map_semantics() {
        let ops: [(&str, Option<i64>); 9] = [
            ("m", Some(1)),
            ("a", Some(2)),
            ("z", Some(3)),
            ("m", Some(4)),
            ("a", None),
            ("b", Some(5)),
            ("q", None),
            ("a", Some(6)),
            ("z", None),
        ];
        let mut ours = Properties::new();
        let mut reference = BTreeMap::new();
        for (key, op) in ops {
            match op {
                Some(value) => assert_eq!(
                    ours.insert(key.into(), int(value)),
                    reference.insert(key.to_string(), int(value))
                ),
                None => assert_eq!(ours.remove(key), reference.remove(key)),
            }
            assert!(ours.iter().eq(reference.iter()));
            assert_eq!(ours.len(), reference.len());
            assert_eq!(ours.last_key_value(), reference.last_key_value());
            assert_eq!(ours.get("m"), reference.get("m"));
        }
        assert_eq!(format!("{ours:?}"), format!("{reference:?}"));
    }

    #[test]
    fn collecting_sorts_and_keeps_the_last_duplicate() {
        let props: Properties = [("b", 1), ("a", 2), ("b", 3), ("c", 4), ("a", 5)]
            .into_iter()
            .map(|(key, value)| (key.to_string(), int(value)))
            .collect();
        let pairs: Vec<_> = props.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        assert_eq!(pairs, [("a", int(5)), ("b", int(3)), ("c", int(4))]);
        assert_eq!(props.entries.capacity(), 3);
        let mut extended = props.clone();
        extended.extend([("0".to_string(), int(0)), ("b".to_string(), int(9))]);
        assert_eq!(extended.keys().collect::<Vec<_>>(), ["0", "a", "b", "c"]);
        assert_eq!(extended.get("b"), Some(&int(9)));
    }

    #[test]
    fn single_property_allocates_one_exact_entry() {
        let mut props = Properties::new();
        props.insert("weight".into(), PropertyValue::Float(0.5));
        assert_eq!(props.entries.capacity(), 1);
        props.retain(|_, _| false);
        assert_eq!(props.entries.capacity(), 0);
    }
}
