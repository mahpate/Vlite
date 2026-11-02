//! Private complete primary records. The embedded dense v1 body is unchanged;
//! named/sparse histories and graph attachments remain in op 10 / op 15.

use super::super::{CRITICAL, Envelope, MIN_BODY, envelope, vectors};
use crate::format::{self, corrupt, u32_at};
use crate::{Error, MAX_METADATA_BYTES, MAX_PAYLOAD_BYTES, Record, Result};
use std::ops::Range;

pub(crate) const KIND: u16 = 0x0101;
pub(crate) const TARGET: usize = 1 << 20;
// One last maximum-size record may cross TARGET; dense components have
// their own independent 1 MiB cap. Includes both envelopes and offset tables.
pub(crate) const MAX_BODY: usize = 2 * TARGET + MAX_PAYLOAD_BYTES + MAX_METADATA_BYTES + 128 * 4096;

thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn enabled() -> bool {
    ENABLED.with(std::cell::Cell::get)
}

pub(crate) struct Activation(bool);

impl Activation {
    pub(crate) fn on() -> Self {
        Self::set(true)
    }

    pub(crate) fn set(enabled: bool) -> Self {
        Self(ENABLED.with(|cell| cell.replace(enabled)))
    }
}

impl Drop for Activation {
    fn drop(&mut self) {
        ENABLED.with(|cell| cell.set(self.0));
    }
}

pub(crate) struct Entry {
    pub id: u64,
    pub revision: u64,
    pub expiry: Option<u64>,
    pub record: Record,
}

pub(crate) struct Columns<'a> {
    rows: vectors::Rows<'a>,
    payloads: &'a [u8],
    payload_offsets: &'a [u8],
    metadata: &'a [u8],
    metadata_offsets: &'a [u8],
    source: Option<SourceBlock>,
}

#[derive(Clone)]
pub(crate) struct SourceRow {
    pub id: u64,
    pub revision: u64,
    pub expiry: Option<u64>,
    pub vector: Range<usize>,
    pub payload: Range<usize>,
    pub metadata: Range<usize>,
}

#[derive(Clone)]
pub(crate) struct SourceBlock {
    pub collection: u16,
    pub frame_id: u64,
    pub layout: vectors::Layout,
    pub rows: Vec<SourceRow>,
}

impl Columns<'_> {
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Requires `row < self.len()`.
    pub(crate) fn id(&self, row: usize) -> u64 {
        self.rows.id(row)
    }

    /// Requires `row < self.len()`.
    pub(crate) fn revision(&self, row: usize) -> u64 {
        self.rows.revision(row)
    }

    /// Requires `row < self.len()`.
    pub(crate) fn expiry(&self, row: usize) -> Option<u64> {
        self.rows.expiry(row)
    }

    /// Requires `row < self.len()`.
    pub(crate) fn payload(&self, row: usize) -> &[u8] {
        &self.payloads[self.part(self.payload_offsets, row)]
    }

    /// Requires `row < self.len()`.
    pub(crate) fn metadata(&self, row: usize) -> &[u8] {
        &self.metadata[self.part(self.metadata_offsets, row)]
    }

    /// Requires `row < self.len()`.
    pub(crate) fn vector(&self, row: usize) -> impl Iterator<Item = f32> + '_ {
        self.rows.vector(row)
    }

    pub(crate) fn into_source(self) -> Option<SourceBlock> {
        self.source
    }

    fn part(&self, offsets: &[u8], row: usize) -> std::ops::Range<usize> {
        u32_at(offsets, 4 * row) as usize..u32_at(offsets, 4 * (row + 1)) as usize
    }
}

/// Sections: descriptor (row count, reserved zero), dense v1 body,
/// payload offsets, payload bytes, metadata offsets, metadata bytes.
pub(crate) fn encode(
    owner: u16,
    partition: u32,
    layout: vectors::Layout,
    rows: &[(vectors::Row<'_>, &Record)],
) -> Result<Vec<u8>> {
    let dense: Vec<_> = rows.iter().map(|(row, _)| *row).collect();
    let body = vectors::encode(owner, partition, layout, &dense, 0)
        .map_err(|reason| corrupt(0, reason))?;
    let mut descriptor = (rows.len() as u32).to_le_bytes().to_vec();
    descriptor.extend_from_slice(&0u32.to_le_bytes());
    let (mut payloads, mut metadata) = (Vec::new(), Vec::new());
    let (mut po, mut mo) = (vec![0; 4], vec![0; 4]);
    for (_, record) in rows {
        if record.payload.len() > MAX_PAYLOAD_BYTES
            || crate::metadata::validate(&record.metadata).is_err()
        {
            return Err(corrupt(0, "invalid complete record"));
        }
        payloads.extend_from_slice(&record.payload);
        metadata.extend_from_slice(&crate::metadata::encode(&record.metadata));
        po.extend_from_slice(&(payloads.len() as u32).to_le_bytes());
        mo.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        if payloads.len() + metadata.len() > TARGET + MAX_PAYLOAD_BYTES + MAX_METADATA_BYTES {
            return Err(corrupt(0, "complete records exceed block target"));
        }
    }
    let bytes = envelope(
        KIND,
        owner,
        u64::from(partition),
        &[&descriptor, &body, &po, &payloads, &mo, &metadata],
        None,
    );
    if bytes.len() > MAX_BODY {
        return Err(corrupt(0, "oversized complete records block"));
    }
    Ok(bytes)
}

struct CheckOptions {
    owner: u16,
    id: u64,
    count: u32,
    layout: vectors::Layout,
    floor: u64,
    offset: u64,
    retain_source: bool,
}

fn check_columns_inner<'a>(body: &'a [u8], options: CheckOptions) -> Result<Columns<'a>> {
    let CheckOptions {
        owner,
        id,
        count,
        layout,
        floor,
        offset,
        retain_source,
    } = options;
    let invalid = || corrupt(offset, "invalid complete records block");
    if !(MIN_BODY..=MAX_BODY).contains(&body.len()) || id as u16 != KIND {
        return Err(invalid());
    }
    let block = Envelope::parse(owner, id, body, offset)?;
    if block.version != 1 {
        return Err(Error::UnsupportedFeature(format::FLAG_COMPACT));
    }
    let [descriptor, dense, po, payloads, mo, metadata] = block.sections.as_slice() else {
        return Err(invalid());
    };
    if block.flags != CRITICAL
        || descriptor.len() != 8
        || u32_at(descriptor, 0) != count
        || u32_at(descriptor, 4) != 0
    {
        return Err(invalid());
    }
    let rows = vectors::check(
        owner,
        u64::from(vectors::KIND_DENSE) | (id & !0xffff),
        count,
        dense,
        layout,
        floor,
        offset,
    )?;
    // Validate every offset and length before allocating any record values.
    let offsets = |offsets: &[u8], data: &[u8], maximum: usize| -> Result<()> {
        if rows.len().checked_add(1).and_then(|n| n.checked_mul(4)) != Some(offsets.len())
            || u32_at(offsets, 0) != 0
        {
            return Err(invalid());
        }
        let mut start = 0;
        for row in 0..rows.len() {
            let end = u32_at(offsets, 4 * (row + 1)) as usize;
            if end < start || end > data.len() || end - start > maximum {
                return Err(invalid());
            }
            start = end;
        }
        if start != data.len() {
            return Err(invalid());
        }
        Ok(())
    };
    offsets(po, payloads, MAX_PAYLOAD_BYTES)?;
    offsets(mo, metadata, MAX_METADATA_BYTES)?;
    if payloads
        .len()
        .checked_add(metadata.len())
        .is_none_or(|n| n > TARGET + MAX_PAYLOAD_BYTES + MAX_METADATA_BYTES)
    {
        return Err(invalid());
    }
    let source = retain_source.then(|| {
        let payload_data_start = block.section_starts[3] as usize;
        let metadata_data_start = block.section_starts[5] as usize;
        let vector_data_start = block.section_starts[1] as usize + rows.component_start();
        let source_rows = (0..rows.len())
            .map(|row| {
                let at = 4 * row;
                let vector = rows.component_range(row);
                SourceRow {
                    id: rows.id(row),
                    revision: rows.revision(row),
                    expiry: rows.expiry(row),
                    vector: vector_data_start + vector.start..vector_data_start + vector.end,
                    payload: payload_data_start + u32_at(po, at) as usize
                        ..payload_data_start + u32_at(po, at + 4) as usize,
                    metadata: metadata_data_start + u32_at(mo, at) as usize
                        ..metadata_data_start + u32_at(mo, at + 4) as usize,
                }
            })
            .collect();
        SourceBlock {
            collection: owner,
            frame_id: id,
            layout: rows.layout(),
            rows: source_rows,
        }
    });
    Ok(Columns {
        rows,
        payloads,
        payload_offsets: po,
        metadata,
        metadata_offsets: mo,
        source,
    })
}

pub(crate) fn check_columns<'a>(
    owner: u16,
    id: u64,
    count: u32,
    body: &'a [u8],
    layout: vectors::Layout,
    floor: u64,
    offset: u64,
) -> Result<Columns<'a>> {
    let columns = check_columns_inner(
        body,
        CheckOptions {
            owner,
            id,
            count,
            layout,
            floor,
            offset,
            retain_source: false,
        },
    )?;
    let invalid = || corrupt(offset, "invalid complete records block");
    for row in 0..columns.len() {
        crate::metadata::decode(columns.metadata(row)).map_err(|_| invalid())?;
    }
    Ok(columns)
}

pub(crate) fn check(
    owner: u16,
    id: u64,
    count: u32,
    body: &[u8],
    layout: vectors::Layout,
    floor: u64,
    offset: u64,
) -> Result<Vec<Entry>> {
    let columns = check_columns_inner(
        body,
        CheckOptions {
            owner,
            id,
            count,
            layout,
            floor,
            offset,
            retain_source: false,
        },
    )?;
    entries(&columns, offset)
}

pub(crate) fn check_with_source(
    owner: u16,
    id: u64,
    count: u32,
    body: &[u8],
    layout: vectors::Layout,
    floor: u64,
    offset: u64,
) -> Result<(Vec<Entry>, SourceBlock)> {
    let columns = check_columns_inner(
        body,
        CheckOptions {
            owner,
            id,
            count,
            layout,
            floor,
            offset,
            retain_source: true,
        },
    )?;
    let entries = entries(&columns, offset)?;
    let source = columns
        .into_source()
        .expect("source descriptors were requested");
    Ok((entries, source))
}

fn entries(columns: &Columns<'_>, offset: u64) -> Result<Vec<Entry>> {
    let entries = (0..columns.len())
        .map(|row| {
            let metadata = crate::metadata::decode(columns.metadata(row))
                .map_err(|_| corrupt(offset, "invalid complete records block"))?;
            Ok(Entry {
                id: columns.id(row),
                revision: columns.revision(row),
                expiry: columns.expiry(row),
                record: Record {
                    vector: columns.vector(row).collect(),
                    payload: columns.payload(row).to_vec(),
                    metadata,
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(entries)
}
