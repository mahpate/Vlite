//! Gap 4 review round 2: a temporal `PutEdge` (tag 65) restates its
//! revision only in a snapshot, within `1..=` the snapshot sequence; every
//! reader (replay, the feeds, log export, the precision image check)
//! enforces that in the frame context it shares (A). A precision image's
//! revision supplement must equal a temporal edge's restated revision
//! rather than overwrite it, in replay and in the image check alike (B).

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use super::super::{Activation, enable_temporal};
use super::writes::{close, create, legacy, populate, revision, seg, stored, write};
use crate::changes::{self, ChangeBatch, FeedCursor};
use crate::format::{self, FLAG_GRAPH, FLAG_PRECISION, FLAG_TEMPORAL, OP_GRAPH_SNAPSHOT};
use crate::property_graph::engine::codec::{OP_GRAPH, TemporalEntry};
use crate::property_graph::engine::{GraphEntry, decode_body, encode_body};
use crate::{CreateOptions, Database, EncryptionKey, Error, Expected, ReplicaFollower, Result};
use crate::{logops, maintenance};

fn key() -> EncryptionKey {
    EncryptionKey::from_bytes([9; 32])
}

/// A temporal copy `t.vlite` in `dir` of a populated file encrypted under
/// `key`.
fn temporal_at(dir: &Path, key: Option<&EncryptionKey>) -> PathBuf {
    let source = dir.join("s.vlite");
    let options = CreateOptions {
        encryption: key.cloned(),
        ..CreateOptions::default()
    };
    let mut db = Database::create_with_options(&source, 2, options).unwrap();
    populate(&mut db);
    db.close().unwrap();
    let path = dir.join("t.vlite");
    enable_temporal(&source, &path, key).unwrap();
    path
}

fn open(path: &Path, key: Option<&EncryptionKey>) -> Database {
    match key {
        Some(key) => Database::open_encrypted(path, key).unwrap(),
        None => Database::open(path).unwrap(),
    }
}

/// Sets the revision of every tag-65 entry in the `operation` frames of
/// the log `bytes` to `revision`, resealing the frames in place (same
/// length); returns how many entries it set.
fn set_revisions(
    bytes: &mut [u8],
    key: Option<&EncryptionKey>,
    operation: u8,
    revision: u64,
) -> usize {
    let mut cursor = Cursor::new(bytes.to_vec());
    let header = format::read_log_header(&mut cursor, key).unwrap();
    let mut offset = header.start;
    let mut set = 0;
    while let Some(frame) = format::next_frame(&mut cursor, &header, offset).unwrap() {
        let at = offset as usize;
        offset += frame.disk.len() as u64;
        if frame.operation != operation {
            continue;
        }
        let mut body = decode_body(frame.body(), frame.count).unwrap();
        for entry in &mut body.entries {
            if let GraphEntry::Temporal(TemporalEntry::PutEdge { revision: r, .. }) = entry {
                *r = revision;
                set += 1;
            }
        }
        let encoded = encode_body(&body).unwrap();
        let plain = if operation == OP_GRAPH_SNAPSHOT {
            format::graph_snapshot_frame(frame.sequence, frame.count, &encoded)
        } else {
            format::graph_frame(frame.sequence, frame.id, frame.count, &encoded)
        };
        let disk = match &header.cipher {
            Some(cipher) => cipher.seal_frame(&plain).unwrap(),
            None => plain,
        };
        assert_eq!(disk.len(), frame.disk.len());
        bytes[at..at + disk.len()].copy_from_slice(&disk);
    }
    set
}

/// The feed of the log `bytes` after `cursor`, with or without temporal
/// changes.
fn feed(
    bytes: &[u8],
    key: Option<&EncryptionKey>,
    cursor: &FeedCursor,
    temporal: bool,
) -> Result<ChangeBatch> {
    changes::read_changes_with(&mut Cursor::new(bytes), key, None, cursor, 100, temporal)
        .map(|(batch, _)| batch)
}

fn is_corrupt<T>(result: Result<T>) -> bool {
    matches!(result, Err(Error::Corrupt { .. }))
}

/// A checksum-valid op-12 `PutEdge` with a nonzero revision is corrupt for
/// replay, both feeds and a follower, plain or encrypted.
#[test]
fn an_ordinary_frame_never_restates_a_revision() {
    let _on = Activation::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        let dir = tempfile::tempdir().unwrap();
        let path = temporal_at(dir.path(), key);
        let mut follower = ReplicaFollower::create(
            dir.path().join("f.vlite"),
            &maintenance::export_log_segment(&path, key, 0, usize::MAX).unwrap(),
            key,
        )
        .unwrap();
        let mut db = open(&path, key);
        let cursor = db.feed_cursor().unwrap();
        let s1 = write(&mut db, &[create("ab", vec![seg(None, None, 1)])])
            .unwrap()
            .revision
            .unwrap();
        write(
            &mut db,
            &[close("ab", None, None, 10, Expected::Revision(s1))],
        )
        .unwrap();
        db.close().unwrap();
        let good = fs::read(&path).unwrap();
        feed(&good, key, &cursor, true).unwrap();

        let mut bad = good.clone();
        assert_eq!(set_revisions(&mut bad, key, OP_GRAPH, 1), 1);
        assert!(is_corrupt(Database::open_from_bytes(bad.clone(), key)));
        for temporal in [true, false] {
            assert!(is_corrupt(feed(&bad, key, &cursor, temporal)), "{temporal}");
        }
        let bad_path = dir.path().join("bad.vlite");
        fs::write(&bad_path, &bad).unwrap();
        let applied = follower.applied_sequence();
        let followed = stored(follower.db());
        assert!(is_corrupt(follower.sync_from_log(&bad_path, usize::MAX)));
        assert_eq!(follower.applied_sequence(), applied);
        assert_eq!(stored(follower.db()), followed);
        follower.sync_from_log(&path, usize::MAX).unwrap();
        assert_eq!(revision(follower.db(), "ab"), s1 + 1);
    }
}

/// A snapshot (op-15) `PutEdge` restates a revision within `1..=` its
/// sequence, or the image is corrupt for replay and both feeds.
#[test]
fn a_snapshot_revision_stays_within_its_sequence() {
    let _on = Activation::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        let dir = tempfile::tempdir().unwrap();
        let path = temporal_at(dir.path(), key);
        let mut db = open(&path, key);
        let r = write(&mut db, &[create("ab", vec![seg(None, None, 1)])])
            .unwrap()
            .revision
            .unwrap();
        write(&mut db, &[legacy("bd", 2)]).unwrap();
        db.close().unwrap();
        let loaded = format::load_with(&mut fs::File::open(&path).unwrap(), key).unwrap();
        let floor = loaded.sequence;
        let lineage = loaded.lineage.unwrap();
        let mut good = Vec::new();
        maintenance::write_v5_image(
            &mut good,
            &loaded,
            lineage,
            None,
            None,
            loaded.cipher.as_ref(),
            loaded.flags & (FLAG_GRAPH | FLAG_TEMPORAL),
            floor,
        )
        .unwrap();
        let cursor = FeedCursor {
            lineage,
            sequence: floor,
        };
        let db = Database::open_from_bytes(good.clone(), key).unwrap();
        assert_eq!(revision(&db, "ab"), r);
        feed(&good, key, &cursor, true).unwrap();

        for wrong in [0, floor + 1] {
            let mut bad = good.clone();
            assert_eq!(set_revisions(&mut bad, key, OP_GRAPH_SNAPSHOT, wrong), 1);
            assert!(is_corrupt(Database::open_from_bytes(bad.clone(), key)));
            for temporal in [true, false] {
                assert!(
                    is_corrupt(feed(&bad, key, &cursor, temporal)),
                    "{wrong} {temporal}"
                );
            }
        }
    }
}

/// A precision image whose supplement disagrees with a temporal edge's
/// restated revision is corrupt for replay, the feed and log export,
/// whichever way it disagrees; ordinary edges keep taking theirs.
#[test]
fn a_precision_supplement_must_match_a_temporal_revision() {
    let _precision = crate::compact::Activation::on();
    let _on = Activation::on();
    let header = format::header_v5_flags(2, 0x5EED_0000_0000_00B2, 0, FLAG_GRAPH, None);
    let mut loaded = format::load_with(&mut Cursor::new(header), None).unwrap();
    loaded.flags |= FLAG_PRECISION | FLAG_TEMPORAL;
    loaded.sequence = 1;
    loaded.collections.get_mut(&0).unwrap().precision =
        Some(crate::compact::catalog::Precision::Float32);
    let image = |loaded: &format::Loaded| {
        let mut bytes = Vec::new();
        maintenance::write_v5_image(
            &mut bytes,
            loaded,
            loaded.lineage.unwrap(),
            None,
            None,
            None,
            loaded.flags & (FLAG_GRAPH | FLAG_PRECISION | FLAG_TEMPORAL),
            loaded.sequence,
        )
        .unwrap();
        bytes
    };
    let mut db = Database::open_from_bytes(image(&loaded), None).unwrap();
    populate(&mut db);
    let r = write(&mut db, &[create("ab", vec![seg(None, None, 1)])])
        .unwrap()
        .revision
        .unwrap();
    write(&mut db, &[legacy("bd", 2)]).unwrap();
    let loaded = format::load_with(&mut Cursor::new(db.to_bytes().unwrap()), None).unwrap();
    let floor = loaded.sequence;
    assert!(r >= 2 && r < floor);
    let good = image(&loaded);
    let cursor = FeedCursor {
        lineage: loaded.lineage.unwrap(),
        sequence: floor,
    };
    let db = Database::open_from_bytes(good.clone(), None).unwrap();
    assert_eq!(revision(&db, "ab"), r);
    feed(&good, None, &cursor, true).unwrap();
    logops::read_segment(&mut Cursor::new(&good), None, None, floor, usize::MAX).unwrap();

    for wrong in [r - 1, r + 1] {
        let mut bad = good.clone();
        assert_eq!(set_revisions(&mut bad, None, OP_GRAPH_SNAPSHOT, wrong), 1);
        assert!(
            is_corrupt(Database::open_from_bytes(bad.clone(), None)),
            "{wrong}"
        );
        assert!(is_corrupt(format::load_with(&mut Cursor::new(&bad), None)));
        assert!(is_corrupt(feed(&bad, None, &cursor, true)), "{wrong}");
        assert!(
            is_corrupt(logops::read_segment(
                &mut Cursor::new(&bad),
                None,
                None,
                floor,
                usize::MAX
            )),
            "{wrong}"
        );
    }
}
