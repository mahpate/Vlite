//! Gap 4: copy-out activation of the temporal capability (the user-approved
//! decision that replaces enabling it with the first temporal write). The
//! copy is a new temporal-enabled database under a new lineage; the source
//! is never modified and keeps its lineage, backups, restore and replicas.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use super::super::activate::FAULT;
use super::super::{Activation, enable_temporal};
use super::writes::{
    PAST, append, bytes_of, close, create, edge, file_len, graph, is_temporal, legacy, populate,
    put_vector, revision, seg, series, stored, write,
};
use crate::format::{
    self, FLAG_COMPACT, FLAG_ENCRYPTED, FLAG_GRAPH, FLAG_HISTORY, FLAG_PRECISION, FLAG_TEMPORAL,
};
use crate::maintenance::failpoint::{self, Stage};
use crate::maintenance::{self, RestoreTarget};
use crate::property_graph::{GraphOp, verify_graph};
use crate::{
    CreateOptions, Database, EncryptionKey, Error, Expected, FormatVersion, ReplicaFollower,
};

/// 2100-01-01: a deadline no test reaches.
const LATER: u64 = 4_102_444_800_000;

fn key() -> EncryptionKey {
    EncryptionKey::from_bytes([7; 32])
}

/// Creates `path` (encrypted under `key`) with graph "g", a vector record,
/// a far deadline on edge "ac" and a due edge "bc", then closes it.
fn source_at(path: &Path, key: Option<&EncryptionKey>) {
    let options = CreateOptions {
        encryption: key.cloned(),
        ..CreateOptions::default()
    };
    let mut db = Database::create_with_options(path, 2, options).unwrap();
    populate(&mut db);
    write(
        &mut db,
        &[
            put_vector(1),
            GraphOp::SetEdgeExpiry {
                edge: edge("ac"),
                expires_at_ms: Some(LATER),
                expected: Expected::Any,
            },
            legacy("bc", 1),
            GraphOp::SetEdgeExpiry {
                edge: edge("bc"),
                expires_at_ms: Some(PAST),
                expected: Expected::Any,
            },
        ],
    )
    .unwrap();
    db.close().unwrap();
}

fn open(path: &Path, key: Option<&EncryptionKey>) -> Database {
    match key {
        Some(key) => Database::open_encrypted(path, key).unwrap(),
        None => Database::open(path).unwrap(),
    }
}

/// What a copy must keep: stored edges, revisions, deadlines, the vector
/// record, the commit sequence and the next IDs.
type Kept = (
    super::writes::Stored,
    u64,
    Option<u64>,
    Option<(Vec<f32>, Vec<u8>)>,
    u64,
    u64,
);

fn kept(db: &Database) -> Kept {
    let graph = graph(db);
    let ac = graph.edge_by_key("ac").unwrap().id;
    (
        stored(db),
        revision(db, "ac"),
        graph.edge_expiry(ac),
        db.get(1)
            .unwrap()
            .map(|record| (record.vector.clone(), record.payload.clone())),
        db.sequence,
        graph.next_edge_id(),
    )
}

/// The names in `dir`.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every file under `root` with its bytes.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// The plaintext frames of the log `bytes` and its header.
fn frames(bytes: &[u8], key: Option<&EncryptionKey>) -> (format::LogHeader, Vec<Vec<u8>>) {
    let mut cursor = Cursor::new(bytes);
    let header = format::read_log_header(&mut cursor, key).unwrap();
    let mut offset = header.start;
    let mut frames = Vec::new();
    while let Some(frame) = format::next_frame(&mut cursor, &header, offset).unwrap() {
        offset += frame.disk.len() as u64;
        frames.push(frame.plain);
    }
    assert_eq!(offset, bytes.len() as u64);
    (header, frames)
}

/// Fails every write of a copy after `bytes` bytes until dropped.
struct Fault;

impl Fault {
    fn after(bytes: u64) -> Self {
        FAULT.with(|budget| budget.set(Some(bytes)));
        Self
    }
}

impl Drop for Fault {
    fn drop(&mut self) {
        FAULT.with(|budget| budget.set(None));
    }
}

#[test]
fn a_copy_keeps_every_frame_under_a_new_lineage_and_the_source_is_untouched() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    source_at(&source, None);
    let before = fs::read(&source).unwrap();
    let source_kept = kept(&Database::open(&source).unwrap());
    let copy = dir.path().join("t.vlite");
    let report = enable_temporal(&source, &copy, None).unwrap();
    assert_eq!(report, maintenance::inspect(&source).unwrap());
    assert_eq!(fs::read(&source).unwrap(), before);

    // The header gains the flag and a new lineage; version, dimensions and
    // collection floor stay, and every frame is copied byte for byte.
    let after = fs::read(&copy).unwrap();
    assert_eq!(after[..16], before[..16]);
    assert_ne!(after[16..24], before[16..24]);
    assert_eq!(after[24..26], before[24..26]);
    let flags = |bytes: &[u8]| u16::from_le_bytes([bytes[26], bytes[27]]);
    assert_eq!(flags(&after), flags(&before) | FLAG_TEMPORAL);
    assert_eq!(after[32..], before[32..]);

    // The copy keeps the logical content, revisions, absolute deadlines
    // (the due edge "bc" is not compacted away) and sequence.
    let mut db = Database::open(&copy).unwrap();
    assert!(is_temporal(&db));
    assert_eq!(kept(&db), source_kept);
    assert!(
        stored(&db)
            .iter()
            .any(|(key, ..)| key.as_deref() == Some("bc"))
    );
    let source_db = Database::open(&source).unwrap();
    assert_ne!(db.lineage, source_db.lineage);
    // The source stays non-temporal and refuses temporal writes.
    let mut source_db = source_db;
    assert!(!is_temporal(&source_db));
    assert!(matches!(
        write(&mut source_db, &[create("ab", Vec::new())]),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    drop(source_db);
    assert_eq!(fs::read(&source).unwrap(), before);

    // The copy takes temporal writes; the next one reclaims "bc".
    write(&mut db, &[create("ab", vec![seg(None, None, 1)])]).unwrap();
    assert!(
        !stored(&db)
            .iter()
            .any(|(key, ..)| key.as_deref() == Some("bc"))
    );
    drop(db);
    verify_graph(&copy, None).unwrap();
    // A copy cannot be activated again.
    let again = dir.path().join("again.vlite");
    assert!(matches!(
        enable_temporal(&copy, &again, None),
        Err(Error::GraphInvalidOptions(_))
    ));
    assert!(!again.exists());

    // Builds without support refuse to activate and to open the copy.
    let _off = Activation::set(false);
    let refused = dir.path().join("off.vlite");
    assert!(matches!(
        enable_temporal(&source, &refused, None),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
    assert!(!refused.exists());
    assert!(matches!(
        Database::open(&copy),
        Err(Error::UnsupportedFeature(FLAG_TEMPORAL))
    ));
}

#[test]
fn activation_refuses_without_publishing_anything() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    source_at(&source, None);
    let before = fs::read(&source).unwrap();
    let destination = dir.path().join("t.vlite");

    // An existing destination is never replaced.
    fs::write(&destination, b"keep").unwrap();
    assert!(matches!(
        enable_temporal(&source, &destination, None),
        Err(Error::Io(_))
    ));
    assert_eq!(fs::read(&destination).unwrap(), b"keep");
    fs::remove_file(&destination).unwrap();
    // An open source is locked.
    {
        let _held = Database::open(&source).unwrap();
        assert!(matches!(
            enable_temporal(&source, &destination, None),
            Err(Error::Locked)
        ));
    }
    // A key for a plain source.
    assert!(matches!(
        enable_temporal(&source, &destination, Some(&key())),
        Err(Error::NotEncrypted)
    ));
    // A file without graphs.
    let v4 = dir.path().join("v4.vlite");
    let options = CreateOptions {
        format: Some(FormatVersion::V4),
        ..CreateOptions::default()
    };
    Database::create_with_options(&v4, 2, options)
        .unwrap()
        .close()
        .unwrap();
    assert!(matches!(
        enable_temporal(&v4, &destination, None),
        Err(Error::GraphsNotEnabled)
    ));
    // A torn tail is reported, not copied or repaired.
    let torn = dir.path().join("torn.vlite");
    fs::write(&torn, &before[..before.len() - 3]).unwrap();
    assert!(matches!(
        enable_temporal(&torn, &destination, None),
        Err(Error::IncompleteTail { .. })
    ));
    assert_eq!(fs::read(&source).unwrap(), before);
    assert_eq!(names(dir.path()), ["s.vlite", "torn.vlite", "v4.vlite"]);
}

/// Staged-write faults (a partial header, a header without frames, a torn
/// frame) and a failure before the name appears publish nothing; one after
/// it is `PublicationIndeterminate` with a complete copy, which a retry
/// never replaces. The source is never touched.
#[test]
fn publication_faults_never_leave_a_partial_copy() {
    let _on = Activation::on();
    for key in [None, Some(key())] {
        let key = key.as_ref();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("s.vlite");
        source_at(&source, key);
        let before = fs::read(&source).unwrap();
        let expected = kept(&open(&source, key));
        let start = frames(&before, key).0.start;
        let listing = names(dir.path());
        let copy = dir.path().join("t.vlite");
        for budget in [16, start, start + 40, before.len() as u64 * 2 / 3] {
            let _fault = Fault::after(budget);
            assert!(
                matches!(enable_temporal(&source, &copy, key), Err(Error::Io(_))),
                "{budget}"
            );
            assert_eq!(names(dir.path()), listing);
        }
        {
            let _armed = failpoint::arm(|stage| match stage {
                Stage::Staged => Err(io::Error::other("injected staged failure")),
                Stage::Visible => Ok(()),
            });
            assert!(matches!(
                enable_temporal(&source, &copy, key),
                Err(Error::Io(_))
            ));
            assert_eq!(names(dir.path()), listing);
        }
        {
            let _armed = failpoint::arm(|stage| match stage {
                Stage::Staged => Ok(()),
                Stage::Visible => Err(io::Error::other("injected directory sync failure")),
            });
            assert!(matches!(
                enable_temporal(&source, &copy, key),
                Err(Error::PublicationIndeterminate { destination, .. }) if destination == copy
            ));
        }
        let published = fs::read(&copy).unwrap();
        let db = open(&copy, key);
        assert!(is_temporal(&db));
        assert_eq!(kept(&db), expected);
        drop(db);
        assert!(matches!(
            enable_temporal(&source, &copy, key),
            Err(Error::Io(_))
        ));
        assert_eq!(fs::read(&copy).unwrap(), published);
        assert_eq!(fs::read(&source).unwrap(), before);
    }
}

/// An encrypted copy stays encrypted under the same key, with a fresh salt
/// (its own cipher domain, like every other copy-out), so its frames are
/// resealed: the plaintext frames are identical, the ciphertext is not.
#[test]
fn an_encrypted_copy_is_resealed_under_the_same_key() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    let key = key();
    source_at(&source, Some(&key));
    let before = fs::read(&source).unwrap();
    let copy = dir.path().join("t.vlite");
    assert!(matches!(
        enable_temporal(&source, &copy, None),
        Err(Error::EncryptionKeyRequired)
    ));
    assert!(matches!(
        enable_temporal(&source, &copy, Some(&EncryptionKey::from_bytes([8; 32]))),
        Err(Error::EncryptionKeyMismatch { .. })
    ));
    assert!(!copy.exists());
    enable_temporal(&source, &copy, Some(&key)).unwrap();
    assert_eq!(fs::read(&source).unwrap(), before);

    let after = fs::read(&copy).unwrap();
    let (source_header, source_frames) = frames(&before, Some(&key));
    let (header, copied) = frames(&after, Some(&key));
    assert_eq!(header.flags, source_header.flags | FLAG_TEMPORAL);
    assert_eq!(header.flags & FLAG_ENCRYPTED, FLAG_ENCRYPTED);
    assert_eq!(header.start, source_header.start);
    let start = header.start as usize;
    assert_ne!(after[32..start], before[32..start]);
    assert_eq!(after.len(), before.len());
    assert_ne!(after[start..], before[start..]);
    assert_eq!(copied, source_frames);

    assert!(matches!(
        Database::open(&copy),
        Err(Error::EncryptionKeyRequired)
    ));
    let expected = kept(&open(&source, Some(&key)));
    let mut db = open(&copy, Some(&key));
    assert_eq!(kept(&db), expected);
    write(&mut db, &[create("ab", vec![seg(None, Some(5), 1)])]).unwrap();
    let written = stored(&db);
    drop(db);
    assert_eq!(stored(&open(&copy, Some(&key))), written);
    maintenance::inspect_with_key(&copy, &key).unwrap();
}

/// The source's backups and point-in-time restore keep working; they
/// cannot anchor the copy's (another lineage), whose own backups and
/// restores carry temporal state.
#[test]
fn backups_and_restore_continue_on_both_sides() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let source = path("s.vlite");
    source_at(&source, None);
    let base = path("base.vlite");
    maintenance::backup(&source, &base).unwrap();
    let mut db = Database::open(&source).unwrap();
    write(&mut db, &[legacy("bd", 2)]).unwrap();
    let activated = db.sequence;
    db.close().unwrap();

    enable_temporal(&source, path("t.vlite"), None).unwrap();
    // The source keeps writing; its old backup still anchors its restore.
    let mut db = Database::open(&source).unwrap();
    write(&mut db, &[legacy("db", 3)]).unwrap();
    db.close().unwrap();
    let report = maintenance::restore_to(
        Some(base.as_path()),
        &source,
        path("r1.vlite"),
        RestoreTarget::Sequence(activated),
        None,
    )
    .unwrap();
    assert_eq!(report.restored_sequence, activated);
    let restored = Database::open(path("r1.vlite")).unwrap();
    assert!(!is_temporal(&restored));
    assert!(graph(&restored).edge_by_key("bd").is_some());
    assert!(graph(&restored).edge_by_key("db").is_none());
    drop(restored);
    assert!(matches!(
        maintenance::restore_to(
            Some(base.as_path()),
            path("t.vlite"),
            path("r2.vlite"),
            RestoreTarget::Sequence(activated),
            None,
        ),
        Err(Error::InvalidRestoreTarget(
            "base backup has a different lineage than the log"
        ))
    ));
    assert!(!path("r2.vlite").exists());

    // The copy's own history restores at any revision, before activation
    // included (as a temporal file without temporal edges).
    let mut db = Database::open(path("t.vlite")).unwrap();
    let s1 = write(&mut db, &[create("ab", vec![seg(None, None, 1)])])
        .unwrap()
        .revision
        .unwrap();
    db.close().unwrap();
    let copy_base = path("t-base.vlite");
    maintenance::backup(path("t.vlite"), &copy_base).unwrap();
    let mut db = Database::open(path("t.vlite")).unwrap();
    let s2 = write(
        &mut db,
        &[
            close("ab", None, None, 10, Expected::Revision(s1)),
            append("ab", seg(Some(10), None, 2), Expected::Revision(s1)),
        ],
    )
    .unwrap()
    .revision
    .unwrap();
    let head = stored(&db);
    db.close().unwrap();
    let restore = |base: Option<&Path>, name: &str, sequence| {
        maintenance::restore_to(
            base,
            path("t.vlite"),
            path(name),
            RestoreTarget::Sequence(sequence),
            None,
        )
        .unwrap();
        Database::open(path(name)).unwrap()
    };
    let db = restore(None, "r3.vlite", activated);
    assert!(is_temporal(&db) && graph(&db).edge_by_key("ab").is_none());
    let db = restore(None, "r4.vlite", s1);
    assert_eq!(series(&db, "ab"), [seg(None, None, 1)]);
    assert_eq!(revision(&db, "ab"), s1);
    let db = restore(Some(copy_base.as_path()), "r5.vlite", s2);
    assert_eq!(stored(&db), head);
    assert_eq!(revision(&db, "ab"), s2);
}

/// Replicas of the source keep following it; the copy is another lineage
/// they refuse unchanged, and its replicas are seeded from it afresh.
#[test]
fn replicas_follow_their_own_lineage_and_reseed_from_the_copy() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let source = path("s.vlite");
    source_at(&source, None);
    let mut follower = ReplicaFollower::create(
        path("f.vlite"),
        &maintenance::export_log_segment(&source, None, 0, usize::MAX).unwrap(),
        None,
    )
    .unwrap();
    let copy = path("t.vlite");
    enable_temporal(&source, &copy, None).unwrap();

    let mut db = Database::open(&source).unwrap();
    write(&mut db, &[legacy("bd", 2)]).unwrap();
    let source_head = db.sequence;
    db.close().unwrap();
    follower.sync_from_log(&source, usize::MAX).unwrap();
    assert_eq!(follower.applied_sequence(), source_head);
    assert!(!is_temporal(follower.db()));

    let mut db = Database::open(&copy).unwrap();
    let s1 = write(&mut db, &[create("ab", vec![seg(None, None, 1)])])
        .unwrap()
        .revision
        .unwrap();
    db.close().unwrap();
    let followed = stored(follower.db());
    let segment = maintenance::export_log_segment(&copy, None, s1 - 1, usize::MAX).unwrap();
    assert!(matches!(
        follower.apply(&segment),
        Err(Error::ReplicaDiverged("segment is from another lineage"))
    ));
    assert_eq!(follower.applied_sequence(), source_head);
    assert_eq!(stored(follower.db()), followed);
    drop(follower);

    let mut reseeded = ReplicaFollower::create(
        path("f2.vlite"),
        &maintenance::export_log_segment(&copy, None, 0, usize::MAX).unwrap(),
        None,
    )
    .unwrap();
    assert!(is_temporal(reseeded.db()));
    let mut db = Database::open(&copy).unwrap();
    write(
        &mut db,
        &[close("ab", None, None, 10, Expected::Revision(s1))],
    )
    .unwrap();
    let head = stored(&db);
    db.close().unwrap();
    reseeded.sync_from_log(&copy, usize::MAX).unwrap();
    assert_eq!(stored(reseeded.db()), head);
    assert_eq!(series(reseeded.db(), "ab"), [seg(None, Some(10), 1)]);
}

/// A directory store copies out as one temporal file and is not touched.
#[test]
fn a_directory_store_copies_out_to_a_single_temporal_file() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("s.vlite");
    source_at(&source, None);
    let store = dir.path().join("store");
    maintenance::convert_to_directory(&source, &store).unwrap();
    let mut db = Database::open(&store).unwrap();
    db.set_checkpoint_threshold(None);
    write(&mut db, &[legacy("bd", 2)]).unwrap();
    assert!(db.checkpoint().unwrap().is_some());
    write(&mut db, &[legacy("db", 3)]).unwrap();
    db.close().unwrap();
    // As the store reopens: its checkpoint restated the ordinary edges.
    let expected = kept(&Database::open(&store).unwrap());
    let files = tree(&store);

    let copy = dir.path().join("t.vlite");
    enable_temporal(&store, &copy, None).unwrap();
    assert_eq!(tree(&store), files);
    assert!(copy.is_file());
    let mut db = Database::open(&copy).unwrap();
    assert!(is_temporal(&db));
    assert_eq!(kept(&db), expected);
    let r = revision(&db, "ac");
    write(
        &mut db,
        &[GraphOp::Temporal(super::super::TemporalOp::PromoteEdge {
            edge: edge("ac"),
            expected: Expected::Revision(r),
        })],
    )
    .unwrap();
    assert_eq!(series(&db, "ac").len(), 1);
    drop(db);
    verify_graph(&copy, None).unwrap();
    let store_db = Database::open(&store).unwrap();
    assert!(!is_temporal(&store_db));
}

/// A precision image keeps its catalog, history floor and precision flags.
#[test]
fn a_precision_image_copies_out_with_its_catalog() {
    let _precision = crate::compact::Activation::on();
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let header = format::header_v5_flags(2, 0x5EED_0000_0000_0065, 0, FLAG_GRAPH, None);
    let mut loaded = format::load_with(&mut Cursor::new(header), None).unwrap();
    loaded.flags |= FLAG_PRECISION;
    loaded.sequence = 1;
    loaded.collections.get_mut(&0).unwrap().precision =
        Some(crate::compact::catalog::Precision::Float32);
    let mut image = Vec::new();
    maintenance::write_v5_image(
        &mut image,
        &loaded,
        loaded.lineage.unwrap(),
        None,
        None,
        None,
        FLAG_GRAPH | FLAG_PRECISION,
        1,
    )
    .unwrap();
    let source = dir.path().join("p.vlite");
    fs::write(&source, image).unwrap();
    let mut db = Database::open(&source).unwrap();
    populate(&mut db);
    write(&mut db, &[put_vector(1)]).unwrap();
    let expected = (stored(&db), db.get(1).unwrap().cloned());
    db.close().unwrap();
    let before = fs::read(&source).unwrap();

    let copy = dir.path().join("t.vlite");
    enable_temporal(&source, &copy, None).unwrap();
    assert_eq!(fs::read(&source).unwrap(), before);
    let loaded = format::load_with(&mut fs::File::open(&copy).unwrap(), None).unwrap();
    let wanted = FLAG_GRAPH | FLAG_PRECISION | FLAG_COMPACT | FLAG_HISTORY | FLAG_TEMPORAL;
    assert_eq!(loaded.flags & wanted, wanted);
    assert_eq!(
        loaded.collections[&0].precision,
        Some(crate::compact::catalog::Precision::Float32)
    );
    let mut db = Database::open(&copy).unwrap();
    assert_eq!((stored(&db), db.get(1).unwrap().cloned()), expected);
    write(&mut db, &[create("ab", vec![seg(None, None, 1)])]).unwrap();
    let written = stored(&db);
    let (_, db) = bytes_of(db, &copy);
    assert_eq!(stored(&db), written);
    assert!(file_len(&copy) > before.len() as u64);
}
