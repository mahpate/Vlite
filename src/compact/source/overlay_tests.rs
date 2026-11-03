use super::dispatch_tests::{compact_dispatch_fixture, gates, point_parity};
use crate::records::CollectionRecords;
use crate::{Error, Expected, Metadata, MetadataValue, Mutation};

fn compact_collection(db: &crate::Database) -> (String, Vec<u64>) {
    let (_, state) = db
        .collections
        .iter()
        .find(|(_, state)| {
            matches!(&state.records, CollectionRecords::Compact { view, .. } if view.len() > 0)
        })
        .expect("fixture contains a compact collection");
    let CollectionRecords::Compact { view, .. } = &state.records else {
        unreachable!();
    };
    (
        state.name.clone(),
        (0..view.len()).filter_map(|row| view.id(row)).collect(),
    )
}

#[test]
fn compact_overlay_ordered_batch_matches_replay() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let (name, ids) = compact_collection(&candidate);
    let (first, second) = (ids[0], ids[1]);
    let metadata = Metadata::from([("batch".into(), MetadataValue::String("last".into()))]);
    let operations = [
        Mutation::Put {
            id: first,
            vector: vec![6.0, 7.0],
            payload: b"first".to_vec(),
        },
        Mutation::Delete { id: second },
        Mutation::PutWithMetadata {
            id: second,
            vector: vec![8.0, 9.0],
            payload: b"last".to_vec(),
            metadata,
        },
        Mutation::Put {
            id: 8_000_000_000,
            vector: vec![10.0, 11.0],
            payload: b"new".to_vec(),
        },
    ];
    candidate
        .collection_mut(&name)
        .unwrap()
        .write_batch(&operations)
        .unwrap();
    oracle
        .collection_mut(&name)
        .unwrap()
        .write_batch(&operations)
        .unwrap();

    point_parity(&candidate, &oracle);
    assert_eq!(candidate.to_bytes(), oracle.to_bytes());
    assert_eq!(
        candidate.collection(&name).unwrap().len(),
        ids.len() + 1,
        "replacements preserve physical count and the new ID adds one row"
    );
}

#[test]
fn compact_delete_tombstone_shadows_base() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let (name, ids) = compact_collection(&candidate);
    let id = *ids
        .iter()
        .find(|&&id| id != 0)
        .expect("unattached base row");
    let before = candidate.collection(&name).unwrap().len();

    assert!(candidate.collection_mut(&name).unwrap().delete(id).unwrap());
    assert!(oracle.collection_mut(&name).unwrap().delete(id).unwrap());
    let collection = candidate.collection(&name).unwrap();
    assert!(collection.get_at(id, 0).unwrap().is_none());
    assert_eq!(collection.revision(id).unwrap(), None);
    assert!(!candidate.state(collection.id()).records.contains_key(&id));
    assert_eq!(collection.len(), before - 1);
    point_parity(&candidate, &oracle);

    let expected = candidate.sequence + 1;
    let revision = candidate
        .collection_mut(&name)
        .unwrap()
        .put_with_metadata_if(
            id,
            &[3.0, 4.0],
            b"resurrected",
            &Metadata::new(),
            Expected::Absent,
        )
        .unwrap();
    oracle
        .collection_mut(&name)
        .unwrap()
        .put_with_metadata_if(
            id,
            &[3.0, 4.0],
            b"resurrected",
            &Metadata::new(),
            Expected::Absent,
        )
        .unwrap();
    assert_eq!(revision, expected);
    point_parity(&candidate, &oracle);
}

#[test]
fn compact_tombstones_hide_membership_and_advance_first_id() {
    let _gates = gates();
    let image = crate::compact::catalog::tests::source_indexed_fixture(None);
    let mut candidate = crate::Database::open_from_bytes(image, None).unwrap();
    assert_eq!(candidate.collection("indexed").unwrap().first_id(), Some(1));

    candidate
        .collection_mut("indexed")
        .unwrap()
        .delete(1)
        .unwrap();
    let collection = candidate.collection("indexed").unwrap();
    assert!(!candidate.state(collection.id()).records.contains_key(&1));
    assert_eq!(collection.first_id(), Some(2));
}

#[test]
fn compact_reinsert_and_replacement_keep_derived_indexes_and_body_accounting() {
    let _gates = gates();
    let image = crate::compact::catalog::tests::source_indexed_fixture(None);
    let mut candidate = crate::Database::open_from_bytes(image.clone(), None).unwrap();
    let mut oracle = {
        let _source_off = crate::compact::source::Activation::set(false);
        crate::Database::open_from_bytes(image, None).unwrap()
    };
    let name = "indexed";

    candidate.collection_mut(name).unwrap().delete(1).unwrap();
    oracle.collection_mut(name).unwrap().delete(1).unwrap();
    candidate
        .collection_mut(name)
        .unwrap()
        .put_with_metadata(
            1,
            &[11.0, 1.0],
            b"green apple",
            &Metadata::from([
                ("tag".into(), MetadataValue::String("gamma".into())),
                ("title".into(), MetadataValue::String("green apple".into())),
            ]),
        )
        .unwrap();
    oracle
        .collection_mut(name)
        .unwrap()
        .put_with_metadata(
            1,
            &[11.0, 1.0],
            b"green apple",
            &Metadata::from([
                ("tag".into(), MetadataValue::String("gamma".into())),
                ("title".into(), MetadataValue::String("green apple".into())),
            ]),
        )
        .unwrap();

    for (id, tag, title) in [(2, "gamma", "green berry"), (2, "delta", "yellow berry")] {
        let metadata = Metadata::from([
            ("tag".into(), MetadataValue::String(tag.into())),
            ("title".into(), MetadataValue::String(title.into())),
        ]);
        candidate
            .collection_mut(name)
            .unwrap()
            .put_with_metadata(id, &[12.0, 1.0], title.as_bytes(), &metadata)
            .unwrap();
        oracle
            .collection_mut(name)
            .unwrap()
            .put_with_metadata(id, &[12.0, 1.0], title.as_bytes(), &metadata)
            .unwrap();
    }

    let candidate_state = candidate.state(candidate.collection(name).unwrap().id());
    let oracle_state = oracle.state(oracle.collection(name).unwrap().id());
    for (tag, expected) in [
        ("alpha", vec![]),
        ("beta", vec![]),
        ("gamma", vec![1]),
        ("delta", vec![2]),
    ] {
        let condition = [crate::Condition::Eq {
            field: "tag".into(),
            value: MetadataValue::String(tag.into()),
        }];
        assert_eq!(
            candidate_state
                .metadata_indexes
                .lookup(&condition, 10)
                .unwrap()
                .ids
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            expected
        );
    }
    let terms = std::collections::BTreeSet::from([
        "green".to_owned(),
        "red".to_owned(),
        "yellow".to_owned(),
    ]);
    assert_eq!(
        candidate_state
            .text_index
            .search(&terms, 10, |_| true)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(
        candidate_state.live_body_bytes, oracle_state.live_body_bytes,
        "compact replacement accounting must match resident records"
    );
    point_parity(&candidate, &oracle);
}

#[test]
fn compact_mutations_match_resident_put_delete_metadata_and_ttl() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let name = "full";
    let id = 3;
    let metadata = Metadata::from([("phase".into(), MetadataValue::String("updated".into()))]);

    candidate
        .collection_mut(name)
        .unwrap()
        .put_with_expiry(id, &[4.0, 5.0], b"put", &Metadata::new(), Some(50))
        .unwrap();
    oracle
        .collection_mut(name)
        .unwrap()
        .put_with_expiry(id, &[4.0, 5.0], b"put", &Metadata::new(), Some(50))
        .unwrap();
    point_parity(&candidate, &oracle);

    candidate
        .collection_mut(name)
        .unwrap()
        .update_metadata(id, &metadata)
        .unwrap();
    oracle
        .collection_mut(name)
        .unwrap()
        .update_metadata(id, &metadata)
        .unwrap();
    assert_eq!(
        candidate.collection(name).unwrap().expires_at(id).unwrap(),
        Some(50)
    );
    point_parity(&candidate, &oracle);

    candidate.collection_mut(name).unwrap().delete(id).unwrap();
    oracle.collection_mut(name).unwrap().delete(id).unwrap();
    point_parity(&candidate, &oracle);
}

#[test]
fn compact_overlays_are_isolated_across_collections() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let metadata = Metadata::from([("scope".into(), MetadataValue::String("half".into()))]);

    candidate
        .collection_mut("half")
        .unwrap()
        .update_metadata(3, &metadata)
        .unwrap();
    oracle
        .collection_mut("half")
        .unwrap()
        .update_metadata(3, &metadata)
        .unwrap();
    candidate
        .collection_mut("full")
        .unwrap()
        .put_with_expiry(
            3,
            &[4.0, 5.0],
            b"full replacement",
            &Metadata::new(),
            Some(50),
        )
        .unwrap();
    oracle
        .collection_mut("full")
        .unwrap()
        .put_with_expiry(
            3,
            &[4.0, 5.0],
            b"full replacement",
            &Metadata::new(),
            Some(50),
        )
        .unwrap();
    candidate
        .collection_mut("half")
        .unwrap()
        .delete(10)
        .unwrap();
    oracle.collection_mut("half").unwrap().delete(10).unwrap();

    assert_eq!(
        candidate
            .collection("full")
            .unwrap()
            .get_at(3, 0)
            .unwrap()
            .unwrap()
            .payload,
        b"full replacement"
    );
    assert_eq!(
        candidate.collection("full").unwrap().expires_at(3).unwrap(),
        Some(50)
    );
    assert!(
        candidate
            .collection("half")
            .unwrap()
            .get_at(10, 0)
            .unwrap()
            .is_none()
    );
    assert!(
        candidate
            .collection("full")
            .unwrap()
            .get_at(10, 0)
            .unwrap()
            .is_some()
    );
    point_parity(&candidate, &oracle);
}

#[test]
fn committed_apply_errors_poison_compact_handle() {
    let _gates = gates();
    for metadata_only in [false, true] {
        let image = crate::compact::catalog::tests::source_indexed_fixture(None);
        let mut candidate = crate::Database::open_from_bytes(image, None).unwrap();
        let id = candidate.collection("indexed").unwrap().id();
        for (&other, state) in &mut candidate.collections {
            if other != id {
                state.records =
                    crate::records::CollectionRecords::Resident(crate::records::RecordMap::new());
            }
        }
        let crate::records::CollectionRecords::Compact { view, .. } =
            &mut candidate.state_mut(id).records
        else {
            panic!("fixture must install compact records");
        };
        let generation = std::sync::Arc::get_mut(&mut view.generation)
            .expect("fixture generation is uniquely owned");
        generation.backing =
            crate::compact::source::Backing::Owned(std::sync::Arc::from(Vec::<u8>::new()));
        let sequence = candidate.sequence;
        let result = if metadata_only {
            candidate
                .collection_mut("indexed")
                .unwrap()
                .update_metadata(
                    1,
                    &Metadata::from([("tag".into(), MetadataValue::String("broken".into()))]),
                )
                .map(|_| ())
        } else {
            candidate
                .collection_mut("indexed")
                .unwrap()
                .put(1, &[3.0, 4.0], b"committed before apply")
                .map(|_| ())
        };
        assert!(matches!(result, Err(Error::Corrupt { .. })));
        assert!(candidate.sequence > sequence, "the frame was committed");
        assert!(candidate.is_poisoned());
        assert!(matches!(candidate.get_at(1, 0), Err(Error::Poisoned)));
        assert!(matches!(
            candidate.collection_mut("indexed"),
            Err(Error::Poisoned)
        ));
    }
}

#[test]
fn compact_expired_overlay_never_reveals_base() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let (name, ids) = compact_collection(&candidate);
    let id = ids
        .into_iter()
        .find(|&id| {
            id != 0
                && candidate
                    .collection(&name)
                    .unwrap()
                    .expires_at(id)
                    .unwrap()
                    .is_none()
        })
        .expect("fixture has a non-expiring base row");
    let old_len = candidate.collection(&name).unwrap().len();

    candidate
        .collection_mut(&name)
        .unwrap()
        .put_with_expiry(id, &[12.0, 13.0], b"overlay", &Metadata::new(), Some(10))
        .unwrap();
    oracle
        .collection_mut(&name)
        .unwrap()
        .put_with_expiry(id, &[12.0, 13.0], b"overlay", &Metadata::new(), Some(10))
        .unwrap();
    let collection = candidate.collection(&name).unwrap();
    assert_eq!(
        collection.get_at(id, 9).unwrap().unwrap().payload,
        b"overlay"
    );
    assert!(collection.get_at(id, 10).unwrap().is_none());
    assert_eq!(collection.len(), old_len);
    assert_eq!(collection.expires_at(id).unwrap(), Some(10));
    point_parity(&candidate, &oracle);
}

#[test]
fn compact_metadata_update_preserves_vector_payload() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let (name, ids) = compact_collection(&candidate);
    let id = ids
        .into_iter()
        .find(|&id| {
            candidate
                .collection(&name)
                .unwrap()
                .expires_at(id)
                .unwrap()
                .is_some()
        })
        .expect("fixture has an expiring base row");
    let expiry = candidate.collection(&name).unwrap().expires_at(id).unwrap();
    let before = candidate
        .collection(&name)
        .unwrap()
        .get_at(id, 0)
        .unwrap()
        .unwrap()
        .clone();
    let metadata = Metadata::from([("changed".into(), MetadataValue::Boolean(true))]);

    let revision = candidate
        .collection_mut(&name)
        .unwrap()
        .update_metadata(id, &metadata)
        .unwrap();
    oracle
        .collection_mut(&name)
        .unwrap()
        .update_metadata(id, &metadata)
        .unwrap();
    let after = candidate
        .collection(&name)
        .unwrap()
        .get_at(id, 0)
        .unwrap()
        .unwrap();
    assert_eq!(after.vector, before.vector);
    assert_eq!(after.payload, before.payload);
    assert_eq!(after.metadata, metadata);
    assert_eq!(
        candidate.collection(&name).unwrap().expires_at(id).unwrap(),
        expiry
    );
    assert_eq!(
        candidate.collection(&name).unwrap().revision(id).unwrap(),
        Some(revision)
    );
    assert_eq!(
        candidate
            .state(candidate.collection(&name).unwrap().id())
            .revisions
            .get(&id),
        Some(&revision),
        "metadata commits must refresh the resident revision map"
    );
    assert_eq!(
        candidate
            .state(candidate.collection(&name).unwrap().id())
            .live_body_bytes,
        oracle
            .state(oracle.collection(&name).unwrap().id())
            .live_body_bytes
    );
    point_parity(&candidate, &oracle);
}

#[test]
fn compact_overlay_snapshot_and_revision_condition_are_immutable() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let (name, ids) = compact_collection(&candidate);
    let id = ids[0];
    let old_revision = candidate
        .collection(&name)
        .unwrap()
        .revision(id)
        .unwrap()
        .unwrap();
    let snapshot = candidate
        .collection(&name)
        .unwrap()
        .read_snapshot()
        .unwrap();
    let before = snapshot.get_at(id, 0).unwrap().unwrap().clone();

    let revision = candidate
        .collection_mut(&name)
        .unwrap()
        .put_with_metadata_if(
            id,
            &[14.0, 15.0],
            b"new snapshot version",
            &Metadata::new(),
            Expected::Revision(old_revision),
        )
        .unwrap();
    oracle
        .collection_mut(&name)
        .unwrap()
        .put_with_metadata_if(
            id,
            &[14.0, 15.0],
            b"new snapshot version",
            &Metadata::new(),
            Expected::Revision(old_revision),
        )
        .unwrap();
    assert_eq!(snapshot.get_at(id, 0).unwrap(), Some(&before));
    assert_eq!(
        candidate
            .collection(&name)
            .unwrap()
            .get_at(id, 0)
            .unwrap()
            .unwrap()
            .payload,
        b"new snapshot version"
    );
    assert_eq!(
        candidate.collection(&name).unwrap().revision(id).unwrap(),
        Some(revision)
    );

    let bytes = candidate.to_bytes().unwrap();
    let sequence = candidate.sequence;
    assert!(matches!(
        candidate.collection_mut(&name).unwrap().put_with_metadata_if(
            id,
            &[20.0, 21.0],
            b"stale",
            &Metadata::new(),
            Expected::Revision(old_revision),
        ),
        Err(Error::RevisionMismatch {
            current: Some(current)
        }) if current == revision
    ));
    assert_eq!(candidate.sequence, sequence);
    assert_eq!(candidate.to_bytes().unwrap(), bytes);
    point_parity(&candidate, &oracle);
    let reopened = crate::Database::open_from_bytes(bytes, None).unwrap();
    point_parity(&reopened, &oracle);
}

#[test]
fn compact_graph_attachment_tracks_effective_row() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    let (name, _) = compact_collection(&candidate);
    let id = 0;
    let metadata = Metadata::from([("graph-update".into(), MetadataValue::Boolean(true))]);
    candidate
        .collection_mut(&name)
        .unwrap()
        .update_metadata(id, &metadata)
        .unwrap();
    oracle
        .collection_mut(&name)
        .unwrap()
        .update_metadata(id, &metadata)
        .unwrap();

    let sequence = candidate.sequence;
    assert!(matches!(
        candidate.collection_mut(&name).unwrap().put_with_expiry(
            id,
            &[3.0, 4.0],
            b"expiry",
            &Metadata::new(),
            Some(1)
        ),
        Err(Error::GraphAttachedRecordExpiry)
    ));
    assert!(matches!(
        oracle.collection_mut(&name).unwrap().put_with_expiry(
            id,
            &[3.0, 4.0],
            b"expiry",
            &Metadata::new(),
            Some(1)
        ),
        Err(Error::GraphAttachedRecordExpiry)
    ));
    assert_eq!(candidate.sequence, sequence);
    assert!(matches!(
        candidate.collection_mut(&name).unwrap().delete(id),
        Err(Error::GraphAttachedRecord)
    ));
    assert_eq!(candidate.sequence, sequence);
    assert!(
        candidate
            .collection(&name)
            .unwrap()
            .get_at(id, 0)
            .unwrap()
            .is_some()
    );
    point_parity(&candidate, &oracle);
}
