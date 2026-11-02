//! Private complete primary records. The embedded dense v1 body is unchanged;
//! named/sparse histories and graph attachments remain in op 10 / op 15.

use super::super::{CRITICAL, Envelope, MIN_BODY, envelope, vectors};
use crate::format::{self, corrupt, u32_at};
use crate::{Error, MAX_METADATA_BYTES, MAX_PAYLOAD_BYTES, Record, Result};

pub(crate) const KIND: u16 = 0x0101;
const TARGET: usize = 1 << 20;
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

pub(crate) fn table_frame(floor: u64, graphs: bool, records: bool) -> Vec<u8> {
    super::table_frame_extra(floor, graphs, if records { &[KIND] } else { &[] })
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

pub(crate) fn check(
    owner: u16,
    id: u64,
    count: u32,
    body: &[u8],
    layout: vectors::Layout,
    floor: u64,
    offset: u64,
) -> Result<Vec<Entry>> {
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
    let part = |offsets: &[u8], row: usize| {
        u32_at(offsets, 4 * row) as usize..u32_at(offsets, 4 * (row + 1)) as usize
    };
    (0..rows.len())
        .map(|row| {
            let metadata =
                crate::metadata::decode(&metadata[part(mo, row)]).map_err(|_| invalid())?;
            Ok(Entry {
                id: rows.id(row),
                revision: rows.revision(row),
                expiry: rows.expiry(row),
                record: Record {
                    vector: rows.vector(row).collect(),
                    payload: payloads[part(po, row)].to_vec(),
                    metadata,
                },
            })
        })
        .collect()
}

pub(crate) fn write_collection(
    owner: u16,
    collection: &format::LoadedCollection,
    floor: u64,
    now: Option<u64>,
    mut emit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let layout = vectors::Layout {
        dimensions: collection.dimensions,
        precision: collection
            .precision
            .ok_or_else(|| corrupt(0, "missing precision"))?,
    };
    let mut rows = Vec::new();
    let mut bytes = 0;
    let mut partition = 0;
    let flush = |rows: &mut Vec<_>, partition: u32, emit: &mut dyn FnMut(&[u8]) -> Result<()>| {
        let body = encode(owner, partition, layout, rows)?;
        emit(&format::compact_block_frame(
            floor,
            owner,
            u64::from(KIND) | (u64::from(partition) << 16),
            rows.len() as u32,
            &body,
        ))?;
        rows.clear();
        Ok::<(), Error>(())
    };
    for (&id, record) in &collection.records {
        let expiry = collection.expiries.get(&id).copied();
        if expiry.is_some_and(|expiry| now.is_some_and(|now| expiry <= now)) {
            continue;
        }
        let revision = collection
            .revisions
            .get(&id)
            .copied()
            .ok_or_else(|| corrupt(0, "record without a revision"))?;
        rows.push((
            vectors::Row {
                id,
                revision,
                expiry,
                vector: &record.vector,
            },
            record,
        ));
        bytes += record.payload.len()
            + crate::metadata::validate(&record.metadata)
                .map_err(|_| corrupt(0, "invalid record metadata"))?;
        if rows.len() == layout.max_rows() || bytes >= TARGET {
            flush(&mut rows, partition, &mut emit)?;
            partition = partition
                .checked_add(1)
                .ok_or_else(|| corrupt(0, "too many record blocks"))?;
            bytes = 0;
        }
    }
    if !rows.is_empty() {
        flush(&mut rows, partition, &mut emit)?;
    }
    Ok(())
}
