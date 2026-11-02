//! P3a Decisions W and P (`docs/projects/roadmap/p3-precision-publication.md`):
//! the catalog, import registry and graph revision codecs, op 17, the
//! precision image check, and private precision stores on every backing,
//! plain and encrypted, through recovery, checkpoints, maintenance, the
//! change feed, export and replicas. Each test switches the crate-private
//! activation on for its own thread; release builds refuse `FLAG_PRECISION`
//! (`release_builds_refuse_precision_stores`).

use super::*;
use crate::changes::{ChangeKind, FeedCursor};
use crate::compact::{
    Activation, CRITICAL, Envelope, ImageCheck, KIND_END, KIND_TABLE, end_frame, table_frame,
};
use crate::crypto::Cipher;
use crate::format::{
    self, FLAG_COMPACT, FLAG_ENCRYPTED, FLAG_GRAPH, FLAG_HISTORY, FLAG_TIMESTAMPS, FOOTER_SIZE,
    ImportMarker, Loaded, LoadedImportMarker, PREFIX_SIZE,
};
use crate::import::{ImportChunkOutcome, ImportState, InvalidRowPolicy};
use crate::logops::{self, RestoreTarget};
use crate::maintenance::failpoint::{self, Stage};
use crate::property_graph::GraphOp;
use crate::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, EncryptionKey, Expected, Metadata,
    Mutation, ReplicaFollower, maintenance,
};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::io::{Cursor, Write};
use std::path::Path;
use std::rc::Rc;

mod canonical;
mod goldens;
mod limits;
mod parity;
mod payload;
mod peak;
mod populated;
mod replayed;
mod storage;
mod suffix;

const LINEAGE: u64 = 0x5EED_0000_0000_0032;
/// A deadline far in the future (2100-01-01).
const FAR: u64 = 4_102_444_800_000;
const F32: Precision = Precision::Float32;
const F16: Precision = Precision::Float16;
const POLICY: InvalidRowPolicy = InvalidRowPolicy::RejectChunk;

type Revs<'a> = &'a [(u64, u64)];
/// An edit of a section or frame under test.
type Edit = dyn Fn(&mut Vec<u8>);

/// The class of `result`, for compact assertions.
fn outcome<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => "ok".into(),
        Err(Error::Corrupt { .. }) => "corrupt".into(),
        Err(Error::UnsupportedFeature(flags)) => format!("unsupported {flags}"),
        Err(error) => format!("{error:?}"),
    }
}

fn key() -> EncryptionKey {
    EncryptionKey::from_bytes([5; 32])
}

fn spec(dimensions: usize) -> CollectionSpec {
    CollectionSpec {
        dimensions,
        settings: CollectionSettings::default(),
    }
}

/// The empty state of a new precision store: `default` (float32, two
/// dimensions) at head and floor 1, the sequence its image reserves.
fn initial(key: Option<&EncryptionKey>) -> Loaded {
    let cipher = key.map(|key| Cipher::create(key).unwrap());
    let flags = FLAG_GRAPH | if key.is_some() { FLAG_ENCRYPTED } else { 0 };
    let header = format::header_v5_flags(2, LINEAGE, 0, flags, cipher.as_ref());
    let mut loaded = format::load_with(&mut Cursor::new(header), key).unwrap();
    loaded.flags |= FLAG_PRECISION;
    loaded.sequence = 1;
    loaded.collections.get_mut(&0).unwrap().precision = Some(F32);
    loaded
}

/// The precision checkpoint image of `loaded` at its head.
fn image_of(loaded: &Loaded) -> Vec<u8> {
    let mut bytes = Vec::new();
    maintenance::write_v5_image(
        &mut bytes,
        loaded,
        loaded.lineage.unwrap(),
        None,
        None,
        loaded.cipher.as_ref(),
        loaded.flags & (FLAG_GRAPH | FLAG_PRECISION),
        loaded.sequence,
    )
    .unwrap();
    bytes
}

fn load(bytes: &[u8], key: Option<&EncryptionKey>) -> Result<Loaded> {
    format::load_with(&mut Cursor::new(bytes), key)
}

/// The header and frames of a plaintext log.
fn split(bytes: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let (header, mut rest) = bytes.split_at(32);
    let mut frames = Vec::new();
    while !rest.is_empty() {
        let body = format::plain_body_len(rest[4], u32_at(rest, 24), u32_at(rest, 28));
        let (frame, tail) = rest.split_at(PREFIX_SIZE + body + FOOTER_SIZE);
        frames.push(frame.to_vec());
        rest = tail;
    }
    (header.to_vec(), frames)
}

/// `header` and the image `frames` (from the advance on) closed by a
/// matching end block.
fn sealed(header: &[u8], frames: &[Vec<u8>]) -> Vec<u8> {
    let mut digest = Sha256::new();
    let mut bytes = header.to_vec();
    for frame in frames {
        digest.update(&frame[..PREFIX_SIZE]);
        bytes.extend_from_slice(frame);
    }
    let floor = u64_at(&frames[0], 8);
    bytes.extend_from_slice(&end_frame(
        floor,
        frames.len() as u64,
        digest.finalize().into(),
    ));
    bytes
}

/// The plaintext `log` with every frame sealed under `key()`, as an
/// encrypted store writes it. The frame prefixes, and so the image end
/// digest, are unchanged.
fn encrypted(log: &[u8]) -> Vec<u8> {
    let (header, frames) = split(log);
    let cipher = Cipher::create(&key()).unwrap();
    let mut bytes = format::header_v5_flags(
        u32_at(&header, 12) as usize,
        u64_at(&header, 16),
        u16_at(&header, 24),
        u16_at(&header, 26) | FLAG_ENCRYPTED,
        Some(&cipher),
    );
    for frame in &frames {
        bytes.extend(cipher.seal_frame(frame).unwrap());
    }
    bytes
}

/// `frame` after `edit` changed its prefix or body (body bytes start at
/// `PREFIX_SIZE`), with its length, checksums and footer refitted.
fn refit(frame: &[u8], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut bytes = frame[..frame.len() - FOOTER_SIZE].to_vec();
    edit(&mut bytes);
    // An op-1 body also holds its `count` vector components.
    let vector = format::plain_body_len(bytes[4], u32_at(&bytes, 24), 0);
    let length = (bytes.len() - PREFIX_SIZE - vector) as u32;
    bytes[28..32].copy_from_slice(&length.to_le_bytes());
    let prefix = format::crc32(&bytes[..32]);
    bytes[32..36].copy_from_slice(&prefix.to_le_bytes());
    let checksum = format::crc32(&bytes);
    let sequence = bytes[8..16].to_vec();
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence);
    bytes
}

/// Operation and, for op 16, block kind of each frame.
fn shape(frames: &[Vec<u8>]) -> Vec<(u8, u16)> {
    frames
        .iter()
        .map(|frame| {
            let kind = if frame[4] == 16 {
                u64_at(frame, 16) as u16
            } else {
                0
            };
            (frame[4], kind)
        })
        .collect()
}

/// The sections of an op-16 frame's block.
fn sections(frame: &[u8]) -> Vec<Vec<u8>> {
    let body = &frame[PREFIX_SIZE..frame.len() - FOOTER_SIZE];
    Envelope::parse(u16_at(frame, 5), u64_at(frame, 16), body, 0)
        .unwrap()
        .sections
        .iter()
        .map(|section| section.to_vec())
        .collect()
}

/// Whether `frame` is an op-16 block of `kind`.
fn is_block(frame: &[u8], kind: u16) -> bool {
    frame[4] == 16 && u64_at(frame, 16) & 0xFFFF == u64::from(kind)
}

/// The image of `loaded` with the blocks of `kind` replaced by `blocks`
/// at version `version`, which the capability table then declares.
fn replaced(loaded: &Loaded, kind: u16, version: u16, blocks: Vec<Vec<u8>>) -> Vec<u8> {
    let (header, mut frames) = split(&image_of(loaded));
    frames.pop();
    let at = frames
        .iter()
        .position(|frame| is_block(frame, kind))
        .unwrap();
    let count = frames.iter().filter(|frame| is_block(frame, kind)).count();
    frames.splice(at..at + count, blocks);
    if version != 1 {
        let versioned = |bytes: &mut Vec<u8>| {
            bytes[PREFIX_SIZE + 2..PREFIX_SIZE + 4].copy_from_slice(&version.to_le_bytes())
        };
        for frame in &mut frames {
            if is_block(frame, kind) {
                *frame = refit(frame, versioned);
            }
        }
        let table = frames
            .iter_mut()
            .find(|frame| is_block(frame, KIND_TABLE))
            .unwrap();
        let mut section = sections(table).remove(0);
        for entry in section.as_chunks_mut::<8>().0 {
            if u16_at(entry, 0) == kind {
                entry[2..4].copy_from_slice(&version.to_le_bytes());
            }
        }
        let entries = (section.len() / 8) as u32;
        *table = block_frame(u64_at(table, 8), KIND_TABLE, 0, 0, entries, &[&section]);
    }
    sealed(&header, &frames)
}

fn entries(list: &[(u16, Precision, usize)]) -> Vec<Entry> {
    list.iter()
        .map(|&(collection, precision, dimensions)| Entry {
            collection,
            precision,
            dimensions,
        })
        .collect()
}

/// Collections of a loaded store with their stored precision.
fn precisions(loaded: &Loaded) -> Vec<(u16, String, Option<Precision>)> {
    loaded
        .collections
        .iter()
        .map(|(&id, collection)| (id, collection.name.clone(), collection.precision))
        .collect()
}

fn listed(list: &[(u16, &str, Precision)]) -> Vec<(u16, String, Option<Precision>)> {
    list.iter()
        .map(|&(id, name, precision)| (id, name.to_owned(), Some(precision)))
        .collect()
}

/// A revision section: its entry count, a zero `u32` and the pairs.
fn revision_section(pairs: Revs) -> Vec<u8> {
    let mut section = (pairs.len() as u32).to_le_bytes().to_vec();
    section.extend_from_slice(&[0; 4]);
    for (id, revision) in pairs {
        section.extend_from_slice(&id.to_le_bytes());
        section.extend_from_slice(&revision.to_le_bytes());
    }
    section
}

/// The golden import registry section: jobs 2 (open, `half`, ID 1),
/// 5 (finished, `default`) and 7 (aborted, its collection 2 `gone`
/// dropped), closed 7 then 5, and high water 9. Entries start at 16, 92
/// and 171, closed IDs at 247.
fn registry_section() -> Vec<u8> {
    let mut section = Vec::new();
    section.extend_from_slice(&9u64.to_le_bytes());
    section.extend_from_slice(&3u32.to_le_bytes());
    section.extend_from_slice(&2u32.to_le_bytes());
    for (job, collection, state, dropped, name, chunk, committed, skipped, digest) in [
        (2u64, 1u16, 0u8, 0u8, "half", 3u64, 10u64, 1u64, [0xAA; 32]),
        (5, 0, 1, 0, "default", 1, 4, 0, [0x55; 32]),
        (7, 2, 2, 1, "gone", 0, 0, 0, [0; 32]),
    ] {
        section.extend_from_slice(&job.to_le_bytes());
        section.extend_from_slice(&collection.to_le_bytes());
        section.extend_from_slice(&[state, dropped, name.len() as u8, 0, 0, 0]);
        for value in [chunk, committed, skipped] {
            section.extend_from_slice(&value.to_le_bytes());
        }
        section.extend_from_slice(&digest);
        section.extend_from_slice(name.as_bytes());
    }
    section.extend_from_slice(&7u64.to_le_bytes());
    section.extend_from_slice(&5u64.to_le_bytes());
    assert_eq!(section.len(), 263);
    section
}

/// A registry section with one job (ID 1) bound to `collection` named
/// `name`: open (`state` 0) or closed, the collection `dropped` or live.
fn binding_section(collection: u16, dropped: u8, state: u8, name: &str) -> Vec<u8> {
    let mut section = 1u64.to_le_bytes().to_vec();
    section.extend_from_slice(&1u32.to_le_bytes());
    section.extend_from_slice(&u32::from(state != 0).to_le_bytes());
    section.extend_from_slice(&1u64.to_le_bytes());
    section.extend_from_slice(&collection.to_le_bytes());
    section.extend_from_slice(&[state, dropped, name.len() as u8, 0, 0, 0]);
    for value in [1u64, 1, 0] {
        section.extend_from_slice(&value.to_le_bytes());
    }
    section.extend_from_slice(&[1; 32]);
    section.extend_from_slice(name.as_bytes());
    if state != 0 {
        section.extend_from_slice(&1u64.to_le_bytes());
    }
    section
}

#[derive(Clone, Copy, Debug)]
enum Backing {
    Memory,
    File,
    Directory,
}

fn open_path(path: &Path, key: Option<&EncryptionKey>) -> Result<Database> {
    match key {
        Some(key) => Database::open_encrypted(path, key),
        None => Database::open(path),
    }
}

/// The complete state stored at `path` (a closed file or directory).
fn stored(path: &Path, key: Option<&EncryptionKey>) -> Loaded {
    format::load_with(&mut maintenance::source(path).unwrap(), key).unwrap()
}

/// A new, empty precision store (test-only constructor): its complete
/// initial image is validated before it is published or installed, and a
/// file or directory is returned under the lock taken before it became
/// visible.
fn create_store(backing: Backing, path: &Path, key: Option<&EncryptionKey>) -> Database {
    let loaded = initial(key);
    let held = match backing {
        Backing::Memory => return Database::open_from_bytes(image_of(&loaded), key).unwrap(),
        Backing::File => {
            let image = image_of(&loaded);
            maintenance::publish_held(path, key, |file| Ok(file.write_all(&image)?))
                .unwrap()
                .1
        }
        Backing::Directory => {
            crate::LockedFile::chain(crate::directory::create_converted(path, &loaded).unwrap())
        }
    };
    Database::open_backed(held, key)
        .and_then(|db| db.with_durability(crate::Durability::default()))
        .unwrap()
}

/// Closes `db` and returns its stored state and a reopened handle.
fn cycle(
    backing: Backing,
    path: &Path,
    key: Option<&EncryptionKey>,
    db: Database,
) -> (Loaded, Database) {
    if let Backing::Memory = backing {
        let bytes = db.to_bytes().unwrap();
        let loaded = load(&bytes, key).unwrap();
        return (loaded, Database::open_from_bytes(bytes, key).unwrap());
    }
    db.close().unwrap();
    (stored(path, key), open_path(path, key).unwrap())
}

// ---- Codecs -------------------------------------------------------------

#[test]
fn catalog_round_trips_its_golden_section_and_rejects_damage() {
    let catalog = entries(&[(0, F32, 2), (1, F16, 3), (65535, F16, MAX_DIMENSIONS)]);
    let mut golden = vec![
        1, 0, 0, 0, 0, 0, 0, 0, // opt-in 1, reserved
        0, 0, 1, 0, 2, 0, 0, 0, // default, float32, 2
        1, 0, 2, 0, 3, 0, 0, 0, // 1, float16, 3
        0xFF, 0xFF, 2, 0, // 65535, float16
    ];
    golden.extend_from_slice(&(MAX_DIMENSIONS as u32).to_le_bytes());
    let frame = catalog_frame(9, &catalog);
    assert_eq!((frame[4], u16_at(&frame, 5), u64_at(&frame, 8)), (16, 0, 9));
    assert_eq!((u64_at(&frame, 16), u32_at(&frame, 24)), (3, 3));
    assert_eq!(u32_at(&frame, 28) as usize, HEADER_LEN + 8 + golden.len());
    assert_eq!(sections(&frame), [golden.clone()]);
    assert_eq!(decode_catalog(&golden, 3, 0).unwrap(), catalog);

    let cases: [(u32, &Edit, &str); 13] = [
        (2, &|_| {}, "corrupt"),
        (0, &|_| {}, "corrupt"),
        (3, &|s| s.truncate(s.len() - 1), "corrupt"),
        (3, &|s| s.extend_from_slice(&[0; 8]), "corrupt"),
        (3, &|s| s[0] = 2, "unsupported 32"),
        (3, &|s| s[4] = 1, "corrupt"),
        (3, &|s| s[18] = 3, "unsupported 32"),
        (3, &|s| s[18] = 0, "unsupported 32"),
        (3, &|s| s[19] = 1, "corrupt"),
        (3, &|s| s[20..24].fill(0), "corrupt"),
        (
            3,
            &|s| s[28..32].copy_from_slice(&(MAX_DIMENSIONS as u32 + 1).to_le_bytes()),
            "corrupt",
        ),
        (3, &|s| s[16..18].fill(0), "corrupt"),
        (3, &|s| s.copy_within(24..32, 16), "corrupt"),
    ];
    for (index, (count, edit, expected)) in cases.into_iter().enumerate() {
        let mut section = golden.clone();
        edit(&mut section);
        assert_eq!(
            outcome(decode_catalog(&section, count, 0)),
            expected,
            "case {index}"
        );
    }

    let full: Vec<_> = (0..MAX_COLLECTIONS as u16).map(|id| (id, F16, 1)).collect();
    let section = sections(&catalog_frame(1, &entries(&full))).remove(0);
    assert_eq!(decode_catalog(&section, 64, 0).unwrap().len(), 64);
    let mut over = section.clone();
    over.extend_from_slice(&[64, 0, 2, 0, 1, 0, 0, 0]);
    assert_eq!(outcome(decode_catalog(&over, 65, 0)), "corrupt");
}

#[test]
fn revision_sections_round_trip_and_reject_damage() {
    let mut frames = Vec::new();
    revision_frames(7, 4, &[(1, 2)], &[], |frame| {
        frames.push(frame.to_vec());
        Ok(())
    })
    .unwrap();
    let [frame] = &frames[..] else {
        panic!("one block")
    };
    assert_eq!(
        (
            frame[4],
            u16_at(frame, 5),
            u64_at(frame, 8),
            u64_at(frame, 16)
        ),
        (16, 4, 7, 5)
    );
    assert_eq!(u32_at(frame, 24), 1);
    let nodes = [
        1, 0, 0, 0, 0, 0, 0, 0, // one entry, reserved
        1, 0, 0, 0, 0, 0, 0, 0, // node 1
        2, 0, 0, 0, 0, 0, 0, 0, // revision 2
    ];
    assert_eq!(sections(frame), [nodes.to_vec(), vec![0; 8]]);

    let nodes = revision_section(&[(1, 2), (5, 7)]);
    let edges = revision_section(&[(3, 7)]);
    let decode = |nodes: &[u8], edges: &[u8], count: u32| {
        outcome(decode_revisions(&[nodes, edges], count, 7, 0))
    };
    assert_eq!(
        decode_revisions(&[&nodes, &edges], 3, 7, 0).unwrap(),
        (vec![(1, 2), (5, 7)], vec![(3, 7)])
    );
    assert_eq!(decode(&nodes, &edges, 2), "corrupt");
    assert_eq!(outcome(decode_revisions(&[&nodes], 2, 7, 0)), "corrupt");
    assert_eq!(
        outcome(decode_revisions(&[&nodes, &edges, &edges], 4, 7, 0)),
        "corrupt"
    );
    for (pairs, expected) in [
        (vec![(1, 0)], "corrupt"),
        (vec![(1, 8)], "corrupt"),
        (vec![(1, 7)], "ok"),
        (vec![(5, 1), (1, 1)], "corrupt"),
        (vec![(5, 1), (5, 1)], "corrupt"),
    ] {
        let section = revision_section(&pairs);
        assert_eq!(decode(&section, &edges, pairs.len() as u32 + 1), expected);
    }
    let mut reserved = nodes.clone();
    reserved[4] = 1;
    assert_eq!(decode(&reserved, &edges, 3), "corrupt");
    assert_eq!(decode(&nodes[..nodes.len() - 1], &edges, 3), "corrupt");
    assert_eq!(decode(&nodes[..4], &edges, 1), "corrupt");
    let many: Vec<_> = (1..=4097).map(|id| (id, 1)).collect();
    let empty = revision_section(&[]);
    assert_eq!(decode(&revision_section(&many), &empty, 4097), "corrupt");
    assert_eq!(decode(&revision_section(&many[..4096]), &empty, 4096), "ok");
    assert_eq!(
        decode(&revision_section(&many[..4096]), &edges, 4097),
        "corrupt"
    );
}

fn run_check() -> ImageCheck {
    let flags = FLAG_HISTORY | FLAG_COMPACT | FLAG_PRECISION | FLAG_GRAPH;
    let mut check = ImageCheck::new(format::V5, flags, 2, 0).unwrap();
    check.floor = Some(1);
    check
}

/// The outcome of a revision block of `graph` at partition 1 after a
/// first block.
fn continued(first: (Revs, Revs), graph: u16, next: (Revs, Revs)) -> String {
    let mut check = run_check();
    check.revision_run(3, 0, first.0, first.1, 0).unwrap();
    outcome(check.revision_run(graph, 1, next.0, next.1, 0))
}

#[test]
fn revision_runs_partition_canonically() {
    let pairs = |count: u64| (1..=count).map(|id| (id, 1)).collect::<Vec<_>>();
    for (nodes, edges, expected) in [
        (4098, 0, vec![(4096, 0), (2, 0)]),
        (4095, 3, vec![(4095, 1), (0, 2)]),
        (2, 4096, vec![(2, 4094), (0, 2)]),
        (0, 0, vec![(0, 0)]),
    ] {
        let mut check = run_check();
        let mut blocks = Vec::new();
        revision_frames(1, 3, &pairs(nodes), &pairs(edges), |frame| {
            let parts = sections(frame);
            let (nodes, edges) =
                decode_revisions(&[&parts[0], &parts[1]], u32_at(frame, 24), 1, 0)?;
            check.revision_run(3, u64_at(frame, 16) >> 16, &nodes, &edges, 0)?;
            blocks.push((nodes.len(), edges.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(blocks, expected);
    }

    let full = pairs(4096);
    let one: Revs = &[(5000, 1)];
    assert_eq!(continued((&full, &[]), 3, (one, &[])), "ok");
    // Decision P: blocks need not be full, and each class continues in its
    // own ascending order (the reference graph golden has counts 2 then 1).
    assert_eq!(continued((&full[..10], &[]), 3, (one, &[])), "ok");
    assert_eq!(continued((&[], &full), 3, (one, &[])), "ok");
    assert_eq!(continued((&full[..2], &[]), 3, (&[], &full[..1])), "ok");
    assert_eq!(continued((&full, &[]), 4, (one, &[])), "corrupt");
    assert_eq!(continued((&full, &[]), 3, (&[], &[])), "corrupt");
    assert_eq!(continued((&full, &[]), 3, (&[(4096, 1)], &[])), "corrupt");
    assert_eq!(continued((&[], &full), 3, (&[], &full[..1])), "corrupt");
    // Only an empty graph has an empty block, its only one.
    assert_eq!(continued((&[], &[]), 3, (one, &[])), "corrupt");
    assert_eq!(
        outcome(run_check().revision_run(3, 1, one, &[], 0)),
        "corrupt"
    );
}

#[test]
fn import_registry_round_trips_its_golden_section_and_rejects_damage() {
    let golden = registry_section();
    let registry = Registry::decode_image(&golden, 3).unwrap();
    assert_eq!(registry.encode_image().unwrap(), (golden.clone(), 3));
    assert_eq!(registry.max_job(), 9);
    let jobs: Vec<_> = registry
        .jobs()
        .map(|(job, state)| (job, state.collection, state.collection_dropped, state.state))
        .collect();
    assert_eq!(
        jobs,
        [
            (2, 1, false, ImportState::Open),
            (5, 0, false, ImportState::Finished),
            (7, 2, true, ImportState::Aborted),
        ]
    );
    assert_eq!(
        (registry.collection_name(2), registry.collection_name(7)),
        ("half", "gone")
    );
    let empty = Registry::default().encode_image().unwrap();
    assert_eq!(empty, (vec![0; 16], 0));
    assert_eq!(
        Registry::decode_image(&empty.0, 0).unwrap(),
        Registry::default()
    );

    fn set(at: usize, value: u64) -> Box<Edit> {
        Box::new(move |s| s[at..at + 8].copy_from_slice(&value.to_le_bytes()))
    }
    let cases: [(u32, Box<Edit>); 20] = [
        // Count, lengths.
        (2, Box::new(|_| {})),
        (3, Box::new(|s| s.truncate(15))),
        (3, Box::new(|s| s.truncate(262))),
        (3, Box::new(|s| s.push(0))),
        // State, dropped flag, an open job of a dropped collection.
        (3, Box::new(|s| s[16 + 10] = 3)),
        (3, Box::new(|s| s[171 + 11] = 2)),
        (3, Box::new(|s| s[16 + 11] = 1)),
        // Chunk 0 with rows, rows without a chunk, a digest without a
        // chunk, reserved bytes.
        (3, set(16 + 16, 0)),
        (3, set(171 + 24, 1)),
        (3, Box::new(|s| s[171 + 40] = 1)),
        (3, Box::new(|s| s[16 + 13] = 1)),
        // High water below a job, unsorted jobs, a bad name.
        (3, set(0, 6)),
        (3, set(92, 2)),
        (3, Box::new(|s| s[16 + 72] = 0xFF)),
        (3, Box::new(|s| s[16 + 12] = 0)),
        // Closed count: too few, too many; closed IDs: an open job, a
        // duplicate, an unknown job.
        (3, Box::new(|s| s[12] = 1)),
        (3, Box::new(|s| s[12] = 4)),
        (3, set(247, 2)),
        (3, set(255, 7)),
        (3, set(255, 8)),
    ];
    for (index, (count, edit)) in cases.iter().enumerate() {
        let mut section = golden.clone();
        edit(&mut section);
        assert!(
            Registry::decode_image(&section, *count).is_err(),
            "case {index}"
        );
    }
    let mut too_many = vec![0; 16];
    too_many[8..12].copy_from_slice(&1089u32.to_le_bytes());
    assert!(Registry::decode_image(&too_many, 1089).is_err());
}

#[test]
fn precise_create_frames_wrap_the_unchanged_create_body() {
    let settings = CollectionSettings::default();
    let frame = format::precise_create_frame(2, 1, "half", 3, &settings, F16);
    let legacy = format::collection_create_frame(2, 1, "half", 3, &settings);
    let create = &legacy[PREFIX_SIZE..legacy.len() - FOOTER_SIZE];
    assert_eq!(&frame[..8], b"VREC\x11\0\0\0");
    assert_eq!(
        (u64_at(&frame, 8), u64_at(&frame, 16), u32_at(&frame, 24)),
        (2, 1, 0)
    );
    assert_eq!(u32_at(&frame, 28) as usize, 8 + create.len());
    let body = &frame[PREFIX_SIZE..frame.len() - FOOTER_SIZE];
    assert_eq!(&body[..8], [1, 0, 2, 0, 0, 0, 0, 0]);
    assert_eq!(&body[8..], create);
    let footer = &frame[frame.len() - FOOTER_SIZE..];
    assert_eq!(&footer[..4], b"VCMT");
    assert_eq!(
        u32_at(footer, 4),
        format::crc32(&frame[..frame.len() - FOOTER_SIZE])
    );
    assert_eq!(u64_at(footer, 8), 2);
    let (precision, name, dimensions, decoded) = format::decode_precise(body, 0).unwrap();
    assert_eq!((precision, name.as_str(), dimensions), (F16, "half", 3));
    assert_eq!(decoded, settings);
    let f32_frame = format::precise_create_frame(2, 1, "half", 3, &settings, F32);
    assert_eq!(f32_frame[PREFIX_SIZE + 2], 1);

    for (at, value, expected) in [
        (0, 2, "unsupported 32"),
        (0, 0, "unsupported 32"),
        (1, 1, "unsupported 32"),
        (2, 0, "unsupported 32"),
        (2, 3, "unsupported 32"),
        (3, 1, "corrupt"),
        (7, 1, "corrupt"),
    ] {
        let mut damaged = body.to_vec();
        damaged[at] = value;
        assert_eq!(
            outcome(format::decode_precise(&damaged, 0)),
            expected,
            "byte {at}"
        );
    }

    assert!(format::precise_prefix_valid(&frame[..PREFIX_SIZE]));
    let prefix = |edit: &dyn Fn(&mut [u8])| {
        let mut prefix = frame[..PREFIX_SIZE].to_vec();
        edit(&mut prefix);
        format::precise_prefix_valid(&prefix)
    };
    assert!(!prefix(&|p| p[5] = 1));
    assert!(!prefix(&|p| p[7] = 1));
    assert!(!prefix(&|p| p[16..24].fill(0)));
    assert!(!prefix(
        &|p| p[16..24].copy_from_slice(&65536u64.to_le_bytes())
    ));
    assert!(prefix(
        &|p| p[16..24].copy_from_slice(&65535u64.to_le_bytes())
    ));
    assert!(!prefix(&|p| p[24] = 1));
    for (length, valid) in [(13u32, false), (14, true), (16_460, true), (16_461, false)] {
        assert_eq!(
            prefix(&|p| p[28..32].copy_from_slice(&length.to_le_bytes())),
            valid,
            "{length}"
        );
    }
}

// ---- Headers, images and the log ---------------------------------------

#[test]
fn release_builds_refuse_precision_stores() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file.vlite");
    let encrypted = directory.path().join("sealed.vlite");
    let chain = directory.path().join("chain");
    let (plain, sealed) = {
        let _gate = Activation::on();
        crate::directory::create_converted(&chain, &initial(None))
            .unwrap()
            .unlock()
            .unwrap();
        (image_of(&initial(None)), image_of(&initial(Some(&key()))))
    };
    std::fs::write(&file, &plain).unwrap();
    std::fs::write(&encrypted, &sealed).unwrap();
    assert!(!crate::compact::precision_supported());
    let refused = "unsupported 32";
    assert_eq!(
        outcome(Database::open_from_bytes(plain.clone(), None)),
        refused
    );
    // Refused before the descriptor: no key is needed.
    assert_eq!(
        outcome(Database::open_from_bytes(sealed.clone(), None)),
        refused
    );
    assert_eq!(
        outcome(Database::open_from_bytes(sealed, Some(&key()))),
        refused
    );
    assert_eq!(outcome(Database::open(&file)), refused);
    assert_eq!(outcome(Database::open(&encrypted)), refused);
    assert_eq!(outcome(Database::open(&chain)), refused);
    assert_eq!(outcome(Database::open_read_only(&chain)), refused);
    assert_eq!(outcome(maintenance::inspect(&file)), refused);
    assert_eq!(outcome(maintenance::verify(&chain)), refused);
    assert_eq!(
        outcome(logops::export_log_segment(&file, None, 1, 1 << 20)),
        refused
    );
    assert_eq!(outcome(ReplicaFollower::open(&file, None)), refused);
    let copy = directory.path().join("copy.vlite");
    assert_eq!(outcome(maintenance::backup(&file, &copy)), refused);
    assert!(!copy.exists());
    assert_eq!(std::fs::read(&file).unwrap(), plain);

    // The activation is per thread.
    let _gate = Activation::on();
    let bytes = plain.clone();
    let elsewhere = std::thread::spawn(move || outcome(Database::open_from_bytes(bytes, None)));
    assert_eq!(elsewhere.join().unwrap(), refused);
    assert_eq!(outcome(Database::open_from_bytes(plain, None)), "ok");
    assert_eq!(outcome(Database::open(&chain)), "ok");
}

#[test]
fn precision_headers_need_their_complete_image() {
    let _gate = Activation::on();
    let header = |flags: u16| format::header_v5_flags(2, LINEAGE, 0, flags, None);
    // A header without its image cannot infer the catalog it lacks.
    for flags in [
        FLAG_PRECISION | FLAG_GRAPH,
        FLAG_PRECISION,
        FLAG_PRECISION | FLAG_HISTORY | FLAG_GRAPH,
        FLAG_PRECISION | FLAG_HISTORY | FLAG_COMPACT | FLAG_GRAPH,
        FLAG_PRECISION | FLAG_TIMESTAMPS | FLAG_GRAPH,
    ] {
        assert_eq!(outcome(load(&header(flags), None)), "corrupt", "{flags}");
        assert_eq!(
            outcome(Database::open_from_bytes(header(flags), None)),
            "corrupt"
        );
    }
    // Without flag 32 the catalog is an unknown critical kind.
    let image = image_of(&initial(None));
    let mut stripped = header(FLAG_HISTORY | FLAG_COMPACT | FLAG_GRAPH);
    stripped.extend_from_slice(&image[32..]);
    assert_eq!(outcome(load(&stripped, None)), "unsupported 16");
    // Op 17 needs flag 32.
    let mut legacy = header(FLAG_GRAPH);
    legacy.extend(format::precise_create_frame(
        1,
        1,
        "half",
        3,
        &CollectionSettings::default(),
        F16,
    ));
    assert_eq!(outcome(load(&legacy, None)), "corrupt");
}

#[test]
fn initial_images_are_complete_and_every_cut_is_corrupt() {
    let _gate = Activation::on();
    let image = image_of(&initial(None));
    let (header, mut frames) = split(&image);
    assert_eq!(
        u16_at(&header, 26),
        FLAG_HISTORY | FLAG_COMPACT | FLAG_PRECISION | FLAG_GRAPH
    );
    assert_eq!((u64_at(&header, 16), u16_at(&header, 24)), (LINEAGE, 0));
    assert_eq!(
        shape(&frames),
        [
            (13, 0),
            (16, KIND_TABLE),
            (16, KIND_CATALOG),
            (16, KIND_IMPORTS),
            (10, 0),
            (16, KIND_END)
        ]
    );
    assert!(frames.iter().all(|frame| u64_at(frame, 8) == 1));
    assert_eq!(
        sections(&frames[1]),
        [vec![3, 0, 1, 0, 1, 0, 0, 0, 4, 0, 1, 0, 1, 0, 0, 0]]
    );
    assert_eq!(
        sections(&frames[2]),
        [vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 2, 0, 0, 0]]
    );
    assert_eq!(sections(&frames[3]), [vec![0; 16]]);
    let end = frames.pop().unwrap();
    let mut digest = Sha256::new();
    for frame in &frames {
        digest.update(&frame[..PREFIX_SIZE]);
    }
    let mut expected = 5u64.to_le_bytes().to_vec();
    expected.extend_from_slice(&digest.finalize());
    assert_eq!(sections(&end), [expected]);
    assert_eq!(sealed(&header, &frames), image);

    let loaded = load(&image, None).unwrap();
    assert_eq!(
        (loaded.sequence, loaded.history_floor, loaded.lineage),
        (1, 1, Some(LINEAGE))
    );
    assert_eq!(precisions(&loaded), listed(&[(0, "default", F32)]));
    assert_eq!(loaded.collection_high, 0);
    assert!(loaded.collections[&0].records.is_empty());
    for cut in 0..image.len() {
        assert_eq!(outcome(load(&image[..cut], None)), "corrupt", "cut {cut}");
    }

    let encrypted = image_of(&initial(Some(&key())));
    assert_eq!(
        u16_at(&encrypted, 26),
        FLAG_HISTORY | FLAG_COMPACT | FLAG_PRECISION | FLAG_GRAPH | FLAG_ENCRYPTED
    );
    let loaded = load(&encrypted, Some(&key())).unwrap();
    assert_eq!(precisions(&loaded), listed(&[(0, "default", F32)]));
    for cut in 0..encrypted.len() {
        assert_eq!(
            outcome(load(&encrypted[..cut], Some(&key()))),
            "corrupt",
            "cut {cut}"
        );
    }
    let other = EncryptionKey::from_bytes([6; 32]);
    assert!(matches!(
        load(&encrypted, Some(&other)),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    // Damage anywhere in the sealed frames is refused, never repaired.
    let start = 32 + crate::crypto::DESCRIPTOR_SIZE;
    for at in start..encrypted.len() {
        let mut damaged = encrypted.clone();
        damaged[at] ^= 0x40;
        assert_ne!(outcome(load(&damaged, Some(&key()))), "ok", "byte {at}");
    }
}

#[test]
fn precise_creates_replay_wholly_after_the_image() {
    let _gate = Activation::on();
    let image = image_of(&initial(None));
    let settings = CollectionSettings::default();
    let create = format::precise_create_frame(2, 1, "half", 3, &settings, F16);
    // An incomplete op 17 is a torn tail: the old catalog stays.
    for cut in 1..create.len() {
        let mut bytes = image.clone();
        bytes.extend_from_slice(&create[..cut]);
        let loaded = load(&bytes, None).unwrap();
        assert_eq!(
            (loaded.sequence, loaded.incomplete_bytes),
            (1, cut as u64),
            "cut {cut}"
        );
        assert_eq!(precisions(&loaded), listed(&[(0, "default", F32)]));
    }
    let mut torn = image.clone();
    torn.extend_from_slice(&create[..create.len() - 1]);
    assert!(matches!(
        Database::open_from_bytes(torn, None),
        Err(Error::IncompleteTail { .. })
    ));
    let mut log = image.clone();
    log.extend_from_slice(&create);
    let loaded = load(&log, None).unwrap();
    assert_eq!(
        precisions(&loaded),
        listed(&[(0, "default", F32), (1, "half", F16)])
    );
    assert_eq!(
        (
            loaded.sequence,
            loaded.history_floor,
            loaded.collection_high
        ),
        (2, 1, 1)
    );
    assert_eq!(loaded.collections[&1].dimensions, 3);

    // Settings replacement keeps the precision; a drop frees the name, not
    // the ID.
    let mut changed = settings.clone();
    changed.rerank_multiplier = 7;
    log.extend(format::settings_frame(3, 1, &changed));
    let loaded = load(&log, None).unwrap();
    assert_eq!(loaded.collections[&1].precision, Some(F16));
    assert_eq!(loaded.collections[&1].settings, changed);
    log.extend(format::collection_drop_frame(4, 1));
    log.extend(format::precise_create_frame(
        5, 2, "half", 2, &settings, F32,
    ));
    let loaded = load(&log, None).unwrap();
    assert_eq!(
        precisions(&loaded),
        listed(&[(0, "default", F32), (2, "half", F32)])
    );
    assert_eq!((loaded.sequence, loaded.collection_high), (5, 2));

    let after_image = |frames: &[Vec<u8>]| {
        let mut bytes = image.clone();
        for frame in frames {
            bytes.extend_from_slice(frame);
        }
        outcome(load(&bytes, None))
    };
    let edited = |edit: &dyn Fn(&mut Vec<u8>)| after_image(&[refit(&create, edit)]);
    assert_eq!(edited(&|_| {}), "ok");
    assert_eq!(edited(&|b| b[PREFIX_SIZE] = 2), "unsupported 32");
    assert_eq!(edited(&|b| b[PREFIX_SIZE + 2] = 3), "unsupported 32");
    assert_eq!(edited(&|b| b[PREFIX_SIZE + 3] = 1), "corrupt");
    assert_eq!(edited(&|b| b[5] = 1), "corrupt");
    assert_eq!(edited(&|b| b[7] = 1), "corrupt");
    assert_eq!(edited(&|b| b[16..24].fill(0)), "corrupt");
    assert_eq!(edited(&|b| b[24] = 1), "corrupt");
    // At or below the floor it would sit inside the image.
    assert_eq!(
        edited(&|b| b[8..16].copy_from_slice(&1u64.to_le_bytes())),
        "corrupt"
    );
    assert_eq!(edited(&|b| b.truncate(PREFIX_SIZE + 13)), "corrupt");
    assert_eq!(edited(&|b| b.push(0)), "corrupt");
    // Zero dimensions.
    assert_eq!(
        edited(&|b| {
            let at = PREFIX_SIZE + 8 + 1 + 4;
            b[at..at + 4].fill(0);
        }),
        "corrupt"
    );
    // Op 5 is image restatement only in a precision log.
    assert_eq!(
        after_image(&[format::collection_create_frame(2, 1, "half", 3, &settings)]),
        "corrupt"
    );
    let again =
        |sequence, id, name| format::precise_create_frame(sequence, id, name, 3, &settings, F16);
    assert_eq!(
        after_image(&[create.clone(), again(3, 1, "more")]),
        "corrupt"
    );
    assert_eq!(
        after_image(&[create.clone(), again(3, 2, "half")]),
        "corrupt"
    );
    assert_eq!(after_image(&[create.clone(), again(3, 2, "more")]), "ok");
}

/// A plaintext precision image at floor 2 that restates `default`
/// (float32) and `half` (ID 1, float16, three dimensions): its header and
/// frames before the end block.
fn named_image() -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut log = image_of(&initial(None));
    log.extend(format::precise_create_frame(
        2,
        1,
        "half",
        3,
        &CollectionSettings::default(),
        F16,
    ));
    let (header, mut frames) = split(&image_of(&load(&log, None).unwrap()));
    assert_eq!(
        frames.pop().map(|end| u64_at(&end, 16)),
        Some(u64::from(KIND_END))
    );
    (header, frames)
}

#[test]
fn precision_images_need_one_exact_catalog_and_registry() {
    let _gate = Activation::on();
    let (header, frames) = named_image();
    assert_eq!(
        shape(&frames),
        [
            (13, 0),
            (5, 0),
            (16, KIND_TABLE),
            (16, KIND_CATALOG),
            (16, KIND_IMPORTS),
            (10, 0),
            (10, 0)
        ]
    );
    assert_eq!(u16_at(&header, 24), 1);
    let check = |frames: Vec<Vec<u8>>| outcome(load(&sealed(&header, &frames), None));
    assert_eq!(check(frames.clone()), "ok");
    let loaded = load(&sealed(&header, &frames), None).unwrap();
    assert_eq!(
        precisions(&loaded),
        listed(&[(0, "default", F32), (1, "half", F16)])
    );

    let with = |index: usize, frame: Vec<u8>| {
        let mut frames = frames.clone();
        frames[index] = frame;
        check(frames)
    };
    let insert = |index: usize, frame: Vec<u8>| {
        let mut frames = frames.clone();
        frames.insert(index, frame);
        check(frames)
    };
    let without = |index: usize| {
        let mut frames = frames.clone();
        frames.remove(index);
        check(frames)
    };
    let catalog = |list: &[(u16, Precision, usize)]| catalog_frame(2, &entries(list));
    let golden = sections(&frames[3]).remove(0);
    let block = |owner: u16, partition: u64, count: u32, parts: &[&[u8]]| {
        block_frame(2, KIND_CATALOG, owner, partition, count, parts)
    };
    let table = |list: &[(u16, u16, u16)]| {
        let mut section = Vec::new();
        for &(kind, version, flags) in list {
            for field in [kind, version, flags, 0] {
                section.extend_from_slice(&field.to_le_bytes());
            }
        }
        block_frame(2, KIND_TABLE, 0, 0, list.len() as u32, &[&section])
    };
    let corrupt = "corrupt";

    // Exactly one catalog, right after the table, then the registry.
    assert_eq!(without(3), corrupt);
    assert_eq!(without(4), corrupt);
    assert_eq!(insert(4, frames[3].clone()), corrupt);
    assert_eq!(insert(5, frames[4].clone()), corrupt);
    let mut swapped = frames.clone();
    swapped.swap(3, 4);
    assert_eq!(check(swapped), corrupt);
    let mut late = frames.clone();
    let moved = late.remove(3);
    late.insert(5, moved);
    assert_eq!(check(late), corrupt);
    assert_eq!(insert(3, frames[5].clone()), corrupt);
    // It lists exactly the restated collections and their dimensions.
    assert_eq!(with(3, catalog(&[(0, F32, 2)])), corrupt);
    assert_eq!(
        with(3, catalog(&[(0, F32, 2), (1, F16, 3), (2, F16, 3)])),
        corrupt
    );
    assert_eq!(with(3, catalog(&[(0, F32, 2), (1, F16, 4)])), corrupt);
    assert_eq!(with(3, catalog(&[(0, F32, 3), (1, F16, 3)])), corrupt);
    assert_eq!(with(3, catalog(&[(0, F16, 2), (1, F32, 3)])), "ok");
    let mut opt_in = golden.clone();
    opt_in[0] = 2;
    assert_eq!(with(3, block(0, 0, 2, &[&opt_in])), "unsupported 32");
    assert_eq!(with(3, block(1, 0, 2, &[&golden])), corrupt);
    assert_eq!(with(3, block(0, 1, 2, &[&golden])), corrupt);
    assert_eq!(with(3, block(0, 0, 2, &[&golden, &golden])), corrupt);
    assert_eq!(with(3, block(0, 0, 2, &[&vec![0; 2000]])), corrupt);
    // Block flags and version must match the critical v1 table entry.
    assert_eq!(
        with(3, refit(&frames[3], |b| b[PREFIX_SIZE + 4] = 0)),
        corrupt
    );
    assert_eq!(
        with(3, refit(&frames[3], |b| b[PREFIX_SIZE + 2] = 2)),
        corrupt
    );
    // The table declares both storage kinds, critical, version 1 only.
    assert_eq!(
        with(2, table(&[(3, 2, CRITICAL), (4, 1, CRITICAL)])),
        "unsupported 16"
    );
    assert_eq!(with(2, table(&[(3, 1, CRITICAL)])), corrupt);
    assert_eq!(with(2, table(&[(4, 1, CRITICAL)])), corrupt);
    assert_eq!(with(2, table(&[(3, 1, 0), (4, 1, CRITICAL)])), corrupt);
    assert_eq!(with(2, table_frame(2, true)), corrupt);
    // No collection restatement after the table, and no op 17 in the image.
    let settings = CollectionSettings::default();
    assert_eq!(
        insert(
            3,
            format::collection_create_frame(2, 2, "late", 2, &settings)
        ),
        corrupt
    );
    assert_eq!(
        insert(
            2,
            format::precise_create_frame(1, 2, "late", 2, &settings, F16)
        ),
        corrupt
    );
    assert_eq!(
        insert(
            2,
            format::precise_create_frame(2, 2, "late", 2, &settings, F16)
        ),
        corrupt
    );

    // The registry decodes, and binds only live or dropped collections.
    let imports = |section: &[u8], count| block_frame(2, KIND_IMPORTS, 0, 0, count, &[section]);
    assert_eq!(with(4, imports(&[0; 15], 0)), corrupt);
    assert_eq!(with(4, imports(&[0; 16], 0)), "ok");
    let bound = |collection: u16, dropped: u8, state: u8, name: &str| {
        with(
            4,
            imports(&binding_section(collection, dropped, state, name), 1),
        )
    };
    assert_eq!(bound(1, 0, 0, "half"), "ok");
    assert_eq!(bound(0, 0, 1, "default"), "ok");
    assert_eq!(bound(1, 0, 0, "other"), corrupt);
    assert_eq!(bound(5, 0, 0, "half"), corrupt);
    assert_eq!(bound(1, 1, 1, "half"), corrupt);
    assert_eq!(bound(2, 1, 1, "gone"), corrupt);
    assert_eq!(bound(0, 1, 1, "default"), corrupt);
}

#[test]
fn graph_revisions_survive_a_precision_image() {
    let _gate = Activation::on();
    let mut db = Database::open_from_bytes(image_of(&initial(None)), None).unwrap();
    db.create_graph("g").unwrap();
    let node = |index: u32| GraphOp::CreateNode {
        key: format!("n{index}"),
        labels: Vec::new(),
        properties: Metadata::new(),
    };
    {
        let mut graph = db.graph_mut("g").unwrap();
        graph
            .write_batch(&(0..4000).map(node).collect::<Vec<_>>())
            .unwrap();
        graph
            .write_batch(&(4000..4097).map(node).collect::<Vec<_>>())
            .unwrap();
        graph
            .create_edge(Some("e"), "n0", "knows", "n1", &Metadata::new())
            .unwrap();
        graph
            .upsert_node("n1", &["x"], &Metadata::new(), Expected::Any)
            .unwrap();
        graph
            .set_node_expiry("n2", Some(FAR), Expected::Any)
            .unwrap();
    }
    let observe = |db: &Database| {
        let graph = db.graph("g").unwrap();
        let nodes = ["n0", "n1", "n2", "n4096"].map(|key| {
            let view = graph.get_node(key).unwrap().output.unwrap();
            (view.revision, view.expires_at_ms)
        });
        let edge = graph.get_edge_by_key("e").unwrap().output.unwrap().revision;
        (nodes, edge)
    };
    let before = observe(&db);
    let loaded = load(&db.to_bytes().unwrap(), None).unwrap();
    let floor = loaded.sequence;
    assert!(before.0[0].0 < floor && before.1 < floor);
    let image = image_of(&loaded);
    let reopened = Database::open_from_bytes(image.clone(), None).unwrap();
    assert_eq!(observe(&reopened), before);
    assert_eq!(reopened.sequence, floor);

    let (header, mut frames) = split(&image);
    frames.pop();
    let runs: Vec<usize> = (0..frames.len())
        .filter(|&index| shape(&frames[index..=index])[0] == (16, KIND_REVISIONS))
        .collect();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1], runs[0] + 1);
    assert_eq!(frames[runs[0] - 1][4], 15);
    let graph = u16_at(&frames[runs[0]], 5);
    let (mut nodes, mut edges) = (Vec::new(), Vec::new());
    for &index in &runs {
        let parts = sections(&frames[index]);
        let count = u32_at(&frames[index], 24);
        let (more_nodes, more_edges) =
            decode_revisions(&[&parts[0], &parts[1]], count, floor, 0).unwrap();
        nodes.extend(more_nodes);
        edges.extend(more_edges);
    }
    assert_eq!((nodes.len(), edges.len()), (4097, 1));
    let replaced = |run: Vec<Vec<u8>>| {
        let mut frames = frames.clone();
        frames.splice(runs[0]..=runs[1], run);
        outcome(load(&sealed(&header, &frames), None))
    };
    let rerun = |owner: u16, nodes: Revs, edges: Revs| {
        let mut run = Vec::new();
        revision_frames(floor, owner, nodes, edges, |frame| {
            run.push(frame.to_vec());
            Ok(())
        })
        .unwrap();
        run
    };
    let corrupt = "corrupt";
    assert_eq!(replaced(rerun(graph, &nodes, &edges)), "ok");
    assert_eq!(replaced(rerun(graph, &nodes[1..], &edges)), corrupt);
    assert_eq!(replaced(rerun(graph, &nodes, &[])), corrupt);
    let mut extra = nodes.clone();
    extra.push((1 << 40, 1));
    assert_eq!(replaced(rerun(graph, &extra, &edges)), corrupt);
    let mut late = nodes.clone();
    late[0].1 = floor + 1;
    assert_eq!(replaced(rerun(graph, &late, &edges)), corrupt);
    late[0].1 = 0;
    assert_eq!(replaced(rerun(graph, &late, &edges)), corrupt);
    // Revisions are not derivable from the snapshot: the run is their only
    // record, restored as written.
    let mut changed = nodes.clone();
    changed[0].1 = if changed[0].1 == 1 { 2 } else { 1 };
    let mut frames_changed = frames.clone();
    frames_changed.splice(runs[0]..=runs[1], rerun(graph, &changed, &edges));
    let restored = Database::open_from_bytes(sealed(&header, &frames_changed), None).unwrap();
    assert_eq!(observe(&restored).0[0].0, changed[0].1);
    assert_eq!(replaced(Vec::new()), corrupt);
    assert_eq!(replaced(rerun(graph + 1, &nodes, &edges)), corrupt);
    // Readers accept a block that is not full when the run still covers
    // every entry exactly once, in order.
    let short = |split: usize, next: u64| {
        vec![
            block_frame(
                floor,
                KIND_REVISIONS,
                graph,
                0,
                split as u32,
                &[&revision_section(&nodes[..split]), &revision_section(&[])],
            ),
            block_frame(
                floor,
                KIND_REVISIONS,
                graph,
                next,
                (nodes.len() - split + edges.len()) as u32,
                &[
                    &revision_section(&nodes[split..]),
                    &revision_section(&edges),
                ],
            ),
        ]
    };
    assert_eq!(replaced(short(4095, 1)), "ok");
    assert_eq!(replaced(short(2, 1)), "ok");
    assert_eq!(replaced(short(1, 1)), corrupt);
    assert_eq!(replaced(short(4095, 2)), corrupt);
    // A revision run right after the registry is out of order.
    let mut early = frames.clone();
    let run: Vec<_> = early.drain(runs[0]..=runs[1]).collect();
    early.splice(4..4, run);
    assert_eq!(outcome(load(&sealed(&header, &early), None)), corrupt);
}

#[test]
fn import_registry_restores_closed_jobs_and_its_high_water() {
    let _gate = Activation::on();
    let settings = CollectionSettings::default();
    let mut log = image_of(&initial(None));
    for frame in [
        format::precise_create_frame(2, 1, "half", 3, &settings, F16),
        format::precise_create_frame(3, 2, "gone", 3, &settings, F32),
        format::collection_drop_frame(4, 2),
    ] {
        log.extend(frame);
    }
    let mut loaded = load(&log, None).unwrap();
    let registry = Registry::decode_image(&registry_section(), 3).unwrap();
    loaded.imports = vec![LoadedImportMarker::Image(Box::new(registry))];
    let image = image_of(&loaded);
    let (_, frames) = split(&image);
    let imports = frames
        .iter()
        .find(|frame| frame[4] == 16 && u64_at(frame, 16) == u64::from(KIND_IMPORTS))
        .unwrap();
    assert_eq!(u32_at(imports, 24), 3);
    assert_eq!(sections(imports), [registry_section()]);
    // No open-job markers besides the registry.
    assert!(!frames.iter().any(|frame| frame[4] == 3));

    let mut db = Database::open_from_bytes(image.clone(), None).unwrap();
    let open = db.import_status(2).unwrap();
    assert_eq!(
        (
            open.collection.as_str(),
            open.collection_dropped,
            open.state
        ),
        ("half", false, ImportState::Open)
    );
    assert_eq!(
        (open.last_chunk, open.rows_committed, open.rows_skipped),
        (3, 10, 1)
    );
    assert_eq!(open.last_digest, [0xAA; 32]);
    let finished = db.import_status(5).unwrap();
    assert_eq!(
        (finished.collection.as_str(), finished.state),
        ("default", ImportState::Finished)
    );
    let gone = db.import_status(7).unwrap();
    assert_eq!(
        (
            gone.collection.as_str(),
            gone.collection_dropped,
            gone.state
        ),
        ("gone", true, ImportState::Aborted)
    );
    assert_eq!(db.next_import_job_id().unwrap(), 10);
    // Job 3 never ran here, but it is at or below the high water.
    let put = |id| Mutation::Put {
        id,
        vector: vec![0.5; 2],
        payload: Vec::new(),
    };
    assert!(matches!(
        db.import_chunk(3, 1, &[put(1)], POLICY),
        Err(Error::ImportConflict(_))
    ));
    let report = db.import_chunk(10, 1, &[put(1)], POLICY).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert_eq!(db.import_status(10).unwrap().rows_committed, 1);

    // A suffix marker reusing an ID at or below the high water is corrupt.
    let marker = |job| {
        format::batch_frame_v4(
            5,
            0,
            &[put(1)],
            Some(&ImportMarker {
                job,
                chunk: 1,
                rows_committed: 1,
                rows_skipped: 0,
                flags: 0,
                digest: [1; 32],
            }),
        )
    };
    let mut reused = image.clone();
    reused.extend(marker(3));
    assert!(matches!(
        load(&reused, None),
        Err(Error::Corrupt {
            reason: "import job ID reused",
            ..
        })
    ));
    let mut fresh = image;
    fresh.extend(marker(10));
    assert_eq!(load(&fresh, None).unwrap().sequence, 5);
}

/// The registry section of `open` open `default` jobs (IDs `1..=open`,
/// one chunk each) and no closed one.
fn open_jobs_section(open: u64) -> Vec<u8> {
    let mut section = open.to_le_bytes().to_vec();
    section.extend_from_slice(&(open as u32).to_le_bytes());
    section.extend_from_slice(&0u32.to_le_bytes());
    for job in 1..=open {
        section.extend_from_slice(&job.to_le_bytes());
        // Collection 0, open, not dropped, a 7-byte name.
        section.extend_from_slice(&[0, 0, 0, 0, 7, 0, 0, 0]);
        for value in [1u64, 1, 0] {
            section.extend_from_slice(&value.to_le_bytes());
        }
        section.extend_from_slice(&[1; 32]);
        section.extend_from_slice(b"default");
    }
    section
}

/// One chunk (or, with `flags`, the close) of `job` in the default
/// collection, framed at `sequence`.
fn job_frame(sequence: u64, job: u64, flags: u32) -> Vec<u8> {
    let closing = flags != 0;
    let rows = if closing {
        Vec::new()
    } else {
        vec![Mutation::Put {
            id: job,
            vector: vec![0.5; 2],
            payload: Vec::new(),
        }]
    };
    let marker = ImportMarker {
        job,
        chunk: 1,
        rows_committed: rows.len() as u64,
        rows_skipped: 0,
        flags,
        digest: if closing { [0; 32] } else { [1; 32] },
    };
    format::batch_frame_v4(sequence, 0, &rows, Some(&marker))
}

/// `import_chunk` never opens a 65th job (`TooManyImports`, the existing
/// `MAX_OPEN_IMPORTS` bound); no registry image or precision suffix may
/// hold more.
#[test]
fn import_registries_hold_at_most_64_open_jobs() {
    let _gate = Activation::on();
    let markers = |open: u64| -> Vec<LoadedImportMarker> {
        (1..=open)
            .map(|job| LoadedImportMarker::Marker {
                collection: 0,
                name: "default".into(),
                marker: ImportMarker {
                    job,
                    chunk: 1,
                    rows_committed: 1,
                    rows_skipped: 0,
                    flags: 0,
                    digest: [1; 32],
                },
            })
            .collect()
    };
    let full = Registry::from_markers(&markers(64));
    assert_eq!(full.encode_image().unwrap(), (open_jobs_section(64), 64));
    assert_eq!(Registry::decode_image(&open_jobs_section(64), 64), Ok(full));
    assert!(Registry::from_markers(&markers(65)).encode_image().is_err());
    assert_eq!(
        Registry::decode_image(&open_jobs_section(65), 65),
        Err("invalid import registry")
    );

    let loaded = initial(None);
    let image = |open: u64| {
        let section = open_jobs_section(open);
        let block = block_frame(1, KIND_IMPORTS, 0, 0, open as u32, &[&section]);
        replaced(&loaded, KIND_IMPORTS, 1, vec![block])
    };
    assert_eq!(outcome(load(&image(65), None)), "corrupt");
    let full = image(64);
    let mut db = Database::open_from_bytes(full.clone(), None).unwrap();
    let put = [Mutation::Put {
        id: 1,
        vector: vec![0.5; 2],
        payload: Vec::new(),
    }];
    assert!(matches!(
        db.import_chunk(65, 1, &put, POLICY),
        Err(Error::TooManyImports)
    ));
    db.finish_import(1).unwrap();
    db.import_chunk(65, 1, &put, POLICY).unwrap();
    assert_eq!(outcome(load(&db.to_bytes().unwrap(), None)), "ok");

    // The suffix after the image keeps the bound: a 65th open job is
    // corrupt, one after a close is not.
    let mut over = full.clone();
    over.extend(job_frame(2, 65, 0));
    assert_eq!(outcome(load(&over, None)), "corrupt");
    let mut closed = full;
    closed.extend(job_frame(2, 1, format::MARKER_FINISHED));
    closed.extend(job_frame(3, 65, 0));
    assert_eq!(outcome(load(&closed, None)), "ok");
    closed.extend(job_frame(4, 66, 0));
    assert_eq!(outcome(load(&closed, None)), "corrupt");
}

// ---- Stores -------------------------------------------------------------

/// Named creates, settings replacement, drop and re-create on one backing:
/// immediate, reopened and (for directories) checkpointed state.
fn lifecycle(backing: Backing, key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let key = key.as_ref();
    let mut db = create_store(backing, &path, key);
    assert_eq!((db.sequence, db.lineage), (1, Some(LINEAGE)));
    assert_eq!(db.create_collection_as("half", spec(3), F16).unwrap().id, 1);
    assert_eq!(db.create_collection("full", spec(2)).unwrap().id, 2);
    db.put(1, &[1.0, 2.0], b"d").unwrap();
    db.collection_mut("full")
        .unwrap()
        .put(7, &[0.5, 0.25], b"f")
        .unwrap();
    let changed = CollectionSettings {
        rerank_multiplier: 7,
        ..CollectionSettings::default()
    };
    db.set_collection_settings("half", changed.clone()).unwrap();
    assert_eq!(db.create_collection_as("temp", spec(2), F16).unwrap().id, 3);
    db.drop_collection("temp").unwrap();
    assert_eq!(db.create_collection_as("temp", spec(2), F16).unwrap().id, 4);
    // Rejected creates write nothing.
    assert!(matches!(
        db.create_collection_as("half", spec(3), F32),
        Err(Error::CollectionExists)
    ));
    assert!(matches!(
        db.create_collection_as("default", spec(2), F16),
        Err(Error::DefaultCollectionImmutable)
    ));
    assert!(db.create_collection_as("bad", spec(0), F16).is_err());
    assert!(db.create_collection_as("bad name!", spec(2), F16).is_err());
    assert_eq!(db.sequence, 9);

    let expected = listed(&[
        (0, "default", F32),
        (1, "half", F16),
        (2, "full", F32),
        (4, "temp", F16),
    ]);
    let flags = FLAG_HISTORY
        | FLAG_COMPACT
        | FLAG_PRECISION
        | FLAG_GRAPH
        | if key.is_some() { FLAG_ENCRYPTED } else { 0 };
    let (loaded, mut db) = cycle(backing, &path, key, db);
    assert_eq!(precisions(&loaded), expected);
    assert_eq!(
        (loaded.sequence, loaded.history_floor, loaded.lineage),
        (9, 1, Some(LINEAGE))
    );
    assert_eq!((loaded.collection_high, loaded.flags), (4, flags));
    assert_eq!(loaded.collections[&1].settings, changed);
    assert_eq!(db.sequence, 9);
    assert_eq!(db.get(1).unwrap().unwrap().vector, [1.0, 2.0]);
    assert_eq!(
        db.collection("full")
            .unwrap()
            .get(7)
            .unwrap()
            .unwrap()
            .vector,
        [0.5, 0.25]
    );

    if let Backing::Directory = backing {
        // A checkpoint restates the catalog at its floor; later named
        // creates leave lineage and floor alone.
        assert_eq!(db.checkpoint().unwrap(), Some(9));
        assert_eq!(db.create_collection_as("late", spec(2), F16).unwrap().id, 5);
        let (loaded, reopened) = cycle(backing, &path, key, db);
        let mut later = expected.clone();
        later.push((5, "late".into(), Some(F16)));
        assert_eq!(precisions(&loaded), later);
        assert_eq!(
            (loaded.sequence, loaded.history_floor, loaded.lineage),
            (10, 9, Some(LINEAGE))
        );
        db = reopened;
    }

    // A wrong key is refused before any catalog or record is used.
    if key.is_some() {
        let other = EncryptionKey::from_bytes([6; 32]);
        let refused = match backing {
            Backing::Memory => Database::open_from_bytes(db.to_bytes().unwrap(), Some(&other)),
            _ => {
                db.close().unwrap();
                open_path(&path, Some(&other))
            }
        };
        assert!(matches!(refused, Err(Error::EncryptionKeyMismatch { .. })));
    }
}

#[test]
fn memory_precision_stores_keep_their_catalog() {
    lifecycle(Backing::Memory, None);
    lifecycle(Backing::Memory, Some(key()));
}

#[test]
fn file_precision_stores_keep_their_catalog() {
    lifecycle(Backing::File, None);
    lifecycle(Backing::File, Some(key()));
}

#[test]
fn directory_precision_stores_keep_their_catalog() {
    lifecycle(Backing::Directory, None);
    lifecycle(Backing::Directory, Some(key()));
}

#[test]
fn legacy_stores_create_only_float32_collections() {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    let head = db.sequence;
    assert_eq!(
        outcome(db.create_collection_as("half", spec(3), F16)),
        "unsupported 32"
    );
    assert_eq!(db.sequence, head);
    db.create_collection_as("full", spec(3), F32).unwrap();
    db.close().unwrap();
    let loaded = stored(&path, None);
    assert_eq!(loaded.flags & FLAG_PRECISION, 0);
    assert_eq!(loaded.collections[&1].precision, None);
}

/// Outcomes of every dense write path into the empty collection `name`,
/// and whether any of them committed.
fn dense_writes(db: &mut Database, name: &str) -> (Vec<String>, bool) {
    if db.graph("g").is_err() {
        db.create_graph("g").unwrap();
    }
    let head = db.sequence;
    let vector = vec![0.5; db.collection(name).unwrap().dimensions()];
    let put = |id| Mutation::Put {
        id,
        vector: vector.clone(),
        payload: Vec::new(),
    };
    let none = Metadata::new();
    let mut outcomes = Vec::new();
    let mut collection = db.collection_mut(name).unwrap();
    outcomes.push(outcome(collection.put(1, &vector, b"")));
    outcomes.push(outcome(
        collection.put_with_metadata(2, &vector, b"", &none),
    ));
    outcomes.push(outcome(collection.put_with_expiry(
        3,
        &vector,
        b"",
        &none,
        Some(FAR),
    )));
    outcomes.push(outcome(collection.write_batch(&[put(4)])));
    outcomes.push(outcome(collection.write_batch(&[
        Mutation::PutWithMetadata {
            id: 5,
            vector: vector.clone(),
            payload: Vec::new(),
            metadata: none.clone(),
        },
    ])));
    for policy in [POLICY, InvalidRowPolicy::SkipInvalid] {
        let job = db.next_import_job_id().unwrap();
        let mut collection = db.collection_mut(name).unwrap();
        outcomes.push(outcome(collection.import_chunk(job, 1, &[put(6)], policy)));
    }
    let vector_op = GraphOp::Vector {
        collection: name.into(),
        mutation: put(7),
    };
    outcomes.push(outcome(
        db.graph_mut("g").unwrap().write_batch(&[vector_op]),
    ));
    (outcomes, db.sequence != head)
}

fn committed() -> (Vec<String>, bool) {
    (vec!["ok".to_owned(); 8], true)
}

/// A collection's stored precision is fixed with it: op 17 installs it,
/// settings replacement keeps it, drop removes it with the collection, and
/// reopening restores it. Every dense write path commits into float16 and
/// float32 collections alike, a float16 one storing each value rounded
/// before its commit (`tests/canonical.rs`).
fn runtime_precision(backing: Backing, key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let key = key.as_ref();
    let mut db = create_store(backing, &path, key);
    db.create_collection_as("half", spec(2), F16).unwrap();
    db.create_collection_as("full", spec(2), F32).unwrap();
    let first_job = db.next_import_job_id().unwrap();
    assert_eq!(dense_writes(&mut db, "half"), committed(), "{backing:?}");
    let changed = CollectionSettings {
        rerank_multiplier: 7,
        ..CollectionSettings::default()
    };
    db.set_collection_settings("half", changed).unwrap();
    assert_eq!(dense_writes(&mut db, "half"), committed());
    let jobs = first_job..db.next_import_job_id().unwrap();
    // Deletes and metadata-only writes carry no dense vector.
    let mut half = db.collection_mut("half").unwrap();
    half.update_metadata(2, &Metadata::new()).unwrap();
    assert!(half.delete(1).unwrap());
    half.write_batch(&[Mutation::Delete { id: 1 }]).unwrap();
    // Only an empty collection without open imports is dropped.
    for id in 2..=7 {
        assert!(half.delete(id).unwrap());
    }
    for job in jobs {
        half.abort_import(job).unwrap();
    }
    // A re-created name is a new collection with its own precision.
    db.drop_collection("half").unwrap();
    db.create_collection_as("half", spec(2), F32).unwrap();
    db.create_collection_as("temp", spec(2), F16).unwrap();
    assert_eq!(dense_writes(&mut db, "half"), committed());

    let (loaded, mut db) = cycle(backing, &path, key, db);
    assert_eq!(
        precisions(&loaded),
        listed(&[
            (0, "default", F32),
            (2, "full", F32),
            (3, "half", F32),
            (4, "temp", F16)
        ])
    );
    assert_eq!(dense_writes(&mut db, "temp"), committed());
    assert_eq!(dense_writes(&mut db, "full"), committed());
}

#[test]
fn memory_precision_is_fixed_at_runtime() {
    runtime_precision(Backing::Memory, None);
    runtime_precision(Backing::Memory, Some(key()));
}

#[test]
fn file_precision_is_fixed_at_runtime() {
    runtime_precision(Backing::File, None);
    runtime_precision(Backing::File, Some(key()));
}

#[test]
fn directory_precision_is_fixed_at_runtime() {
    runtime_precision(Backing::Directory, None);
    runtime_precision(Backing::Directory, Some(key()));
}

/// A directory checkpoint keeps the catalog, every import job (open,
/// finished, aborted, dropped) with its high water, and original graph
/// revisions and deadlines.
fn checkpoint_preserves(key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let key = key.as_ref();
    let mut db = create_store(Backing::Directory, &path, key);
    db.create_collection_as("half", spec(2), F16).unwrap();
    db.create_collection("gone", spec(2)).unwrap();
    let put = |id| {
        vec![Mutation::Put {
            id,
            vector: vec![1.0, 0.0],
            payload: Vec::new(),
        }]
    };
    let finished = db.next_import_job_id().unwrap();
    let mut gone = db.collection_mut("gone").unwrap();
    gone.import_chunk(finished, 1, &put(1), POLICY).unwrap();
    gone.finish_import(finished).unwrap();
    let aborted = db.next_import_job_id().unwrap();
    db.import_chunk(aborted, 1, &put(10), POLICY).unwrap();
    db.abort_import(aborted).unwrap();
    let dropped = db.next_import_job_id().unwrap();
    let mut gone = db.collection_mut("gone").unwrap();
    gone.import_chunk(dropped, 1, &put(2), POLICY).unwrap();
    gone.finish_import(dropped).unwrap();
    gone.delete(1).unwrap();
    gone.delete(2).unwrap();
    db.drop_collection("gone").unwrap();
    let open = db.next_import_job_id().unwrap();
    db.import_chunk(open, 1, &put(11), POLICY).unwrap();
    db.create_graph("g").unwrap();
    {
        let mut graph = db.graph_mut("g").unwrap();
        let none = Metadata::new();
        graph.create_node("a", &[], &none).unwrap();
        graph.create_node("b", &[], &none).unwrap();
        graph
            .create_edge(Some("e"), "a", "knows", "b", &none)
            .unwrap();
        graph
            .upsert_node("a", &["x"], &none, Expected::Any)
            .unwrap();
        graph
            .set_node_expiry("b", Some(FAR), Expected::Any)
            .unwrap();
    }
    let jobs = [finished, aborted, dropped, open];
    let observe = |db: &Database| {
        let graph = db.graph("g").unwrap();
        let node = |key| {
            let view = graph.get_node(key).unwrap().output.unwrap();
            (view.revision, view.expires_at_ms)
        };
        let edge = graph.get_edge_by_key("e").unwrap().output.unwrap().revision;
        (
            jobs.map(|job| db.import_status(job).unwrap()),
            db.next_import_job_id().unwrap(),
            [node("a"), node("b")],
            edge,
        )
    };
    let before = observe(&db);
    assert!(before.0[0].collection_dropped && before.0[2].collection_dropped);
    let floor = db.checkpoint().unwrap().unwrap();
    assert_eq!(floor, db.sequence);
    assert!(before.2[0].0 < floor && before.3 < floor);
    assert_eq!(before.2[1].1, Some(FAR));
    assert_eq!(observe(&db), before);
    db.close().unwrap();
    let loaded = stored(&path, key);
    assert_eq!(loaded.history_floor, floor);
    assert_eq!(
        precisions(&loaded),
        listed(&[(0, "default", F32), (1, "half", F16)])
    );

    let mut db = open_path(&path, key).unwrap();
    assert_eq!(observe(&db), before);
    // The open job continues; closed jobs stay closed.
    let report = db.import_chunk(open, 2, &put(12), POLICY).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    assert!(db.import_chunk(aborted, 2, &put(13), POLICY).is_err());
    assert_eq!(db.next_import_job_id().unwrap(), before.1);
    db.create_collection_as("late", spec(2), F16).unwrap();
    let second = db.checkpoint().unwrap().unwrap();
    db.close().unwrap();
    let db = open_path(&path, key).unwrap();
    assert_eq!(db.import_status(open).unwrap().last_chunk, 2);
    assert_eq!(db.import_status(dropped).unwrap(), before.0[2]);
    assert_eq!(db.import_status(finished).unwrap(), before.0[0]);
    assert_eq!(db.import_status(aborted).unwrap(), before.0[1]);
    let after = observe(&db);
    assert_eq!((after.2, after.3), (before.2, before.3));
    db.close().unwrap();
    let loaded = stored(&path, key);
    assert_eq!(loaded.history_floor, second);
    assert_eq!(
        precisions(&loaded),
        listed(&[(0, "default", F32), (1, "half", F16), (3, "late", F16)])
    );
}

#[test]
fn checkpoints_preserve_imports_and_graph_revisions() {
    checkpoint_preserves(None);
    checkpoint_preserves(Some(key()));
}

// ---- Maintenance and transport ------------------------------------------

/// Byte copies and frame transcoders keep the catalog; rewrites that would
/// drop it refuse before any output.
fn maintenance_keeps_or_refuses(key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let destination = |name: &str| directory.path().join(name);
    let key = key.as_ref();
    let mut db = create_store(Backing::File, &source, key);
    db.create_collection_as("half", spec(3), F16).unwrap();
    db.put(1, &[1.0, 2.0], b"a").unwrap();
    // Deadline 0: valid and already expired.
    db.put_with_expiry(9, &[0.0, 1.0], b"z", &Metadata::new(), Some(0))
        .unwrap();
    db.close().unwrap();
    let original = std::fs::read(&source).unwrap();
    let expected = listed(&[(0, "default", F32), (1, "half", F16)]);
    let keyed = |path: &Path, key: Option<&EncryptionKey>| {
        let loaded = stored(path, key);
        assert_eq!(precisions(&loaded), expected, "{}", path.display());
        loaded
    };
    let backup = |name: &str| match key {
        Some(key) => maintenance::backup_with_key(&source, destination(name), key),
        None => maintenance::backup(&source, destination(name)),
    };

    // Backup is a byte copy that keeps even a deadline-0 record.
    backup("backup").unwrap();
    assert_eq!(std::fs::read(destination("backup")).unwrap(), original);
    let copied = keyed(&destination("backup"), key);
    assert_eq!(copied.collections[&0].expiries.get(&9), Some(&0));
    // A second backup never replaces the first.
    assert!(backup("backup").is_err());
    assert_eq!(std::fs::read(destination("backup")).unwrap(), original);

    // Restore: a new lineage, the same flags and catalog.
    match key {
        Some(key) => maintenance::restore_with_key(&source, destination("restored"), key),
        None => maintenance::restore(&source, destination("restored")),
    }
    .unwrap();
    let restored = keyed(&destination("restored"), key);
    assert_ne!(restored.lineage, Some(LINEAGE));
    assert_eq!(restored.flags, copied.flags);

    // Re-encryption transcodes every frame under the new key.
    let other = EncryptionKey::from_bytes([9; 32]);
    logops::reencrypt(&source, key, destination("rekeyed"), Some(&other)).unwrap();
    let rekeyed = keyed(&destination("rekeyed"), Some(&other));
    assert_eq!(rekeyed.flags, copied.flags | FLAG_ENCRYPTED);
    assert_eq!(rekeyed.collections[&0].expiries.get(&9), Some(&0));
    logops::reencrypt(&source, key, destination("plain"), None).unwrap();
    assert_eq!(
        keyed(&destination("plain"), None).flags,
        copied.flags & !FLAG_ENCRYPTED
    );

    // Point-in-time restore from the floor on.
    let to = |name: &str, sequence| {
        logops::restore_to(
            None,
            &source,
            destination(name),
            RestoreTarget::Sequence(sequence),
            key,
        )
    };
    to("head", 4).unwrap();
    keyed(&destination("head"), key);
    to("floor", 1).unwrap();
    assert_eq!(
        precisions(&stored(&destination("floor"), key)),
        listed(&[(0, "default", F32)])
    );
    assert!(matches!(
        to("before", 0),
        Err(Error::InvalidRestoreTarget(_))
    ));
    assert!(!destination("before").exists());

    // Directory conversion relocates the store under its lineage and keeps
    // everything it retains, even a record that is already expired.
    let convert = || match key {
        Some(key) => maintenance::convert_to_directory_with_key(&source, destination("chain"), key),
        None => maintenance::convert_to_directory(&source, destination("chain")),
    };
    convert().unwrap();
    let converted = keyed(&destination("chain"), key);
    assert!(converted.collections[&0].records.contains_key(&1));
    assert_eq!(converted.collections[&0].expiries.get(&9), Some(&0));
    assert_eq!(converted.lineage, Some(LINEAGE));
    assert!(convert().is_err());
    keyed(&destination("chain"), key);

    match key {
        Some(key) => {
            maintenance::inspect_with_key(&source, key).unwrap();
            maintenance::verify_with_key(&source, key).unwrap();
        }
        None => {
            maintenance::inspect(&source).unwrap();
            maintenance::verify(&source).unwrap();
        }
    }

    // Rewrites refuse before any output.
    let refused = "unsupported 32";
    let compacted = match key {
        Some(key) => maintenance::compact_with_key(&source, destination("compact"), key),
        None => maintenance::compact(&source, destination("compact")),
    };
    assert_eq!(outcome(compacted), refused);
    if key.is_none() {
        assert_eq!(
            outcome(maintenance::upgrade(&source, destination("upgrade"))),
            refused
        );
    }
    assert_eq!(
        outcome(crate::property_graph::strip_graph(
            &source,
            destination("strip"),
            key,
            true
        )),
        refused
    );
    assert!(crate::property_graph::enable_graph(&source, destination("enable"), key).is_err());
    for name in ["compact", "upgrade", "strip", "enable"] {
        assert!(!destination(name).exists(), "{name}");
    }
    assert_eq!(std::fs::read(&source).unwrap(), original);
}

#[test]
fn maintenance_keeps_the_catalog_or_refuses() {
    maintenance_keeps_or_refuses(None);
    maintenance_keeps_or_refuses(Some(key()));
}

/// The graph `g` of `loaded`.
fn graph_g(loaded: &Loaded) -> &crate::property_graph::engine::Graph {
    loaded.graph.as_ref().unwrap().graph_by_name("g").unwrap()
}

/// Directory conversion of a precision file relocates the whole store
/// under its lineage (Decision P preservation): records, deadlines (even
/// already-expired ones), revisions, open and closed imports and the graph
/// are kept exactly, and the source is untouched. The converted store's own
/// checkpoints then apply ordinary TTL cleanup.
fn conversion_preserves(key: Option<EncryptionKey>) {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let chain = directory.path().join("chain");
    let key = key.as_ref();
    let none = Metadata::new();
    let put = |id| {
        vec![Mutation::Put {
            id,
            vector: vec![1.0, 0.0],
            payload: Vec::new(),
        }]
    };
    let mut db = create_store(Backing::File, &source, key);
    db.put(1, &[1.0, 0.0], b"a").unwrap();
    db.put_with_expiry(9, &[0.0, 1.0], b"z", &none, Some(0))
        .unwrap();
    db.put_with_expiry(8, &[0.5, 0.5], b"y", &none, Some(1))
        .unwrap();
    let finished = db.next_import_job_id().unwrap();
    db.import_chunk(finished, 1, &put(2), POLICY).unwrap();
    db.finish_import(finished).unwrap();
    let open = db.next_import_job_id().unwrap();
    db.import_chunk(open, 1, &put(3), POLICY).unwrap();
    db.create_graph("g").unwrap();
    {
        let mut graph = db.graph_mut("g").unwrap();
        graph.create_node("a", &[], &none).unwrap();
        graph.create_node("b", &[], &none).unwrap();
        graph
            .create_edge(Some("e"), "a", "knows", "b", &none)
            .unwrap();
        graph
            .upsert_node("a", &["x"], &none, Expected::Any)
            .unwrap();
        graph.set_node_expiry("b", Some(1), Expected::Any).unwrap();
    }
    db.close().unwrap();
    let original = std::fs::read(&source).unwrap();
    match key {
        Some(key) => maintenance::convert_to_directory_with_key(&source, &chain, key),
        None => maintenance::convert_to_directory(&source, &chain),
    }
    .unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), original);
    let (from, to) = (stored(&source, key), stored(&chain, key));
    let records = |loaded: &Loaded| {
        loaded
            .collections
            .iter()
            .map(|(&id, collection)| {
                (
                    id,
                    collection.records.clone(),
                    collection.expiries.clone(),
                    collection.revisions.clone(),
                    collection.precision,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(records(&to), records(&from));
    assert_eq!(from.collections[&0].expiries.get(&9), Some(&0));
    assert_eq!(
        Registry::from_markers(&to.imports),
        Registry::from_markers(&from.imports)
    );
    assert_eq!(graph_g(&from).node_count(), 2);
    assert_eq!(to.graph, from.graph);
    assert_eq!(
        (to.sequence, to.lineage, to.flags),
        (from.sequence, from.lineage, from.flags)
    );

    let mut db = open_path(&chain, key).unwrap();
    let report = db.import_chunk(open, 2, &put(4), POLICY).unwrap();
    assert_eq!(report.outcome, ImportChunkOutcome::Committed);
    db.checkpoint().unwrap().unwrap();
    db.close().unwrap();
    let after = stored(&chain, key);
    let kept = &after.collections[&0];
    assert!(kept.records.contains_key(&1) && kept.records.contains_key(&4));
    assert!(!kept.records.contains_key(&9) && !kept.records.contains_key(&8));
    let graph = graph_g(&after);
    assert!(graph.node_by_key("a").is_some() && graph.node_by_key("b").is_none());
    assert_eq!(graph.edge_count(), 0);
}

#[test]
fn directory_conversion_preserves_the_whole_store() {
    conversion_preserves(None);
    conversion_preserves(Some(key()));
}

/// Converting a legacy file keeps its ordinary cleanup of records that are
/// already expired.
#[test]
fn legacy_directory_conversion_still_drops_expired_records() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy.vlite");
    let chain = directory.path().join("chain");
    let mut db = Database::create(&source, 2).unwrap();
    db.put(1, &[1.0, 0.0], b"a").unwrap();
    db.put_with_expiry(9, &[0.0, 1.0], b"z", &Metadata::new(), Some(0))
        .unwrap();
    db.close().unwrap();
    maintenance::convert_to_directory(&source, &chain).unwrap();
    let converted = stored(&chain, None);
    assert!(converted.collections[&0].records.contains_key(&1));
    assert!(!converted.collections[&0].records.contains_key(&9));
}

// ---- Publication --------------------------------------------------------

/// The entries of `directory`, sorted.
fn listing(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A failpoint hook that fails `stage` with an I/O error.
fn failing(stage: Stage) -> impl FnMut(Stage) -> std::io::Result<()> + 'static {
    move |at| {
        if at == stage {
            return Err(std::io::Error::other(format!("{at:?} failpoint")));
        }
        Ok(())
    }
}

/// A hook that records how a writer and a reader open `path` while the
/// published name is visible, then fails that stage.
fn probing(
    path: &Path,
    seen: &Rc<RefCell<Vec<String>>>,
) -> impl FnMut(Stage) -> std::io::Result<()> + 'static {
    let (path, seen) = (path.to_path_buf(), Rc::clone(seen));
    move |stage| {
        if stage != Stage::Visible {
            return Ok(());
        }
        let mut seen = seen.borrow_mut();
        seen.push(outcome(Database::open(&path)));
        seen.push(outcome(Database::open_read_only(&path)));
        Err(std::io::Error::other("visible failpoint"))
    }
}

/// A file store is complete, synced and locked by its publisher before its
/// final name appears. Failures before that leave nothing behind; after
/// it, they are indeterminate and never remove the visible file.
#[test]
fn file_publication_locks_before_its_name_is_visible() {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.vlite");
    let image = image_of(&initial(None));
    let publish =
        |bytes: &[u8]| maintenance::publish_with(&path, None, |file| Ok(file.write_all(bytes)?));
    let failed = maintenance::publish_with(&path, None, |_| {
        Err(std::io::Error::other("write failed").into())
    });
    assert!(failed.is_err());
    assert!(publish(&image[..image.len() - 1]).is_err());
    {
        let _armed = failpoint::arm(failing(Stage::Staged));
        assert!(matches!(publish(&image), Err(Error::Io(_))));
    }
    assert!(listing(directory.path()).is_empty());

    let seen = Rc::new(RefCell::new(Vec::new()));
    {
        let _armed = failpoint::arm(probing(&path, &seen));
        let result = publish(&image);
        assert!(
            matches!(result, Err(Error::PublicationIndeterminate { .. })),
            "{}",
            outcome(result)
        );
    }
    assert_eq!(*seen.borrow(), ["Locked", "Locked"]);
    assert_eq!(listing(directory.path()), ["store.vlite"]);
    assert_eq!(std::fs::read(&path).unwrap(), image);
    open_path(&path, None).unwrap().close().unwrap();
    // A published name is never replaced.
    assert!(publish(&image).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), image);
}

/// A directory store is locked from its creation; it is visible once its
/// manifest is. Failures before that remove the directory; after it, a
/// failed directory sync is indeterminate and keeps the store.
#[test]
fn directory_publication_never_removes_a_visible_store() {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.vlite");
    let mut db = create_store(Backing::File, &source, None);
    db.put(1, &[1.0, 0.0], b"a").unwrap();
    db.close().unwrap();
    let expected = stored(&source, None);
    let convert =
        |destination: &Path| maintenance::convert_to_directory(&source, destination).map(drop);
    let create = |destination: &Path| {
        Database::create_directory(destination, 2, CreateOptions::default()).map(drop)
    };
    type Publisher<'a> = &'a dyn Fn(&Path) -> Result<()>;
    let publishers: [(&str, Publisher); 2] = [("converted", &convert), ("created", &create)];
    for (name, publish) in publishers {
        let destination = directory.path().join(name);
        {
            let _armed = failpoint::arm(failing(Stage::Staged));
            assert!(publish(&destination).is_err(), "{name}");
        }
        assert!(!destination.exists(), "{name}");

        let seen = Rc::new(RefCell::new(Vec::new()));
        {
            let _armed = failpoint::arm(probing(&destination, &seen));
            let result = publish(&destination);
            assert!(
                matches!(result, Err(Error::PublicationIndeterminate { .. })),
                "{name}: {}",
                outcome(result)
            );
        }
        assert_eq!(*seen.borrow(), ["Locked", "Locked"], "{name}");
        let reopened = Database::open(&destination).unwrap();
        reopened.close().unwrap();
        assert!(publish(&destination).is_err(), "{name}");
    }
    let converted = stored(&directory.path().join("converted"), None);
    assert_eq!(
        converted.collections[&0].revisions,
        expected.collections[&0].revisions
    );
    assert_eq!(converted.sequence, expected.sequence);
}

/// Checkpoint manifests keep their established handling: a manifest whose
/// directory sync failed is in place, so the checkpoint succeeds but keeps
/// every file the previous manifest listed; a failed install keeps the
/// state, and a later checkpoint succeeds.
#[test]
fn checkpoints_survive_manifest_failures() {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store");
    let mut db = create_store(Backing::Directory, &path, None);
    db.put(1, &[1.0, 0.0], b"a").unwrap();
    let before = listing(&path);
    {
        let _armed = failpoint::arm(failing(Stage::Visible));
        db.checkpoint().unwrap().unwrap();
    }
    let after = listing(&path);
    for name in &before {
        assert!(after.contains(name), "{name} removed");
    }
    db.put(2, &[1.0, 0.0], b"b").unwrap();
    let mut staged = 0;
    {
        let _armed = failpoint::arm(move |stage| {
            staged += usize::from(stage == Stage::Staged);
            if staged == 2 && stage == Stage::Staged {
                return Err(std::io::Error::other("install failpoint"));
            }
            Ok(())
        });
        assert!(db.checkpoint().is_err());
    }
    db.put(3, &[1.0, 0.0], b"c").unwrap();
    let floor = db.checkpoint().unwrap().unwrap();
    let (loaded, db) = cycle(Backing::Directory, &path, None, db);
    assert_eq!(loaded.history_floor, floor);
    for id in 1..=3 {
        assert!(loaded.collections[&0].records.contains_key(&id), "{id}");
    }
    db.close().unwrap();
}

#[test]
fn feeds_exports_and_replicas_follow_precise_creates() {
    let _gate = Activation::on();
    let directory = tempfile::tempdir().unwrap();
    let primary = directory.path().join("primary.vlite");
    let base = directory.path().join("base.vlite");
    let second = directory.path().join("second.vlite");
    create_store(Backing::File, &primary, None).close().unwrap();
    maintenance::backup(&primary, &base).unwrap();
    maintenance::backup(&primary, &second).unwrap();
    let mut db = open_path(&primary, None).unwrap();
    db.create_collection_as("half", spec(3), F16).unwrap();
    db.put(1, &[1.0, 0.0], b"a").unwrap();

    let cursor = |sequence| FeedCursor {
        lineage: LINEAGE,
        sequence,
    };
    let batch = db.changes_after(&cursor(1), 10).unwrap();
    assert_eq!(batch.changes.len(), 2);
    let created = &batch.changes[0];
    assert_eq!((created.sequence, created.collection.as_str()), (2, "half"));
    assert!(matches!(
        created.kind,
        ChangeKind::CollectionCreated { dimensions: 3, .. }
    ));
    assert!(matches!(
        db.changes_after(&cursor(0), 10),
        Err(Error::CursorBeforeHistory {
            cursor: 0,
            floor: 1
        })
    ));
    assert!(matches!(
        db.export_log(0, 1 << 20),
        Err(Error::CursorBeforeHistory {
            cursor: 0,
            floor: 1
        })
    ));
    let segment = db.export_log(1, 1 << 20).unwrap();
    assert_eq!((segment.base_sequence, segment.last_sequence), (1, 3));
    db.close().unwrap();

    // A follower bootstraps from a backup at the floor and replays op 17
    // as is.
    let mut follower = ReplicaFollower::open(&base, None).unwrap();
    assert_eq!(follower.apply(&segment).unwrap().applied_sequence, 3);
    assert!(
        follower
            .db()
            .collection("half")
            .unwrap()
            .get(1)
            .unwrap()
            .is_none()
    );
    drop(follower);
    assert_eq!(
        precisions(&stored(&base, None)),
        listed(&[(0, "default", F32), (1, "half", F16)])
    );
    assert_eq!(
        std::fs::read(&base).unwrap(),
        std::fs::read(&primary).unwrap()
    );
    // The replayed create installed the precision with the collection.
    let mut follower = ReplicaFollower::open(&second, None).unwrap();
    follower.apply(&segment).unwrap();
    let mut db = follower.promote();
    assert_eq!(dense_writes(&mut db, "half"), committed());
    assert_eq!(dense_writes(&mut db, "default"), committed());
}
