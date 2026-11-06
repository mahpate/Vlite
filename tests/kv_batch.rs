//! The key-value batch through the public API: one commit, nothing written
//! when nothing changes, and a crash at any byte leaves the state before the
//! batch. The planner's rules are pinned by the unit tests in `src/kv`.

use std::fs;
use vlite::{
    Database, Error, Expected, FormatVersion,
    kv::{KvOp, KvOutcome, KvPut, first_slot},
    maintenance,
};

fn put<'a>(namespace: &'a str, key: &'a str, value: &'a [u8]) -> KvOp<'a> {
    KvOp::Put {
        namespace,
        key,
        value,
        options: KvPut::default(),
    }
}

fn delete<'a>(namespace: &'a str, key: &'a str) -> KvOp<'a> {
    KvOp::Delete {
        namespace,
        key,
        expected: Expected::Any,
    }
}

fn version(outcome: &KvOutcome) -> u64 {
    match outcome {
        KvOutcome::Put { version } => *version,
        other => panic!("not a put: {other:?}"),
    }
}

fn seeded(path: &std::path::Path) -> Database {
    let mut db = Database::create_with_format(path, 1, FormatVersion::V5).unwrap();
    let mut kv = db.kv_store("default").unwrap();
    kv.put("n", "old", b"old value", KvPut::default()).unwrap();
    kv.put("n", "gone", b"to delete", KvPut::default()).unwrap();
    db
}

#[test]
fn a_batch_is_one_commit_whose_puts_share_a_version_and_survives_reopen_and_maintenance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("batch.vlite");
    let db = seeded(&path);
    db.close().unwrap();
    let commits = maintenance::inspect(&path).unwrap().commits;

    let mut db = Database::open(&path).unwrap();
    let outcomes = db
        .kv_store("default")
        .unwrap()
        .write_batch(&[
            put("n", "new", b"new value"),
            delete("n", "gone"),
            put("m", "old", b"other namespace"),
            delete("n", "never-there"),
            put("n", "old", b"replaced"),
        ])
        .unwrap();
    db.close().unwrap();
    assert_eq!(maintenance::inspect(&path).unwrap().commits, commits + 1);

    assert_eq!(outcomes.len(), 5);
    let shared = version(&outcomes[0]);
    assert_eq!(version(&outcomes[2]), shared);
    assert_eq!(version(&outcomes[4]), shared);
    assert_eq!(outcomes[1], KvOutcome::Deleted(true));
    assert_eq!(outcomes[3], KvOutcome::Deleted(false));

    let expect = |db: &Database, versions: bool| {
        let view = db.kv_view("default").unwrap();
        let entries = view
            .get_many(&[
                ("n", "new"),
                ("n", "gone"),
                ("m", "old"),
                ("n", "never-there"),
                ("n", "old"),
            ])
            .unwrap();
        let values: Vec<_> = entries
            .iter()
            .map(|entry| entry.as_ref().map(|entry| entry.value.clone()))
            .collect();
        assert_eq!(
            values,
            [
                Some(b"new value".to_vec()),
                None,
                Some(b"other namespace".to_vec()),
                None,
                Some(b"replaced".to_vec())
            ]
        );
        if versions {
            for index in [0, 2, 4] {
                assert_eq!(entries[index].as_ref().unwrap().version, shared);
            }
        }
    };
    expect(&Database::open(&path).unwrap(), true);

    let backup = directory.path().join("backup.vlite");
    maintenance::backup(&path, &backup).unwrap();
    let restored = directory.path().join("restored.vlite");
    maintenance::restore(&backup, &restored).unwrap();
    let compact = directory.path().join("compact.vlite");
    maintenance::compact(&restored, &compact).unwrap();
    // Revisions restart after a restore or a compaction: only the entries are
    // kept.
    for copy in [&backup, &restored, &compact] {
        expect(&Database::open(copy).unwrap(), false);
    }
}

#[test]
fn a_batch_that_changes_no_record_writes_no_frame() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("noop.vlite");
    let db = seeded(&path);
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    {
        let mut kv = db.kv_store("default").unwrap();
        assert!(kv.write_batch(&[]).unwrap().is_empty());
        assert_eq!(
            kv.write_batch(&[delete("n", "absent"), delete("m", "absent")])
                .unwrap(),
            [KvOutcome::Deleted(false), KvOutcome::Deleted(false)]
        );
        assert_eq!(
            kv.write_batch(&[KvOp::Delete {
                namespace: "n",
                key: "absent",
                expected: Expected::Absent
            }])
            .unwrap(),
            [KvOutcome::Deleted(false)]
        );
    }
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn a_refused_batch_writes_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("refused.vlite");
    let db = seeded(&path);
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    {
        let mut kv = db.kv_store("default").unwrap();
        let stale = KvOp::Put {
            namespace: "n",
            key: "old",
            value: b"x",
            options: KvPut {
                expected: Expected::Revision(999_999),
                expires_at_ms: None,
            },
        };
        let with_expiry = KvOp::Put {
            namespace: "n",
            key: "new",
            value: b"x",
            options: KvPut {
                expected: Expected::Any,
                expires_at_ms: Some(1),
            },
        };
        for refused in [
            vec![put("n", "first", b"x"), stale],
            vec![put("n", "first", b"x"), with_expiry],
            vec![put("n", "first", b"x"), put("n", "first", b"y")],
            vec![put("n", "first", b"x"), delete("", "k")],
        ] {
            assert!(kv.write_batch(&refused).is_err());
        }
        assert!(kv.get("n", "first").unwrap().is_none());
    }
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn every_interrupted_batch_byte_keeps_the_whole_previous_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("interrupt.vlite");
    let db = seeded(&path);
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let commits = maintenance::inspect(&path).unwrap().commits;
    let mut db = Database::open(&path).unwrap();
    db.kv_store("default")
        .unwrap()
        .write_batch(&[
            put("n", "new", b"new value"),
            delete("n", "gone"),
            put("m", "old", b"other namespace"),
            put("n", "old", b"replaced"),
        ])
        .unwrap();
    db.close().unwrap();
    let complete = fs::read(&path).unwrap();
    assert!(complete.len() > before.len());

    let keys = [("n", "new"), ("n", "gone"), ("m", "old"), ("n", "old")];
    let seen = |path: &std::path::Path| -> Vec<Option<Vec<u8>>> {
        Database::open(path)
            .unwrap()
            .kv_view("default")
            .unwrap()
            .get_many(&keys)
            .unwrap()
            .into_iter()
            .map(|entry| entry.map(|entry| entry.value))
            .collect()
    };
    for cut in before.len() + 1..complete.len() {
        fs::write(&path, &complete[..cut]).unwrap();
        assert!(
            matches!(Database::open(&path), Err(Error::IncompleteTail { .. })),
            "cut {cut}"
        );
        assert_eq!(maintenance::inspect(&path).unwrap().commits, commits);
        let report = Database::discard_incomplete_tail(&path).unwrap();
        assert_eq!(report.retained_commits, commits);
        assert_eq!(fs::read(&path).unwrap(), before, "cut {cut}");
        assert_eq!(
            seen(&path),
            [
                None,
                Some(b"to delete".to_vec()),
                None,
                Some(b"old value".to_vec())
            ],
            "cut {cut}"
        );
    }
    fs::write(&path, &complete).unwrap();
    assert_eq!(
        seen(&path),
        [
            Some(b"new value".to_vec()),
            None,
            Some(b"other namespace".to_vec()),
            Some(b"replaced".to_vec())
        ]
    );
}

#[test]
fn a_read_only_handle_refuses_a_batch_and_still_reads_many() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read-only.vlite");
    let db = seeded(&path);
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let mut db = Database::open_read_only(&path).unwrap();
    assert!(matches!(
        db.kv_store("default")
            .unwrap()
            .write_batch(&[put("n", "new", b"x")]),
        Err(Error::ReadOnly)
    ));
    // A batch that changes no record attempts no write, so the mode of the
    // handle does not matter, as for a single delete of an absent key.
    let mut kv = db.kv_store("default").unwrap();
    assert!(kv.write_batch(&[]).unwrap().is_empty());
    assert_eq!(
        kv.write_batch(&[delete("n", "absent")]).unwrap(),
        [KvOutcome::Deleted(false)]
    );
    assert!(!kv.delete("n", "absent", Expected::Any).unwrap());
    let entries = db
        .kv_view("default")
        .unwrap()
        .get_many(&[("n", "old"), ("n", "new")])
        .unwrap();
    assert_eq!(entries[0].as_ref().unwrap().value, b"old value");
    assert!(entries[1].is_none());
    drop(db);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn a_graph_attached_record_is_not_deleted_by_a_batch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("attached.vlite");
    let mut db = seeded(&path);
    db.create_graph("g").unwrap();
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            vlite::property_graph::GraphOp::CreateNode {
                key: "node".into(),
                labels: Vec::new(),
                properties: vlite::Metadata::new(),
            },
            vlite::property_graph::GraphOp::Attach {
                node: "node".into(),
                collection: "default".into(),
                record: first_slot("n", "old"),
            },
        ])
        .unwrap();
    db.close().unwrap();
    let before = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    {
        let mut kv = db.kv_store("default").unwrap();
        assert!(matches!(
            kv.write_batch(&[put("n", "first", b"x"), delete("n", "old")]),
            Err(Error::GraphAttachedRecord)
        ));
        assert!(kv.get("n", "first").unwrap().is_none());
        // The first operation that cannot run is the one reported, as when
        // they run one at a time: the attached delete before a failed
        // condition, and a failed condition before the attached delete.
        let failing = KvOp::Put {
            namespace: "n",
            key: "first",
            value: b"x",
            options: KvPut {
                expected: Expected::Revision(1),
                expires_at_ms: None,
            },
        };
        assert!(matches!(
            kv.write_batch(&[delete("n", "old"), failing]),
            Err(Error::GraphAttachedRecord)
        ));
        assert!(matches!(
            kv.write_batch(&[failing, delete("n", "old")]),
            Err(Error::RevisionMismatch { .. })
        ));
        // A put over the attached record is as allowed as the single call.
        kv.write_batch(&[put("n", "old", b"replaced")]).unwrap();
    }
    db.close().unwrap();
    assert_ne!(fs::read(&path).unwrap(), before);
    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.kv_view("default")
            .unwrap()
            .get("n", "old")
            .unwrap()
            .unwrap()
            .value,
        b"replaced"
    );
}

#[test]
fn a_batch_put_clears_a_deadline_and_a_due_entry_counts_as_absent() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("deadline.vlite");
    let mut db = Database::create_with_format(&path, 1, FormatVersion::V5).unwrap();
    let mut kv = db.kv_store("default").unwrap();
    kv.put_at(
        "n",
        "lease",
        b"v1",
        KvPut {
            expected: Expected::Any,
            expires_at_ms: Some(1_000),
        },
        0,
    )
    .unwrap();
    kv.put_at(
        "n",
        "due",
        b"v1",
        KvPut {
            expected: Expected::Any,
            expires_at_ms: Some(1_000),
        },
        0,
    )
    .unwrap();
    let outcomes = kv
        .write_batch_at(
            &[
                put("n", "lease", b"v2"),
                KvOp::Put {
                    namespace: "n",
                    key: "due",
                    value: b"v2",
                    options: KvPut {
                        expected: Expected::Absent,
                        expires_at_ms: None,
                    },
                },
            ],
            // `lease` is live at this clock and `due` is not: Absent holds.
            999,
        )
        .unwrap_err();
    assert!(matches!(outcomes, Error::RevisionMismatch { .. }));
    let outcomes = kv
        .write_batch_at(
            &[
                put("n", "lease", b"v2"),
                KvOp::Put {
                    namespace: "n",
                    key: "due",
                    value: b"v2",
                    options: KvPut {
                        expected: Expected::Absent,
                        expires_at_ms: None,
                    },
                },
            ],
            // Both are due: the put over `due` meets an absent key.
            1_000,
        )
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    let view = kv.view();
    for key in ["lease", "due"] {
        let entry = view.get_at("n", key, u64::MAX).unwrap().unwrap();
        assert_eq!(entry.value, b"v2");
        assert_eq!(entry.expires_at_ms, None, "{key}");
    }
}
