//! Gap 3 TTL graph feed across maintenance and physical paths
//! (`docs/projects/roadmap/gap3-ttl-feed.md`, section 10).
//!
//! Every source is the hand-built `ttl_log` reclaim fixture (or a small
//! sibling); expected streams are the literal `RECLAIM_TTL` lines or written
//! out here. A rewrite that changes the lineage must refuse the old cursor;
//! one that restates state must not invent deletes for what it dropped.

mod ttl_log;

use std::path::Path;

use tempfile::tempdir;
use ttl_log::{LINEAGE, RECLAIM_TTL, reclaim_fixture};
use vlite::changes::{
    FeedCursor, GraphEventSet, GraphFeedBatch, GraphFeedCursor, GraphFeedError,
    read_log_graph_changes,
};
use vlite::maintenance::{self, RestoreTarget};
use vlite::property_graph::{enable_graph, strip_graph};
use vlite::{Database, EncryptionKey, Error, ReplicaFollower};

const TTL: GraphEventSet = GraphEventSet::Ttl;

fn at(lineage: u64, sequence: u64) -> GraphFeedCursor {
    GraphFeedCursor::at_commit(FeedCursor { lineage, sequence }, TTL)
}

fn split(lineage: u64, sequence: u64, ordinal: u32) -> GraphFeedCursor {
    let text = format!("vlc2.{lineage:016x}.{sequence:016x}.{ordinal:08x}.ttl");
    let token = format!("{text}.{:08x}", ttl_log::crc32(text.as_bytes()));
    GraphFeedCursor::from_token(&token).unwrap()
}

fn lines(page: &GraphFeedBatch) -> Vec<String> {
    page.changes.iter().map(ttl_log::describe).collect()
}

/// Every line from `from` to the end of the closed file, in pages of 2.
fn read_all(
    path: &Path,
    key: Option<&EncryptionKey>,
    mut from: GraphFeedCursor,
) -> Result<Vec<String>, GraphFeedError> {
    let mut all = Vec::new();
    loop {
        let page = read_log_graph_changes(path, key, &from, TTL, 2)?;
        all.extend(lines(&page));
        if !page.has_more {
            return Ok(all);
        }
        from = page.next;
    }
}

fn source(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("source.vlite");
    std::fs::write(&path, reclaim_fixture().bytes).unwrap();
    path
}

fn lineage(path: &Path, key: Option<&EncryptionKey>) -> u64 {
    let db = match key {
        Some(key) => Database::open_encrypted(path, key),
        None => Database::open(path),
    }
    .unwrap();
    let lineage = db.lineage().unwrap();
    db.close().unwrap();
    lineage
}

/// The old lineage's cursors, at a commit and inside a reclaim, are refused.
fn assert_compacted(path: &Path, key: Option<&EncryptionKey>, current: u64) {
    assert_ne!(current, LINEAGE);
    for old in [at(LINEAGE, 7), split(LINEAGE, 7, 0)] {
        match read_log_graph_changes(path, key, &old, TTL, 10) {
            Err(GraphFeedError::Engine(Error::CursorCompacted {
                cursor_lineage,
                current_lineage,
            })) => assert_eq!((cursor_lineage, current_lineage), (LINEAGE, current)),
            other => panic!("{other:?}"),
        }
    }
}

/// The current state of the fixture after commit 14, restated by a rewrite:
/// the reclaimed, deleted and dropped history leaves no delete behind.
const RESTATED: [&str; 4] = [
    "1/0 put 2",
    "2/0 VectorsUpdated",
    "3/0 create g",
    "3/1 g: node+0:a ttl=null",
];

/// Backup copies bytes and lineage, so cursors keep working; restore and
/// point-in-time restore keep the logged history (PITR cut at a whole
/// commit) under a new lineage.
#[test]
fn ttl_backup_restore() {
    let dir = tempdir().unwrap();
    let src = source(dir.path());
    let backup = dir.path().join("backup.vlite");
    maintenance::backup(&src, &backup).unwrap();
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        std::fs::read(&src).unwrap()
    );
    assert_eq!(
        read_all(&backup, None, GraphFeedCursor::beginning(TTL)).unwrap(),
        RECLAIM_TTL
    );
    assert_eq!(
        read_all(&backup, None, at(LINEAGE, 7)).unwrap(),
        RECLAIM_TTL[8..]
    );
    assert_eq!(
        read_all(&backup, None, split(LINEAGE, 7, 0)).unwrap(),
        RECLAIM_TTL[7..]
    );

    let restored = dir.path().join("restored.vlite");
    maintenance::restore(&backup, &restored).unwrap();
    let new = lineage(&restored, None);
    assert_compacted(&restored, None, new);
    assert_eq!(
        read_all(&restored, None, GraphFeedCursor::beginning(TTL)).unwrap(),
        RECLAIM_TTL
    );
    assert_eq!(
        read_all(&restored, None, split(new, 8, 0)).unwrap(),
        RECLAIM_TTL[9..]
    );

    let pitr = dir.path().join("pitr.vlite");
    maintenance::restore_to(None, &src, &pitr, RestoreTarget::Sequence(8), None).unwrap();
    let new = lineage(&pitr, None);
    assert_compacted(&pitr, None, new);
    let page =
        read_log_graph_changes(&pitr, None, &GraphFeedCursor::beginning(TTL), TTL, 100).unwrap();
    assert_eq!(lines(&page), RECLAIM_TTL[..10]);
    assert_eq!(
        (page.head, page.next, page.has_more),
        (8, at(new, 8), false)
    );
}

/// Compaction and upgrade restate current state under a new lineage; a
/// due-but-unreclaimed node is dropped by the rewrite without a delete.
#[test]
fn ttl_rewrite_lineage() {
    let dir = tempdir().unwrap();
    let src = source(dir.path());
    let compacted = dir.path().join("compacted.vlite");
    maintenance::compact_at(&src, &compacted, 1_000).unwrap();
    let upgraded = dir.path().join("upgraded.vlite");
    maintenance::upgrade(&src, &upgraded).unwrap();
    let mut lineages = Vec::new();
    for path in [&compacted, &upgraded] {
        let new = lineage(path, None);
        assert_compacted(path, None, new);
        assert_eq!(
            read_all(path, None, GraphFeedCursor::beginning(TTL)).unwrap(),
            RESTATED
        );
        lineages.push(new);
    }
    assert_ne!(lineages[0], lineages[1]);

    // Node a (deadline 1) is due at the rewrite clock, b (deadline 5000) is
    // not: a and its edge vanish without a delete, b keeps its deadline.
    use ttl_log::Entry::*;
    let mut log = ttl_log::Log::new(2);
    log.graph(0, &[CreateGraph("g")]).graph(
        0,
        &[
            PutNode(0, "a", &[]),
            NodeExpiry(0, Some(1)),
            PutNode(1, "b", &[]),
            NodeExpiry(1, Some(5_000)),
            PutEdge(0, 0, 1, "R", Some("ab")),
        ],
    );
    let due = dir.path().join("due.vlite");
    std::fs::write(&due, &log.bytes).unwrap();
    let rewritten = dir.path().join("due-compacted.vlite");
    maintenance::compact_at(&due, &rewritten, 1_000).unwrap();
    assert_eq!(
        read_all(&rewritten, None, GraphFeedCursor::beginning(TTL)).unwrap(),
        [
            "1/0 VectorsUpdated",
            "2/0 create g",
            "2/1 g: node+1:b ttl=null, node~1:b ttl=5000!",
        ]
    );
}

/// Stripping removes every graph event under a new lineage; enabling the
/// stripped file again is another new lineage with no graph history.
#[test]
fn ttl_graph_conversion() {
    let dir = tempdir().unwrap();
    let src = source(dir.path());
    let stripped = dir.path().join("stripped.vlite");
    strip_graph(&src, &stripped, None, true).unwrap();
    let enabled = dir.path().join("enabled.vlite");
    enable_graph(&stripped, &enabled, None).unwrap();
    let mut lineages = Vec::new();
    for path in [&stripped, &enabled] {
        let new = lineage(path, None);
        assert_compacted(path, None, new);
        assert_eq!(
            read_all(path, None, GraphFeedCursor::beginning(TTL)).unwrap(),
            RESTATED[..2]
        );
        lineages.push(new);
    }
    assert_ne!(lineages[0], lineages[1]);
}

/// Re-encryption keeps the authenticated plaintext history; the feed needs
/// the right key and the old lineage's cursors are refused.
#[test]
fn ttl_encrypted_history() {
    let dir = tempdir().unwrap();
    let src = source(dir.path());
    let key = EncryptionKey::from_bytes([5; 32]);
    let sealed = dir.path().join("sealed.vlite");
    maintenance::reencrypt(&src, None, &sealed, Some(&key)).unwrap();
    let new = lineage(&sealed, Some(&key));
    assert_compacted(&sealed, Some(&key), new);
    assert_eq!(
        read_all(&sealed, Some(&key), GraphFeedCursor::beginning(TTL)).unwrap(),
        RECLAIM_TTL
    );
    assert_eq!(
        read_all(&sealed, Some(&key), split(new, 13, 0)).unwrap(),
        RECLAIM_TTL[15..]
    );
    let beginning = GraphFeedCursor::beginning(TTL);
    assert!(matches!(
        read_log_graph_changes(&sealed, None, &beginning, TTL, 10),
        Err(GraphFeedError::Engine(Error::EncryptionKeyRequired))
    ));
    let wrong = EncryptionKey::from_bytes([6; 32]);
    assert!(matches!(
        read_log_graph_changes(&sealed, Some(&wrong), &beginning, TTL, 10),
        Err(GraphFeedError::Engine(Error::EncryptionKeyMismatch { .. }))
    ));
    let db = Database::open_encrypted(&sealed, &key).unwrap();
    let page = db.graph_changes_after(&split(new, 7, 0), TTL, 100).unwrap();
    assert_eq!(lines(&page), RECLAIM_TTL[7..]);
    db.close().unwrap();
}

/// Export ships the source frames unchanged; a follower fed in several
/// segments reconstructs the same TTL stream and cursors.
#[test]
fn ttl_replica_export() {
    let dir = tempdir().unwrap();
    let src = source(dir.path());
    let bytes = std::fs::read(&src).unwrap();
    let whole = maintenance::export_log_segment(&src, None, 0, 1 << 20).unwrap();
    assert_eq!(
        (whole.lineage, whole.base_sequence, whole.last_sequence),
        (LINEAGE, 0, 14)
    );
    assert_eq!(whole.header, bytes[..32]);
    assert_eq!(whole.frames, bytes[32..]);

    let first = maintenance::export_log_segment(&src, None, 0, 300).unwrap();
    assert!(first.last_sequence < 14);
    let path = dir.path().join("follower.vlite");
    let mut follower = ReplicaFollower::create(&path, &first, None).unwrap();
    while follower.applied_sequence() < 14 {
        let next =
            maintenance::export_log_segment(&src, None, follower.applied_sequence(), 300).unwrap();
        follower.apply(&next).unwrap();
    }
    let db = follower.db();
    let page = db
        .graph_changes_after(&GraphFeedCursor::beginning(TTL), TTL, 100)
        .unwrap();
    assert_eq!(lines(&page), RECLAIM_TTL);
    let page = db
        .graph_changes_after(&split(LINEAGE, 8, 0), TTL, 100)
        .unwrap();
    assert_eq!(lines(&page), RECLAIM_TTL[9..]);
}

/// Production gates: the compact golden image (history floor 4, derived
/// blocks only) reads as an empty TTL page at its floor, with the floor
/// rule below it; a corrupted image is refused, never misread.
#[test]
fn ttl_compact_precision_images() {
    const IMAGE_LINEAGE: u64 = 0x5EED_0000_0000_0001;
    let text = include_str!("fixtures/compact/derived-only-v5.hex");
    let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
    let bytes: Vec<u8> = digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let dir = tempdir().unwrap();
    let path = dir.path().join("compact.vlite");
    std::fs::write(&path, &bytes).unwrap();
    let read = |cursor: &GraphFeedCursor| read_log_graph_changes(&path, None, cursor, TTL, 10);
    let page = read(&at(IMAGE_LINEAGE, 4)).unwrap();
    assert_eq!(
        (page.changes.len(), page.next, page.head, page.has_more),
        (0, at(IMAGE_LINEAGE, 4), 4, false)
    );
    assert!(matches!(
        read(&at(IMAGE_LINEAGE, 3)),
        Err(GraphFeedError::Engine(Error::CursorBeforeHistory {
            cursor: 3,
            floor: 4
        }))
    ));
    assert!(matches!(
        read(&split(IMAGE_LINEAGE, 4, 0)),
        Err(GraphFeedError::CursorBeforeHistory {
            cursor: 4,
            floor: 4
        })
    ));
    let mut corrupt = bytes.clone();
    corrupt[40] ^= 1;
    std::fs::write(&path, &corrupt).unwrap();
    let refused = read(&at(IMAGE_LINEAGE, 4));
    assert!(
        matches!(refused, Err(GraphFeedError::Engine(Error::Corrupt { .. }))),
        "{refused:?}"
    );
}
