//! P5 step 2 filter summaries (`docs/projects/roadmap/p5-filters-and-serving.md`).
//!
//! A collection that declares metadata indexes also keeps, for those fields,
//! a summary of every block of `BLOCK_ROWS` live records in ID order: first
//! and last public ID, row count and per field the count of each value type,
//! the inclusive integer and float intervals (signed zero normalized), the
//! booleans seen and bounded string and point dictionaries. A per-generation
//! bitmap marks the rows superseded since the summaries were derived, and an
//! overlay holds the IDs whose latest revision lives outside them.
//!
//! Summaries only ever narrow the exact filtered scan to a superset of its
//! matches: a block is skipped only when its summary proves that no row in
//! it can satisfy a condition, and the scan re-verifies every candidate with
//! the full filter, the latest revision and the expiry at the query clock.
//! Fields without a summary, geo radii and shapes that validation rejects
//! never prune. Summaries are derived from the loaded records on the first
//! filtered query that can use them, updated by every committed change and
//! derived again once churn passes `threshold`.

use crate::records::RecordMap;
use crate::{Condition, Metadata, MetadataValue};
use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

/// Derived summary block codec (kind 0x0400). Preactivation: test-only, so
/// no reader decodes the kind and no writer emits it until the shared
/// compact writer wiring exists; loaders skip it as an unknown derived kind.
#[cfg(test)]
mod codec;
#[cfg(test)]
mod tests;

/// Rows per summarized block.
pub(crate) const BLOCK_ROWS: usize = 1024;
/// A string or point dictionary keeps at most 32 values and 1024 value
/// bytes (16 per point); past that it only answers "possibly present".
const MAX_DICTIONARY_VALUES: usize = 32;
const MAX_DICTIONARY_BYTES: usize = 1024;
/// Memory one collection's summaries may use, IDs and bitmap included.
/// Past it the collection keeps scanning everything.
const BUDGET_BYTES: usize = 64 << 20;
/// Changes after which summaries are derived again: superseded rows plus
/// overlaid IDs above `max(MIN_REBUILD_CHURN, rows / 4)`.
const MIN_REBUILD_CHURN: usize = 4096;

fn threshold(rows: usize) -> usize {
    MIN_REBUILD_CHURN.max(rows / 4)
}

/// Signed zero normalized to `+0.0`, so that summaries compare as the
/// filter does (`-0.0 == 0.0`).
fn zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

fn field_of(condition: &Condition) -> &str {
    match condition {
        Condition::Eq { field, .. }
        | Condition::Range { field, .. }
        | Condition::GeoRadius { field, .. } => field,
    }
}

/// What a summary proves about a condition over its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Some row may match.
    Keep,
    /// No row can match.
    Prune,
    /// The summary cannot decide: never prunes.
    Unknown,
}

/// Every distinct value of one type in a block, sorted, or `Overflow` once
/// there are too many to keep.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Dictionary<T> {
    Values(Vec<T>),
    Overflow,
}

impl<T> Default for Dictionary<T> {
    fn default() -> Self {
        Self::Values(Vec::new())
    }
}

/// Bytes a dictionary value counts against `MAX_DICTIONARY_BYTES`.
pub(crate) trait Weight {
    fn weight(&self) -> usize;
}

impl Weight for String {
    fn weight(&self) -> usize {
        self.len()
    }
}

impl Weight for Point {
    fn weight(&self) -> usize {
        16
    }
}

impl<T: Ord + Weight> Dictionary<T> {
    fn insert<Q>(&mut self, value: &Q)
    where
        T: Borrow<Q>,
        Q: Ord + ToOwned<Owned = T> + ?Sized,
    {
        let Self::Values(values) = self else {
            return;
        };
        if let Err(at) = values.binary_search_by(|item| item.borrow().cmp(value)) {
            values.insert(at, value.to_owned());
            let bytes: usize = values.iter().map(Weight::weight).sum();
            if values.len() > MAX_DICTIONARY_VALUES || bytes > MAX_DICTIONARY_BYTES {
                *self = Self::Overflow;
            }
        }
    }
}

impl<T: Ord> Dictionary<T> {
    fn may_contain<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        match self {
            Self::Values(values) => values
                .binary_search_by(|item| item.borrow().cmp(value))
                .is_ok(),
            Self::Overflow => true,
        }
    }
}

/// A geo point with signed zero normalized, ordered by `f64::total_cmp`
/// (latitude, then longitude).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Point {
    pub latitude: f64,
    pub longitude: f64,
}

impl Point {
    pub(crate) fn new(latitude: f64, longitude: f64) -> Self {
        Self {
            latitude: zero(latitude),
            longitude: zero(longitude),
        }
    }
}

impl Ord for Point {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.latitude
            .total_cmp(&other.latitude)
            .then_with(|| self.longitude.total_cmp(&other.longitude))
    }
}

impl PartialOrd for Point {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Point {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Point {}

/// The values one covered field takes in a block. A row without the field
/// adds nothing; NaN (which no filter matches) is not counted either, so a
/// nonzero count always comes with its interval, booleans or dictionary.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FieldSummary {
    /// Values per type: string, integer, float, boolean, geo point.
    pub counts: [u32; 5],
    pub ints: Option<(i64, i64)>,
    pub floats: Option<(f64, f64)>,
    /// Bit 0: `false` seen; bit 1: `true` seen.
    pub bools: u8,
    pub strings: Dictionary<String>,
    pub points: Dictionary<Point>,
}

impl FieldSummary {
    fn add(&mut self, value: &MetadataValue) {
        match value {
            MetadataValue::String(value) => {
                self.counts[0] += 1;
                self.strings.insert(value.as_str());
            }
            MetadataValue::Integer(value) => {
                self.counts[1] += 1;
                self.ints = Some(self.ints.map_or((*value, *value), |(low, high)| {
                    (low.min(*value), high.max(*value))
                }));
            }
            MetadataValue::Float(value) if !value.is_nan() => {
                self.counts[2] += 1;
                let value = zero(*value);
                self.floats = Some(self.floats.map_or((value, value), |(low, high)| {
                    (low.min(value), high.max(value))
                }));
            }
            MetadataValue::Boolean(value) => {
                self.counts[3] += 1;
                self.bools |= 1 << u8::from(*value);
            }
            MetadataValue::GeoPoint {
                latitude,
                longitude,
            } if !latitude.is_nan() && !longitude.is_nan() => {
                self.counts[4] += 1;
                self.points.insert(&Point::new(*latitude, *longitude));
            }
            MetadataValue::Float(_) | MetadataValue::GeoPoint { .. } => {}
        }
    }

    /// Whether a row of this block may satisfy `condition` on this field,
    /// following `metadata::matches`: equality by type and value (signed
    /// zero equal, NaN never), ranges over values of their bounds' type.
    fn possible(&self, condition: &Condition) -> Status {
        let keep = |possible: bool| {
            if possible {
                Status::Keep
            } else {
                Status::Prune
            }
        };
        match condition {
            Condition::Eq { value, .. } => keep(match value {
                MetadataValue::String(value) => self.strings.may_contain(value.as_str()),
                MetadataValue::Integer(value) => self
                    .ints
                    .is_some_and(|(low, high)| (low..=high).contains(value)),
                MetadataValue::Float(value) => self
                    .floats
                    .is_some_and(|(low, high)| low <= *value && *value <= high),
                MetadataValue::Boolean(value) => self.bools & (1 << u8::from(*value)) != 0,
                MetadataValue::GeoPoint {
                    latitude,
                    longitude,
                } => self.points.may_contain(&Point::new(*latitude, *longitude)),
            }),
            Condition::Range { min, max, .. } => {
                let integer = |bound: &Option<MetadataValue>| match bound {
                    None => Some(None),
                    Some(MetadataValue::Integer(value)) => Some(Some(*value)),
                    Some(_) => None,
                };
                let float = |bound: &Option<MetadataValue>| match bound {
                    None => Some(None),
                    Some(MetadataValue::Float(value)) => Some(Some(*value)),
                    Some(_) => None,
                };
                if min.is_none() && max.is_none() {
                    Status::Unknown
                } else if let (Some(low), Some(high)) = (integer(min), integer(max)) {
                    let (low, high) = (low.unwrap_or(i64::MIN), high.unwrap_or(i64::MAX));
                    keep(
                        self.ints
                            .is_some_and(|(min, max)| min <= high && low <= max),
                    )
                } else if let (Some(low), Some(high)) = (float(min), float(max)) {
                    let low = low.unwrap_or(f64::NEG_INFINITY);
                    let high = high.unwrap_or(f64::INFINITY);
                    keep(
                        self.floats
                            .is_some_and(|(min, max)| min <= high && low <= max),
                    )
                } else {
                    // Mixed or non-numeric bounds: validation rejects them.
                    Status::Unknown
                }
            }
            Condition::GeoRadius { .. } => Status::Unknown,
        }
    }

    fn heap_bytes(&self) -> usize {
        let strings = match &self.strings {
            Dictionary::Values(values) => values
                .iter()
                .map(|value| size_of::<String>() + value.capacity())
                .sum(),
            Dictionary::Overflow => 0,
        };
        let points = match &self.points {
            Dictionary::Values(values) => values.capacity() * size_of::<Point>(),
            Dictionary::Overflow => 0,
        };
        strings + points
    }
}

/// The summary of one immutable block: its rows' ID span and count and one
/// `FieldSummary` per covered field (`names`, sorted and unique).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BlockSummary {
    pub first: u64,
    pub last: u64,
    pub rows: u32,
    pub names: Arc<[String]>,
    pub fields: Vec<FieldSummary>,
}

impl BlockSummary {
    /// Summarizes `rows` (public ID, metadata) over the covered `names`.
    /// An empty block has first = last = 0.
    pub(crate) fn build<'a>(
        names: &Arc<[String]>,
        rows: impl IntoIterator<Item = (u64, &'a Metadata)>,
    ) -> Self {
        let mut summary = Self {
            first: u64::MAX,
            last: 0,
            rows: 0,
            names: Arc::clone(names),
            fields: vec![FieldSummary::default(); names.len()],
        };
        for (id, metadata) in rows {
            summary.first = summary.first.min(id);
            summary.last = summary.last.max(id);
            summary.rows += 1;
            for (name, field) in names.iter().zip(&mut summary.fields) {
                if let Some(value) = metadata.get(name) {
                    field.add(value);
                }
            }
        }
        if summary.rows == 0 {
            summary.first = 0;
        }
        summary
    }

    /// Whether a row of the block may satisfy the conjunction: `Prune` as
    /// soon as one condition proves it cannot (or the block is empty),
    /// `Unknown` when a condition cannot be decided, else `Keep`.
    pub(crate) fn status(&self, conditions: &[Condition]) -> Status {
        if self.rows == 0 {
            return Status::Prune;
        }
        let mut status = Status::Keep;
        for condition in conditions {
            let field = field_of(condition);
            let decided = match self.names.binary_search_by(|name| name.as_str().cmp(field)) {
                Ok(index) => self.fields[index].possible(condition),
                Err(_) => Status::Unknown,
            };
            match decided {
                Status::Prune => return Status::Prune,
                Status::Unknown => status = Status::Unknown,
                Status::Keep => {}
            }
        }
        status
    }

    fn heap_bytes(&self) -> usize {
        size_of::<Self>()
            + self
                .fields
                .iter()
                .map(|field| size_of::<FieldSummary>() + field.heap_bytes())
                .sum::<usize>()
    }
}

/// A dense bitmap over row ordinals.
#[derive(Debug)]
pub(crate) struct Bitmap(Vec<u64>);

impl Bitmap {
    pub(crate) fn new(universe: usize) -> Self {
        Self(vec![0; universe.div_ceil(64)])
    }

    /// Sets `bit`; true when it was clear.
    pub(crate) fn insert(&mut self, bit: usize) -> bool {
        let (word, mask) = (bit / 64, 1 << (bit % 64));
        let clear = self.0[word] & mask == 0;
        self.0[word] |= mask;
        clear
    }

    pub(crate) fn contains(&self, bit: usize) -> bool {
        self.0[bit / 64] & (1 << (bit % 64)) != 0
    }
}

/// One generation: the IDs of its rows by ordinal, one summary per
/// `BLOCK_ROWS` ordinals and the rows superseded since (deleted, or with a
/// later revision in a newer generation or the overlay).
#[derive(Debug)]
pub(crate) struct Segment {
    pub ids: Vec<u64>,
    pub blocks: Vec<BlockSummary>,
    pub superseded: Bitmap,
    pub superseded_rows: usize,
}

impl Segment {
    pub(crate) fn new(ids: Vec<u64>, blocks: Vec<BlockSummary>) -> Self {
        debug_assert_eq!(blocks.len(), ids.len().div_ceil(BLOCK_ROWS));
        Self {
            superseded: Bitmap::new(ids.len()),
            superseded_rows: 0,
            ids,
            blocks,
        }
    }

    /// Supersedes the row of `id`, if this generation (in ascending ID
    /// order) holds one.
    fn supersede(&mut self, id: u64) {
        if let Ok(ordinal) = self.ids.binary_search(&id) {
            self.supersede_row(ordinal);
        }
    }

    /// Supersedes the row at `ordinal`; true when it was still visible.
    pub(crate) fn supersede_row(&mut self, ordinal: usize) -> bool {
        let fresh = self.superseded.insert(ordinal);
        self.superseded_rows += usize::from(fresh);
        fresh
    }
}

/// Generations from oldest to newest, and the IDs whose latest revision is
/// in none of them.
#[derive(Debug, Default)]
pub(crate) struct View {
    pub segments: Vec<Segment>,
    pub overlay: BTreeSet<u64>,
}

impl View {
    /// The IDs of every visible row in a block that may match, plus the
    /// overlay, and whether any block was pruned; `None` once more than
    /// `cap` IDs are collected. Visibility here is generational only: the
    /// caller re-verifies every candidate's latest revision, filter and
    /// expiry at the query clock.
    pub(crate) fn candidates(
        &self,
        conditions: &[Condition],
        cap: usize,
    ) -> Option<(BTreeSet<u64>, bool)> {
        let mut ids = BTreeSet::new();
        let mut pruned = false;
        for segment in &self.segments {
            for (index, block) in segment.blocks.iter().enumerate() {
                if block.status(conditions) == Status::Prune {
                    pruned = true;
                    continue;
                }
                let start = index * BLOCK_ROWS;
                let end = segment.ids.len().min(start + BLOCK_ROWS);
                for ordinal in start..end {
                    if segment.superseded.contains(ordinal) {
                        continue;
                    }
                    let id = segment.ids[ordinal];
                    debug_assert!((block.first..=block.last).contains(&id));
                    ids.insert(id);
                }
                if ids.len() > cap {
                    return None;
                }
            }
        }
        ids.extend(&self.overlay);
        (ids.len() <= cap).then_some((ids, pruned))
    }
}

/// Filter summaries of one collection over its declared index fields.
#[derive(Debug)]
pub(crate) struct Summaries {
    /// Covered fields, sorted and unique.
    names: Arc<[String]>,
    budget: usize,
    /// Derived lazily; reset to derive again.
    state: OnceLock<State>,
}

#[derive(Debug)]
enum State {
    Ready(View),
    /// Over `budget` at `rows` records: scan everything until `pending`
    /// changes pass `threshold(rows)`, then try again.
    OverBudget {
        rows: usize,
        pending: usize,
    },
}

impl Default for Summaries {
    fn default() -> Self {
        Self {
            names: Arc::from([]),
            budget: BUDGET_BYTES,
            state: OnceLock::new(),
        }
    }
}

impl Summaries {
    pub(crate) fn new(names: impl IntoIterator<Item = String>) -> Self {
        let mut names: Vec<String> = names.into_iter().collect();
        names.sort();
        names.dedup();
        Self {
            names: names.into(),
            ..Self::default()
        }
    }

    fn covers(&self, field: &str) -> bool {
        self.names
            .binary_search_by(|name| name.as_str().cmp(field))
            .is_ok()
    }

    /// A superset of the live `records` that may match `conditions`, when
    /// summaries prune at least one block and at most `cap` IDs remain;
    /// otherwise `None` (scan everything). Derives the summaries first if
    /// a condition other than a geo radius is on a covered field.
    pub(crate) fn candidates(
        &self,
        conditions: &[Condition],
        records: &RecordMap,
        cap: usize,
    ) -> Option<BTreeSet<u64>> {
        let prunable = conditions.iter().any(|condition| {
            !matches!(condition, Condition::GeoRadius { .. }) && self.covers(field_of(condition))
        });
        if !prunable {
            return None;
        }
        match self.state.get_or_init(|| self.derive(records)) {
            State::Ready(view) => view
                .candidates(conditions, cap)
                .and_then(|(ids, pruned)| pruned.then_some(ids)),
            State::OverBudget { .. } => None,
        }
    }

    /// One generation of `records` in ID order, or `OverBudget` (logged)
    /// when its IDs, bitmap and summaries would pass `budget`.
    fn derive(&self, records: &RecordMap) -> State {
        let rows = records.len();
        let over = |bytes: usize| {
            log::warn!(
                target: "vlite::filter",
                r#"{{"event":"filter_summary_budget","outcome":"exact_scan","rows":{rows},"min_bytes":{bytes},"budget":{}}}"#,
                self.budget
            );
            State::OverBudget { rows, pending: 0 }
        };
        let mut bytes = rows
            .saturating_mul(8)
            .saturating_add(rows.div_ceil(64).saturating_mul(8));
        if bytes > self.budget {
            return over(bytes);
        }
        let ids: Vec<u64> = records.keys().copied().collect();
        let mut blocks = Vec::with_capacity(rows.div_ceil(BLOCK_ROWS));
        let mut scan = records.iter();
        loop {
            let block = BlockSummary::build(
                &self.names,
                scan.by_ref()
                    .take(BLOCK_ROWS)
                    .map(|(&id, record)| (id, &record.metadata)),
            );
            if block.rows == 0 {
                break;
            }
            bytes = bytes.saturating_add(block.heap_bytes());
            if bytes > self.budget {
                return over(bytes);
            }
            blocks.push(block);
        }
        State::Ready(View {
            segments: vec![Segment::new(ids, blocks)],
            overlay: BTreeSet::new(),
        })
    }

    /// Applies one committed change (`current` None means delete), observed
    /// before the record is replaced. Changes to uncovered fields only keep
    /// the row in its block; others supersede it and overlay the latest
    /// revision. Past `threshold` the summaries are dropped and derived
    /// again by the next query that can use them.
    pub(crate) fn track(
        &mut self,
        id: u64,
        previous: Option<&Metadata>,
        current: Option<&Metadata>,
    ) {
        let Some(state) = self.state.get_mut() else {
            return;
        };
        let changed = match (previous, current) {
            (None, None) => false,
            (Some(previous), Some(current)) => self
                .names
                .iter()
                .any(|name| previous.get(name) != current.get(name)),
            _ => true,
        };
        if !changed {
            return;
        }
        let reset = match state {
            State::Ready(view) => {
                for segment in &mut view.segments {
                    segment.supersede(id);
                }
                if current.is_some() {
                    view.overlay.insert(id);
                } else {
                    view.overlay.remove(&id);
                }
                let rows = view.segments.iter().map(|segment| segment.ids.len()).sum();
                let churn = view
                    .segments
                    .iter()
                    .map(|segment| segment.superseded_rows)
                    .sum::<usize>()
                    + view.overlay.len();
                churn > threshold(rows)
            }
            State::OverBudget { rows, pending } => {
                *pending += 1;
                *pending > threshold(*rows)
            }
        };
        if reset {
            self.state = OnceLock::new();
        }
    }
}
