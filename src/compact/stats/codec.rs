//! Derived filter summary block: kind 0x0400, codec version 1 (P5).
//!
//! The block summarizes the rows of one immutable record block. It is
//! derived (flags 0, never critical): its header's source digest binds it
//! to the exact rows and covered fields it was built from, so a reader
//! serves it only when the digest, field list, row count and ID span match
//! the rows it holds; otherwise it rebuilds the summary from those rows. A
//! newer codec version is rebuilt without reading its sections. Anything
//! structurally wrong is `Corrupt`.
//!
//! Source digest: the first 8 bytes (little-endian `u64`) of the SHA-256 of
//! `b"vlite.p5.summary.v1\0"`, the field count (`u16`) and each covered
//! field (`u16` length, UTF-8), the row count (`u32`), then per row in
//! block order its public ID (`u64`), revision (`u64`), encoded metadata
//! length (`u32`) and `metadata::encode` bytes.
//!
//! Section 0, coverage: first and last public ID (`u64` each), rows (`u32`,
//! 1..=1024, equal to the frame count), field count (`u16`, 1..=16),
//! reserved `u16` 0, then each field name (`u16` length 1..=256, UTF-8),
//! strictly ascending. `first <= last` and the span holds every row.
//!
//! Section 1, one entry per field in name order: five `u32` value counts
//! (string, integer, float, boolean, geo point) summing to at most the rows,
//! then for each nonzero count in that order:
//! - strings: `u16` n, `0xFFFF` for an overflowed dictionary, else 1..=32
//!   (at most the count) values (`u16` length, UTF-8) strictly ascending
//!   with at most 1024 value bytes;
//! - integers: inclusive `i64` minimum and maximum;
//! - floats: inclusive finite `f64` minimum and maximum, never `-0.0`;
//! - booleans: one byte, bit 0 `false` seen, bit 1 `true` seen;
//! - points: `u16` n, `0xFFFF` only past 32 points, else 1..=32 (at most the
//!   count) points (`f64` latitude in [-90, 90], longitude in [-180, 180],
//!   finite, never `-0.0`) strictly ascending by total order.
//!
//! With one value, minimum and maximum are equal.

use super::{
    BLOCK_ROWS, BlockSummary, Dictionary, FieldSummary, MAX_DICTIONARY_BYTES,
    MAX_DICTIONARY_VALUES, Point,
};
use crate::compact::{Envelope, MAX_BODY, MIN_BODY, envelope};
use crate::format::{corrupt, u64_at};
use crate::{Metadata, Result};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// First kind of the P5 derived range 0x0400-0x04FF.
pub(crate) const KIND_SUMMARY: u16 = 0x0400;
const VERSION: u16 = 1;
const MAX_FIELDS: usize = 16;
const MAX_NAME: usize = 256;
const OVERFLOW: u16 = 0xFFFF;
const DOMAIN: &[u8] = b"vlite.p5.summary.v1\0";
const INVALID: &str = "invalid filter summary block";

/// One source row of a summarized block.
#[derive(Debug)]
pub(crate) struct Row<'a> {
    pub id: u64,
    pub revision: u64,
    pub metadata: &'a Metadata,
}

/// The source digest of `rows` summarized over `names`.
pub(crate) fn digest(names: &[String], rows: &[Row<'_>]) -> u64 {
    let mut hash = Sha256::new();
    hash.update(DOMAIN);
    hash.update((names.len() as u16).to_le_bytes());
    for name in names {
        hash.update((name.len() as u16).to_le_bytes());
        hash.update(name.as_bytes());
    }
    hash.update((rows.len() as u32).to_le_bytes());
    for row in rows {
        let encoded = crate::metadata::encode(row.metadata);
        hash.update(row.id.to_le_bytes());
        hash.update(row.revision.to_le_bytes());
        hash.update((encoded.len() as u32).to_le_bytes());
        hash.update(&encoded);
    }
    let digest = hash.finalize();
    let mut head = [0; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(head)
}

/// The version-1 block body of `summary` for `owner` and `partition`, bound
/// to its source by `digest`.
pub(crate) fn encode(owner: u16, partition: u64, summary: &BlockSummary, digest: u64) -> Vec<u8> {
    let mut coverage = Vec::new();
    coverage.extend_from_slice(&summary.first.to_le_bytes());
    coverage.extend_from_slice(&summary.last.to_le_bytes());
    coverage.extend_from_slice(&summary.rows.to_le_bytes());
    coverage.extend_from_slice(&(summary.names.len() as u16).to_le_bytes());
    coverage.extend_from_slice(&0u16.to_le_bytes());
    for name in summary.names.iter() {
        text(&mut coverage, name);
    }
    let mut fields = Vec::new();
    for field in &summary.fields {
        for count in field.counts {
            fields.extend_from_slice(&count.to_le_bytes());
        }
        let [strings, ints, floats, bools, points] = field.counts.map(|count| count > 0);
        if strings {
            dictionary(&mut fields, &field.strings, |bytes, value| {
                text(bytes, value)
            });
        }
        if ints {
            let (min, max) = field.ints.unwrap_or_default();
            fields.extend_from_slice(&min.to_le_bytes());
            fields.extend_from_slice(&max.to_le_bytes());
        }
        if floats {
            let (min, max) = field.floats.unwrap_or_default();
            fields.extend_from_slice(&min.to_le_bytes());
            fields.extend_from_slice(&max.to_le_bytes());
        }
        if bools {
            fields.push(field.bools);
        }
        if points {
            dictionary(&mut fields, &field.points, |bytes, point| {
                bytes.extend_from_slice(&point.latitude.to_le_bytes());
                bytes.extend_from_slice(&point.longitude.to_le_bytes());
            });
        }
    }
    let mut body = envelope(KIND_SUMMARY, owner, partition, &[&coverage, &fields], None);
    // Derived: not critical, and bound to its source by the digest.
    body[4..6].copy_from_slice(&0u16.to_le_bytes());
    body[16..24].copy_from_slice(&digest.to_le_bytes());
    body
}

fn text(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn dictionary<T>(bytes: &mut Vec<u8>, dictionary: &Dictionary<T>, put: impl Fn(&mut Vec<u8>, &T)) {
    match dictionary {
        Dictionary::Values(values) => {
            bytes.extend_from_slice(&(values.len() as u16).to_le_bytes());
            for value in values {
                put(bytes, value);
            }
        }
        Dictionary::Overflow => bytes.extend_from_slice(&OVERFLOW.to_le_bytes()),
    }
}

/// A summary block checked against the rows it claims to summarize.
#[derive(Debug)]
pub(crate) enum Bound {
    /// Authentic for exactly these rows and fields: may be served.
    Fresh(BlockSummary),
    /// Well formed but for other rows, revisions or fields: rebuild.
    Stale,
    /// A codec version this build does not read: rebuild.
    UnsupportedVersion,
}

/// Checks the summary block `body` of frame `id` (`count` rows) in
/// `collection` against the block's `rows` and the covered `names`.
pub(crate) fn bind(
    collection: u16,
    id: u64,
    count: u32,
    body: &[u8],
    names: &Arc<[String]>,
    rows: &[Row<'_>],
    offset: u64,
) -> Result<Bound> {
    if !(MIN_BODY..=MAX_BODY).contains(&body.len()) {
        return Err(corrupt(offset, "invalid compact block length"));
    }
    if id as u16 != KIND_SUMMARY {
        return Err(corrupt(offset, INVALID));
    }
    let block = Envelope::parse(collection, id, body, offset)?;
    if block.version != VERSION {
        return Ok(Bound::UnsupportedVersion);
    }
    let invalid = || corrupt(offset, INVALID);
    let &[coverage, fields] = block.sections.as_slice() else {
        return Err(invalid());
    };
    if block.flags != 0 {
        return Err(invalid());
    }
    let decoded = decode(coverage, fields, count).ok_or_else(invalid)?;
    let first = rows.iter().map(|row| row.id).min();
    let last = rows.iter().map(|row| row.id).max();
    let fresh = u64_at(body, 16) == digest(names, rows)
        && *decoded.names == **names
        && decoded.rows as usize == rows.len()
        && first == Some(decoded.first)
        && last == Some(decoded.last);
    if !fresh {
        return Ok(Bound::Stale);
    }
    Ok(Bound::Fresh(BlockSummary {
        names: Arc::clone(names),
        ..decoded
    }))
}

/// The block's summary: decoded when fresh, otherwise rebuilt from `rows`.
/// Corrupt blocks fail.
pub(crate) fn adopt_or_rebuild(
    collection: u16,
    id: u64,
    count: u32,
    body: &[u8],
    names: &Arc<[String]>,
    rows: &[Row<'_>],
    offset: u64,
) -> Result<BlockSummary> {
    Ok(
        match bind(collection, id, count, body, names, rows, offset)? {
            Bound::Fresh(summary) => summary,
            Bound::Stale | Bound::UnsupportedVersion => {
                BlockSummary::build(names, rows.iter().map(|row| (row.id, row.metadata)))
            }
        },
    )
}

/// Little-endian reads that fail past the end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(length)?;
        self.0 = tail;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    fn i64(&mut self) -> Option<i64> {
        self.array().map(i64::from_le_bytes)
    }

    fn f64(&mut self) -> Option<f64> {
        self.array().map(f64::from_le_bytes)
    }

    fn text(&mut self) -> Option<&'a str> {
        let length = usize::from(self.u16()?);
        std::str::from_utf8(self.take(length)?).ok()
    }

    fn end(&self) -> Option<()> {
        self.0.is_empty().then_some(())
    }
}

fn decode(coverage: &[u8], fields: &[u8], count: u32) -> Option<BlockSummary> {
    let mut at = Reader(coverage);
    let (first, last, rows) = (at.u64()?, at.u64()?, at.u32()?);
    let (width, reserved) = (usize::from(at.u16()?), at.u16()?);
    let valid = (1..=BLOCK_ROWS as u32).contains(&rows)
        && rows == count
        && (1..=MAX_FIELDS).contains(&width)
        && reserved == 0
        && first <= last
        && last - first >= u64::from(rows - 1);
    if !valid {
        return None;
    }
    let mut names: Vec<String> = Vec::with_capacity(width);
    for _ in 0..width {
        let name = at.text()?;
        if !(1..=MAX_NAME).contains(&name.len())
            || names.last().is_some_and(|last| last.as_str() >= name)
        {
            return None;
        }
        names.push(name.to_owned());
    }
    at.end()?;
    let mut at = Reader(fields);
    let fields = (0..width)
        .map(|_| field(&mut at, rows))
        .collect::<Option<Vec<_>>>()?;
    at.end()?;
    Some(BlockSummary {
        first,
        last,
        rows,
        names: names.into(),
        fields,
    })
}

fn field(at: &mut Reader<'_>, rows: u32) -> Option<FieldSummary> {
    let mut summary = FieldSummary::default();
    for count in &mut summary.counts {
        *count = at.u32()?;
    }
    if summary
        .counts
        .iter()
        .map(|&count| u64::from(count))
        .sum::<u64>()
        > u64::from(rows)
    {
        return None;
    }
    let [strings, ints, floats, bools, points] = summary.counts;
    if strings > 0 {
        summary.strings = values(at, strings, true, |at| {
            let value = at.text()?;
            Some((value.to_owned(), value.len()))
        })?;
    }
    if ints > 0 {
        let (min, max) = (at.i64()?, at.i64()?);
        if min > max || (ints == 1 && min != max) {
            return None;
        }
        summary.ints = Some((min, max));
    }
    if floats > 0 {
        let (min, max) = (at.f64()?, at.f64()?);
        if !canonical(min) || !canonical(max) || min > max || (floats == 1 && min != max) {
            return None;
        }
        summary.floats = Some((min, max));
    }
    if bools > 0 {
        let mask = at.u8()?;
        if !(1..=3).contains(&mask) || mask.count_ones() > bools {
            return None;
        }
        summary.bools = mask;
    }
    if points > 0 {
        let overflow = points as usize > MAX_DICTIONARY_VALUES;
        summary.points = values(at, points, overflow, |at| {
            let (latitude, longitude) = (at.f64()?, at.f64()?);
            let valid = canonical(latitude)
                && canonical(longitude)
                && (-90.0..=90.0).contains(&latitude)
                && (-180.0..=180.0).contains(&longitude);
            valid.then_some((
                Point {
                    latitude,
                    longitude,
                },
                16,
            ))
        })?;
    }
    Some(summary)
}

/// Finite and never `-0.0`.
fn canonical(value: f64) -> bool {
    value.is_finite() && value.to_bits() != (-0.0f64).to_bits()
}

/// A dictionary of at most `count` values read by `value` (with its
/// weight), or `Overflow` where `overflow` allows it.
fn values<T: Ord>(
    at: &mut Reader<'_>,
    count: u32,
    overflow: bool,
    mut value: impl FnMut(&mut Reader<'_>) -> Option<(T, usize)>,
) -> Option<Dictionary<T>> {
    let length = at.u16()?;
    if length == OVERFLOW {
        return overflow.then_some(Dictionary::Overflow);
    }
    let length = usize::from(length);
    if length == 0 || length > MAX_DICTIONARY_VALUES || length as u64 > u64::from(count) {
        return None;
    }
    let mut values: Vec<T> = Vec::with_capacity(length);
    let mut bytes = 0;
    for _ in 0..length {
        let (item, weight) = value(at)?;
        bytes += weight;
        if bytes > MAX_DICTIONARY_BYTES || values.last().is_some_and(|last| *last >= item) {
            return None;
        }
        values.push(item);
    }
    Some(Dictionary::Values(values))
}
