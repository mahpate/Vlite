//! Private complete primary records. The embedded dense v1 body is unchanged;
//! named/sparse histories and graph attachments remain in op 10 / op 15.

use super::super::{CRITICAL, Envelope, MIN_BODY, envelope, vectors};
use crate::format::{self, corrupt, u32_at};
use crate::{Error, MAX_METADATA_BYTES, MAX_PAYLOAD_BYTES, Record, Result};

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
