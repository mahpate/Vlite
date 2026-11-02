//! P3a precision policy storage (`docs/projects/roadmap/p3-precision-publication.md`,
//! Decisions W and P): the stored precision of a collection, and the three
//! critical storage kinds of a `FLAG_PRECISION` checkpoint image.
//!
//! A precision store fixes each collection's precision when it is created:
//! an op-17 frame creates a named collection with it in the ordinary log,
//! and every checkpoint image restates all live collections in one catalog
//! block (kind 3) right after the capability table, then the import
//! registry (kind 4), and after each graph's snapshot frames that graph's
//! node and edge revisions (kind 5), so a checkpoint drops nothing the log
//! held. Record bodies stay the existing float32 source frames.
//!
//! Preactivation: only builds whose `precision_supported` gate is on (tests
//! of this crate) accept the flag; release readers refuse it with
//! `UnsupportedFeature(FLAG_PRECISION)` before reading any frame.

use super::{HEADER_LEN, MAX_PADDING, block_frame};
use crate::format::{FLAG_PRECISION, MAX_COLLECTIONS, corrupt, u16_at, u32_at, u64_at};
use crate::import::{MAX_IMAGE_SECTION, Registry};
use crate::{Error, MAX_DIMENSIONS, Result};

/// The precision catalog: one section, an opt-in `u32` (1), a zero `u32`
/// and one entry per live collection in ascending ID order: ID (`u16`),
/// precision (`u8`), a zero byte and dimensions (`u32`). The frame count
/// is the number of entries, `default` included.
pub(crate) const KIND_CATALOG: u16 = 3;
/// The import registry (`Registry::encode_image`); the frame count is the
/// number of jobs.
pub(crate) const KIND_IMPORTS: u16 = 4;
/// Revisions of one graph's nodes and edges, owned by the graph: two
/// sections, nodes then edges, each an entry count (`u32`), a zero `u32`
/// and (ID, revision) `u64` pairs in ascending ID order. The run of one
/// graph covers every node and every edge of its snapshot exactly once,
/// each class continuing in ascending ID order across the blocks, at most
/// `MAX_REVISIONS` per block. The writer fills every block but the last;
/// readers accept any non-empty block, and an empty graph has a single
/// empty block. The frame count is the block's entries.
pub(crate) const KIND_REVISIONS: u16 = 5;
pub(crate) const MAX_REVISIONS: usize = 4096;

/// Stored precision of a collection's dense vectors. float32 is the default
/// and keeps every input bit; float16 is an explicit, lossy choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Precision {
    #[default]
    Float32,
    Float16,
}

impl Precision {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Float32 => 1,
            Self::Float16 => 2,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Float32),
            2 => Some(Self::Float16),
            _ => None,
        }
    }
}

/// One catalog entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub collection: u16,
    pub precision: Precision,
    pub dimensions: usize,
}

/// Largest body of a storage block of `kind`: header, section entries, at
/// most `MAX_PADDING` bytes before each section and the largest sections.
pub(crate) fn max_body(kind: u16) -> usize {
    match kind {
        KIND_CATALOG => HEADER_LEN + 8 + MAX_PADDING + 8 + 8 * MAX_COLLECTIONS,
        KIND_IMPORTS => HEADER_LEN + 8 + MAX_PADDING + MAX_IMAGE_SECTION,
        _ => HEADER_LEN + 16 + 2 * MAX_PADDING + 16 + 16 * MAX_REVISIONS,
    }
}

/// Checks the catalog section of a block with frame `count`. An opt-in or
/// precision this build does not know is `UnsupportedFeature`; anything
/// else out of place is corruption.
pub(crate) fn decode_catalog(section: &[u8], count: u32, offset: u64) -> Result<Vec<Entry>> {
    let count = count as usize;
    if !(1..=MAX_COLLECTIONS).contains(&count) || section.len() != 8 + 8 * count {
        return Err(corrupt(offset, "invalid precision catalog length"));
    }
    if u32_at(section, 0) != 1 {
        return Err(Error::UnsupportedFeature(FLAG_PRECISION));
    }
    if u32_at(section, 4) != 0 {
        return Err(corrupt(offset, "invalid precision catalog"));
    }
    let mut entries: Vec<Entry> = Vec::with_capacity(count);
    for entry in section[8..].as_chunks::<8>().0 {
        let precision =
            Precision::from_code(entry[2]).ok_or(Error::UnsupportedFeature(FLAG_PRECISION))?;
        let collection = u16_at(entry, 0);
        let dimensions = u32_at(entry, 4) as usize;
        if entry[3] != 0
            || !(1..=MAX_DIMENSIONS).contains(&dimensions)
            || entries
                .last()
                .is_some_and(|last| last.collection >= collection)
        {
            return Err(corrupt(offset, "invalid precision catalog entry"));
        }
        entries.push(Entry {
            collection,
            precision,
            dimensions,
        });
    }
    Ok(entries)
}

/// (ID, revision) pairs of nodes and of edges.
pub(crate) type Pairs = Vec<(u64, u64)>;

/// Checks the sections of a revision block with frame `count`: lengths
/// match the counts, IDs ascend within each section and every revision is
/// in `1..=floor`. Continuity across blocks is the image check's.
pub(crate) fn decode_revisions(
    sections: &[&[u8]],
    count: u32,
    floor: u64,
    offset: u64,
) -> Result<(Pairs, Pairs)> {
    let invalid = || corrupt(offset, "invalid graph revisions");
    let &[nodes, edges] = sections else {
        return Err(invalid());
    };
    let parse = |section: &[u8]| -> Result<Pairs> {
        if section.len() < 8 || u32_at(section, 4) != 0 {
            return Err(invalid());
        }
        let entries = u32_at(section, 0) as usize;
        if entries > MAX_REVISIONS || section.len() != 8 + 16 * entries {
            return Err(invalid());
        }
        let pairs: Pairs = section[8..]
            .as_chunks::<16>()
            .0
            .iter()
            .map(|entry| (u64_at(entry, 0), u64_at(entry, 8)))
            .collect();
        if pairs.windows(2).any(|pair| pair[0].0 >= pair[1].0)
            || pairs
                .iter()
                .any(|(_, revision)| !(1..=floor).contains(revision))
        {
            return Err(invalid());
        }
        Ok(pairs)
    };
    if count as usize > MAX_REVISIONS {
        return Err(invalid());
    }
    let (nodes, edges) = (parse(nodes)?, parse(edges)?);
    if nodes.len() + edges.len() != count as usize {
        return Err(invalid());
    }
    Ok((nodes, edges))
}

/// The catalog block of `entries`, every live collection in ascending ID
/// order.
pub(crate) fn catalog_frame(floor: u64, entries: &[Entry]) -> Vec<u8> {
    let mut section = Vec::with_capacity(8 + 8 * entries.len());
    section.extend_from_slice(&1u32.to_le_bytes());
    section.extend_from_slice(&0u32.to_le_bytes());
    for entry in entries {
        section.extend_from_slice(&entry.collection.to_le_bytes());
        section.push(entry.precision.code());
        section.push(0);
        section.extend_from_slice(&(entry.dimensions as u32).to_le_bytes());
    }
    block_frame(floor, KIND_CATALOG, 0, 0, entries.len() as u32, &[&section])
}

/// The import registry block.
pub(crate) fn imports_frame(floor: u64, registry: &Registry) -> Result<Vec<u8>> {
    let (section, jobs) = registry.encode_image()?;
    Ok(block_frame(floor, KIND_IMPORTS, 0, 0, jobs, &[&section]))
}

/// Emits the revision blocks of graph `graph`, whose snapshot frames were
/// just written.
pub(crate) fn revision_frames(
    floor: u64,
    graph: u16,
    nodes: &[(u64, u64)],
    edges: &[(u64, u64)],
    mut emit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let total = nodes.len() + edges.len();
    let section = |pairs: &[(u64, u64)]| {
        let mut bytes = Vec::with_capacity(8 + 16 * pairs.len());
        bytes.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for (id, revision) in pairs {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&revision.to_le_bytes());
        }
        bytes
    };
    let mut start = 0;
    for partition in 0u64.. {
        let end = total.min(start + MAX_REVISIONS);
        let in_nodes = |at: usize| at.min(nodes.len());
        let in_edges = |at: usize| at.saturating_sub(nodes.len());
        let node_part = section(&nodes[in_nodes(start)..in_nodes(end)]);
        let edge_part = section(&edges[in_edges(start)..in_edges(end)]);
        emit(&block_frame(
            floor,
            KIND_REVISIONS,
            graph,
            partition,
            (end - start) as u32,
            &[&node_part, &edge_part],
        ))?;
        start = end;
        if start == total {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod payload;

#[cfg(test)]
mod column;
