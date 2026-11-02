//! Private borrowed access to verified compact source columns.
//!
//! Float32 components are decoded into the bounded scratch cache too: this
//! avoids alignment-dependent casts until a generation-pinned accessor exists.

use super::payload;
use crate::compact::vectors::{self, Layout, Rows};
use crate::format::{FLAG_COMPACT, corrupt};
use crate::{Error, Result};
use std::collections::BTreeMap;

pub(crate) struct DenseColumn<'a> {
    rows: Rows<'a>,
    layout: Layout,
}

impl<'a> DenseColumn<'a> {
    pub(crate) fn check(
        collection: u16,
        id: u64,
        count: u32,
        body: &'a [u8],
        layout: Layout,
        floor: u64,
        offset: u64,
    ) -> Result<Self> {
        let rows = vectors::check(collection, id, count, body, layout, floor, offset)
            .map_err(|error| checked_error(error, offset, "invalid dense rows block"))?;
        Ok(Self { rows, layout })
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn id(&self, row: usize) -> u64 {
        self.rows.id(row)
    }

    pub(crate) fn revision(&self, row: usize) -> u64 {
        self.rows.revision(row)
    }

    pub(crate) fn expiry(&self, row: usize) -> Option<u64> {
        self.rows.expiry(row)
    }

    pub(crate) fn row_of(&self, id: u64) -> Option<usize> {
        self.search(id)
    }

    fn search(&self, id: u64) -> Option<usize> {
        binary_search(self.rows.len(), id, |row| self.rows.id(row))
    }

    pub(crate) fn vector<'c>(
        &self,
        cache: &'c mut ScratchCache,
        block: u64,
        row: usize,
    ) -> Result<&'c [f32]> {
        let row_count = self.rows.len();
        let dimensions = self.layout.dimensions;
        if row >= row_count {
            return Err(Error::InvalidQuery("compact column row is out of range"));
        }
        let decoded_bytes = decoded_bytes(row_count, dimensions)?;
        cache.ensure(block, decoded_bytes, |values| {
            for index in 0..row_count {
                values.extend(self.rows.vector(index));
            }
        })?;
        let values = cache.entry(block).ok_or(Error::InvalidQuery(
            "compact scratch cache entry is missing",
        ))?;
        Ok(&values[row * dimensions..(row + 1) * dimensions])
    }
}

pub(crate) struct RecordColumn<'a> {
    rows: payload::Columns<'a>,
    layout: Layout,
}

impl<'a> RecordColumn<'a> {
    pub(crate) fn check(
        collection: u16,
        id: u64,
        count: u32,
        body: &'a [u8],
        layout: Layout,
        floor: u64,
        offset: u64,
    ) -> Result<Self> {
        let rows = payload::check_columns(collection, id, count, body, layout, floor, offset)
            .map_err(|error| checked_error(error, offset, "invalid complete records block"))?;
        Ok(Self { rows, layout })
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn id(&self, row: usize) -> u64 {
        self.rows.id(row)
    }

    pub(crate) fn revision(&self, row: usize) -> u64 {
        self.rows.revision(row)
    }

    pub(crate) fn expiry(&self, row: usize) -> Option<u64> {
        self.rows.expiry(row)
    }

    pub(crate) fn row_of(&self, id: u64) -> Option<usize> {
        binary_search(self.rows.len(), id, |row| self.rows.id(row))
    }

    pub(crate) fn payload(&self, row: usize) -> &[u8] {
        self.rows.payload(row)
    }

    pub(crate) fn metadata(&self, row: usize) -> &[u8] {
        self.rows.metadata(row)
    }

    pub(crate) fn vector<'c>(
        &self,
        cache: &'c mut ScratchCache,
        block: u64,
        row: usize,
    ) -> Result<&'c [f32]> {
        let row_count = self.rows.len();
        let dimensions = self.layout.dimensions;
        if row >= row_count {
            return Err(Error::InvalidQuery("compact column row is out of range"));
        }
        let decoded_bytes = decoded_bytes(row_count, dimensions)?;
        cache.ensure(block, decoded_bytes, |values| {
            for index in 0..row_count {
                values.extend(self.rows.vector(index));
            }
        })?;
        let values = cache.entry(block).ok_or(Error::InvalidQuery(
            "compact scratch cache entry is missing",
        ))?;
        Ok(&values[row * dimensions..(row + 1) * dimensions])
    }
}

fn decoded_bytes(rows: usize, dimensions: usize) -> Result<usize> {
    rows.checked_mul(dimensions)
        .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
        .ok_or(Error::InvalidQuery("compact scratch size overflow"))
}

fn binary_search(len: usize, id: u64, mut at: impl FnMut(usize) -> u64) -> Option<usize> {
    let (mut low, mut high) = (0, len);
    while low < high {
        let middle = low + (high - low) / 2;
        match at(middle).cmp(&id) {
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
            std::cmp::Ordering::Equal => return Some(middle),
        }
    }
    None
}

fn checked_error(error: Error, offset: u64, reason: &'static str) -> Error {
    match error {
        Error::UnsupportedFeature(FLAG_COMPACT) => corrupt(offset, reason),
        other => other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockRange {
    pub first_id: u64,
    pub last_id: u64,
    pub rows: usize,
}

pub(crate) struct BlockDirectory {
    blocks: Vec<BlockRange>,
}

impl BlockDirectory {
    pub(crate) fn new(blocks: Vec<BlockRange>) -> Result<Self> {
        let invalid = || corrupt(0, "compact column blocks overlap or are out of order");
        if blocks
            .iter()
            .any(|block| block.rows == 0 || block.first_id > block.last_id)
            || blocks
                .windows(2)
                .any(|pair| pair[0].last_id >= pair[1].first_id)
        {
            return Err(invalid());
        }
        Ok(Self { blocks })
    }

    pub(crate) fn from_dense(columns: &[DenseColumn<'_>]) -> Result<Self> {
        Self::new(
            columns
                .iter()
                .map(|column| BlockRange {
                    first_id: column.id(0),
                    last_id: column.id(column.len() - 1),
                    rows: column.len(),
                })
                .collect(),
        )
    }

    pub(crate) fn from_records(columns: &[RecordColumn<'_>]) -> Result<Self> {
        Self::new(
            columns
                .iter()
                .map(|column| BlockRange {
                    first_id: column.id(0),
                    last_id: column.id(column.len() - 1),
                    rows: column.len(),
                })
                .collect(),
        )
    }

    pub(crate) fn block_of(&self, id: u64) -> Option<usize> {
        let candidate = self.blocks.partition_point(|block| block.last_id < id);
        self.blocks
            .get(candidate)
            .filter(|block| block.first_id <= id)
            .map(|_| candidate)
    }
}

struct CachedBlock {
    values: Vec<f32>,
    last_used: u64,
}

pub(crate) struct ScratchCache {
    budget_bytes: usize,
    used_bytes: usize,
    clock: u64,
    entries: BTreeMap<u64, CachedBlock>,
}

impl ScratchCache {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            used_bytes: 0,
            clock: 0,
            entries: BTreeMap::new(),
        }
    }

    pub(crate) fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    fn entry(&self, block: u64) -> Option<&[f32]> {
        self.entries
            .get(&block)
            .map(|entry| entry.values.as_slice())
    }

    fn ensure(
        &mut self,
        block: u64,
        decoded_bytes: usize,
        decode: impl FnOnce(&mut Vec<f32>),
    ) -> Result<()> {
        if decoded_bytes > self.budget_bytes {
            return Err(Error::InvalidQuery(
                "compact scratch budget is smaller than one decoded block",
            ));
        }
        self.tick()?;
        if let Some(entry) = self.entries.get_mut(&block) {
            entry.last_used = self.clock;
            return Ok(());
        }
        while self
            .used_bytes
            .checked_add(decoded_bytes)
            .is_none_or(|required| required > self.budget_bytes)
        {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(key, entry)| (entry.last_used, **key))
                .map(|(&key, _)| key)
                .expect("over-budget cache has an eviction candidate");
            let removed = self.entries.remove(&oldest).expect("candidate exists");
            self.used_bytes -= removed.values.len() * std::mem::size_of::<f32>();
        }
        let mut values = Vec::with_capacity(decoded_bytes / std::mem::size_of::<f32>());
        decode(&mut values);
        debug_assert_eq!(values.len() * std::mem::size_of::<f32>(), decoded_bytes);
        self.used_bytes += decoded_bytes;
        self.entries.insert(
            block,
            CachedBlock {
                values,
                last_used: self.clock,
            },
        );
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        self.clock = self.clock.checked_add(1).ok_or(Error::InvalidQuery(
            "compact cache access counter exhausted",
        ))?;
        Ok(())
    }

    #[cfg(test)]
    fn keys_lru(&self) -> Vec<u64> {
        let mut entries: Vec<_> = self
            .entries
            .iter()
            .map(|(&key, entry)| (entry.last_used, key))
            .collect();
        entries.sort_unstable();
        entries.into_iter().map(|(_, key)| key).collect()
    }
}

#[cfg(test)]
mod tests;
