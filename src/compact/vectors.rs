//! P3a primary dense rows (`docs/projects/roadmap/p3-compact-vectors.md`):
//! the stored precision, canonical float32 to float16 conversion and the
//! source-column block of kind `KIND_DENSE`.
//!
//! Preactivation: this module is compiled for tests only. No reader accepts
//! the kind and no writer emits it until the complete write, read, replay
//! and maintenance paths are qualified, so release builds still refuse it
//! with `UnsupportedFeature(FLAG_COMPACT)`.
//!
//! A block holds, for up to `Layout::max_rows` records of one collection in
//! ascending ID order, their IDs, revisions, optional expiry and primary
//! dense vector, and nothing else. Payloads, metadata, named and sparse
//! vectors stay in other frames: these blocks alone never reconstruct a
//! collection.

use super::{CRITICAL, Envelope, HEADER_LEN, MAX_BODY, MIN_BODY};
use crate::format::{FLAG_COMPACT, corrupt, u16_at, u32_at, u64_at};
use crate::{Error, MAX_DIMENSIONS, Result};

/// Primary dense rows, the first P3 source kind. Version 1 is the shared
/// envelope with six sections, each 64-byte aligned in the file when the
/// writer knows the body's offset (alignment is advisory), little-endian:
///
/// 0. Descriptor, 8 bytes: dimensions (`u32`), precision (`u8`: 1 float32,
///    2 float16) and three zero bytes. Both must match the collection.
/// 1. IDs: a `u64` per row, strictly ascending.
/// 2. Revisions: a `u64` per row, each in `1..=S`, the checkpoint.
/// 3. Expiring rows: `u32` row numbers, strictly ascending, below the count.
/// 4. Deadlines: a `u64` per expiring row.
/// 5. Components: row-major float32 or float16 values, all finite.
///
/// The frame `count` is the number of rows, `1..=Layout::max_rows()`.
pub(crate) const KIND_DENSE: u16 = 0x0100;
const VERSION: u16 = 1;
const SECTIONS: usize = 6;
/// A block holds at most this many rows, and more than one row only while
/// their components fit `BLOCK_COMPONENTS` bytes, so an encrypted block
/// opens into about 1 MiB, like a P2 record block.
const MAX_ROWS: usize = 4096;
const BLOCK_COMPONENTS: usize = 1 << 20;
const ALIGN: u64 = 64;

/// Stored precision of a collection's dense vectors. float32 is the default
/// and keeps every input bit; float16 is an explicit, lossy choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Precision {
    #[default]
    Float32,
    Float16,
}

/// Why a float32 value has no stored value in a precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComponentError {
    /// Infinity or NaN, refused in every precision.
    NonFinite,
    /// Finite, but rounds to a float16 infinity.
    Overflow,
}

impl Precision {
    /// The value stored for `value`: `value` itself in float32, the nearest
    /// float16 (ties to even) otherwise.
    pub(crate) fn canonical(self, value: f32) -> std::result::Result<f32, ComponentError> {
        match self {
            Self::Float32 if value.is_finite() => Ok(value),
            Self::Float32 => Err(ComponentError::NonFinite),
            Self::Float16 => to_half(value).map(from_half),
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Float32 => 1,
            Self::Float16 => 2,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Float32),
            2 => Some(Self::Float16),
            _ => None,
        }
    }

    /// Bytes per stored component.
    fn width(self) -> usize {
        match self {
            Self::Float32 => 4,
            Self::Float16 => 2,
        }
    }

    /// Appends the stored bytes of `value` if it is exactly a stored value.
    fn store(self, value: f32, out: &mut Vec<u8>) -> bool {
        match self {
            Self::Float32 if value.is_finite() => out.extend_from_slice(&value.to_le_bytes()),
            Self::Float16 => match to_half(value) {
                Ok(half) if from_half(half).to_bits() == value.to_bits() => {
                    out.extend_from_slice(&half.to_le_bytes())
                }
                _ => return false,
            },
            Self::Float32 => return false,
        }
        true
    }
}

/// The float16 bits nearest to `value`, ties to even. Signed zero and
/// subnormals are kept; anything that would round to infinity is refused.
pub(crate) fn to_half(value: f32) -> std::result::Result<u16, ComponentError> {
    if !value.is_finite() {
        return Err(ComponentError::NonFinite);
    }
    let bits = value.to_bits();
    let magnitude = bits & 0x7fff_ffff;
    let exponent = magnitude >> 23;
    let half = match exponent {
        // Below half the smallest subnormal, 2^-25: zero.
        ..102 => 0,
        // Subnormal: the significand in units of 2^-24.
        102..113 => round_shift(magnitude & 0x7f_ffff | 0x80_0000, 126 - exponent),
        // Normal: rebias the exponent from 127 to 15. A carry out of the
        // significand correctly bumps the exponent.
        _ => round_shift(magnitude - (112 << 23), 13),
    };
    if half >= 0x7c00 {
        return Err(ComponentError::Overflow);
    }
    Ok((bits >> 16) as u16 & 0x8000 | half as u16)
}

/// `value >> shift`, rounded to nearest, ties to even; `shift` is 13 to 24.
fn round_shift(value: u32, shift: u32) -> u32 {
    let kept = value >> shift;
    let rest = value & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    kept + u32::from(rest > half || (rest == half && kept & 1 == 1))
}

/// The exact float32 value of float16 `bits`.
pub(crate) fn from_half(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = u32::from(bits >> 10 & 0x1f);
    let mantissa = u32::from(bits & 0x3ff);
    let magnitude = match exponent {
        // Subnormal: mantissa * 2^-24, exact in float32.
        0 => (f32::from(bits & 0x3ff) * f32::from_bits(0x3380_0000)).to_bits(),
        0x1f => 0x7f80_0000 | mantissa << 13,
        _ => (exponent + 112) << 23 | mantissa << 13,
    };
    f32::from_bits(sign | magnitude)
}

/// What a collection's blocks store per row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub dimensions: usize,
    pub precision: Precision,
}

impl Layout {
    /// Rows per full block: up to 1 MiB of components, at least one row and
    /// at most 4,096.
    pub(crate) fn max_rows(self) -> usize {
        let row = self.dimensions.saturating_mul(self.precision.width());
        (BLOCK_COMPONENTS / row.max(1)).clamp(1, MAX_ROWS)
    }
}

/// One live record's columns, with a vector already canonical for the
/// collection's precision.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Row<'a> {
    pub id: u64,
    pub revision: u64,
    pub expiry: Option<u64>,
    pub vector: &'a [f32],
}

/// Encodes the body of block `partition` of `collection`, whose first byte
/// is at file offset `body_offset`. Refuses rows a block cannot hold
/// exactly: a layout outside a collection's 1 to 65,536 dimensions, a count
/// outside `1..=layout.max_rows()`, IDs that do not ascend, other
/// dimensions, or a component that is not already its stored value.
/// Rounding happens before a commit, never at a checkpoint.
pub(crate) fn encode(
    collection: u16,
    partition: u32,
    layout: Layout,
    rows: &[Row<'_>],
    body_offset: u64,
) -> std::result::Result<Vec<u8>, &'static str> {
    let Layout {
        dimensions,
        precision,
    } = layout;
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err("dense rows need 1 to 65,536 dimensions");
    }
    if !(1..=layout.max_rows()).contains(&rows.len()) {
        return Err("dense block row count out of range");
    }
    if rows.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return Err("dense rows must ascend by ID");
    }
    let mut components = Vec::with_capacity(rows.len() * dimensions * precision.width());
    for row in rows {
        if row.vector.len() != dimensions {
            return Err("dense row has other dimensions");
        }
        for &value in row.vector {
            if !precision.store(value, &mut components) {
                return Err("dense component is not stored exactly in the precision");
            }
        }
    }
    let mut descriptor = [0; 8];
    descriptor[..4].copy_from_slice(&(dimensions as u32).to_le_bytes());
    descriptor[4] = precision.code();
    let ids: Vec<u8> = rows.iter().flat_map(|row| row.id.to_le_bytes()).collect();
    let revisions: Vec<u8> = rows
        .iter()
        .flat_map(|row| row.revision.to_le_bytes())
        .collect();
    // At most `MAX_ROWS` rows, so every row number fits a u32.
    let expiring: Vec<u8> = (0u32..)
        .zip(rows)
        .filter(|(_, row)| row.expiry.is_some())
        .flat_map(|(number, _)| number.to_le_bytes())
        .collect();
    let deadlines: Vec<u8> = rows
        .iter()
        .filter_map(|row| row.expiry)
        .flat_map(u64::to_le_bytes)
        .collect();
    let sections: [&[u8]; SECTIONS] = [
        &descriptor,
        &ids,
        &revisions,
        &expiring,
        &deadlines,
        &components,
    ];
    Ok(envelope(collection, partition, &sections, body_offset))
}

/// A critical version-1 block body for `KIND_DENSE` whose sections each
/// start on a 64-byte file boundary, given the body's file offset.
fn envelope(
    collection: u16,
    partition: u32,
    sections: &[&[u8]; SECTIONS],
    body_offset: u64,
) -> Vec<u8> {
    let mut body = Vec::new();
    for field in [KIND_DENSE, VERSION, CRITICAL, SECTIONS as u16] {
        body.extend_from_slice(&field.to_le_bytes());
    }
    let object = u64::from(collection) | u64::from(partition) << 16;
    body.extend_from_slice(&object.to_le_bytes());
    // No source digest; the first section's offset is filled in below.
    body.resize(HEADER_LEN + 8 * SECTIONS, 0);
    for (index, section) in sections.iter().enumerate() {
        let padding = body_offset.wrapping_add(body.len() as u64).wrapping_neg() % ALIGN;
        body.resize(body.len() + padding as usize, 0);
        // A block is far below 4 GiB: `max_rows` bounds its components.
        let start = (body.len() as u32).to_le_bytes();
        let entry = HEADER_LEN + 8 * index;
        body[entry..entry + 4].copy_from_slice(&start);
        body[entry + 4..entry + 8].copy_from_slice(&(section.len() as u32).to_le_bytes());
        if index == 0 {
            body[24..28].copy_from_slice(&start);
        }
        body.extend_from_slice(section);
    }
    body
}

/// The checked rows of a block, borrowed from its verified body.
pub(crate) struct Rows<'a> {
    layout: Layout,
    ids: &'a [u8],
    revisions: &'a [u8],
    expiring: &'a [u8],
    deadlines: &'a [u8],
    components: &'a [u8],
}

impl<'a> Rows<'a> {
    pub(crate) fn len(&self) -> usize {
        self.ids.len() / 8
    }

    pub(crate) fn id(&self, row: usize) -> u64 {
        u64_at(self.ids, 8 * row)
    }

    pub(crate) fn revision(&self, row: usize) -> u64 {
        u64_at(self.revisions, 8 * row)
    }

    pub(crate) fn expiry(&self, row: usize) -> Option<u64> {
        let (numbers, _) = self.expiring.as_chunks::<4>();
        let index = numbers
            .binary_search_by_key(&row, |number| u32::from_le_bytes(*number) as usize)
            .ok()?;
        Some(u64_at(self.deadlines, 8 * index))
    }

    /// The stored components of `row`, as float32.
    pub(crate) fn vector(&self, row: usize) -> impl Iterator<Item = f32> + 'a {
        let Layout {
            dimensions,
            precision,
        } = self.layout;
        let width = precision.width();
        let bytes = &self.components[row * dimensions * width..][..dimensions * width];
        bytes
            .chunks_exact(width)
            .map(move |component| match precision {
                Precision::Float32 => f32::from_bits(u32_at(component, 0)),
                Precision::Float16 => from_half(u16_at(component, 0)),
            })
    }
}

/// Checks the verified body of the op-16 frame (`collection`, `id`, `count`)
/// at `offset` as dense rows of a collection with `layout`, restated by the
/// checkpoint `floor`. Every length and count is checked, with checked
/// arithmetic, against the body before it is read; nothing is allocated but
/// the envelope's section list. Only version 1 is supported.
pub(crate) fn check<'a>(
    collection: u16,
    id: u64,
    count: u32,
    body: &'a [u8],
    layout: Layout,
    floor: u64,
    offset: u64,
) -> Result<Rows<'a>> {
    if !(MIN_BODY..=MAX_BODY).contains(&body.len()) {
        return Err(corrupt(offset, "invalid compact block length"));
    }
    let invalid = || corrupt(offset, "invalid dense rows block");
    if id as u16 != KIND_DENSE {
        return Err(invalid());
    }
    let block = Envelope::parse(collection, id, body, offset)?;
    if block.version != VERSION {
        return Err(Error::UnsupportedFeature(FLAG_COMPACT));
    }
    let Ok([descriptor, ids, revisions, expiring, deadlines, components]) =
        <[&[u8]; SECTIONS]>::try_from(block.sections)
    else {
        return Err(invalid());
    };
    if block.flags != CRITICAL {
        return Err(invalid());
    }
    let rows = count as usize;
    if !(1..=layout.max_rows()).contains(&rows) {
        return Err(corrupt(offset, "invalid dense rows count"));
    }
    let &[a, b, c, d, code, 0, 0, 0] = descriptor else {
        return Err(corrupt(offset, "invalid dense rows descriptor"));
    };
    let Some(precision) = Precision::from_code(code) else {
        return Err(corrupt(offset, "invalid dense rows descriptor"));
    };
    let dimensions = u32::from_le_bytes([a, b, c, d]) as usize;
    if (Layout {
        dimensions,
        precision,
    }) != layout
    {
        return Err(corrupt(offset, "dense rows do not match their collection"));
    }
    // `rows` is at most `MAX_ROWS`, so only the component length can
    // overflow; `expiring` holds whole row numbers, no more than rows.
    let numbers = expiring.len() / 4;
    if ids.len() != 8 * rows
        || revisions.len() != 8 * rows
        || expiring.len() % 4 != 0
        || numbers > rows
        || deadlines.len() != 8 * numbers
        || rows
            .checked_mul(dimensions)
            .and_then(|values| values.checked_mul(precision.width()))
            != Some(components.len())
    {
        return Err(corrupt(
            offset,
            "dense rows section lengths do not match the row count",
        ));
    }
    let (keys, _) = ids.as_chunks::<8>();
    if keys
        .windows(2)
        .any(|pair| u64::from_le_bytes(pair[0]) >= u64::from_le_bytes(pair[1]))
    {
        return Err(corrupt(offset, "dense rows do not ascend by ID"));
    }
    let (stamps, _) = revisions.as_chunks::<8>();
    if stamps
        .iter()
        .any(|stamp| !(1..=floor).contains(&u64::from_le_bytes(*stamp)))
    {
        return Err(corrupt(offset, "dense row revision outside the checkpoint"));
    }
    let (numbers, _) = expiring.as_chunks::<4>();
    let mut next = 0;
    for number in numbers {
        let number = u32::from_le_bytes(*number) as usize;
        if number < next || number >= rows {
            return Err(corrupt(offset, "invalid dense row expiry"));
        }
        next = number + 1;
    }
    let finite = match precision {
        Precision::Float32 => components
            .as_chunks::<4>()
            .0
            .iter()
            .all(|value| f32::from_le_bytes(*value).is_finite()),
        Precision::Float16 => components
            .as_chunks::<2>()
            .0
            .iter()
            .all(|value| u16::from_le_bytes(*value) & 0x7c00 != 0x7c00),
    };
    if !finite {
        return Err(corrupt(offset, "non-finite stored vector component"));
    }
    Ok(Rows {
        layout,
        ids,
        revisions,
        expiring,
        deadlines,
        components,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::tests::{FLOOR, block, frame};
    use crate::compact::{KIND_END, KIND_TABLE};
    use crate::crypto::{Cipher, EncryptionKey, SEAL_OVERHEAD};
    use crate::format::{self, FLAG_HISTORY, FOOTER_SIZE, PREFIX_SIZE};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;
    use std::io::Cursor;

    const LAYOUT16: Layout = Layout {
        dimensions: 3,
        precision: Precision::Float16,
    };
    const BLOCK: &str = "invalid dense rows block";
    const COUNT: &str = "invalid dense rows count";
    const DESCRIPTOR_REASON: &str = "invalid dense rows descriptor";
    const MISMATCH: &str = "dense rows do not match their collection";
    const LENGTHS: &str = "dense rows section lengths do not match the row count";
    const ORDER: &str = "dense rows do not ascend by ID";
    const REVISION: &str = "dense row revision outside the checkpoint";
    const EXPIRY: &str = "invalid dense row expiry";
    const NON_FINITE: &str = "non-finite stored vector component";

    /// One case of `tests/fixtures/compact/float16-cases.json`, which
    /// `tests/compact_float16_oracle.py` verifies independently: the input
    /// bits, then the float16 and canonical float32 bits or the refusal.
    struct Case {
        name: String,
        input: u32,
        expected: std::result::Result<(u16, u32), ComponentError>,
    }

    fn cases() -> Vec<Case> {
        let document: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/compact/float16-cases.json"
        ))
        .unwrap();
        assert_eq!(document["version"], 1);
        let bits = |case: &serde_json::Value, key: &str| {
            u32::from_str_radix(case[key].as_str().unwrap(), 16).unwrap()
        };
        let cases = document["cases"].as_array().unwrap().iter();
        cases
            .map(|case| Case {
                name: case["name"].as_str().unwrap().to_owned(),
                input: bits(case, "input_f32_bits"),
                expected: match case.get("error").map(|error| error.as_str().unwrap()) {
                    None => Ok((
                        u16::try_from(bits(case, "expected_f16_bits")).unwrap(),
                        bits(case, "canonical_f32_bits"),
                    )),
                    Some("non_finite") => Err(ComponentError::NonFinite),
                    Some("overflow") => Err(ComponentError::Overflow),
                    Some(other) => panic!("unknown fixture error {other}"),
                },
            })
            .collect()
    }

    /// Decoded rows: ID, revision, expiry and component bits.
    type Decoded = Vec<(u64, u64, Option<u64>, Vec<u32>)>;

    fn decoded(rows: &Rows<'_>) -> Decoded {
        let row = |row| {
            let vector = rows.vector(row).map(f32::to_bits).collect();
            (rows.id(row), rows.revision(row), rows.expiry(row), vector)
        };
        (0..rows.len()).map(row).collect()
    }

    fn expected(rows: &[Row<'_>]) -> Decoded {
        let row = |row: &Row<'_>| {
            let vector = row.vector.iter().map(|value| value.to_bits()).collect();
            (row.id, row.revision, row.expiry, vector)
        };
        rows.iter().map(row).collect()
    }

    /// The frame ID of block `partition`.
    fn id(partition: u32) -> u64 {
        u64::from(KIND_DENSE) | u64::from(partition) << 16
    }

    /// Checks block `partition` of collection 1 at the checkpoint `FLOOR`.
    fn read(body: &[u8], partition: u32, count: u32, layout: Layout) -> Result<Rows<'_>> {
        check(1, id(partition), count, body, layout, FLOOR, 0)
    }

    fn reason(result: Result<Rows<'_>>) -> &'static str {
        match result {
            Err(Error::Corrupt { reason, .. }) => reason,
            Err(error) => panic!("expected Corrupt, got {error:?}"),
            Ok(rows) => panic!("expected Corrupt, got {} rows", rows.len()),
        }
    }

    fn le(values: &[u64]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn halves(values: &[u16]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// All 80 shared goldens, through the conversion both ways and through
    /// each precision's stored value.
    #[test]
    fn float16_conversion_matches_every_shared_golden() {
        let cases = cases();
        assert_eq!(cases.len(), 80);
        let inputs: BTreeSet<u32> = cases.iter().map(|case| case.input).collect();
        assert_eq!(inputs.len(), cases.len());
        let bits = |value: std::result::Result<f32, ComponentError>| value.map(f32::to_bits);
        for case in &cases {
            let (name, input) = (&case.name, f32::from_bits(case.input));
            // float32 keeps every finite input bit for bit.
            let float32 = match input.is_finite() {
                true => Ok(case.input),
                false => Err(ComponentError::NonFinite),
            };
            assert_eq!(bits(Precision::Float32.canonical(input)), float32, "{name}");
            let float16 = case.expected.map(|(_, canonical)| canonical);
            assert_eq!(bits(Precision::Float16.canonical(input)), float16, "{name}");
            assert_eq!(
                to_half(input),
                case.expected.map(|(half, _)| half),
                "{name}"
            );
            if let Ok((half, canonical)) = case.expected {
                assert_eq!(from_half(half).to_bits(), canonical, "{name}");
            }
        }
    }

    /// Every finite float16 survives both conversions unchanged, and
    /// between any two neighbours the exact midpoint rounds to the even one
    /// while the float32 values beside it round to the nearer one.
    #[test]
    fn every_finite_half_round_trips_and_ties_round_to_even() {
        for bits in (0..=u16::MAX).filter(|bits| bits & 0x7c00 != 0x7c00) {
            assert_eq!(to_half(from_half(bits)), Ok(bits), "{bits:04x}");
        }
        for low in 0..0x7bff_u16 {
            let high = low + 1;
            let middle = ((f64::from(from_half(low)) + f64::from(from_half(high))) / 2.0) as f32;
            let even = if low & 1 == 0 { low } else { high };
            for sign in [0, 0x8000] {
                let signed = |bits: u32| f32::from_bits(bits | u32::from(sign) << 16);
                let at = middle.to_bits();
                assert_eq!(to_half(signed(at)), Ok(even | sign), "{low:04x}");
                assert_eq!(to_half(signed(at - 1)), Ok(low | sign), "{low:04x}");
                assert_eq!(to_half(signed(at + 1)), Ok(high | sign), "{low:04x}");
            }
        }
        assert_eq!(to_half(65519.996), Ok(0x7bff));
        assert_eq!(to_half(65520.0), Err(ComponentError::Overflow));
    }

    /// float32 stays the default and keeps every bit: signed zero,
    /// subnormals, extremes, values float16 cannot hold, extreme IDs and
    /// expiry values, at every file alignment of the body. Every section
    /// starts 64-byte aligned, and encoding is deterministic.
    #[test]
    fn float32_rows_round_trip_bit_exactly_at_every_alignment() {
        assert_eq!(Precision::default(), Precision::Float32);
        let layout = Layout {
            dimensions: 4,
            precision: Precision::Float32,
        };
        let vectors = [
            [-0.0, f32::from_bits(1), f32::MAX, f32::MIN],
            [
                0.1,
                -f32::MIN_POSITIVE,
                f32::from_bits(0x007f_ffff),
                65520.0,
            ],
            [1.0 / 3.0, -1e-30, f32::EPSILON, -2.5],
        ];
        let rows = [
            Row {
                id: 0,
                revision: 1,
                expiry: Some(0),
                vector: &vectors[0],
            },
            Row {
                id: 5,
                revision: FLOOR,
                expiry: None,
                vector: &vectors[1],
            },
            Row {
                id: u64::MAX,
                revision: 2,
                expiry: Some(u64::MAX),
                vector: &vectors[2],
            },
        ];
        for body_offset in 0..64 {
            let body = encode(1, 7, layout, &rows, body_offset).unwrap();
            assert_eq!(encode(1, 7, layout, &rows, body_offset).unwrap(), body);
            for section in 0..SECTIONS {
                let start = u32_at(&body, HEADER_LEN + 8 * section);
                assert_eq!((body_offset + u64::from(start)) % 64, 0, "{body_offset}");
            }
            let checked = read(&body, 7, 3, layout).unwrap();
            assert_eq!(decoded(&checked), expected(&rows), "{body_offset}");
        }
    }

    /// float16 rows keep every accepted golden's canonical value bit for
    /// bit, while float32 rows keep the raw inputs. The encoder refuses any
    /// value that is not already stored exactly, so a checkpoint never
    /// rounds, overflows or hides a non-finite value.
    #[test]
    fn float16_rows_store_only_canonical_values() {
        let cases = cases();
        let accepted: Vec<(u32, u32)> = cases
            .iter()
            .filter_map(|case| Some((case.input, case.expected.ok()?.1)))
            .collect();
        assert_eq!(accepted.len(), 62);
        let canonical: Vec<f32> = accepted
            .iter()
            .map(|&(_, bits)| f32::from_bits(bits))
            .collect();
        let negated: Vec<f32> = canonical.iter().map(|value| -value).collect();
        let layout = Layout {
            dimensions: canonical.len(),
            precision: Precision::Float16,
        };
        let first = Row {
            id: 1,
            revision: 1,
            expiry: None,
            vector: &canonical,
        };
        let rows = [
            first,
            Row {
                id: 2,
                revision: 2,
                expiry: Some(9),
                vector: &negated,
            },
        ];
        let body = encode(1, 0, layout, &rows, 0).unwrap();
        assert_eq!(
            decoded(&read(&body, 0, 2, layout).unwrap()),
            expected(&rows)
        );

        let inputs: Vec<f32> = accepted
            .iter()
            .map(|&(bits, _)| f32::from_bits(bits))
            .collect();
        let float32 = Layout {
            precision: Precision::Float32,
            ..layout
        };
        let raw = [Row {
            vector: &inputs,
            ..first
        }];
        let body = encode(1, 0, float32, &raw, 0).unwrap();
        assert_eq!(
            decoded(&read(&body, 0, 1, float32).unwrap()),
            expected(&raw)
        );

        for case in &cases {
            let stored = matches!(case.expected, Ok((_, bits)) if bits == case.input);
            let mut vector = canonical.clone();
            vector[0] = f32::from_bits(case.input);
            let row = [Row {
                vector: &vector,
                ..first
            }];
            let result = encode(1, 0, layout, &row, 0).map(drop);
            let refused = Err("dense component is not stored exactly in the precision");
            assert_eq!(
                result,
                if stored { Ok(()) } else { refused },
                "{}",
                case.name
            );
        }
    }

    /// The golden body of two float16 rows of collection 1, partition 2, at
    /// file offset 36 (after the prefix of a 64-byte aligned frame), written
    /// field by field without the encoder: zero padding, then hex bytes.
    const GOLDEN: &[(usize, &str)] = &[
        // Kind 0x0100, version 1, critical, six sections.
        (0, "0001 0100 0100 0600"),
        // Object: collection 1, partition 2. No source digest.
        (0, "0100 0200 0000 0000"),
        (0, "0000 0000 0000 0000"),
        // First section at 92, then reserved zero.
        (0, "5c00 0000 0000 0000"),
        // Sections (offset, length): descriptor, IDs, revisions, expiring
        // rows, deadlines, components. 36 + offset is a multiple of 64.
        (0, "5c00 0000 0800 0000"),
        (0, "9c00 0000 1000 0000"),
        (0, "dc00 0000 1000 0000"),
        (0, "1c01 0000 0400 0000"),
        (0, "5c01 0000 0800 0000"),
        (0, "9c01 0000 0c00 0000"),
        // Three dimensions, float16.
        (12, "0300 0000 02 000000"),
        // IDs 7 and 9.
        (56, "0700 0000 0000 0000 0900 0000 0000 0000"),
        // Revisions 3 and 4.
        (48, "0300 0000 0000 0000 0400 0000 0000 0000"),
        // Row 1 expires at 0x0102030405060708.
        (48, "0100 0000"),
        (60, "0807 0605 0403 0201"),
        // 1, -0, 65504, then 2^-24, -0.5, 1.5.
        (56, "003c 0080 ff7b 0100 00b8 003e"),
    ];

    fn golden() -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(padding, hex) in GOLDEN {
            bytes.resize(bytes.len() + padding, 0);
            let digits: Vec<u8> = hex.bytes().filter(u8::is_ascii_hexdigit).collect();
            bytes.extend(
                digits.chunks(2).map(|pair| {
                    u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
                }),
            );
        }
        bytes
    }

    fn golden_rows<'a>(vectors: &'a [[f32; 3]; 2]) -> [Row<'a>; 2] {
        [
            Row {
                id: 7,
                revision: 3,
                expiry: None,
                vector: &vectors[0],
            },
            Row {
                id: 9,
                revision: 4,
                expiry: Some(0x0102_0304_0506_0708),
                vector: &vectors[1],
            },
        ]
    }

    const GOLDEN_VECTORS: [[f32; 3]; 2] = [[1.0, -0.0, 65504.0], [5.960_464_5e-8, -0.5, 1.5]];

    #[test]
    fn golden_float16_block_matches_its_independent_encoding() {
        let golden = golden();
        assert_eq!(golden.len(), 424);
        let rows = golden_rows(&GOLDEN_VECTORS);
        assert_eq!(rows[1].vector[0].to_bits(), 0x3380_0000);
        assert_eq!(encode(1, 2, LAYOUT16, &rows, 36).unwrap(), golden);
        assert_eq!(
            decoded(&read(&golden, 2, 2, LAYOUT16).unwrap()),
            expected(&rows)
        );
    }

    /// A body for collection 1, partition 2 with unpadded sections,
    /// assembled without the encoder.
    fn raw(version: u16, flags: u16, sections: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for field in [KIND_DENSE, version, flags, sections.len() as u16] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        body.extend_from_slice(&(1u64 | 2 << 16).to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        let mut start = HEADER_LEN + 8 * sections.len();
        for field in [start, 0] {
            body.extend_from_slice(&(field as u32).to_le_bytes());
        }
        for section in sections {
            for field in [start, section.len()] {
                body.extend_from_slice(&(field as u32).to_le_bytes());
            }
            start += section.len();
        }
        sections
            .iter()
            .for_each(|section| body.extend_from_slice(section));
        body
    }

    /// The golden rows' sections, unpadded.
    fn golden_sections() -> Vec<Vec<u8>> {
        vec![
            vec![3, 0, 0, 0, 2, 0, 0, 0],
            le(&[7, 9]),
            le(&[3, 4]),
            1u32.to_le_bytes().to_vec(),
            le(&[0x0102_0304_0506_0708]),
            halves(&[0x3c00, 0x8000, 0x7bff, 0x0001, 0xb800, 0x3e00]),
        ]
    }

    #[test]
    fn malformed_blocks_are_corrupt() {
        let sections = golden_sections();
        let valid = raw(1, CRITICAL, &sections);
        // Padding is optional: alignment is advisory.
        let rows = golden_rows(&GOLDEN_VECTORS);
        assert_eq!(
            decoded(&read(&valid, 2, 2, LAYOUT16).unwrap()),
            expected(&rows)
        );
        let with = |index: usize, bytes: Vec<u8>| {
            let mut sections = sections.clone();
            sections[index] = bytes;
            raw(1, CRITICAL, &sections)
        };
        let expiring = |rows: &[u32], deadlines: &[u64]| {
            let mut sections = sections.clone();
            sections[3] = rows.iter().flat_map(|row| row.to_le_bytes()).collect();
            sections[4] = le(deadlines);
            raw(1, CRITICAL, &sections)
        };
        let cases: Vec<(&str, Vec<u8>, u32, &str)> = vec![
            (
                "short body",
                valid[..MIN_BODY - 1].to_vec(),
                2,
                "invalid compact block length",
            ),
            ("derived flag", raw(1, 0, &sections), 2, BLOCK),
            ("five sections", raw(1, CRITICAL, &sections[..5]), 2, BLOCK),
            ("no rows", valid.clone(), 0, COUNT),
            ("more than 4096 rows", valid.clone(), 4097, COUNT),
            ("count beyond the rows", valid.clone(), 3, LENGTHS),
            (
                "short descriptor",
                with(0, vec![3, 0, 0, 0, 2, 0, 0]),
                2,
                DESCRIPTOR_REASON,
            ),
            (
                "no precision",
                with(0, vec![3, 0, 0, 0, 0, 0, 0, 0]),
                2,
                DESCRIPTOR_REASON,
            ),
            (
                "unknown precision",
                with(0, vec![3, 0, 0, 0, 3, 0, 0, 0]),
                2,
                DESCRIPTOR_REASON,
            ),
            (
                "reserved byte",
                with(0, vec![3, 0, 0, 0, 2, 0, 1, 0]),
                2,
                DESCRIPTOR_REASON,
            ),
            (
                "other dimensions",
                with(0, vec![4, 0, 0, 0, 2, 0, 0, 0]),
                2,
                MISMATCH,
            ),
            (
                "other precision",
                with(0, vec![3, 0, 0, 0, 1, 0, 0, 0]),
                2,
                MISMATCH,
            ),
            ("short IDs", with(1, le(&[7])), 2, LENGTHS),
            ("duplicate IDs", with(1, le(&[7, 7])), 2, ORDER),
            ("descending IDs", with(1, le(&[9, 7])), 2, ORDER),
            ("long revisions", with(2, le(&[3, 4, 4])), 2, LENGTHS),
            ("zero revision", with(2, le(&[0, 4])), 2, REVISION),
            (
                "revision after the checkpoint",
                with(2, le(&[3, FLOOR + 1])),
                2,
                REVISION,
            ),
            ("partial expiring row", with(3, vec![1, 0, 0]), 2, LENGTHS),
            (
                "deadline without a row",
                expiring(&[1], &[1, 2]),
                2,
                LENGTHS,
            ),
            (
                "row without a deadline",
                expiring(&[0, 1], &[1]),
                2,
                LENGTHS,
            ),
            (
                "more expiring rows than rows",
                expiring(&[0, 1, 2], &[1, 2, 3]),
                2,
                LENGTHS,
            ),
            (
                "expiring row beyond the rows",
                expiring(&[2], &[1]),
                2,
                EXPIRY,
            ),
            (
                "repeated expiring row",
                expiring(&[1, 1], &[1, 2]),
                2,
                EXPIRY,
            ),
            (
                "descending expiring rows",
                expiring(&[1, 0], &[1, 2]),
                2,
                EXPIRY,
            ),
            ("short components", with(5, halves(&[0; 5])), 2, LENGTHS),
            ("long components", with(5, halves(&[0; 7])), 2, LENGTHS),
            (
                "infinite half",
                with(5, halves(&[0, 0, 0, 0, 0xfc00, 0])),
                2,
                NON_FINITE,
            ),
            (
                "NaN half",
                with(5, halves(&[0x7e00, 0, 0, 0, 0, 0])),
                2,
                NON_FINITE,
            ),
        ];
        for (case, body, count, expected) in cases {
            assert_eq!(reason(read(&body, 2, count, LAYOUT16)), expected, "{case}");
        }
        // The frame must name this block.
        let frame_reason = "compact block header does not match its frame";
        assert_eq!(reason(read(&valid, 3, 2, LAYOUT16)), frame_reason);
        assert_eq!(
            reason(check(2, id(2), 2, &valid, LAYOUT16, FLOOR, 0)),
            frame_reason
        );
        let other_kind = u64::from(KIND_DENSE + 1) | 2 << 16;
        assert_eq!(
            reason(check(1, other_kind, 2, &valid, LAYOUT16, FLOOR, 0)),
            BLOCK
        );
        // A float32 NaN is as corrupt as a float16 one.
        let float32 = Layout {
            precision: Precision::Float32,
            ..LAYOUT16
        };
        let mut nan = sections.clone();
        nan[0] = vec![3, 0, 0, 0, 1, 0, 0, 0];
        nan[5] = [0.0f32, 1.0, 2.0, 3.0, f32::NAN, 5.0]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        assert_eq!(
            reason(read(&raw(1, CRITICAL, &nan), 2, 2, float32)),
            NON_FINITE
        );
        // Only version 1 exists.
        assert!(matches!(
            read(&raw(2, CRITICAL, &sections), 2, 2, LAYOUT16),
            Err(Error::UnsupportedFeature(FLAG_COMPACT))
        ));
    }

    #[test]
    fn encoder_refuses_rows_a_block_cannot_hold() {
        let vector = [1.0, 2.0, 3.0];
        let row = Row {
            id: 7,
            revision: 1,
            expiry: None,
            vector: &vector,
        };
        let float16 = |rows: &[Row<'_>]| encode(1, 0, LAYOUT16, rows, 0).map(drop);
        let rows = "dense block row count out of range";
        let order = "dense rows must ascend by ID";
        let dimensions = "dense row has other dimensions";
        let inexact = "dense component is not stored exactly in the precision";
        assert_eq!(float16(&[]), Err(rows));
        assert_eq!(float16(&[row, row]), Err(order));
        assert_eq!(float16(&[row, Row { id: 6, ..row }]), Err(order));
        assert_eq!(
            float16(&[Row {
                vector: &[1.0, 2.0],
                ..row
            }]),
            Err(dimensions)
        );
        assert_eq!(
            float16(&[Row {
                vector: &[1.0, 2.0, 3.0, 4.0],
                ..row
            }]),
            Err(dimensions)
        );
        assert_eq!(
            float16(&[Row {
                vector: &[1.0, f32::NAN, 3.0],
                ..row
            }]),
            Err(inexact)
        );
        assert_eq!(
            float16(&[Row {
                vector: &[1.0, 0.1, 3.0],
                ..row
            }]),
            Err(inexact)
        );
        let float32 = Layout {
            precision: Precision::Float32,
            ..LAYOUT16
        };
        let infinite = [Row {
            vector: &[1.0, f32::INFINITY, 3.0],
            ..row
        }];
        assert_eq!(encode(1, 0, float32, &infinite, 0).map(drop), Err(inexact));
        // Collections have 1 to 65,536 dimensions.
        let need = "dense rows need 1 to 65,536 dimensions";
        let flat = |dimensions| Layout {
            dimensions,
            precision: Precision::Float32,
        };
        let empty = [Row { vector: &[], ..row }];
        assert_eq!(encode(1, 0, flat(0), &empty, 0).map(drop), Err(need));
        let wide = vec![0.0; MAX_DIMENSIONS + 1];
        let wide = [Row {
            vector: &wide,
            ..row
        }];
        assert_eq!(
            encode(1, 0, flat(MAX_DIMENSIONS + 1), &wide, 0).map(drop),
            Err(need)
        );
    }

    /// Blocks hold at most 4,096 rows, and more than one row only within
    /// 1 MiB of components; the largest float32 rows fill one exactly.
    #[test]
    fn blocks_are_bounded_by_rows_and_component_bytes() {
        let max_rows = |dimensions, precision| {
            Layout {
                dimensions,
                precision,
            }
            .max_rows()
        };
        let widest = MAX_DIMENSIONS;
        assert_eq!(max_rows(1, Precision::Float32), 4096);
        assert_eq!(max_rows(768, Precision::Float32), 341);
        assert_eq!(max_rows(768, Precision::Float16), 682);
        assert_eq!(max_rows(widest, Precision::Float32), 4);
        assert_eq!(max_rows(widest, Precision::Float16), 8);
        let layout = Layout {
            dimensions: widest,
            precision: Precision::Float32,
        };
        let vector = vec![0.5; widest];
        let rows: Vec<Row<'_>> = (0..5)
            .map(|id| Row {
                id,
                revision: 1,
                expiry: None,
                vector: &vector,
            })
            .collect();
        let refused = encode(1, 0, layout, &rows, 0).map(drop);
        assert_eq!(refused, Err("dense block row count out of range"));
        let body = encode(1, 0, layout, &rows[..4], 0).unwrap();
        assert!(body.len() < BLOCK_COMPONENTS + 4096, "{}", body.len());
        assert_eq!(read(&body, 0, 4, layout).unwrap().len(), 4);
        assert_eq!(reason(read(&body, 0, 5, layout)), COUNT);
    }

    /// A sealed block authenticates its prefix and body like every frame:
    /// opened, it is the plaintext block; damaged, it never decodes.
    #[test]
    fn encrypted_blocks_authenticate_before_they_decode() {
        let body = golden();
        let plain = frame(1, id(2), 2, &body);
        let cipher = Cipher::create(&EncryptionKey::from_bytes([7; 32])).unwrap();
        let sealed = cipher.seal_frame(&plain).unwrap();
        assert_eq!(sealed.len(), plain.len() + SEAL_OVERHEAD);
        let opened = format::verified_frame(&sealed, Some(&cipher), FLOOR, 0).unwrap();
        assert_eq!(opened[..], plain[..]);
        let opened = &opened[PREFIX_SIZE..opened.len() - FOOTER_SIZE];
        let rows = golden_rows(&GOLDEN_VECTORS);
        assert_eq!(
            decoded(&read(opened, 2, 2, LAYOUT16).unwrap()),
            expected(&rows)
        );
        let failed = |disk: &[u8], cipher: &Cipher| {
            matches!(
                format::verified_frame(disk, Some(cipher), FLOOR, 0),
                Err(Error::Corrupt {
                    reason: "frame authentication failed",
                    ..
                })
            )
        };
        // Damage to the prefix (the frame ID) or the ciphertext, under a
        // valid checksum.
        for at in [16, PREFIX_SIZE + 12 + 100] {
            let mut damaged = sealed.clone();
            damaged[at] ^= 1;
            let end = damaged.len() - FOOTER_SIZE;
            let checksum = format::crc32(&damaged[..end]);
            damaged[end + 4..end + 8].copy_from_slice(&checksum.to_le_bytes());
            assert!(failed(&damaged, &cipher), "{at}");
        }
        let other = Cipher::create(&EncryptionKey::from_bytes([8; 32])).unwrap();
        assert!(failed(&sealed, &other));
    }

    /// Preactivation: a complete image whose dense rows block is valid is
    /// still refused by the release readers before anything is replayed.
    #[test]
    fn release_readers_still_refuse_valid_dense_rows() {
        let layout = Layout {
            dimensions: 2,
            precision: Precision::Float32,
        };
        let row = Row {
            id: 1,
            revision: FLOOR,
            expiry: None,
            vector: &[0.25, -0.0],
        };
        let body = encode(0, 0, layout, &[row], 0).unwrap();
        check(0, id(0), 1, &body, layout, FLOOR, 0).unwrap();
        let mut entry = Vec::new();
        for field in [KIND_DENSE, VERSION, CRITICAL, 0] {
            entry.extend_from_slice(&field.to_le_bytes());
        }
        let mut frames = vec![
            format::advance_frame(FLOOR),
            block(KIND_TABLE, 1, CRITICAL, 0, 0, 1, &entry),
            frame(0, id(0), 1, &body),
        ];
        let mut digest = Sha256::new();
        frames
            .iter()
            .for_each(|frame| digest.update(&frame[..PREFIX_SIZE]));
        let mut end = (frames.len() as u64).to_le_bytes().to_vec();
        end.extend_from_slice(&digest.finalize());
        frames.push(block(KIND_END, 1, CRITICAL, 0, 0, 1, &end));
        let flags = FLAG_HISTORY | FLAG_COMPACT;
        let mut bytes = format::header_v5_flags(2, 0x5EED_0000_0000_0003, 0, flags, None);
        frames
            .iter()
            .for_each(|frame| bytes.extend_from_slice(frame));
        assert!(matches!(
            format::load(&mut Cursor::new(bytes.clone())),
            Err(Error::UnsupportedFeature(FLAG_COMPACT))
        ));
        assert!(matches!(
            crate::Database::open_from_bytes(bytes, None),
            Err(Error::UnsupportedFeature(FLAG_COMPACT))
        ));
    }
}
