//! Test-gated pinned point-read views over blocks checked by `ImageCheck`.

use super::catalog::column::{BlockDirectory, BlockRange};
use super::catalog::payload::{SourceBlock, SourceRow};
use super::half::from_half;
use crate::compact::catalog::Precision;
use crate::crypto::Cipher;
use crate::format::u32_at;
use crate::format::{self, FOOTER_SIZE, PREFIX_SIZE, corrupt};
use crate::{Error, Record, Result};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

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

#[derive(Clone)]
struct PendingBlock {
    source: SourceBlock,
    frame: Range<usize>,
    kind: u16,
    checksum: u32,
    sequence: u64,
    offset: u64,
}

#[derive(Default)]
pub(crate) struct Capture {
    bytes: Vec<u8>,
    blocks: Vec<PendingBlock>,
}

impl Capture {
    pub(crate) fn push(
        &mut self,
        source: SourceBlock,
        frame: &[u8],
        sequence: u64,
        offset: u64,
    ) -> Result<()> {
        if frame.len() < PREFIX_SIZE + FOOTER_SIZE {
            return Err(corrupt(offset, "compact source frame is truncated"));
        }
        let start = self.bytes.len();
        let end = start
            .checked_add(frame.len())
            .ok_or(Error::InvalidQuery("compact source size overflow"))?;
        self.bytes.extend_from_slice(frame);
        let kind = source.frame_id as u16;
        self.blocks.push(PendingBlock {
            source,
            frame: start..end,
            kind,
            checksum: u32_at(frame, frame.len() - FOOTER_SIZE + 4),
            sequence,
            offset,
        });
        Ok(())
    }

    pub(crate) fn finish(self, cipher: Option<Cipher>) -> Result<Arc<CompactGeneration>> {
        CompactGeneration::new(Backing::Owned(Arc::from(self.bytes)), self.blocks, cipher)
            .map(Arc::new)
    }
}

#[derive(Clone)]
enum Backing {
    Owned(Arc<[u8]>),
    #[cfg(not(target_arch = "wasm32"))]
    Mapped(Arc<memmap2::Mmap>),
}

impl Backing {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            #[cfg(not(target_arch = "wasm32"))]
            Self::Mapped(bytes) => bytes,
        }
    }
}

#[derive(Clone)]
struct SourceFrame {
    source: SourceBlock,
    frame: Range<usize>,
    kind: u16,
    checksum: u32,
    sequence: u64,
    offset: u64,
}

struct RecordCell {
    record: OnceLock<Record>,
    initializing: Mutex<()>,
}

impl RecordCell {
    fn new() -> Self {
        Self {
            record: OnceLock::new(),
            initializing: Mutex::new(()),
        }
    }
}

struct CollectionSource {
    blocks: Vec<SourceFrame>,
    block_starts: Vec<usize>,
    directory: BlockDirectory,
    cells: Vec<RecordCell>,
}

impl CollectionSource {
    fn new(blocks: Vec<SourceFrame>) -> Result<Self> {
        let ranges = blocks
            .iter()
            .map(|block| {
                let first = block.source.rows.first().map_or(0, |row| row.id);
                let last = block.source.rows.last().map_or(0, |row| row.id);
                BlockRange {
                    first_id: first,
                    last_id: last,
                    rows: block.source.rows.len(),
                }
            })
            .collect();
        let directory = BlockDirectory::new(ranges)?;
        let mut count = 0usize;
        let mut block_starts = Vec::with_capacity(blocks.len());
        for block in &blocks {
            block_starts.push(count);
            count = count
                .checked_add(block.source.rows.len())
                .ok_or(Error::InvalidQuery("compact source row count overflow"))?;
        }
        Ok(Self {
            blocks,
            block_starts,
            directory,
            cells: (0..count).map(|_| RecordCell::new()).collect(),
        })
    }

    fn row(&self, id: u64) -> Option<(usize, usize)> {
        let block = self.directory.block_of(id)?;
        let source = &self.blocks[block].source.rows;
        let row = source.binary_search_by_key(&id, |record| record.id).ok()?;
        Some((block, row))
    }

    fn total_rows(&self) -> usize {
        self.cells.len()
    }

    fn row_at(&self, row: usize) -> Option<(&SourceFrame, &SourceRow, &RecordCell)> {
        if row >= self.total_rows() {
            return None;
        }
        let block = self.block_starts.partition_point(|&start| start <= row) - 1;
        let local = row - self.block_starts[block];
        Some((
            &self.blocks[block],
            &self.blocks[block].source.rows[local],
            &self.cells[row],
        ))
    }
}

pub(crate) struct CompactGeneration {
    backing: Backing,
    collections: BTreeMap<u16, CollectionSource>,
    cipher: Option<Cipher>,
    decode_count: std::sync::atomic::AtomicUsize,
}

impl CompactGeneration {
    pub(crate) fn has_collection(&self, collection: u16) -> bool {
        self.collections.contains_key(&collection)
    }

    fn new(backing: Backing, blocks: Vec<PendingBlock>, cipher: Option<Cipher>) -> Result<Self> {
        let mut by_collection: BTreeMap<u16, Vec<SourceFrame>> = BTreeMap::new();
        for block in blocks {
            by_collection
                .entry(block.source.collection)
                .or_default()
                .push(SourceFrame {
                    source: block.source,
                    frame: block.frame,
                    kind: block.kind,
                    checksum: block.checksum,
                    sequence: block.sequence,
                    offset: block.offset,
                });
        }
        let collections = by_collection
            .into_iter()
            .map(|(collection, blocks)| {
                CollectionSource::new(blocks).map(|source| (collection, source))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            backing,
            collections,
            cipher,
            decode_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn view(self: &Arc<Self>, collection: u16) -> CompactView {
        CompactView {
            generation: Arc::clone(self),
            collection,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[cfg(test)]
    fn mapped_copy(&self, path: &std::path::Path) -> Result<Arc<Self>> {
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.write_all(self.backing.as_slice())?;
        file.sync_all()?;
        let mapping = {
            // SAFETY: the mapped source is immutable; its file is retained by
            // the caller while the view is exercised.
            unsafe { memmap2::MmapOptions::new().map(&file) }
        }?;
        let blocks = self
            .collections
            .values()
            .flat_map(|source| source.blocks.iter())
            .map(|block| PendingBlock {
                source: block.source.clone(),
                frame: block.frame.clone(),
                kind: block.kind,
                checksum: block.checksum,
                sequence: block.sequence,
                offset: block.offset,
            })
            .collect();
        Self::new(
            Backing::Mapped(Arc::new(mapping)),
            blocks,
            self.cipher.clone(),
        )
        .map(Arc::new)
    }

    fn decode(&self, block: &SourceFrame, row: &SourceRow) -> Result<Record> {
        self.decode_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let disk = self
            .backing
            .as_slice()
            .get(block.frame.clone())
            .ok_or_else(|| corrupt(block.offset, "compact source frame is out of bounds"))?;
        if disk.len() < PREFIX_SIZE + FOOTER_SIZE
            || block.kind != block.source.frame_id as u16
            || u32_at(disk, disk.len() - FOOTER_SIZE + 4) != block.checksum
        {
            return Err(corrupt(block.offset, "compact source frame changed"));
        }
        let verified;
        let frame = if let Some(cipher) = &self.cipher {
            verified = format::verified_frame(disk, Some(cipher), block.sequence, block.offset)?;
            verified.as_ref()
        } else {
            disk
        };
        if frame.len() < PREFIX_SIZE + FOOTER_SIZE {
            return Err(corrupt(block.offset, "compact source frame is truncated"));
        }
        let body = &frame[PREFIX_SIZE..frame.len() - FOOTER_SIZE];
        let vector = body
            .get(row.vector.clone())
            .ok_or_else(|| corrupt(block.offset, "compact vector extent is out of bounds"))?;
        let payload = body
            .get(row.payload.clone())
            .ok_or_else(|| corrupt(block.offset, "compact payload extent is out of bounds"))?;
        let metadata = body
            .get(row.metadata.clone())
            .ok_or_else(|| corrupt(block.offset, "compact metadata extent is out of bounds"))?;
        let width = match block.source.layout.precision {
            Precision::Float32 => 4,
            Precision::Float16 => 2,
        };
        let vector = vector
            .chunks_exact(width)
            .map(|component| match block.source.layout.precision {
                Precision::Float32 => f32::from_le_bytes(component.try_into().unwrap()),
                Precision::Float16 => from_half(u16::from_le_bytes(component.try_into().unwrap())),
            })
            .collect();
        let metadata = crate::metadata::decode(metadata)
            .map_err(|_| corrupt(block.offset, "invalid compact record metadata"))?;
        Ok(Record {
            vector,
            payload: payload.to_vec(),
            metadata,
        })
    }
}

#[derive(Clone)]
pub(crate) struct CompactView {
    generation: Arc<CompactGeneration>,
    collection: u16,
}

impl CompactView {
    fn source(&self) -> Option<&CollectionSource> {
        self.generation.collections.get(&self.collection)
    }

    /// Physical row count, including rows hidden by expiry.
    pub(crate) fn len(&self) -> usize {
        self.source().map_or(0, CollectionSource::total_rows)
    }

    pub(crate) fn first_id(&self) -> Option<u64> {
        self.id(0)
    }

    /// Whether an ID has a physical row; expiry is applied by `get_at`.
    pub(crate) fn contains_id(&self, id: u64) -> bool {
        self.row_of(id).is_some()
    }

    pub(crate) fn row_of(&self, id: u64) -> Option<usize> {
        let source = self.source()?;
        let (block, row) = source.row(id)?;
        Some(source.block_starts[block] + row)
    }

    pub(crate) fn id(&self, row: usize) -> Option<u64> {
        self.source()?.row_at(row).map(|(_, record, _)| record.id)
    }

    pub(crate) fn revision(&self, row: usize) -> Option<u64> {
        self.source()?
            .row_at(row)
            .map(|(_, record, _)| record.revision)
    }

    pub(crate) fn expiry(&self, row: usize) -> Option<Option<u64>> {
        self.source()?
            .row_at(row)
            .map(|(_, record, _)| record.expiry)
    }

    pub(crate) fn get(&self, id: u64, now_ms: u64) -> Result<Option<&Record>> {
        self.get_at(id, now_ms)
    }

    pub(crate) fn get_at(&self, id: u64, now_ms: u64) -> Result<Option<&Record>> {
        let Some(source) = self.source() else {
            return Ok(None);
        };
        let Some((block_index, row_index)) = source.row(id) else {
            return Ok(None);
        };
        let block = &source.blocks[block_index];
        let row = &block.source.rows[row_index];
        if row.expiry.is_some_and(|expiry| expiry <= now_ms) {
            return Ok(None);
        }
        self.get_physical(id)
    }

    pub(crate) fn get_physical(&self, id: u64) -> Result<Option<&Record>> {
        let Some(source) = self.source() else {
            return Ok(None);
        };
        let Some((block_index, row_index)) = source.row(id) else {
            return Ok(None);
        };
        let block = &source.blocks[block_index];
        let row = &block.source.rows[row_index];
        let cell = &source.cells[source.block_starts[block_index] + row_index];
        if cell.record.get().is_none() {
            let _guard = cell
                .initializing
                .lock()
                .map_err(|_| Error::InvalidQuery("compact record cell lock poisoned"))?;
            if cell.record.get().is_none() {
                let record = self.generation.decode(block, row)?;
                let _ = cell.record.set(record);
            }
        }

        Ok(cell.record.get())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dispatch_tests;
#[cfg(test)]
mod install_tests;
#[cfg(test)]
mod method_tests;
#[cfg(test)]
mod overlay_tests;
