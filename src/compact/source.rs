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
    identity: GenerationId,
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
            identity: GenerationId::next()?,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct GenerationId(u64);

impl GenerationId {
    fn next() -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        use std::sync::atomic::Ordering::Relaxed;
        let mut id = NEXT.load(Relaxed);
        loop {
            let next = id
                .checked_add(1)
                .ok_or(Error::InvalidQuery("compact generation identity exhausted"))?;
            match NEXT.compare_exchange_weak(id, next, Relaxed, Relaxed) {
                Ok(_) => return Ok(Self(id)),
                Err(current) => id = current,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CacheKey {
    generation: GenerationId,
    collection: u16,
    kind: u16,
    partition: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SeamAccounting {
    pub(crate) mapped_live: usize,
    pub(crate) mapped_peak: usize,
    pub(crate) vectors_live: usize,
    pub(crate) vectors_peak: usize,
    pub(crate) plaintext_live: usize,
    pub(crate) plaintext_peak: usize,
    pub(crate) cells_live: usize,
    pub(crate) cells_peak: usize,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) evictions: u64,
}

struct VectorBlock {
    values: Vec<f32>,
    dimensions: usize,
    precision: Precision,
    used: u64,
}

/// Serial operation-local storage, never shared by a generation or persisted.
pub(crate) struct ReadWorkspace {
    budget: usize,
    clock: u64,
    vectors: BTreeMap<CacheKey, VectorBlock>,
    plaintext: Vec<u8>,
    accounting: SeamAccounting,
}

fn increment(value: &mut u64) -> Result<()> {
    *value = value
        .checked_add(1)
        .ok_or(Error::InvalidQuery("compact workspace counter exhausted"))?;
    Ok(())
}

impl ReadWorkspace {
    pub(crate) fn new(budget: usize) -> Result<Self> {
        if budget > 4 * 1024 * 1024 {
            return Err(Error::InvalidQuery("compact workspace exceeds 4 MiB"));
        }
        Ok(Self {
            budget,
            clock: 0,
            vectors: BTreeMap::new(),
            plaintext: Vec::new(),
            accounting: SeamAccounting::default(),
        })
    }

    pub(crate) fn accounting(&mut self, view: &CompactView) -> Result<SeamAccounting> {
        let mapped = match &view.generation.backing {
            Backing::Owned(_) => 0,
            #[cfg(not(target_arch = "wasm32"))]
            Backing::Mapped(bytes) => bytes.len(),
        };
        let mut cells = 0usize;
        for collection in view.generation.collections.values() {
            for record in collection.cells.iter().filter_map(|cell| cell.record.get()) {
                cells = cells
                    .checked_add(
                        usize::try_from(crate::format::record_body_len(record))
                            .map_err(|_| Error::InvalidQuery("compact cell accounting overflow"))?,
                    )
                    .ok_or(Error::InvalidQuery("compact cell accounting overflow"))?;
            }
        }
        self.accounting.mapped_live = mapped;
        self.accounting.mapped_peak = self.accounting.mapped_peak.max(mapped);
        self.accounting.cells_live = cells;
        self.accounting.cells_peak = self.accounting.cells_peak.max(cells);
        Ok(self.accounting)
    }

    fn body<'a>(&'a mut self, view: &'a CompactView, block: &SourceFrame) -> Result<&'a [u8]> {
        // Release the previous plaintext before authentication allocates its frame.
        self.plaintext = Vec::new();
        self.accounting.plaintext_live = 0;
        let disk = view
            .generation
            .backing
            .as_slice()
            .get(block.frame.clone())
            .ok_or_else(|| corrupt(block.offset, "compact source frame is out of bounds"))?;
        if disk.len() < PREFIX_SIZE + FOOTER_SIZE
            || disk.len() - PREFIX_SIZE - FOOTER_SIZE
                > super::catalog::payload::MAX_BODY + crate::crypto::SEAL_OVERHEAD
            || block.kind != block.source.frame_id as u16
            || u32_at(disk, disk.len() - FOOTER_SIZE + 4) != block.checksum
        {
            return Err(corrupt(block.offset, "compact source frame changed"));
        }
        let frame = if let Some(cipher) = &view.generation.cipher {
            self.plaintext =
                format::verified_frame(disk, Some(cipher), block.sequence, block.offset)?
                    .into_owned();
            self.accounting.plaintext_live = self.plaintext.len();
            self.accounting.plaintext_peak = self
                .accounting
                .plaintext_peak
                .max(self.accounting.plaintext_live);
            self.plaintext.as_slice()
        } else {
            disk
        };
        let end = frame
            .len()
            .checked_sub(FOOTER_SIZE)
            .ok_or_else(|| corrupt(block.offset, "compact source frame is truncated"))?;
        let body = frame
            .get(PREFIX_SIZE..end)
            .ok_or_else(|| corrupt(block.offset, "compact source frame is truncated"))?;
        if body.len() > super::catalog::payload::MAX_BODY {
            return Err(corrupt(block.offset, "compact source body exceeds limit"));
        }
        Ok(body)
    }
}

#[derive(Clone)]
pub(crate) struct CompactView {
    generation: Arc<CompactGeneration>,
    collection: u16,
}

impl CompactView {
    pub(crate) fn generation_id(&self) -> GenerationId {
        self.generation.identity
    }

    pub(crate) fn read_metadata(
        &self,
        row: usize,
        workspace: &mut ReadWorkspace,
    ) -> Result<Option<crate::Metadata>> {
        let Some((block, row, _)) = self.source().and_then(|source| source.row_at(row)) else {
            return Ok(None);
        };
        let bytes = workspace
            .body(self, block)?
            .get(row.metadata.clone())
            .ok_or_else(|| corrupt(block.offset, "compact metadata extent is out of bounds"))?;
        crate::metadata::decode(bytes)
            .map(Some)
            .map_err(|_| corrupt(block.offset, "invalid compact record metadata"))
    }

    pub(crate) fn read_vector<'w>(
        &self,
        row: usize,
        workspace: &'w mut ReadWorkspace,
    ) -> Result<Option<&'w [f32]>> {
        let Some(source) = self.source() else {
            return Ok(None);
        };
        let Some((block, _, _)) = source.row_at(row) else {
            return Ok(None);
        };
        let dimensions = block.source.layout.dimensions;
        let elements = block
            .source
            .rows
            .len()
            .checked_mul(dimensions)
            .ok_or(Error::InvalidQuery("compact vector size overflow"))?;
        let bytes = elements
            .checked_mul(4)
            .ok_or(Error::InvalidQuery("compact vector size overflow"))?;
        if bytes > workspace.budget {
            return Err(Error::InvalidQuery(
                "compact scratch budget is smaller than one block",
            ));
        }
        let key = CacheKey {
            generation: self.generation_id(),
            collection: self.collection,
            kind: block.kind,
            partition: block.source.frame_id >> 16,
        };
        increment(&mut workspace.clock)?;
        if workspace.vectors.contains_key(&key) {
            // Authentication is per read, even when decoded vectors are cached.
            workspace.body(self, block)?;
            let entry = workspace
                .vectors
                .get_mut(&key)
                .expect("checked cache entry");
            if entry.values.len() != elements
                || entry.dimensions != dimensions
                || entry.precision != block.source.layout.precision
            {
                return Err(Error::InvalidQuery("compact cached block shape changed"));
            }
            increment(&mut workspace.accounting.hits)?;
            entry.used = workspace.clock;
        } else {
            increment(&mut workspace.accounting.misses)?;
            while workspace
                .accounting
                .vectors_live
                .checked_add(bytes)
                .ok_or(Error::InvalidQuery("compact vector accounting overflow"))?
                > workspace.budget
            {
                let key = workspace
                    .vectors
                    .iter()
                    .min_by_key(|(key, entry)| (entry.used, **key))
                    .map(|(key, _)| *key)
                    .ok_or(Error::InvalidQuery("compact cache accounting mismatch"))?;
                let removed = workspace.vectors.remove(&key).expect("selected entry");
                workspace.accounting.vectors_live = workspace
                    .accounting
                    .vectors_live
                    .checked_sub(removed.values.len() * 4)
                    .ok_or(Error::InvalidQuery("compact vector accounting underflow"))?;
                increment(&mut workspace.accounting.evictions)?;
            }
            let body = workspace.body(self, block)?;
            let width = match block.source.layout.precision {
                Precision::Float32 => 4,
                Precision::Float16 => 2,
            };
            let mut values = Vec::with_capacity(elements);
            for row in &block.source.rows {
                let vector = body
                    .get(row.vector.clone())
                    .filter(|extent| extent.len() == dimensions * width)
                    .ok_or_else(|| corrupt(block.offset, "invalid compact vector extent"))?;
                values.extend(vector.chunks_exact(width).map(|component| match width {
                    4 => f32::from_le_bytes(component.try_into().expect("component width")),
                    2 => from_half(u16::from_le_bytes(
                        component.try_into().expect("component width"),
                    )),
                    _ => unreachable!("checked precision"),
                }));
            }
            workspace.accounting.vectors_live = workspace
                .accounting
                .vectors_live
                .checked_add(bytes)
                .ok_or(Error::InvalidQuery("compact vector accounting overflow"))?;
            workspace.accounting.vectors_peak = workspace
                .accounting
                .vectors_peak
                .max(workspace.accounting.vectors_live);
            workspace.vectors.insert(
                key,
                VectorBlock {
                    values,
                    dimensions,
                    precision: block.source.layout.precision,
                    used: workspace.clock,
                },
            );
        }
        let block_index = source.block_starts.partition_point(|&start| start <= row) - 1;
        let start = (row - source.block_starts[block_index]) * dimensions;
        Ok(Some(
            &workspace.vectors[&key].values[start..start + dimensions],
        ))
    }

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
#[cfg(test)]
mod seam_tests;
