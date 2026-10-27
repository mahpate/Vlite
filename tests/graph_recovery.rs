//! EPIC-006 TEST-004 (ITEM-026): crash and fault injection on graph-enabled
//! files.
//!
//! A crash during a graph append leaves the state before or after the batch,
//! never a part of it (AC-005, INV-007): every truncation of the last graph
//! commit and seeded random truncations of earlier ones are an incomplete
//! tail that repair removes back to the last whole commit. Flipped bits in
//! committed frames and committed frames that would break an attachment are
//! corruption errors with nothing applied or published (AC-006, FM-002).
//! Plain, encrypted and commit-timestamp files are covered.

mod graph_support;

use std::fs;
use std::path::{Path, PathBuf};

use graph_support::*;
use tempfile::tempdir;
use vlite::maintenance;
use vlite::{Database, EncryptionKey, Error};

/// File images after each commit (index = sequence; 0 is the bare header)
/// of `populate` plus `extend`, and the file.
struct History {
    path: PathBuf,
    images: Vec<Vec<u8>>,
}

impl History {
    fn record(dir: &Path, kind: Kind) -> Self {
        let path = dir.join(format!("history-{kind:?}.vlite"));
        let mut db = Database::create_with_options(&path, 2, kind.options()).unwrap();
        clock(&mut db, 0);
        let mut images = vec![live_bytes(&db)];
        populate_with(&mut db, &mut |db| images.push(live_bytes(db)));
        extend(&mut db);
        images.push(live_bytes(&db));
        db.close().unwrap();
        assert_eq!(&fs::read(&path).unwrap(), images.last().unwrap());
        Self { path, images }
    }

    fn full(&self) -> &[u8] {
        self.images.last().unwrap()
    }

    /// Image before the last (graph) commit.
    fn base(&self) -> &[u8] {
        &self.images[self.images.len() - 2]
    }

    fn header_len(&self) -> usize {
        self.images[0].len()
    }

    /// Sequence of the commit ending exactly at `len` bytes.
    fn commit_at(&self, len: usize) -> Option<usize> {
        self.images.iter().position(|image| image.len() == len)
    }
}

fn repair(path: &Path, key: Option<&EncryptionKey>) -> vlite::RepairReport {
    match key {
        Some(key) => Database::discard_incomplete_tail_encrypted(path, key),
        None => Database::discard_incomplete_tail(path),
    }
    .unwrap()
}

/// Deterministic pseudo-random offsets in `range` (64-bit LCG).
fn offsets(seed: u64, range: std::ops::Range<usize>, count: usize) -> Vec<usize> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            range.start + (state >> 33) as usize % range.len()
        })
        .collect()
}

#[test]
fn every_truncation_of_the_last_graph_commit_recovers_the_previous_state() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let history = History::record(dir.path(), kind);
        let (base, full) = (history.base(), history.full());
        let before = snapshot_bytes(base.to_vec(), key.as_ref());
        let after = snapshot_bytes(full.to_vec(), key.as_ref());
        assert_eq!(before.counts(), POPULATED);
        assert_eq!(after.counts(), [3, 7, 7, 5]);
        assert_ne!(before, after);

        let torn = dir.path().join(format!("torn-{kind:?}.vlite"));
        for cut in base.len() + 1..full.len() {
            match Database::open_from_bytes(full[..cut].to_vec(), key.as_ref()) {
                Err(Error::IncompleteTail { offset, bytes }) => {
                    assert_eq!(
                        (offset, bytes),
                        (base.len() as u64, (cut - base.len()) as u64),
                        "{kind:?} cut {cut}"
                    );
                }
                other => panic!("{kind:?} cut {cut}: {:?}", other.map(|_| ())),
            }
            fs::write(&torn, &full[..cut]).unwrap();
            let report = repair(&torn, key.as_ref());
            assert_eq!(report.discarded_bytes, (cut - base.len()) as u64);
            assert_eq!(report.retained_commits, history.images.len() as u64 - 2);
            assert_eq!(fs::read(&torn).unwrap(), base, "{kind:?} cut {cut}");
        }
        // The repaired file is the pre-batch state: no node, edge,
        // attachment or vector entry of the torn batch is visible.
        assert_eq!(snapshot(&torn, key.as_ref()), before);
        let verification = maintenance::verify_graph(&torn, key.as_ref()).unwrap();
        assert_counts(&verification.counts, POPULATED);
        let verification = maintenance::verify_graph(&history.path, key.as_ref()).unwrap();
        assert_counts(&verification.counts, [3, 7, 7, 5]);
        // Reopening and writing again continues from the repaired state.
        let mut db = open(&torn, key.as_ref());
        clock(&mut db, history.images.len() as u64 - 2);
        extend(&mut db);
        db.close().unwrap();
        assert_eq!(snapshot(&torn, key.as_ref()), after);
    }
}

#[test]
fn random_truncations_recover_the_last_whole_commit() {
    let dir = tempdir().unwrap();
    for (seed, kind) in KINDS.into_iter().enumerate() {
        let key = kind.key();
        let history = History::record(dir.path(), kind);
        let full = history.full();
        let torn = dir.path().join(format!("random-{kind:?}.vlite"));
        let mut repaired_commits = std::collections::BTreeSet::new();
        for cut in offsets(seed as u64 + 1, history.header_len() + 1..full.len(), 48) {
            let commit = match Database::open_from_bytes(full[..cut].to_vec(), key.as_ref()) {
                Ok(_) => history
                    .commit_at(cut)
                    .unwrap_or_else(|| panic!("{kind:?}: opened at non-commit {cut}")),
                Err(Error::IncompleteTail { offset, bytes }) => {
                    assert_eq!(offset + bytes, cut as u64);
                    history
                        .commit_at(offset as usize)
                        .unwrap_or_else(|| panic!("{kind:?}: tail at non-commit {offset}"))
                }
                Err(other) => panic!("{kind:?} cut {cut}: {other:?}"),
            };
            fs::write(&torn, &full[..cut]).unwrap();
            repair(&torn, key.as_ref());
            assert_eq!(fs::read(&torn).unwrap(), history.images[commit]);
            if repaired_commits.insert(commit) {
                assert_eq!(
                    snapshot(&torn, key.as_ref()),
                    snapshot_bytes(history.images[commit].clone(), key.as_ref()),
                    "{kind:?} commit {commit}"
                );
            }
        }
        assert!(
            repaired_commits.len() > 10,
            "{kind:?}: {repaired_commits:?}"
        );
    }
}

#[test]
fn flipped_bits_in_committed_graph_frames_are_corruption() {
    let dir = tempdir().unwrap();
    for (seed, kind) in KINDS.into_iter().enumerate() {
        let key = kind.key();
        let history = History::record(dir.path(), kind);
        let full = history.full();
        let last_commit = history.base().len()..full.len();
        let earlier = offsets(
            seed as u64 + 7,
            history.header_len()..history.base().len(),
            64,
        );
        for at in last_commit.chain(earlier) {
            let mut bytes = full.to_vec();
            bytes[at] ^= 1 << (at % 8);
            match Database::open_from_bytes(bytes, key.as_ref()) {
                Err(Error::Corrupt { .. }) => {}
                other => panic!("{kind:?} flip at {at}: {:?}", other.map(|_| ())),
            }
        }
        // Maintenance refuses the damaged file and publishes nothing.
        let mut bytes = full.to_vec();
        let at = full.len() - 20;
        bytes[at] ^= 0x10;
        let damaged = dir.path().join(format!("damaged-{kind:?}.vlite"));
        fs::write(&damaged, &bytes).unwrap();
        assert!(matches!(
            maintenance::verify_graph(&damaged, key.as_ref()),
            Err(Error::Corrupt { .. })
        ));
        let output = dir.path().join(format!("damaged-copy-{kind:?}.vlite"));
        assert!(matches!(
            maintenance::strip_graph(&damaged, &output, key.as_ref(), true),
            Err(Error::Corrupt { .. })
        ));
        assert!(!output.exists());
        assert_eq!(fs::read(&damaged).unwrap(), bytes);
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// Plaintext frame in the `format::wrap` layout.
fn frame(op: u8, collection: u16, sequence: u64, id: u64, body: &[u8]) -> Vec<u8> {
    let mut bytes = b"VREC".to_vec();
    bytes.push(op);
    bytes.extend_from_slice(&collection.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes.extend_from_slice(&id.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(body);
    let checksum = crc32(&bytes);
    bytes.extend_from_slice(b"VCMT");
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes.extend_from_slice(&sequence.to_le_bytes());
    bytes
}

#[test]
fn a_committed_delete_of_an_attached_record_is_corruption() {
    // INV-008 fault: a well-formed vector delete of record 1, which `ada` has
    // attached, committed after the graph history.
    let dir = tempdir().unwrap();
    let path = graph_file(dir.path(), "inv008", Kind::Plain);
    let next = commits(&path, None) + 1;
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(&frame(2, 0, next, 1, &[]));
    fs::write(&path, &bytes).unwrap();
    match Database::open(&path) {
        Err(Error::Corrupt { reason, .. }) => assert_eq!(
            reason,
            "graph attachment references a missing or expiring record"
        ),
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(matches!(
        maintenance::verify_graph(&path, None),
        Err(Error::Corrupt { .. })
    ));
    let output = dir.path().join("inv008-compact.vlite");
    assert!(matches!(
        maintenance::compact(&path, &output),
        Err(Error::Corrupt { .. })
    ));
    assert!(!output.exists());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    // A delete of an unattached record in the same position is fine.
    let path = graph_file(dir.path(), "unattached", Kind::Plain);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(&frame(2, 0, next, 5, &[]));
    fs::write(&path, &bytes).unwrap();
    let verification = maintenance::verify_graph(&path, None).unwrap();
    assert_counts(&verification.counts, POPULATED);
}
