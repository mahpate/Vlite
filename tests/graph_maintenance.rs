//! EPIC-006 TEST-008 (ITEM-022..ITEM-026): graph-aware maintenance, built
//! only with the `graph` feature.
//!
//! Graph state must be equal after `compact*`, `backup*`/`restore*`, `drill`,
//! `reencrypt` and `restore_to` (by sequence and by timestamp) in plain,
//! encrypted and commit-timestamp files; `FLAG_GRAPH` is carried into every
//! copy; compaction keeps the node/edge high-water marks across reopen;
//! `enable_graph`/`strip_graph` are verified copy-outs; `inspect_graph` and
//! `verify_graph` report graph counts; `upgrade` and change feeds reject graph
//! files explicitly; replica followers replay graph frames (including a torn
//! tail and promotion); and `vlite-maintain` exposes `enable-graph` and
//! `strip-graph`.

mod graph_support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use graph_support::*;
use tempfile::tempdir;
use vlite::changes::{FeedCursor, read_log_changes};
use vlite::maintenance::{self, RestoreTarget};
use vlite::{Database, EncryptionKey, Error, FormatVersion, Metadata, ReplicaFollower};

fn compact(
    source: &Path,
    output: &Path,
    key: Option<&EncryptionKey>,
) -> maintenance::CompactionReport {
    match key {
        Some(key) => maintenance::compact_with_key(source, output, key),
        None => maintenance::compact_at(source, output, 5_000),
    }
    .unwrap()
}

#[test]
fn compaction_keeps_graph_state_flag_and_high_water_marks() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let source = graph_file(dir.path(), "source", kind);
        let original = fs::read(&source).unwrap();
        let before = snapshot(&source, key.as_ref());
        assert_eq!(before.counts(), POPULATED);

        let output = dir.path().join(format!("compact-{kind:?}.vlite"));
        let report = compact(&source, &output, key.as_ref());
        assert_eq!(fs::read(&source).unwrap(), original, "{kind:?}");
        // Compaction drops commit timestamps, exactly as for vector files.
        assert_eq!(
            header_flags(&output),
            kind.graph_flags() & !FLAG_TIMESTAMPS,
            "{kind:?}"
        );
        assert!(report.output.commits < report.source.commits);
        // Sequences restart, so revisions differ; everything else is equal.
        assert_eq!(
            snapshot(&output, key.as_ref()).without_revisions(),
            before.clone().without_revisions(),
            "{kind:?}"
        );
        let inspection = maintenance::inspect_graph(&output, key.as_ref()).unwrap();
        assert!(inspection.graph_enabled);
        assert_counts(&inspection.counts, POPULATED);
        assert_eq!(inspection.inspection, report.output);

        // Reopen: the deleted highest node and edge IDs are never reused.
        let mut db = open(&output, key.as_ref());
        assert!(!db.has_commit_timestamps());
        let mut alpha = db.graph_mut("alpha").unwrap();
        let node = alpha
            .create_node("fresh", &["New"], &Metadata::new())
            .unwrap();
        let edge = alpha
            .create_edge(None, "fresh", "NEW", "ada", &Metadata::new())
            .unwrap();
        assert_eq!(
            HighWater {
                node: node.id,
                edge: edge.id
            },
            ALPHA_HIGH_WATER,
            "{kind:?}"
        );
        db.close().unwrap();
        let db = open(&output, key.as_ref());
        let alpha = db.graph("alpha").unwrap();
        assert_eq!(alpha.get_node("fresh").unwrap().output.unwrap().id, node.id);
        assert_eq!(
            alpha.get_edge(edge.id).unwrap().output.unwrap().source_key,
            "fresh"
        );
        drop(db);

        // A compacted file compacts again to the same state.
        let again = dir.path().join(format!("compact-again-{kind:?}.vlite"));
        compact(&output, &again, key.as_ref());
        assert_eq!(
            snapshot(&again, key.as_ref()).without_revisions(),
            snapshot(&output, key.as_ref()).without_revisions()
        );
    }
}

#[test]
fn backup_restore_and_drill_copy_graph_files_exactly() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let source = graph_file(dir.path(), "source", kind);
        let before = snapshot(&source, key.as_ref());
        let backup = dir.path().join(format!("backup-{kind:?}.vlite"));
        let restored = dir.path().join(format!("restored-{kind:?}.vlite"));
        match &key {
            Some(key) => {
                maintenance::backup_with_key(&source, &backup, key).unwrap();
                maintenance::restore_with_key(&backup, &restored, key).unwrap();
            }
            None => {
                maintenance::backup(&source, &backup).unwrap();
                maintenance::restore(&backup, &restored).unwrap();
            }
        }
        assert_eq!(fs::read(&backup).unwrap(), fs::read(&source).unwrap());
        assert_eq!(header_flags(&restored), kind.graph_flags());
        assert_eq!(snapshot(&restored, key.as_ref()), before, "{kind:?}");
        assert_eq!(
            open(&restored, key.as_ref()).has_commit_timestamps(),
            kind == Kind::Timestamped
        );
        if key.is_none() {
            let workdir = dir.path().join(format!("drill-{kind:?}"));
            fs::create_dir(&workdir).unwrap();
            let report = maintenance::drill(&backup, &workdir).unwrap();
            assert_eq!((report.collections, report.probes), (2, 2));
            assert_eq!(fs::read_dir(&workdir).unwrap().count(), 0);
        }
    }
}

#[test]
fn reencrypt_reseals_graph_frames_and_keeps_the_flag() {
    let dir = tempdir().unwrap();
    let cases = [
        (Kind::Plain, Some(other_key())),
        (Kind::Encrypted, None),
        (Kind::Encrypted, Some(other_key())),
        (Kind::Timestamped, Some(other_key())),
    ];
    for (index, (kind, destination_key)) in cases.into_iter().enumerate() {
        let key = kind.key();
        let source = graph_file(dir.path(), &format!("source-{index}"), kind);
        let before = snapshot(&source, key.as_ref());
        let output = dir.path().join(format!("reencrypted-{index}.vlite"));
        maintenance::reencrypt(&source, key.as_ref(), &output, destination_key.as_ref()).unwrap();
        let encrypted = if destination_key.is_some() {
            FLAG_ENCRYPTED
        } else {
            0
        };
        assert_eq!(
            header_flags(&output),
            (kind.graph_flags() & !FLAG_ENCRYPTED) | encrypted,
            "{kind:?}"
        );
        // The whole log is kept, so revisions are equal too.
        assert_eq!(
            snapshot(&output, destination_key.as_ref()),
            before,
            "{kind:?}"
        );
        let mut db = open(&output, destination_key.as_ref());
        assert_eq!(db.has_commit_timestamps(), kind == Kind::Timestamped);
        db.graph_mut("beta")
            .unwrap()
            .create_node("after", &[], &Metadata::new())
            .unwrap();
        db.close().unwrap();
        let verification = maintenance::verify_graph(&output, destination_key.as_ref()).unwrap();
        assert_counts(&verification.counts, [3, 7, 6, 4]);
    }
}

#[test]
fn restore_to_cuts_graph_logs_at_whole_commits() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let source = dir.path().join(format!("pitr-{kind:?}.vlite"));
        let mut db = Database::create_with_graph(&source, 2, kind.options()).unwrap();
        clock(&mut db, 0);
        let mut states = vec![snapshot_bytes(live_bytes(&db), key.as_ref())];
        populate_with(&mut db, &mut |db| {
            let segment = db.export_log(0, usize::MAX).unwrap();
            assert_eq!(segment.last_sequence, states.len() as u64);
            let mut bytes = segment.header;
            bytes.extend_from_slice(&segment.frames);
            states.push(snapshot_bytes(bytes, key.as_ref()));
        });
        db.close().unwrap();
        let head = states.len() as u64 - 1;
        assert_eq!(commits(&source, key.as_ref()), head);
        for (sequence, expected) in states.iter().enumerate() {
            let sequence = sequence as u64;
            let output = dir.path().join(format!("pitr-{kind:?}-{sequence}.vlite"));
            let report = maintenance::restore_to(
                None,
                &source,
                &output,
                RestoreTarget::Sequence(sequence),
                key.as_ref(),
            )
            .unwrap();
            assert_eq!(
                (report.restored_sequence, report.source_head),
                (sequence, head)
            );
            assert_eq!(header_flags(&output), kind.graph_flags());
            assert_eq!(
                &snapshot(&output, key.as_ref()),
                expected,
                "{kind:?} {sequence}"
            );
            if kind != Kind::Timestamped {
                continue;
            }
            // Commit n was recorded at 1000 + 100 n; 50 ms later is still n.
            let at = dir.path().join(format!("pitr-at-{sequence}.vlite"));
            let report = maintenance::restore_to(
                None,
                &source,
                &at,
                RestoreTarget::TimestampMs(1_050 + 100 * sequence),
                None,
            )
            .unwrap();
            assert_eq!(report.restored_sequence, sequence);
            assert_eq!(
                report.restored_at_ms,
                (sequence > 0).then_some(1_000 + 100 * sequence)
            );
            assert_eq!(&snapshot(&at, None), expected, "timestamp {sequence}");
        }
    }
}

#[test]
fn enable_graph_copies_v4_and_v5_sources_without_modifying_them() {
    let dir = tempdir().unwrap();
    let v4 = dir.path().join("v4.vlite");
    let mut db = Database::create_with_format(&v4, 2, FormatVersion::V4).unwrap();
    populate_vectors(&mut db);
    db.close().unwrap();
    let mut sources = vec![(v4, None, 0)];
    for kind in KINDS {
        let flags = kind.graph_flags() & FLAG_ENCRYPTED;
        sources.push((vector_file(dir.path(), "vector", kind), kind.key(), flags));
    }
    for (index, (source, key, encrypted)) in sources.into_iter().enumerate() {
        let original = fs::read(&source).unwrap();
        let before = snapshot(&source, key.as_ref());
        let output = dir.path().join(format!("enabled-{index}.vlite"));
        let report = maintenance::enable_graph(&source, &output, key.as_ref()).unwrap();
        assert_eq!(
            fs::read(&source).unwrap(),
            original,
            "source {index} changed"
        );
        assert_eq!(report.output.format_version, 5);
        // Like compaction, the copy restarts sequences without timestamps.
        assert_eq!(header_flags(&output), FLAG_GRAPH | encrypted, "{index}");
        let after = snapshot(&output, key.as_ref());
        assert!(after.graph_enabled && after.graphs.is_empty());
        assert_eq!(
            after.vector_only().without_revisions(),
            before.without_revisions(),
            "{index}"
        );

        // The copy accepts graph writes over the carried-over records.
        let mut db = open(&output, key.as_ref());
        db.create_graph("g").unwrap();
        let mut graph = db.graph_mut("g").unwrap();
        graph.create_node("n", &[], &Metadata::new()).unwrap();
        graph.attach("n", "docs", 10).unwrap();
        db.close().unwrap();
        let verification = maintenance::verify_graph(&output, key.as_ref()).unwrap();
        assert_counts(&verification.counts, [1, 1, 0, 1]);
    }

    // V3 sources must be upgraded first; graph sources are refused. No
    // rejection touches its source or creates the destination.
    let v3 = dir.path().join("v3.vlite");
    let mut db = Database::create(&v3, 2).unwrap();
    db.put(1, &[1.0, 1.0], b"one").unwrap();
    db.close().unwrap();
    let graph = graph_file(dir.path(), "graph", Kind::Plain);
    let encrypted = vector_file(dir.path(), "locked", Kind::Encrypted);
    let refused = dir.path().join("refused.vlite");
    for (source, key, expected) in [
        (&v3, None, "options"),
        (&graph, None, "options"),
        (&encrypted, None, "key"),
        (&encrypted, Some(other_key()), "mismatch"),
    ] {
        let original = fs::read(source).unwrap();
        let result = maintenance::enable_graph(source, &refused, key.as_ref());
        match (expected, &result) {
            ("options", Err(Error::GraphInvalidOptions(_)))
            | ("key", Err(Error::EncryptionKeyRequired))
            | ("mismatch", Err(Error::EncryptionKeyMismatch { .. })) => {}
            _ => panic!("{source:?}: {:?}", result.map(|_| ())),
        }
        assert_eq!(fs::read(source).unwrap(), original);
        assert!(!refused.exists());
    }
    // An existing destination is never replaced.
    let plain = vector_file(dir.path(), "plain", Kind::Plain);
    fs::write(&refused, b"keep").unwrap();
    assert!(maintenance::enable_graph(&plain, &refused, None).is_err());
    assert_eq!(fs::read(&refused).unwrap(), b"keep");
}

#[test]
fn strip_graph_requires_confirmation_and_reports_removed_counts() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let source = graph_file(dir.path(), "source", kind);
        let original = fs::read(&source).unwrap();
        let before = snapshot(&source, key.as_ref());
        let output = dir.path().join(format!("stripped-{kind:?}.vlite"));
        assert!(matches!(
            maintenance::strip_graph(&source, &output, key.as_ref(), false),
            Err(Error::GraphInvalidOptions(_))
        ));
        assert!(!output.exists());
        assert_eq!(fs::read(&source).unwrap(), original);

        let report = maintenance::strip_graph(&source, &output, key.as_ref(), true).unwrap();
        assert_counts(&report.removed, POPULATED);
        assert_eq!(
            report.compaction.source.commits,
            commits(&source, key.as_ref())
        );
        assert_eq!(fs::read(&source).unwrap(), original);
        // No FLAG_GRAPH means no op-12 frame can be present: the loader
        // rejects op 12 in files without the flag, in every build.
        assert_eq!(
            header_flags(&output),
            kind.graph_flags() & FLAG_ENCRYPTED,
            "{kind:?}"
        );
        let stripped = snapshot(&output, key.as_ref());
        assert_eq!(
            stripped.clone().without_revisions(),
            before.vector_only().without_revisions(),
            "{kind:?}"
        );
        let inspection = maintenance::inspect_graph(&output, key.as_ref()).unwrap();
        assert!(!inspection.graph_enabled);
        assert_counts(&inspection.counts, [0; 4]);

        // Round trip: enabling the stripped copy gives an empty graph file
        // with the same vector data.
        let again = dir.path().join(format!("reenabled-{kind:?}.vlite"));
        maintenance::enable_graph(&output, &again, key.as_ref()).unwrap();
        let reenabled = snapshot(&again, key.as_ref());
        assert!(reenabled.graph_enabled && reenabled.graphs.is_empty());
        assert_eq!(
            reenabled.vector_only().without_revisions(),
            stripped.without_revisions()
        );
    }
    let vector = vector_file(dir.path(), "vector", Kind::Plain);
    let original = fs::read(&vector).unwrap();
    let output = dir.path().join("not-a-graph.vlite");
    assert!(matches!(
        maintenance::strip_graph(&vector, &output, None, true),
        Err(Error::GraphsNotEnabled)
    ));
    assert!(!output.exists());
    assert_eq!(fs::read(&vector).unwrap(), original);
}

#[test]
fn inspect_graph_and_verify_graph_report_graph_counts() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let path = graph_file(dir.path(), "graph", kind);
        let (inspection, verification) = match &key {
            Some(key) => (
                maintenance::inspect_with_key(&path, key).unwrap(),
                maintenance::verify_with_key(&path, key).unwrap(),
            ),
            None => (
                maintenance::inspect(&path).unwrap(),
                maintenance::verify(&path).unwrap(),
            ),
        };
        let graph_inspection = maintenance::inspect_graph(&path, key.as_ref()).unwrap();
        assert_eq!(graph_inspection.inspection, inspection);
        assert!(graph_inspection.graph_enabled);
        assert_counts(&graph_inspection.counts, POPULATED);
        let graph_verification = maintenance::verify_graph(&path, key.as_ref()).unwrap();
        assert_eq!(graph_verification.verification, verification);
        assert!(graph_verification.graph_enabled);
        assert_counts(&graph_verification.counts, POPULATED);
        if key.is_some() {
            assert!(matches!(
                maintenance::inspect_graph(&path, None),
                Err(Error::EncryptionKeyRequired)
            ));
            assert!(matches!(
                maintenance::verify_graph(&path, None),
                Err(Error::EncryptionKeyRequired)
            ));
        }

        let vector = vector_file(dir.path(), "vector", kind);
        let report = maintenance::verify_graph(&vector, key.as_ref()).unwrap();
        assert!(!report.graph_enabled);
        assert_counts(&report.counts, [0; 4]);
    }

    // Like `verify`, `verify_graph` refuses an incomplete tail that
    // `inspect_graph` reports.
    let path = graph_file(dir.path(), "torn", Kind::Plain);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"VREC");
    fs::write(&path, &bytes).unwrap();
    assert_eq!(
        maintenance::inspect_graph(&path, None)
            .unwrap()
            .inspection
            .incomplete_tail_bytes,
        4
    );
    assert!(matches!(
        maintenance::verify_graph(&path, None),
        Err(Error::IncompleteTail { bytes: 4, .. })
    ));
}

#[test]
fn upgrade_and_change_feeds_reject_graph_files_explicitly() {
    let dir = tempdir().unwrap();
    let start = FeedCursor {
        lineage: 0,
        sequence: 0,
    };
    for kind in [Kind::Plain, Kind::Encrypted] {
        let key = kind.key();
        let path = graph_file(dir.path(), "graph", kind);
        let original = fs::read(&path).unwrap();
        let output = dir.path().join(format!("upgraded-{kind:?}.vlite"));
        if key.is_none() {
            assert!(matches!(
                maintenance::upgrade(&path, &output),
                Err(Error::GraphUnsupportedOperation)
            ));
            assert!(!output.exists());
        }
        assert!(matches!(
            read_log_changes(&path, key.as_ref(), &start, 16),
            Err(Error::GraphChangeFeedUnsupported)
        ));
        let db = open(&path, key.as_ref());
        assert!(matches!(
            db.feed_cursor(),
            Err(Error::GraphChangeFeedUnsupported)
        ));
        assert!(matches!(
            db.changes_after(&start, 16),
            Err(Error::GraphChangeFeedUnsupported)
        ));
        drop(db);
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    // Vector-only files keep their feeds in graph builds.
    let vector = vector_file(dir.path(), "vector", Kind::Plain);
    let db = open(&vector, None);
    let cursor = db.feed_cursor().unwrap();
    assert!(db.changes_after(&start, 16).unwrap().changes.len() > 1);
    assert!(db.changes_after(&cursor, 16).unwrap().changes.is_empty());
}

#[test]
fn replica_followers_replay_graph_frames() {
    let dir = tempdir().unwrap();
    for kind in KINDS {
        let key = kind.key();
        let primary = dir.path().join(format!("primary-{kind:?}.vlite"));
        let replica = dir.path().join(format!("replica-{kind:?}.vlite"));
        let mut db = Database::create_with_graph(&primary, 2, kind.options()).unwrap();
        clock(&mut db, 0);
        // Ship every commit as its own segment, alternating in-memory and
        // file-shipped segments; the follower must match the primary after
        // each one.
        let shipped = dir.path().join(format!("segment-{kind:?}.bin"));
        let mut follower: Option<ReplicaFollower> = None;
        populate_with(&mut db, &mut |db| match &mut follower {
            None => {
                let segment = db.export_log(0, usize::MAX).unwrap();
                follower = Some(ReplicaFollower::create(&replica, &segment, key.as_ref()).unwrap());
            }
            Some(follower) => {
                let segment = db
                    .export_log(follower.applied_sequence(), usize::MAX)
                    .unwrap();
                if segment.last_sequence % 2 == 0 {
                    follower.apply(&segment).unwrap();
                } else {
                    segment.save(&shipped).unwrap();
                    follower.apply_file(&shipped).unwrap();
                }
                assert_eq!(follower.applied_sequence(), segment.last_sequence);
                assert_eq!(state(follower.db()), state(db));
            }
        });
        drop(follower);
        db.close().unwrap();
        assert_eq!(fs::read(&replica).unwrap(), fs::read(&primary).unwrap());
        let head = commits(&primary, key.as_ref());

        // A crash while applying the next segment leaves a torn graph frame;
        // opening the follower discards it and the follower catches up.
        let mut db = open(&primary, key.as_ref());
        clock(&mut db, head);
        extend(&mut db);
        db.close().unwrap();
        let segment =
            maintenance::export_log_segment(&primary, key.as_ref(), head, usize::MAX).unwrap();
        let mut torn = fs::read(&replica).unwrap();
        let clean = torn.clone();
        torn.extend_from_slice(&segment.frames[..segment.frames.len() - 3]);
        fs::write(&replica, &torn).unwrap();
        let mut follower = ReplicaFollower::open(&replica, key.as_ref()).unwrap();
        assert_eq!(follower.applied_sequence(), head);
        follower.sync_from_log(&primary, usize::MAX).unwrap();
        assert_eq!(follower.applied_sequence(), head + 1);
        drop(follower);
        assert_eq!(fs::read(&replica).unwrap(), fs::read(&primary).unwrap());
        assert_eq!(
            snapshot(&replica, key.as_ref()),
            snapshot(&primary, key.as_ref())
        );
        assert_ne!(fs::read(&replica).unwrap(), clean);

        // Promotion keeps the replayed high-water marks.
        let mut promoted = ReplicaFollower::open(&replica, key.as_ref())
            .unwrap()
            .promote();
        let node = promoted
            .graph_mut("alpha")
            .unwrap()
            .create_node("promoted", &[], &Metadata::new())
            .unwrap();
        assert_eq!(node.id, ALPHA_HIGH_WATER.node);
    }

    // A vector-only follower never accepts a graph primary's segments and
    // stays byte-identical.
    let vector_primary = vector_file(dir.path(), "vector-primary", Kind::Plain);
    let segment = maintenance::export_log_segment(&vector_primary, None, 0, usize::MAX).unwrap();
    let follower_path = dir.path().join("vector-follower.vlite");
    drop(ReplicaFollower::create(&follower_path, &segment, None).unwrap());
    let before = fs::read(&follower_path).unwrap();
    let graph = graph_file(dir.path(), "graph-primary", Kind::Plain);
    let mut follower = ReplicaFollower::open(&follower_path, None).unwrap();
    assert!(matches!(
        follower.sync_from_log(&graph, usize::MAX),
        Err(Error::ReplicaDiverged(_))
    ));
    let graph_segment = maintenance::export_log_segment(&graph, None, 0, usize::MAX).unwrap();
    assert!(follower.apply(&graph_segment).is_err());
    drop(follower);
    assert_eq!(fs::read(&follower_path).unwrap(), before);
}

fn maintain(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vlite-maintain"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn maintain_cli_enables_and_strips_graphs() {
    let dir = tempdir().unwrap();
    let usage = maintain(&[]);
    assert!(!usage.status.success());
    let usage = String::from_utf8(usage.stderr).unwrap();
    assert!(
        usage.contains("verify PATH | drill BACKUP WORKDIR"),
        "{usage}"
    );
    assert!(
        usage.contains(
            "enable-graph SOURCE DESTINATION | strip-graph SOURCE DESTINATION --confirm-graph-data-loss"
        ),
        "{usage}"
    );

    let vector = vector_file(dir.path(), "vector", Kind::Plain);
    let enabled = dir.path().join("enabled.vlite");
    let result = maintain(&[
        "enable-graph".as_ref(),
        vector.as_os_str(),
        enabled.as_os_str(),
    ]);
    assert!(result.status.success(), "{result:?}");
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("format_version=5")
    );
    assert_eq!(header_flags(&enabled), FLAG_GRAPH);

    let graph = graph_file(dir.path(), "graph", Kind::Plain);
    let original = fs::read(&graph).unwrap();
    let stripped = dir.path().join("stripped.vlite");
    let refused = maintain(&[
        "strip-graph".as_ref(),
        graph.as_os_str(),
        stripped.as_os_str(),
    ]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8(refused.stderr)
            .unwrap()
            .contains("confirm_graph_data_loss")
    );
    let typo = maintain(&[
        "strip-graph".as_ref(),
        graph.as_os_str(),
        stripped.as_os_str(),
        "--confirm".as_ref(),
    ]);
    assert!(!typo.status.success());
    assert!(String::from_utf8(typo.stderr).unwrap().contains("usage:"));
    assert!(!stripped.exists());
    let result = maintain(&[
        "strip-graph".as_ref(),
        graph.as_os_str(),
        stripped.as_os_str(),
        "--confirm-graph-data-loss".as_ref(),
    ]);
    assert!(result.status.success(), "{result:?}");
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("removed_graphs=3 removed_nodes=6 removed_edges=6 removed_attachments=4")
    );
    assert_eq!(header_flags(&stripped), 0);
    assert_eq!(fs::read(&graph).unwrap(), original);

    // The existing actions handle graph files in graph builds.
    for action in ["inspect", "verify"] {
        let result = maintain(&[action.as_ref(), graph.as_os_str()]);
        assert!(result.status.success(), "{action}: {result:?}");
    }
    let compacted = dir.path().join("compacted.vlite");
    let result = maintain(&["compact".as_ref(), graph.as_os_str(), compacted.as_os_str()]);
    assert!(result.status.success(), "{result:?}");
    assert_eq!(header_flags(&compacted), FLAG_GRAPH);
    let upgraded = dir.path().join("upgraded.vlite");
    let result = maintain(&["upgrade".as_ref(), graph.as_os_str(), upgraded.as_os_str()]);
    assert!(!result.status.success());
    assert!(!upgraded.exists());
}
