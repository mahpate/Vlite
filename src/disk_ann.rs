//! Disk-based ANN: an HNSW graph whose nodes and vectors live in a rebuildable
//! sidecar file and are read on demand through a byte-bounded LRU cache, so a
//! collection can be searched with memory far below the size of its vectors.
//!
//! The sidecar (`<datafile>.dann`) is derived data. The datafile never stores
//! it, it is bound to the exact lineage and revision it was built from, and
//! stale or damaged copies are refused, never trusted. Sidecars hold plaintext
//! vectors, so they are not built for encrypted databases.
//!
//! Memory model. Resident memory is the header, the upper graph layers (about
//! `1/m` of the nodes) and the cache budget. Building streams nodes to disk but
//! first constructs the ordinary in-memory HNSW graph, so *build* memory is the
//! same as `build_hnsw`; only query-time memory is bounded.

use crate::hnsw::{self, HnswConfig};
use crate::{
    AnnOptions, Database, DistanceMetric, Error, Result, SearchHit, metric_distance,
    validate_ann_metric, validate_metric_query,
};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAGIC: &[u8; 8] = b"VLDANN01";
const FOOTER_MAGIC: &[u8; 8] = b"VLDANNEN";
const HEADER_SIZE: u64 = 128;
const FOOTER_SIZE: u64 = 16;
const VERSION: u32 = 1;
const NO_ENTRY: u64 = u64::MAX;
/// Default cache budget for `DiskAnnOptions`.
pub const DEFAULT_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Query-time resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskAnnOptions {
    /// Upper bound on cached node bytes (rounded up to one node if smaller).
    pub cache_bytes: usize,
}

impl Default for DiskAnnOptions {
    fn default() -> Self {
        Self {
            cache_bytes: DEFAULT_CACHE_BYTES,
        }
    }
}

/// Memory and IO accounting. `resident_bytes` excludes the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskAnnMemory {
    pub resident_bytes: usize,
    pub cache_capacity_bytes: usize,
    pub cache_used_bytes: usize,
    pub cached_nodes: usize,
    pub node_bytes: usize,
    pub disk_reads: u64,
    pub cache_hits: u64,
    pub total_nodes: usize,
    /// Size of all node records on disk (what would be resident in memory).
    pub node_data_bytes: u64,
}

/// What the sidecar was built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskAnnSource {
    pub lineage: u64,
    pub sequence: u64,
    pub collection_id: u16,
    pub live_records: u64,
}

struct Node {
    id: u64,
    norm: f64,
    neighbors: Vec<u32>,
    vector: Vec<f32>,
}

#[derive(Default)]
struct Cache {
    nodes: HashMap<u32, (Arc<Node>, u64)>,
    order: BTreeMap<u64, u32>,
    tick: u64,
    used: usize,
    reads: u64,
    hits: u64,
}

pub struct DiskAnnIndex {
    path: PathBuf,
    file: Mutex<File>,
    dimensions: usize,
    metric: DistanceMetric,
    config: HnswConfig,
    node_count: usize,
    entry: Option<u32>,
    max_level: usize,
    max_degree: usize,
    stride: usize,
    source: DiskAnnSource,
    upper: HashMap<u32, Vec<Vec<u32>>>,
    upper_bytes: usize,
    capacity: usize,
    cache: Mutex<Cache>,
}

fn corrupt(reason: &'static str) -> Error {
    Error::Corrupt { offset: 0, reason }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn metric_code(metric: DistanceMetric) -> u8 {
    u8::from(metric == DistanceMetric::Cosine)
}

fn sidecar_path(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_owned();
    name.push(".dann");
    PathBuf::from(name)
}

/// Conventional sidecar location for a datafile: `<datafile>.dann`.
pub fn default_sidecar_path(database: impl AsRef<Path>) -> PathBuf {
    sidecar_path(database.as_ref())
}

impl Database {
    /// Builds the disk-based ANN sidecar for `collection` at `path` (written
    /// to a temporary file, fsynced, then atomically renamed over `path`).
    ///
    /// Supports squared-L2 and cosine. Records are indexed as stored; expiry
    /// and metadata filters are not applied by the index. Not available for
    /// encrypted databases (`SidecarUnavailable`).
    pub fn build_disk_ann(
        &self,
        collection: &str,
        path: impl AsRef<Path>,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Result<DiskAnnSource> {
        self.ensure_healthy()?;
        if self.cipher.is_some() {
            return Err(Error::SidecarUnavailable(
                "disk ANN sidecars hold plaintext vectors and are not built for encrypted databases",
            ));
        }
        config.validate()?;
        validate_ann_metric(metric)?;
        let id = self.resolve(collection)?;
        let state = self.state(id);
        let lineage = self.lineage.unwrap_or(0);
        let index = hnsw::Index::build(&state.records, config, metric, AnnOptions::default());
        let graph = index.export_graph().ok_or(Error::UnsupportedAnnMetric)?;
        let source = DiskAnnSource {
            lineage,
            sequence: self.sequence,
            collection_id: id,
            live_records: state.records.len() as u64,
        };
        write_sidecar(
            path.as_ref(),
            &graph,
            &state.records,
            state.dimensions,
            config,
            metric,
            source,
        )?;
        Ok(source)
    }

    /// Searches through `index` after checking it still matches this
    /// database's lineage and revision; otherwise `StaleIndex`. Any write to
    /// the database (to any collection) makes an existing index stale.
    pub fn search_disk_ann(
        &self,
        index: &DiskAnnIndex,
        query: &[f32],
        k: usize,
        ef_search: usize,
    ) -> Result<Vec<SearchHit>> {
        self.ensure_healthy()?;
        if !index.is_fresh_for(self) {
            return Err(Error::StaleIndex);
        }
        index.search(query, k, ef_search)
    }
}

fn write_sidecar(
    path: &Path,
    graph: &hnsw::GraphExport,
    records: &BTreeMap<u64, crate::Record>,
    dimensions: usize,
    config: HnswConfig,
    metric: DistanceMetric,
    source: DiskAnnSource,
) -> Result<()> {
    let count = graph.nodes.len();
    let max_degree = graph
        .nodes
        .iter()
        .map(|(_, _, links)| links[0].len())
        .max()
        .unwrap_or(0);
    let stride = 8 + 8 + 4 + 4 * max_degree + 4 * dimensions + 4;
    let max_level = graph
        .nodes
        .iter()
        .map(|(_, _, links)| links.len() - 1)
        .max()
        .unwrap_or(0);

    let mut upper = Vec::new();
    for (index, (_, _, links)) in graph.nodes.iter().enumerate() {
        if links.len() > 1 {
            upper.extend_from_slice(&(index as u32).to_le_bytes());
            upper.extend_from_slice(&[(links.len() - 1) as u8, 0, 0, 0]);
            for layer in &links[1..] {
                upper.extend_from_slice(&(layer.len() as u32).to_le_bytes());
                for &neighbor in layer {
                    upper.extend_from_slice(&(neighbor as u32).to_le_bytes());
                }
            }
        }
    }

    let mut header = [0u8; HEADER_SIZE as usize];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(dimensions as u32).to_le_bytes());
    header[16] = metric_code(metric);
    header[20..24].copy_from_slice(&(config.m as u32).to_le_bytes());
    header[24..28].copy_from_slice(&(config.ef_construction as u32).to_le_bytes());
    header[28..32].copy_from_slice(&(max_degree as u32).to_le_bytes());
    header[32..40].copy_from_slice(&config.seed.to_le_bytes());
    header[40..48].copy_from_slice(&(count as u64).to_le_bytes());
    let entry = graph.entry.map_or(NO_ENTRY, |entry| entry as u64);
    header[48..56].copy_from_slice(&entry.to_le_bytes());
    header[56..60].copy_from_slice(&(max_level as u32).to_le_bytes());
    header[60..64].copy_from_slice(&(stride as u32).to_le_bytes());
    header[64..72].copy_from_slice(&source.lineage.to_le_bytes());
    header[72..80].copy_from_slice(&source.sequence.to_le_bytes());
    header[80..88].copy_from_slice(&source.live_records.to_le_bytes());
    header[88..92].copy_from_slice(&u32::from(source.collection_id).to_le_bytes());
    let upper_offset = HEADER_SIZE + count as u64 * stride as u64;
    header[96..104].copy_from_slice(&upper_offset.to_le_bytes());
    header[104..112].copy_from_slice(&(upper.len() as u64).to_le_bytes());
    let checksum = crate::format::crc32(&header[..124]);
    header[124..128].copy_from_slice(&checksum.to_le_bytes());

    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    {
        let mut out = BufWriter::new(temporary.as_file_mut());
        out.write_all(&header)?;
        let mut record = vec![0u8; stride];
        for (id, norm, links) in &graph.nodes {
            record.fill(0);
            let vector = &records[id].vector;
            record[..8].copy_from_slice(&id.to_le_bytes());
            record[8..16].copy_from_slice(&norm.to_le_bytes());
            record[16..20].copy_from_slice(&(links[0].len() as u32).to_le_bytes());
            for (slot, &neighbor) in links[0].iter().enumerate() {
                record[20 + slot * 4..24 + slot * 4]
                    .copy_from_slice(&(neighbor as u32).to_le_bytes());
            }
            let vectors_at = 20 + 4 * max_degree;
            for (slot, value) in vector.iter().enumerate() {
                record[vectors_at + slot * 4..vectors_at + slot * 4 + 4]
                    .copy_from_slice(&value.to_le_bytes());
            }
            let checksum = crate::format::crc32(&record[..stride - 4]);
            record[stride - 4..].copy_from_slice(&checksum.to_le_bytes());
            out.write_all(&record)?;
        }
        out.write_all(&upper)?;
        out.write_all(FOOTER_MAGIC)?;
        out.write_all(&crate::format::crc32(&upper).to_le_bytes())?;
        out.write_all(&checksum.to_le_bytes())?;
        out.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| Error::Io(error.error))?;
    Ok(())
}

impl DiskAnnIndex {
    pub fn open(path: impl AsRef<Path>, options: DiskAnnOptions) -> Result<Self> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        let length = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        if length < HEADER_SIZE + FOOTER_SIZE {
            return Err(corrupt("truncated disk ANN header"));
        }
        let mut header = [0u8; HEADER_SIZE as usize];
        file.read_exact(&mut header)?;
        if &header[..8] != MAGIC || u32_at(&header, 8) != VERSION {
            return Err(corrupt("not a disk ANN sidecar"));
        }
        if crate::format::crc32(&header[..124]) != u32_at(&header, 124) {
            return Err(corrupt("disk ANN header checksum mismatch"));
        }
        let dimensions = u32_at(&header, 12) as usize;
        let metric = match header[16] {
            0 => DistanceMetric::SquaredL2,
            1 => DistanceMetric::Cosine,
            _ => return Err(corrupt("unknown disk ANN metric")),
        };
        let config = HnswConfig {
            m: u32_at(&header, 20) as usize,
            ef_construction: u32_at(&header, 24) as usize,
            seed: u64_at(&header, 32),
        };
        let max_degree = u32_at(&header, 28) as usize;
        let node_count = u64_at(&header, 40) as usize;
        let entry = u64_at(&header, 48);
        let max_level = u32_at(&header, 56) as usize;
        let stride = u32_at(&header, 60) as usize;
        let upper_offset = u64_at(&header, 96);
        let upper_len = u64_at(&header, 104);
        if !(1..=crate::MAX_DIMENSIONS).contains(&dimensions)
            || max_degree > 4096
            || max_level > 16
            || stride != 8 + 8 + 4 + 4 * max_degree + 4 * dimensions + 4
            || config.validate().is_err()
        {
            return Err(corrupt("invalid disk ANN header fields"));
        }
        let nodes_bytes = (node_count as u64)
            .checked_mul(stride as u64)
            .ok_or_else(|| corrupt("disk ANN size overflow"))?;
        if upper_offset != HEADER_SIZE + nodes_bytes
            || length != upper_offset + upper_len + FOOTER_SIZE
            || (entry != NO_ENTRY && entry >= node_count as u64)
            || (entry == NO_ENTRY) != (node_count == 0)
            || upper_len > 1 << 31
        {
            return Err(corrupt("inconsistent disk ANN layout"));
        }
        file.seek(SeekFrom::Start(upper_offset))?;
        let mut upper_bytes = vec![0u8; upper_len as usize];
        file.read_exact(&mut upper_bytes)?;
        let mut footer = [0u8; FOOTER_SIZE as usize];
        file.read_exact(&mut footer)?;
        if &footer[..8] != FOOTER_MAGIC
            || u32_at(&footer, 8) != crate::format::crc32(&upper_bytes)
            || u32_at(&footer, 12) != u32_at(&header, 124)
        {
            return Err(corrupt("disk ANN footer mismatch"));
        }
        let upper = parse_upper(&upper_bytes, node_count)?;
        let resident = HEADER_SIZE as usize
            + upper_bytes.len()
            + upper
                .values()
                .map(|layers| {
                    layers
                        .iter()
                        .map(|layer| layer.len() * 4 + 24)
                        .sum::<usize>()
                        + 48
                })
                .sum::<usize>();
        Ok(Self {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            dimensions,
            metric,
            config,
            node_count,
            entry: (entry != NO_ENTRY).then_some(entry as u32),
            max_level,
            max_degree,
            stride,
            source: DiskAnnSource {
                lineage: u64_at(&header, 64),
                sequence: u64_at(&header, 72),
                live_records: u64_at(&header, 80),
                collection_id: u32_at(&header, 88) as u16,
            },
            upper,
            upper_bytes: resident,
            capacity: options.cache_bytes.max(stride + 64),
            cache: Mutex::new(Cache::default()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub fn metric(&self) -> DistanceMetric {
        self.metric
    }

    pub fn config(&self) -> HnswConfig {
        self.config
    }

    pub fn len(&self) -> usize {
        self.node_count
    }

    pub fn is_empty(&self) -> bool {
        self.node_count == 0
    }

    pub fn source(&self) -> DiskAnnSource {
        self.source
    }

    /// True when the database is exactly the state this index was built from.
    pub fn is_fresh_for(&self, database: &Database) -> bool {
        database.lineage.unwrap_or(0) == self.source.lineage
            && database.sequence == self.source.sequence
    }

    pub fn memory(&self) -> DiskAnnMemory {
        let cache = self.cache.lock().expect("cache lock");
        DiskAnnMemory {
            resident_bytes: self.upper_bytes,
            cache_capacity_bytes: self.capacity,
            cache_used_bytes: cache.used,
            cached_nodes: cache.nodes.len(),
            node_bytes: self.stride,
            disk_reads: cache.reads,
            cache_hits: cache.hits,
            total_nodes: self.node_count,
            node_data_bytes: self.node_count as u64 * self.stride as u64,
        }
    }

    fn load_node(&self, index: u32) -> Result<Arc<Node>> {
        {
            let mut cache = self.cache.lock().expect("cache lock");
            cache.tick += 1;
            let tick = cache.tick;
            if let Some((node, previous)) = cache.nodes.get_mut(&index) {
                let node = Arc::clone(node);
                let old = std::mem::replace(previous, tick);
                cache.order.remove(&old);
                cache.order.insert(tick, index);
                cache.hits += 1;
                return Ok(node);
            }
        }
        let node = Arc::new(self.read_node(index)?);
        let mut cache = self.cache.lock().expect("cache lock");
        cache.reads += 1;
        cache.tick += 1;
        let tick = cache.tick;
        if let Some((_, previous)) = cache.nodes.insert(index, (Arc::clone(&node), tick)) {
            cache.order.remove(&previous);
            cache.used -= self.stride;
        }
        cache.order.insert(tick, index);
        cache.used += self.stride;
        while cache.used > self.capacity {
            let Some((&oldest, &victim)) = cache.order.iter().next() else {
                break;
            };
            cache.order.remove(&oldest);
            cache.nodes.remove(&victim);
            cache.used -= self.stride;
        }
        Ok(node)
    }

    fn read_node(&self, index: u32) -> Result<Node> {
        if index as usize >= self.node_count {
            return Err(corrupt("disk ANN neighbor out of range"));
        }
        let mut bytes = vec![0u8; self.stride];
        {
            let mut file = self.file.lock().expect("file lock");
            file.seek(SeekFrom::Start(
                HEADER_SIZE + u64::from(index) * self.stride as u64,
            ))?;
            file.read_exact(&mut bytes)?;
        }
        self.parse_node(&bytes)
    }

    fn parse_node(&self, bytes: &[u8]) -> Result<Node> {
        if crate::format::crc32(&bytes[..self.stride - 4]) != u32_at(bytes, self.stride - 4) {
            return Err(corrupt("disk ANN node checksum mismatch"));
        }
        let degree = u32_at(bytes, 16) as usize;
        if degree > self.max_degree {
            return Err(corrupt("disk ANN node degree out of range"));
        }
        let neighbors: Vec<u32> = (0..degree)
            .map(|slot| u32_at(bytes, 20 + slot * 4))
            .collect();
        if neighbors.iter().any(|&n| n as usize >= self.node_count) {
            return Err(corrupt("disk ANN neighbor out of range"));
        }
        let vectors_at = 20 + 4 * self.max_degree;
        let vector = (0..self.dimensions)
            .map(|slot| {
                f32::from_le_bytes(
                    bytes[vectors_at + slot * 4..vectors_at + slot * 4 + 4]
                        .try_into()
                        .expect("4 bytes"),
                )
            })
            .collect();
        Ok(Node {
            id: u64_at(bytes, 0),
            norm: f64::from_bits(u64_at(bytes, 8)),
            neighbors,
            vector,
        })
    }

    /// Reads every node and the upper layers, verifying all checksums and
    /// structural invariants. Streams one node at a time.
    pub fn verify(&self) -> Result<()> {
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(HEADER_SIZE))?;
        let mut bytes = vec![0u8; self.stride];
        let mut previous = None;
        for _ in 0..self.node_count {
            file.read_exact(&mut bytes)?;
            let node = self.parse_node(&bytes)?;
            if previous.is_some_and(|previous| previous >= node.id) {
                return Err(corrupt("disk ANN node IDs are not strictly increasing"));
            }
            previous = Some(node.id);
        }
        Ok(())
    }

    fn distance(&self, query: &[f32], query_norm: f64, node: &Node) -> f64 {
        metric_distance(query, &node.vector, self.metric, query_norm, node.norm)
            .expect("cosine graph contains only nonzero vectors")
    }

    /// Approximate k-nearest search. `ef_search` is raised to `k` when smaller.
    /// Recall tracks in-memory HNSW built with the same configuration; an
    /// index with `ef_search >= len()` visits every reachable node.
    pub fn search(&self, query: &[f32], k: usize, ef_search: usize) -> Result<Vec<SearchHit>> {
        if query.len() != self.dimensions {
            return Err(Error::DimensionMismatch {
                expected: self.dimensions,
                actual: query.len(),
            });
        }
        if k == 0 || ef_search > crate::MAX_EF_SEARCH {
            return Err(Error::InvalidEfSearch);
        }
        let query_norm = validate_metric_query(query, self.metric)?;
        let Some(entry) = self.entry else {
            return Ok(Vec::new());
        };
        let ef = ef_search.max(k);
        let entry_node = self.load_node(entry)?;
        let mut best = (self.distance(query, query_norm, entry_node.as_ref()), entry);
        drop(entry_node);
        for layer in (1..=self.max_level).rev() {
            loop {
                let previous = best;
                let links = self
                    .upper
                    .get(&best.1)
                    .and_then(|layers| layers.get(layer - 1));
                for &neighbor in links.map(Vec::as_slice).unwrap_or(&[]) {
                    let node = self.load_node(neighbor)?;
                    let candidate = (self.distance(query, query_norm, &node), neighbor);
                    if cmp(candidate, best).is_lt() {
                        best = candidate;
                    }
                }
                if best == previous {
                    break;
                }
            }
        }

        let start = Ordered(best.0, best.1);
        let mut visited = HashSet::from([start.1]);
        let mut candidates = BinaryHeap::from([Reverse(start)]);
        let mut nearest = BinaryHeap::from([start]);
        while let Some(Reverse(current)) = candidates.pop() {
            if nearest.len() >= ef && current > *nearest.peek().expect("nonempty") {
                break;
            }
            let node = self.load_node(current.1)?;
            for &neighbor in &node.neighbors {
                if !visited.insert(neighbor) {
                    continue;
                }
                let loaded = self.load_node(neighbor)?;
                let candidate = Ordered(self.distance(query, query_norm, &loaded), neighbor);
                if nearest.len() < ef || candidate < *nearest.peek().expect("nonempty") {
                    candidates.push(Reverse(candidate));
                    nearest.push(candidate);
                    if nearest.len() > ef {
                        nearest.pop();
                    }
                }
            }
        }
        let mut found = nearest.into_vec();
        found.sort_unstable();
        let mut hits = Vec::with_capacity(k.min(found.len()));
        for candidate in found {
            let node = self.load_node(candidate.1)?;
            hits.push(SearchHit {
                id: node.id,
                distance: candidate.0,
            });
        }
        hits.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.id.cmp(&b.id)));
        hits.truncate(k);
        Ok(hits)
    }
}

fn cmp(a: (f64, u32), b: (f64, u32)) -> std::cmp::Ordering {
    a.0.total_cmp(&b.0).then(a.1.cmp(&b.1))
}

#[derive(Clone, Copy, Debug)]
struct Ordered(f64, u32);

impl PartialEq for Ordered {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Ordered {}
impl PartialOrd for Ordered {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ordered {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

fn parse_upper(bytes: &[u8], node_count: usize) -> Result<HashMap<u32, Vec<Vec<u32>>>> {
    let mut upper = HashMap::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let header = bytes
            .get(at..at + 8)
            .ok_or_else(|| corrupt("truncated disk ANN upper layers"))?;
        let index = u32_at(header, 0);
        let levels = usize::from(header[4]);
        if index as usize >= node_count || levels == 0 || levels > 16 || header[5..8] != [0; 3] {
            return Err(corrupt("invalid disk ANN upper layer entry"));
        }
        at += 8;
        let mut layers = Vec::with_capacity(levels);
        for _ in 0..levels {
            let count = u32_at(
                bytes
                    .get(at..at + 4)
                    .ok_or_else(|| corrupt("truncated disk ANN upper layers"))?,
                0,
            ) as usize;
            at += 4;
            let end = count
                .checked_mul(4)
                .and_then(|n| at.checked_add(n))
                .filter(|&end| end <= bytes.len())
                .ok_or_else(|| corrupt("truncated disk ANN upper layers"))?;
            let layer: Vec<u32> = (at..end).step_by(4).map(|p| u32_at(bytes, p)).collect();
            if layer.iter().any(|&n| n as usize >= node_count) {
                return Err(corrupt("disk ANN upper neighbor out of range"));
            }
            layers.push(layer);
            at = end;
        }
        if upper.insert(index, layers).is_some() {
            return Err(corrupt("duplicate disk ANN upper layer entry"));
        }
    }
    Ok(upper)
}
