//! Lexical BM25 index over declared `text_fields` for hybrid search.
//!
//! Only `String` metadata values of declared fields are indexed. Postings are
//! derived, never persisted, rebuilt on open or declaration change and
//! maintained after successful commits. Exceeding `MAX_TEXT_POSTINGS` marks
//! the index unavailable (text queries return `IndexNotReady`) without ever
//! rejecting a durable write; changing the declarations (or reopening)
//! rebuilds it.

use crate::{CollectionMut, CollectionRef, Database, Metadata, MetadataValue, Record};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Maximum declared text fields per collection.
pub const MAX_TEXT_FIELDS: usize = 8;
/// Maximum (term, record) posting entries per collection.
pub const MAX_TEXT_POSTINGS: usize = 16_777_216;
/// Tokens longer than this many UTF-8 bytes (after lowercasing) are dropped.
pub const MAX_TOKEN_BYTES: usize = 64;
/// Only the first this-many kept tokens of each field value are indexed.
pub const MAX_TOKENS_PER_FIELD: usize = 4096;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Splits on non-alphanumeric characters, lowercases, drops tokens over
/// `MAX_TOKEN_BYTES` and keeps at most `MAX_TOKENS_PER_FIELD` tokens.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .filter(|token| token.len() <= MAX_TOKEN_BYTES)
        .take(MAX_TOKENS_PER_FIELD)
        .collect()
}

/// Observable state of one collection's text index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextIndexStatus {
    /// Number of declared text fields (0 = no text index).
    pub declared_fields: usize,
    /// False after the posting limit was exceeded, until rebuilt.
    pub available: bool,
    /// Records with at least one indexed token.
    pub documents: u64,
    /// Distinct indexed terms.
    pub terms: u64,
    /// (term, record) posting entries; bounded by `MAX_TEXT_POSTINGS`.
    pub postings: u64,
}

/// Derived, never-persisted text postings for one collection.
#[derive(Debug)]
pub(crate) struct TextIndex {
    fields: Vec<String>,
    postings: HashMap<String, BTreeMap<u64, u32>>,
    doc_lengths: BTreeMap<u64, u32>,
    total_length: u64,
    entries: usize,
    available: bool,
    limit: usize,
}

impl Default for TextIndex {
    fn default() -> Self {
        Self {
            fields: Vec::new(),
            postings: HashMap::new(),
            doc_lengths: BTreeMap::new(),
            total_length: 0,
            entries: 0,
            available: true,
            limit: MAX_TEXT_POSTINGS,
        }
    }
}

impl TextIndex {
    /// Term frequencies and token count of one record over declared fields.
    fn document(&self, metadata: &Metadata) -> (BTreeMap<String, u32>, u32) {
        let mut terms = BTreeMap::new();
        let mut length = 0u32;
        for field in &self.fields {
            if let Some(MetadataValue::String(text)) = metadata.get(field) {
                for token in tokenize(text) {
                    length += 1;
                    *terms.entry(token).or_insert(0) += 1;
                }
            }
        }
        (terms, length)
    }

    /// Rebuilds from all live records after open or a declaration change.
    pub(crate) fn rebuild(&mut self, fields: &[String], records: &BTreeMap<u64, Record>) {
        *self = Self {
            fields: fields.to_vec(),
            limit: self.limit,
            ..Self::default()
        };
        if self.fields.is_empty() {
            return;
        }
        for (&id, record) in records {
            self.track(id, None, Some(&record.metadata));
            if !self.available {
                return;
            }
        }
    }

    /// Applies one committed change (`current` None means delete).
    pub(crate) fn track(
        &mut self,
        id: u64,
        previous: Option<&Metadata>,
        current: Option<&Metadata>,
    ) {
        if self.fields.is_empty() || !self.available {
            return;
        }
        let unchanged = |metadata: Option<&Metadata>, other: Option<&Metadata>| {
            self.fields.iter().all(|field| {
                metadata.and_then(|m| m.get(field)) == other.and_then(|m| m.get(field))
            })
        };
        if previous.is_some() && unchanged(previous, current) {
            return;
        }
        if let Some(previous) = previous {
            let (terms, length) = self.document(previous);
            for (term, _) in terms {
                if let Some(ids) = self.postings.get_mut(&term) {
                    if ids.remove(&id).is_some() {
                        self.entries -= 1;
                    }
                    if ids.is_empty() {
                        self.postings.remove(&term);
                    }
                }
            }
            if self.doc_lengths.remove(&id).is_some() {
                self.total_length -= u64::from(length);
            }
        }
        let Some(current) = current else {
            return;
        };
        let (terms, length) = self.document(current);
        if length == 0 {
            return;
        }
        if self.entries + terms.len() > self.limit {
            // Never reject a durable write: drop postings until a rebuild.
            *self = Self {
                fields: std::mem::take(&mut self.fields),
                available: false,
                limit: self.limit,
                ..Self::default()
            };
            return;
        }
        self.entries += terms.len();
        for (term, frequency) in terms {
            self.postings.entry(term).or_default().insert(id, frequency);
        }
        self.doc_lengths.insert(id, length);
        self.total_length += u64::from(length);
    }

    pub(crate) fn status(&self) -> TextIndexStatus {
        TextIndexStatus {
            declared_fields: self.fields.len(),
            available: self.available,
            documents: self.doc_lengths.len() as u64,
            terms: self.postings.len() as u64,
            postings: self.entries as u64,
        }
    }

    /// BM25 top-`n` over unique query `terms` for records accepted by `keep`,
    /// ordered by score descending then ID ascending. Scores are finite and
    /// positive: idf > 0, document lengths ≥ 1 and the average length ≥ 1.
    pub(crate) fn search(
        &self,
        terms: &BTreeSet<String>,
        n: usize,
        mut keep: impl FnMut(u64) -> bool,
    ) -> Vec<(u64, f64)> {
        let documents = self.doc_lengths.len() as f64;
        if documents == 0.0 || n == 0 {
            return Vec::new();
        }
        let average = self.total_length as f64 / documents;
        let mut scores: HashMap<u64, f64> = HashMap::new();
        for term in terms {
            let Some(ids) = self.postings.get(term) else {
                continue;
            };
            let df = ids.len() as f64;
            let idf = (1.0 + (documents - df + 0.5) / (df + 0.5)).ln();
            for (&id, &frequency) in ids {
                let tf = f64::from(frequency);
                let length = f64::from(self.doc_lengths[&id]);
                let norm = K1 * (1.0 - B + B * length / average);
                *scores.entry(id).or_insert(0.0) += idf * tf * (K1 + 1.0) / (tf + norm);
            }
        }
        let mut hits: Vec<(u64, f64)> = scores.into_iter().filter(|&(id, _)| keep(id)).collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.truncate(n);
        hits
    }
}

impl CollectionRef<'_> {
    /// State of this collection's BM25 text index.
    pub fn text_index_status(&self) -> TextIndexStatus {
        self.db.state(self.id).text_index.status()
    }
}

impl CollectionMut<'_> {
    /// See `CollectionRef::text_index_status`.
    pub fn text_index_status(&self) -> TextIndexStatus {
        self.view().text_index_status()
    }
}

impl Database {
    /// Text index state of the default collection.
    pub fn text_index_status(&self) -> TextIndexStatus {
        CollectionRef { db: self, id: 0 }.text_index_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(texts: &[(u64, &str)]) -> BTreeMap<u64, Record> {
        texts
            .iter()
            .map(|&(id, text)| {
                (
                    id,
                    Record {
                        vector: vec![0.0],
                        payload: vec![],
                        metadata: Metadata::from([(
                            "body".to_owned(),
                            MetadataValue::String(text.to_owned()),
                        )]),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn tokenizer_splits_lowercases_and_bounds() {
        assert_eq!(
            tokenize("Hello, WORLD!  héllo_42"),
            ["hello", "world", "héllo", "42"]
        );
        let long = "a".repeat(MAX_TOKEN_BYTES + 1);
        assert_eq!(tokenize(&format!("x {long} y")), ["x", "y"]);
        assert_eq!(
            tokenize(&"t ".repeat(MAX_TOKENS_PER_FIELD + 10)).len(),
            MAX_TOKENS_PER_FIELD
        );
    }

    #[test]
    fn bm25_matches_hand_computation_and_tracks_updates() {
        let mut index = TextIndex::default();
        let data = records(&[(1, "apple apple banana"), (2, "banana"), (3, "cherry")]);
        index.rebuild(&["body".into()], &data);
        let terms = BTreeSet::from(["apple".to_owned()]);
        let hits = index.search(&terms, 10, |_| true);
        let (n, df, tf, dl, avg) = (3.0f64, 1.0f64, 2.0f64, 3.0f64, 5.0f64 / 3.0);
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let expected = idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * dl / avg));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 1);
        assert!((hits[0].1 - expected).abs() < 1e-12);
        let both = index.search(&BTreeSet::from(["banana".to_owned()]), 10, |_| true);
        assert_eq!(both.iter().map(|h| h.0).collect::<Vec<_>>(), [2, 1]);
        index.track(2, Some(&data[&2].metadata), None);
        assert_eq!(index.status().documents, 2);
        let only = index.search(&BTreeSet::from(["banana".to_owned()]), 10, |id| id != 1);
        assert!(only.is_empty());
    }

    #[test]
    fn exceeding_the_posting_limit_marks_unavailable_until_rebuild() {
        let mut index = TextIndex {
            limit: 3,
            ..TextIndex::default()
        };
        let data = records(&[(1, "a b"), (2, "c d")]);
        index.rebuild(&["body".into()], &BTreeMap::new());
        index.track(1, None, Some(&data[&1].metadata));
        assert!(index.status().available);
        index.track(2, None, Some(&data[&2].metadata));
        let status = index.status();
        assert!(!status.available);
        assert_eq!((status.postings, status.documents), (0, 0));
        index.track(3, None, Some(&data[&1].metadata));
        assert!(!index.status().available);
        index.limit = 10;
        index.rebuild(&["body".into()], &data);
        assert_eq!(index.status().postings, 4);
        assert!(index.status().available);
    }
}
