//! Gap 4 review round 4: log export checks the temporal frame context and
//! capability of every graph frame in each graph mode a header admits: a
//! history image (flag 8), a compact image without precision (flag 16,
//! whose image check only checks its blocks), and a precision image
//! (flags 16 and 32, whose image check replays graph frames). Flag 32
//! without 16 is refused by the header. Each mode is checked plain and
//! encrypted, for a closed file, an open log and a `Database::export_log`
//! handle, at each cursor, whole and cut at one byte, where replay and both
//! feeds refuse the same log; valid logs keep exporting their frames
//! verbatim.

use std::fs;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::super::Activation;
use super::revisions::{
    feed, is_corrupt, key, open, set_revisions, temporal_at, without_capability,
};
use super::writes::{create, legacy, revision, seg, write};
use crate::changes::FeedCursor;
use crate::format::{
    self, FLAG_COMPACT, FLAG_GRAPH, FLAG_HISTORY, FLAG_PRECISION, FLAG_TEMPORAL, LogHeader,
    OP_GRAPH_SNAPSHOT, PREFIX_SIZE,
};
use crate::property_graph::engine::codec::OP_GRAPH;
use crate::{Database, EncryptionKey, Error, Result, logops, maintenance};

/// The capability table's kind and the critical block flag
/// (`compact::KIND_TABLE`, `compact::CRITICAL`).
const KIND_TABLE: u16 = 1;
const CRITICAL: u16 = 1;
/// A declared derived kind, which this build skips (`compact::registered`).
const DERIVED: u16 = 0x0200;

/// Graph modes of a history image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    History,
    Compact,
    Precision,
}

/// `plain` sealed as `header`'s log stores it.
fn seal(header: &LogHeader, plain: Vec<u8>) -> Vec<u8> {
    match &header.cipher {
        Some(cipher) => cipher.seal_frame(&plain).unwrap(),
        None => plain,
    }
}

/// An op-16 frame at `floor` with a version-1 block of `kind` for owner 0
/// and partition 0, with `flags` and one unpadded `section`.
fn block(
    header: &LogHeader,
    floor: u64,
    kind: u16,
    flags: u16,
    count: u32,
    section: &[u8],
) -> Vec<u8> {
    let start = 40u32;
    let mut body = Vec::new();
    for field in [kind, 1, flags, 1] {
        body.extend_from_slice(&field.to_le_bytes());
    }
    // Owner 0, partition 0; no source digest.
    body.extend_from_slice(&[0; 16]);
    for field in [start, 0, start, section.len() as u32] {
        body.extend_from_slice(&field.to_le_bytes());
    }
    body.extend_from_slice(section);
    let frame = format::compact_block_frame(floor, 0, u64::from(kind), count, &body);
    seal(header, frame)
}

/// The log `bytes` of a history image restated as a compact image without
/// precision (flag 16 alone): a capability table declaring one derived
/// kind after the image's collection frames, one block of that kind, and
/// the end block over every image frame; the frames after the image follow
/// unchanged.
fn compact(bytes: &[u8], key: Option<&EncryptionKey>) -> Vec<u8> {
    let mut cursor = Cursor::new(bytes);
    let header = format::read_log_header(&mut cursor, key).unwrap();
    let mut out = bytes[..header.start as usize].to_vec();
    let flags = u16::from_le_bytes([out[26], out[27]]);
    assert_eq!(flags & (FLAG_HISTORY | FLAG_COMPACT), FLAG_HISTORY);
    out[26..28].copy_from_slice(&(flags | FLAG_COMPACT).to_le_bytes());
    let crc = format::crc32(&out[..28]);
    out[28..32].copy_from_slice(&crc.to_le_bytes());
    let mut image = Vec::new();
    let mut after = Vec::new();
    let mut floor = 0;
    let mut table = false;
    let mut offset = header.start;
    while let Some(frame) = format::next_frame(&mut cursor, &header, offset).unwrap() {
        offset += frame.disk.len() as u64;
        if frame.operation == format::OP_ADVANCE {
            floor = frame.sequence;
        } else if frame.sequence > floor {
            after.extend_from_slice(&frame.disk);
            continue;
        } else if !table && !matches!(frame.operation, 5 | 7) {
            let entry = [DERIVED, 1, 0, 0].map(u16::to_le_bytes).concat();
            image.push(block(&header, floor, KIND_TABLE, CRITICAL, 1, &entry));
            image.push(block(&header, floor, DERIVED, 0, 1, b"x"));
            table = true;
        }
        image.push(frame.disk);
    }
    assert!(table);
    let mut digest = Sha256::new();
    for frame in &image {
        digest.update(&frame[..PREFIX_SIZE]);
    }
    let end = crate::compact::end_frame(floor, image.len() as u64, digest.finalize().into());
    image.push(seal(&header, end));
    out.extend(image.concat());
    out.extend(after);
    out
}

/// A temporal log in `mode` in the new directory `dir`: an image at
/// `floor` restating the temporal edge "ab" in its snapshot, then the
/// commits `floor + 1` (ordinary), `floor + 2` (the temporal edge "cd", an
/// op-12 frame) and `floor + 3` (ordinary, the head).
struct Fixture {
    good: Vec<u8>,
    cursor: FeedCursor,
    ab: u64,
}

fn fixture(dir: &Path, key: Option<&EncryptionKey>, mode: Mode) -> Fixture {
    let path = temporal_at(dir, key);
    let mut db = open(&path, key);
    let ab = write(&mut db, &[create("ab", vec![seg(None, None, 1)])])
        .unwrap()
        .revision
        .unwrap();
    write(&mut db, &[legacy("bd", 2)]).unwrap();
    db.close().unwrap();
    let mut loaded = format::load_with(&mut fs::File::open(&path).unwrap(), key).unwrap();
    let floor = loaded.sequence;
    let lineage = loaded.lineage.unwrap();
    let mut flags = loaded.flags & (FLAG_GRAPH | FLAG_TEMPORAL);
    if mode == Mode::Precision {
        flags |= FLAG_PRECISION;
        for collection in loaded.collections.values_mut() {
            collection.precision = Some(crate::compact::catalog::Precision::Float32);
        }
    }
    let mut image = Vec::new();
    maintenance::write_v5_image(
        &mut image,
        &loaded,
        lineage,
        None,
        None,
        loaded.cipher.as_ref(),
        flags,
        floor,
    )
    .unwrap();
    let path = dir.join("image.vlite");
    fs::write(&path, &image).unwrap();
    let mut db = open(&path, key);
    write(&mut db, &[legacy("ba", 3)]).unwrap();
    let cd = write(&mut db, &[create("cd", vec![seg(None, None, 5)])])
        .unwrap()
        .revision
        .unwrap();
    assert_eq!(cd, floor + 2);
    write(&mut db, &[legacy("ca", 4)]).unwrap();
    db.close().unwrap();
    let mut good = fs::read(&path).unwrap();
    if mode == Mode::Compact {
        good = compact(&good, key);
    }
    let header = format::read_log_header(&mut Cursor::new(&good), key).unwrap();
    let wanted = match mode {
        Mode::History => FLAG_HISTORY,
        Mode::Compact => FLAG_HISTORY | FLAG_COMPACT,
        Mode::Precision => FLAG_HISTORY | FLAG_COMPACT | FLAG_PRECISION,
    };
    let modes = FLAG_HISTORY | FLAG_COMPACT | FLAG_PRECISION | FLAG_GRAPH | FLAG_TEMPORAL;
    assert_eq!(header.flags & modes, wanted | FLAG_GRAPH | FLAG_TEMPORAL);
    let cursor = FeedCursor {
        lineage,
        sequence: floor,
    };
    Fixture { good, cursor, ab }
}

/// The frames of the log `bytes` after `after`, verbatim: what an uncut
/// export of a log without time anchors holds.
fn tail(bytes: &[u8], key: Option<&EncryptionKey>, after: u64) -> Vec<u8> {
    let mut cursor = Cursor::new(bytes);
    let header = format::read_log_header(&mut cursor, key).unwrap();
    let mut offset = header.start;
    let mut frames = Vec::new();
    while let Some(frame) = format::next_frame(&mut cursor, &header, offset).unwrap() {
        offset += frame.disk.len() as u64;
        if frame.operation != format::OP_ADVANCE && frame.sequence > after {
            frames.extend_from_slice(&frame.disk);
        }
    }
    frames
}

/// Exports of the log `bytes` after `after`, cut once `max_bytes` were
/// collected: of a closed file, of an open log (`read_segment`), and by the
/// handle `db`, whose file is overwritten with `bytes` first.
fn exports(
    dir: &Path,
    db: &mut Database,
    bytes: &[u8],
    key: Option<&EncryptionKey>,
    after: u64,
    max_bytes: usize,
) -> [(&'static str, Result<logops::LogSegment>); 3] {
    let path = dir.join("export.vlite");
    fs::write(&path, bytes).unwrap();
    let closed = maintenance::export_log_segment(&path, key, after, max_bytes);
    fs::remove_file(&path).unwrap();
    let open = logops::read_segment(&mut Cursor::new(bytes), key, None, after, max_bytes)
        .map(|(segment, _)| segment);
    db.file.seek(SeekFrom::Start(0)).unwrap();
    db.file.write_all(bytes).unwrap();
    let handle = db.export_log(after, max_bytes);
    [("closed", closed), ("open", open), ("handle", handle)]
}

/// In every graph mode, plain and encrypted, a log whose op-12 `PutEdge`
/// restates a revision, whose snapshot restates one outside `1..=` its
/// sequence, or whose header lost the temporal flag is corrupt for replay,
/// both feeds and every log export, at each cursor, whole or cut at one
/// byte; the valid log exports the same frames verbatim on every path.
#[test]
fn every_graph_mode_exports_only_valid_temporal_frames() {
    let _precision = crate::compact::Activation::on();
    let _on = Activation::on();
    let mut checked = 0;
    let mut missed = Vec::new();
    for mode in [Mode::History, Mode::Compact, Mode::Precision] {
        for key in [None, Some(key())] {
            let key = key.as_ref();
            let dir = tempfile::tempdir().unwrap();
            let Fixture { good, cursor, ab } = fixture(dir.path(), key, mode);
            let floor = cursor.sequence;
            let db = Database::open_from_bytes(good.clone(), key).unwrap();
            assert_eq!(revision(&db, "ab"), ab);
            assert_eq!(revision(&db, "cd"), floor + 2);
            assert!(format::load_with(&mut Cursor::new(&good), key).is_ok());
            feed(&good, key, &cursor, true).unwrap();
            let handle_path = dir.path().join("handle.vlite");
            fs::write(&handle_path, &good).unwrap();
            let mut handle = open(&handle_path, key);
            // The cursor at the image skips its snapshot; the commit after
            // it precedes "cd", which the one-byte cut then skips; the
            // cursor after "cd" and at the head skip it too.
            let cursors = [floor, floor + 1, floor + 2, floor + 3];
            for after in cursors {
                for max_bytes in [usize::MAX, 1] {
                    let mut segments = Vec::new();
                    for (path, result) in
                        exports(dir.path(), &mut handle, &good, key, after, max_bytes)
                    {
                        let segment = result.unwrap_or_else(|error| {
                            let sealed = key.is_some();
                            panic!("{mode:?} {sealed} {path} {after} {max_bytes}: {error}")
                        });
                        segments.push(segment.to_bytes());
                        if max_bytes == usize::MAX {
                            assert_eq!(segment.frames, tail(&good, key, after));
                        }
                    }
                    assert!(segments.windows(2).all(|pair| pair[0] == pair[1]));
                }
            }

            let mut cases = Vec::new();
            let mut bad = good.clone();
            assert_eq!(set_revisions(&mut bad, key, OP_GRAPH, 1), 1);
            cases.push(("op-12 revision 1", bad));
            for wrong in [0, floor + 1] {
                let mut bad = good.clone();
                assert_eq!(set_revisions(&mut bad, key, OP_GRAPH_SNAPSHOT, wrong), 1);
                cases.push((
                    if wrong == 0 {
                        "snapshot revision 0"
                    } else {
                        "snapshot revision > floor"
                    },
                    bad,
                ));
            }
            cases.push(("temporal flag cleared", without_capability(&good)));
            for (case, bad) in cases {
                let mut refused = |reader: String, corrupt: bool| {
                    checked += 1;
                    if !corrupt {
                        missed.push(format!("{mode:?} {} {case}: {reader}", key.is_some()));
                    }
                };
                refused(
                    "replay".into(),
                    is_corrupt(Database::open_from_bytes(bad.clone(), key)),
                );
                // The other feed refuses a temporal log at its first
                // temporal entry: the image's valid one, before "cd".
                for temporal in [true, false] {
                    let result = feed(&bad, key, &cursor, temporal);
                    let refusal = !temporal
                        && matches!(result, Err(Error::UnsupportedFeature(FLAG_TEMPORAL)));
                    refused(format!("feed {temporal}"), refusal || is_corrupt(result));
                }
                for after in cursors {
                    for max_bytes in [usize::MAX, 1] {
                        for (path, result) in
                            exports(dir.path(), &mut handle, &bad, key, after, max_bytes)
                        {
                            refused(format!("{path} {after} {max_bytes}"), is_corrupt(result));
                        }
                    }
                }
            }
            // The handle's own file is valid again.
            for (_, result) in exports(dir.path(), &mut handle, &good, key, floor, usize::MAX) {
                result.unwrap();
            }
            handle.close().unwrap();
        }
    }
    assert!(
        missed.is_empty(),
        "{} of {checked} refusals missed: {missed:#?}",
        missed.len()
    );
    // 3 modes x 2 keys x 4 cases x (replay + 2 feeds + 4 cursors x 2 cuts
    // x 3 export paths).
    assert_eq!(checked, 3 * 2 * 4 * (3 + 4 * 2 * 3));
}
