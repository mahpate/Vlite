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
    let id = ids[0];
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
        candidate.collection(&name).unwrap().revision(id).unwrap(),
        Some(revision)
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
