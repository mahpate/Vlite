//! Per-collection metadata indexes.
//!
//! Declarations are persisted by the collection settings; postings are derived
//! in memory, rebuilt on open or declaration change and maintained only after
//! successful commits. Lookups return a candidate *superset* that callers must
//! re-verify with `metadata::matches`, so indexed and unindexed searches return
//! identical results. Numeric typing follows the filter semantics exactly:
//! `Integer` and `Float` are distinct (`Integer(1)` never matches `Float(1.0)`)
//! and `-0.0` equals `0.0`.

use crate::{Condition, Metadata, MetadataValue, Record};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Maximum declared metadata indexes per collection.
pub const MAX_METADATA_INDEXES: usize = 16;

/// Kind of a declared metadata index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetadataIndexKind {
    /// Equality postings.
    Eq,
    /// Ordered numeric postings.
    Range,
    /// Both equality and ordered numeric postings.
    EqRange,
}

impl MetadataIndexKind {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Eq => 1,
            Self::Range => 2,
            Self::EqRange => 3,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Eq),
            2 => Some(Self::Range),
            3 => Some(Self::EqRange),
            _ => None,
        }
    }
}

/// One declared index: a metadata field name (1..=256 UTF-8 bytes) and kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MetadataIndexSpec {
    pub field: String,
    pub kind: MetadataIndexKind,
}

/// Derived, never-persisted metadata postings for one collection.
///
/// Maintained only after successful commits (PAT-002) and rebuilt from live
/// records on open or declaration change.
#[derive(Debug, Default)]
pub(crate) struct MetadataIndexes {
    fields: BTreeMap<String, FieldIndex>,
}

/// Result of a metadata-index lookup over a conjunction of conditions.
#[derive(Debug)]
pub(crate) struct IndexLookup {
    /// Upper bound on matching records: exact posting sizes for equality,
    /// and range sums that stop (saturating at `cap + 1`) once over the cap.
    /// `ids` is `None` exactly when the bound exceeds the cap.
    pub estimate: u64,
    /// Field of the most selective indexed condition.
    pub field: String,
    /// Candidate superset (all conditions must still be re-verified);
    /// materialized only when `estimate <= cap`.
    pub ids: Option<BTreeSet<u64>>,
}

type Postings = BTreeSet<u64>;

/// Finite f64 key ordered by value with `-0.0` normalized to `0.0`, so the
/// key order equals `partial_cmp` on the finite values metadata permits.
#[derive(Clone, Copy, Debug)]
struct OrdF64(f64);

impl OrdF64 {
    fn new(value: f64) -> Self {
        Self(if value == 0.0 { 0.0 } else { value })
    }
}

impl PartialEq for OrdF64 {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Equality key with the same identity as `MetadataValue::eq` on valid
/// (finite) metadata: types are distinct and `-0.0 == 0.0`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum EqKey {
    String(String),
    Integer(i64),
    Float(u64),
    Boolean(bool),
}

impl EqKey {
    fn of(value: &MetadataValue) -> Self {
        match value {
            MetadataValue::String(value) => Self::String(value.clone()),
            MetadataValue::Integer(value) => Self::Integer(*value),
            MetadataValue::Float(value) => Self::Float(OrdF64::new(*value).0.to_bits()),
            MetadataValue::Boolean(value) => Self::Boolean(*value),
        }
    }
}

#[derive(Debug, Default)]
struct Numeric {
    ints: BTreeMap<i64, Postings>,
    floats: BTreeMap<OrdF64, Postings>,
}

#[derive(Debug, Default)]
struct FieldIndex {
    eq: Option<HashMap<EqKey, Postings>>,
    range: Option<Numeric>,
}

fn add<K: Ord>(map: &mut BTreeMap<K, Postings>, key: K, id: u64) {
    map.entry(key).or_default().insert(id);
}

fn remove<K: Ord>(map: &mut BTreeMap<K, Postings>, key: &K, id: u64) {
    if let Some(ids) = map.get_mut(key) {
        ids.remove(&id);
        if ids.is_empty() {
            map.remove(key);
        }
    }
}

/// Visits posting sets in `[min, max]` until `visit` returns false.
fn visit_range<K: Ord + Copy>(
    map: &BTreeMap<K, Postings>,
    min: Option<K>,
    max: Option<K>,
    mut visit: impl FnMut(&Postings) -> bool,
) {
    use std::ops::Bound::{Included, Unbounded};
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return;
    }
    let lower = min.map_or(Unbounded, Included);
    let upper = max.map_or(Unbounded, Included);
    for ids in map.range((lower, upper)).map(|(_, ids)| ids) {
        if !visit(ids) {
            return;
        }
    }
}

impl FieldIndex {
    fn insert(&mut self, id: u64, value: &MetadataValue) {
        if let Some(eq) = &mut self.eq {
            eq.entry(EqKey::of(value)).or_default().insert(id);
        }
        if let Some(range) = &mut self.range {
            match value {
                MetadataValue::Integer(value) => add(&mut range.ints, *value, id),
                MetadataValue::Float(value) => add(&mut range.floats, OrdF64::new(*value), id),
                _ => {}
            }
        }
    }

    fn remove(&mut self, id: u64, value: &MetadataValue) {
        if let Some(eq) = &mut self.eq {
            let key = EqKey::of(value);
            if let Some(ids) = eq.get_mut(&key) {
                ids.remove(&id);
                if ids.is_empty() {
                    eq.remove(&key);
                }
            }
        }
        if let Some(range) = &mut self.range {
            match value {
                MetadataValue::Integer(value) => remove(&mut range.ints, value, id),
                MetadataValue::Float(value) => remove(&mut range.floats, &OrdF64::new(*value), id),
                _ => {}
            }
        }
    }

    /// Posting set for an equality value, or `None` when not indexable here.
    fn eq_postings(&self, value: &MetadataValue) -> Option<Option<&Postings>> {
        if let Some(eq) = &self.eq {
            return Some(eq.get(&EqKey::of(value)));
        }
        let range = self.range.as_ref()?;
        match value {
            MetadataValue::Integer(value) => Some(range.ints.get(value)),
            MetadataValue::Float(value) => Some(range.floats.get(&OrdF64::new(*value))),
            _ => None,
        }
    }

    /// Visits range postings; returns false when ranges are not indexed here.
    fn visit(
        &self,
        min: &Option<MetadataValue>,
        max: &Option<MetadataValue>,
        visit: impl FnMut(&Postings) -> bool,
    ) -> bool {
        let Some(range) = &self.range else {
            return false;
        };
        let int = |value: &Option<MetadataValue>| match value {
            Some(MetadataValue::Integer(value)) => Some(*value),
            _ => None,
        };
        let float = |value: &Option<MetadataValue>| match value {
            Some(MetadataValue::Float(value)) => Some(OrdF64::new(*value)),
            _ => None,
        };
        // Validated ranges have numeric bounds of one type; an integer range
        // can only match integers and a float range only floats.
        let integer = matches!(min, Some(MetadataValue::Integer(_)))
            || matches!(max, Some(MetadataValue::Integer(_)));
        if integer {
            visit_range(&range.ints, int(min), int(max), visit);
        } else {
            visit_range(&range.floats, float(min), float(max), visit);
        }
        true
    }

    /// Upper bound on matches saturating at `cap + 1`; `None` = not indexed.
    fn count(&self, condition: &Condition, cap: usize) -> Option<usize> {
        match condition {
            Condition::Eq { value, .. } => self
                .eq_postings(value)
                .map(|ids| ids.map_or(0, BTreeSet::len)),
            Condition::Range { min, max, .. } => {
                let mut total = 0usize;
                self.visit(min, max, |ids| {
                    total = total.saturating_add(ids.len());
                    total <= cap
                })
                .then_some(total.min(cap.saturating_add(1)))
            }
        }
    }

    fn ids(&self, condition: &Condition) -> Postings {
        match condition {
            Condition::Eq { value, .. } => self
                .eq_postings(value)
                .flatten()
                .cloned()
                .unwrap_or_default(),
            Condition::Range { min, max, .. } => {
                let mut all = Postings::new();
                self.visit(min, max, |ids| {
                    all.extend(ids);
                    true
                });
                all
            }
        }
    }
}

fn field_of(condition: &Condition) -> &str {
    match condition {
        Condition::Eq { field, .. } | Condition::Range { field, .. } => field,
    }
}

impl MetadataIndexes {
    /// Rebuilds from all live records after open or a declaration change.
    pub(crate) fn rebuild(&mut self, specs: &[MetadataIndexSpec], records: &BTreeMap<u64, Record>) {
        self.fields.clear();
        for spec in specs {
            let index = self.fields.entry(spec.field.clone()).or_default();
            if matches!(
                spec.kind,
                MetadataIndexKind::Eq | MetadataIndexKind::EqRange
            ) {
                index.eq.get_or_insert_with(HashMap::new);
            }
            if matches!(
                spec.kind,
                MetadataIndexKind::Range | MetadataIndexKind::EqRange
            ) {
                index.range.get_or_insert_with(Numeric::default);
            }
        }
        if self.fields.is_empty() {
            return;
        }
        for (&id, record) in records {
            self.track(id, None, Some(&record.metadata));
        }
    }

    /// Applies one committed change (`current` None means delete).
    pub(crate) fn track(
        &mut self,
        id: u64,
        previous: Option<&Metadata>,
        current: Option<&Metadata>,
    ) {
        for (field, index) in &mut self.fields {
            let old = previous.and_then(|metadata| metadata.get(field));
            let new = current.and_then(|metadata| metadata.get(field));
            if old.is_some() && old == new {
                continue;
            }
            if let Some(old) = old {
                index.remove(id, old);
            }
            if let Some(new) = new {
                index.insert(id, new);
            }
        }
    }

    /// Looks up the most selective indexed condition. Returns `None` when no
    /// condition can use an index. Candidate IDs are materialized (and
    /// intersected with every other indexed condition within `cap`) only when
    /// the best bound is `<= cap`.
    pub(crate) fn lookup(&self, conditions: &[Condition], cap: usize) -> Option<IndexLookup> {
        let counted: Vec<(usize, &FieldIndex, &Condition)> = conditions
            .iter()
            .filter_map(|condition| {
                let index = self.fields.get(field_of(condition))?;
                Some((index.count(condition, cap)?, index, condition))
            })
            .collect();
        let best = counted
            .iter()
            .enumerate()
            .min_by_key(|(_, (count, ..))| *count)?
            .0;
        let (count, index, condition) = counted[best];
        let field = field_of(condition).to_owned();
        if count > cap {
            return Some(IndexLookup {
                estimate: count as u64,
                field,
                ids: None,
            });
        }
        let mut ids = index.ids(condition);
        for (position, &(other_count, other, other_condition)) in counted.iter().enumerate() {
            if position == best || other_count > cap || ids.is_empty() {
                continue;
            }
            let other_ids = other.ids(other_condition);
            ids.retain(|id| other_ids.contains(id));
        }
        Some(IndexLookup {
            estimate: ids.len() as u64,
            field,
            ids: Some(ids),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: &[(&str, MetadataValue)]) -> Record {
        Record {
            vector: vec![0.0],
            payload: vec![],
            metadata: fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    fn spec(field: &str, kind: MetadataIndexKind) -> MetadataIndexSpec {
        MetadataIndexSpec {
            field: field.into(),
            kind,
        }
    }

    #[test]
    fn typed_equality_ranges_and_negative_zero_follow_filter_semantics() {
        use MetadataValue::{Float, Integer, String as Text};
        let records = BTreeMap::from([
            (1, record(&[("n", Integer(1))])),
            (2, record(&[("n", Float(1.0))])),
            (3, record(&[("n", Float(-0.0))])),
            (4, record(&[("n", Float(0.0))])),
            (5, record(&[("n", Integer(i64::MIN))])),
            (6, record(&[("n", Integer(i64::MAX))])),
            (7, record(&[("n", Text("1".into()))])),
        ]);
        for kind in [
            MetadataIndexKind::Eq,
            MetadataIndexKind::Range,
            MetadataIndexKind::EqRange,
        ] {
            let mut indexes = MetadataIndexes::default();
            indexes.rebuild(&[spec("n", kind)], &records);
            let ids = |condition: Condition| {
                indexes
                    .lookup(&[condition], usize::MAX - 1)
                    .and_then(|lookup| lookup.ids)
                    .map(|ids| ids.into_iter().collect::<Vec<_>>())
            };
            let eq = |value| Condition::Eq {
                field: "n".into(),
                value,
            };
            assert_eq!(ids(eq(Integer(1))), Some(vec![1]));
            assert_eq!(ids(eq(Float(1.0))), Some(vec![2]));
            assert_eq!(ids(eq(Float(0.0))), Some(vec![3, 4]));
            assert_eq!(ids(eq(Float(-0.0))), Some(vec![3, 4]));
            let text = ids(eq(Text("1".into())));
            let range = |min, max| Condition::Range {
                field: "n".into(),
                min,
                max,
            };
            let ints = ids(range(Some(Integer(i64::MIN)), Some(Integer(i64::MAX))));
            let floats = ids(range(Some(Float(-0.0)), None));
            if kind == MetadataIndexKind::Eq {
                assert_eq!(text, Some(vec![7]));
                assert_eq!(ints, None);
                assert_eq!(floats, None);
            } else {
                assert_eq!(
                    text,
                    if kind == MetadataIndexKind::Range {
                        None
                    } else {
                        Some(vec![7])
                    }
                );
                assert_eq!(ints, Some(vec![1, 5, 6]));
                assert_eq!(floats, Some(vec![2, 3, 4]));
            }
        }
    }

    #[test]
    fn tracking_updates_and_lookup_caps_and_intersects() {
        let mut indexes = MetadataIndexes::default();
        indexes.rebuild(
            &[
                spec("a", MetadataIndexKind::Eq),
                spec("b", MetadataIndexKind::Range),
            ],
            &BTreeMap::new(),
        );
        for id in 0..10 {
            let metadata = record(&[
                ("a", MetadataValue::Integer(id as i64 % 2)),
                ("b", MetadataValue::Integer(id as i64)),
            ])
            .metadata;
            indexes.track(id, None, Some(&metadata));
        }
        let a0 = Condition::Eq {
            field: "a".into(),
            value: MetadataValue::Integer(0),
        };
        let b = Condition::Range {
            field: "b".into(),
            min: Some(MetadataValue::Integer(2)),
            max: Some(MetadataValue::Integer(6)),
        };
        let lookup = indexes.lookup(&[a0.clone(), b.clone()], 100).unwrap();
        assert_eq!(
            lookup.ids.unwrap().into_iter().collect::<Vec<_>>(),
            vec![2, 4, 6]
        );
        let capped = indexes.lookup(&[a0.clone(), b], 3).unwrap();
        assert_eq!((capped.estimate, capped.ids), (4, None));
        let old = record(&[("a", MetadataValue::Integer(0))]).metadata;
        indexes.track(0, Some(&old), None);
        let new = record(&[("a", MetadataValue::Integer(1))]).metadata;
        indexes.track(2, Some(&old), Some(&new));
        let lookup = indexes.lookup(&[a0], 100).unwrap();
        assert_eq!(
            lookup.ids.unwrap().into_iter().collect::<Vec<_>>(),
            vec![4, 6, 8]
        );
        assert!(
            indexes
                .lookup(
                    &[Condition::Eq {
                        field: "z".into(),
                        value: MetadataValue::Boolean(true)
                    }],
                    100
                )
                .is_none()
        );
    }
}
