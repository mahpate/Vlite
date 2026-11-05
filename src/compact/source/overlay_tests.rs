use super::dispatch_tests::{compact_dispatch_fixture, gates, point_parity};
use crate::records::CollectionRecords;
use crate::{Error, Expected, Metadata, MetadataValue, Mutation};

mod consumer_cursor;
mod consumer_exact;
mod consumer_graph;
mod consumer_group;
mod consumer_hybrid;
mod consumer_kv;
mod consumer_query;
mod consumer_vectors;

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
    let resident_collection_id = oracle.collection(name).unwrap().id();
    let resident_record = oracle
        .state(resident_collection_id)
        .records
        .resident()
        .unwrap()
        .get(&id)
        .unwrap() as *const crate::Record;
    oracle
        .collection_mut(name)
        .unwrap()
        .update_metadata(id, &metadata)
        .unwrap();
    assert_eq!(
        oracle
            .state(resident_collection_id)
            .records
            .resident()
            .unwrap()
            .get(&id)
            .unwrap() as *const crate::Record,
        resident_record,
        "resident metadata updates must preserve the existing record allocation"
    );
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

#[test]
fn compact_ordered_merge_matches_resident_with_gaps() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    assert_compact_resident_pair(&candidate, &oracle, "full");
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("full").unwrap();
        collection.delete(10).unwrap();
        collection
            .put_with_expiry(3, &[2.0, 3.0], b"replacement", &Metadata::new(), Some(50))
            .unwrap();
        collection.put(7, &[3.0, 4.0], b"gap seven").unwrap();
        collection.put(13, &[4.0, 5.0], b"gap thirteen").unwrap();
    }

    let actual = candidate
        .collection("full")
        .unwrap()
        .browse_at(None, 100, 20)
        .unwrap();
    let expected = oracle
        .collection("full")
        .unwrap()
        .browse_at(None, 100, 20)
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![0, 3, 7, 13, 20, u64::MAX]
    );
}

#[test]
fn compact_exact_scan_is_bit_identical_at_same_clock() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    assert_compact_resident_pair(&candidate, &oracle, "full");
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("full").unwrap();
        collection.delete(10).unwrap();
        collection
            .put_with_expiry(
                3,
                &[0.2, -0.0],
                b"expires at 20",
                &Metadata::new(),
                Some(20),
            )
            .unwrap();
        collection.put(7, &[0.1, -0.0], b"gap").unwrap();
    }

    let query = [0.1, -0.0];
    let actual = candidate
        .collection("full")
        .unwrap()
        .search_filtered_with_metric_at(&query, 20, &[], crate::DistanceMetric::SquaredL2, 20)
        .unwrap();
    let expected = oracle
        .collection("full")
        .unwrap()
        .search_filtered_with_metric_at(&query, 20, &[], crate::DistanceMetric::SquaredL2, 20)
        .unwrap();
    assert_eq!(
        actual
            .iter()
            .map(|hit| (hit.id, hit.distance.to_bits()))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|hit| (hit.id, hit.distance.to_bits()))
            .collect::<Vec<_>>()
    );

    let candidate_snapshot = candidate
        .collection("full")
        .unwrap()
        .read_snapshot()
        .unwrap();
    let oracle_snapshot = oracle.collection("full").unwrap().read_snapshot().unwrap();
    let mut actual_stats = crate::SearchStats::default();
    let mut expected_stats = crate::SearchStats::default();
    let actual = candidate_snapshot
        .search_with_stats_at(
            &query,
            20,
            &[],
            crate::DistanceMetric::SquaredL2,
            20,
            &mut actual_stats,
        )
        .unwrap();
    let expected = oracle_snapshot
        .search_with_stats_at(
            &query,
            20,
            &[],
            crate::DistanceMetric::SquaredL2,
            20,
            &mut expected_stats,
        )
        .unwrap();
    assert_eq!(
        actual
            .iter()
            .map(|hit| (hit.id, hit.distance.to_bits()))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|hit| (hit.id, hit.distance.to_bits()))
            .collect::<Vec<_>>()
    );
    assert_eq!(actual_stats, expected_stats);
}

#[test]
fn compact_exact_scan_reports_workspace_exhaustion() {
    let _gates = gates();
    let (candidate, _, _) = compact_dispatch_fixture(false);
    let id = candidate.collection("full").unwrap().id();
    let state = candidate.state(id);
    let mut workspace = crate::compact::source::ReadWorkspace::new(0).unwrap();
    let error = crate::exact_search_compact_with_workspace(
        &state.records,
        &state.expiries,
        &[0.1, -0.0],
        0.0,
        1,
        &[],
        crate::DistanceMetric::SquaredL2,
        None,
        0,
        &mut crate::SearchStats::default(),
        &mut workspace,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidQuery("compact scratch budget is smaller than one block")
    ));
}

#[test]
fn compact_search_filters_after_overlay_resolution() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    assert_compact_resident_pair(&candidate, &oracle, "full");
    let yes = Metadata::from([("marker".into(), MetadataValue::String("yes".into()))]);
    let no = Metadata::from([("marker".into(), MetadataValue::String("no".into()))]);
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("full").unwrap();
        collection
            .put_with_metadata(3, &[0.1, -0.0], b"matching replacement", &yes)
            .unwrap();
        collection
            .put_with_metadata(10, &[0.1, -0.0], b"non-matching replacement", &no)
            .unwrap();
        collection
            .put_with_metadata(7, &[0.1, -0.0], b"matching gap", &yes)
            .unwrap();
    }

    let condition = [crate::Condition::Eq {
        field: "marker".into(),
        value: MetadataValue::String("yes".into()),
    }];
    let query = [0.1, -0.0];
    let actual = candidate
        .collection("full")
        .unwrap()
        .search_filtered_with_metric_at(
            &query,
            20,
            &condition,
            crate::DistanceMetric::SquaredL2,
            20,
        )
        .unwrap();
    let expected = oracle
        .collection("full")
        .unwrap()
        .search_filtered_with_metric_at(
            &query,
            20,
            &condition,
            crate::DistanceMetric::SquaredL2,
            20,
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![3, 7]
    );
}

#[test]
fn compact_browse_cursor_skips_tombstones_and_expiry() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    assert_compact_resident_pair(&candidate, &oracle, "full");
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("full").unwrap();
        collection.delete(3).unwrap();
        collection
            .put_with_expiry(
                11,
                &[1.0, 2.0],
                b"expired at 20",
                &Metadata::new(),
                Some(20),
            )
            .unwrap();
        collection
            .put_with_expiry(
                12,
                &[1.0, 2.0],
                b"expired before 20",
                &Metadata::new(),
                Some(19),
            )
            .unwrap();
        collection
            .put(13, &[1.0, 2.0], b"first after cursor")
            .unwrap();
    }

    let actual = candidate
        .collection("full")
        .unwrap()
        .browse_at(Some(9), 1, 0)
        .unwrap();
    let expected = oracle
        .collection("full")
        .unwrap()
        .browse_at(Some(9), 1, 0)
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        actual.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![10]
    );
    assert_eq!(actual.next_after, Some(10));
    let after_base_cursor = candidate
        .collection("full")
        .unwrap()
        .browse_at(Some(10), 1, 20)
        .unwrap();
    assert_eq!(
        after_base_cursor
            .hits
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        vec![13]
    );
    assert_eq!(after_base_cursor.next_after, Some(13));
    let after_overlay_cursor = candidate
        .collection("full")
        .unwrap()
        .browse_at(Some(13), 1, 20)
        .unwrap();
    assert_eq!(
        after_overlay_cursor
            .hits
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        vec![20]
    );
    assert_eq!(after_overlay_cursor.next_after, Some(20));
    let after_last_base = candidate
        .collection("full")
        .unwrap()
        .browse_at(Some(20), 1, 20)
        .unwrap();
    assert_eq!(
        after_last_base
            .hits
            .iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>(),
        vec![u64::MAX]
    );
    let after_final_id = candidate
        .collection("full")
        .unwrap()
        .browse_at(Some(u64::MAX), 1, 20)
        .unwrap();
    assert!(after_final_id.hits.is_empty());
    assert_eq!(after_final_id.next_after, None);
}

#[test]
fn compact_poisoned_text_only_query_preserves_health_error() {
    let _gates = gates();
    let (mut candidate, _, _) = compact_dispatch_fixture(false);
    let id = candidate.collection("full").unwrap().id();
    candidate.poisoned = true;
    let query = crate::query_language::parse("SEARCH TEXT MATCH \"apple\"").unwrap();
    let collection = crate::CollectionRef { db: &candidate, id };
    assert!(matches!(
        query.execute(collection, &std::collections::BTreeMap::new()),
        Err(Error::Poisoned)
    ));
}

fn indexed_dispatch_fixture() -> (crate::Database, crate::Database) {
    let image = crate::compact::catalog::tests::source_indexed_fixture(None);
    let oracle = {
        let _source = super::Activation::set(false);
        crate::Database::open_from_bytes(image.clone(), None).unwrap()
    };
    let candidate = crate::Database::open_from_bytes(image, None).unwrap();
    (candidate, oracle)
}

fn assert_compact_resident_pair(candidate: &crate::Database, oracle: &crate::Database, name: &str) {
    let candidate_id = candidate.collection(name).unwrap().id();
    let oracle_id = oracle.collection(name).unwrap().id();
    assert!(candidate.state(candidate_id).records.is_compact());
    assert!(!oracle.state(oracle_id).records.is_compact());
}

fn mutate_indexed_rows(db: &mut crate::Database, expiry: u64) {
    let mut collection = db.collection_mut("indexed").unwrap();
    collection
        .put_with_metadata(
            1,
            &[4.0, 1.0],
            b"apple apple apple apple",
            &Metadata::from([
                ("tag".into(), MetadataValue::String("gamma".into())),
                (
                    "title".into(),
                    MetadataValue::String("apple apple apple apple".into()),
                ),
            ]),
        )
        .unwrap();
    collection.delete(2).unwrap();
    collection
        .put_with_expiry(
            7,
            &[3.0, 1.0],
            b"apple apple apple",
            &Metadata::from([
                ("tag".into(), MetadataValue::String("alpha".into())),
                (
                    "title".into(),
                    MetadataValue::String("apple apple apple".into()),
                ),
            ]),
            Some(expiry),
        )
        .unwrap();
    for (id, tag, title, vector) in [
        (8, "alpha", "apple apple", [2.0, 1.0]),
        (9, "alpha", "apple", [1.0, 1.0]),
    ] {
        collection
            .put_with_metadata(
                id,
                &vector,
                title.as_bytes(),
                &Metadata::from([
                    ("tag".into(), MetadataValue::String(tag.into())),
                    ("title".into(), MetadataValue::String(title.into())),
                ]),
            )
            .unwrap();
    }
}

#[test]
fn compact_kv_point_and_prefix_match_resident() {
    let _gates = gates();
    let image = crate::compact::catalog::tests::source_kv_fixture();
    let oracle = {
        let _source = super::Activation::set(false);
        crate::Database::open_from_bytes(image.clone(), None).unwrap()
    };
    let mut candidate = crate::Database::open_from_bytes(image, None).unwrap();
    assert_compact_resident_pair(&candidate, &oracle, "kv");
    let actual = candidate.kv_view("kv").expect("Database::kv_view");
    let expected = oracle.kv_view("kv").unwrap();

    for now_ms in [19, 20] {
        assert_eq!(
            actual.get_at("fruit", "apricot", now_ms).unwrap(),
            expected.get_at("fruit", "apricot", now_ms).unwrap()
        );
        assert_eq!(
            actual.list_at("fruit", "ap", None, 10, now_ms).unwrap(),
            expected.list_at("fruit", "ap", None, 10, now_ms).unwrap()
        );
    }
    let first_page = actual.list_at("fruit", "ap", None, 1, 0).unwrap();
    assert_eq!(first_page.len(), 1);
    assert_eq!(first_page[0].key, "apartment");
    assert_eq!(
        actual.list_at("fruit", "ap", Some("apple"), 10, 0).unwrap(),
        expected
            .list_at("fruit", "ap", Some("apple"), 10, 0)
            .unwrap()
    );
    assert_eq!(
        actual
            .list_at("fruit", "ap", Some("apple"), 10, 0)
            .unwrap()
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["apricot"]
    );
    assert_eq!(
        actual.list_at("vegetable", "ap", None, 10, 0).unwrap(),
        expected.list_at("vegetable", "ap", None, 10, 0).unwrap()
    );
    assert_eq!(
        actual
            .list_at("vegetable", "ap", None, 10, 0)
            .unwrap()
            .iter()
            .map(|entry| entry.key.as_str())
            .collect::<Vec<_>>(),
        vec!["apricot"]
    );
    assert_eq!(
        actual
            .get_at("fruit", "probe-window", 0)
            .unwrap()
            .unwrap()
            .value,
        b"displaced"
    );
    assert!(matches!(
        actual.get_at("", "apple", 0),
        Err(Error::InvalidKvKey(_))
    ));
    assert!(matches!(
        actual.get_at("fruit", "", 0),
        Err(Error::InvalidKvKey(_))
    ));
    assert!(matches!(
        actual.get_at("fruit", &"k".repeat(crate::kv::MAX_KV_KEY_BYTES + 1), 0),
        Err(Error::InvalidKvKey(_))
    ));
    assert!(matches!(
        actual.list_at("", "ap", None, 10, 0),
        Err(Error::InvalidKvKey(_))
    ));
    assert!(matches!(
        actual.list_at(
            &"n".repeat(crate::kv::MAX_KV_NAMESPACE_BYTES + 1),
            "ap",
            None,
            10,
            0
        ),
        Err(Error::InvalidKvKey(_))
    ));
    assert!(matches!(
        actual.list_at("fruit", "ap", None, 0, 0),
        Err(Error::InvalidKvKey(_))
    ));
    candidate.poisoned = true;
    assert!(matches!(candidate.kv_view("kv"), Err(Error::Poisoned)));
    consumer_kv::point_and_prefix_phases();
}

#[test]
fn compact_named_vector_visibility_matches_resident() {
    for name in ["full", "half"] {
        named_vector_visibility_matches_resident(name);
    }
}

/// One collection of the fixture: `full` is float32 and `half` float16.
fn named_vector_visibility_matches_resident(name: &str) {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    for db in [&mut candidate, &mut oracle] {
        let collection_id = db.collection(name).unwrap().id();
        {
            let mut collection = db.collection_mut(name).unwrap();
            collection.delete(10).unwrap();
            collection
                .put_with_expiry(
                    3,
                    &[2.0, 3.0],
                    b"expired at boundary",
                    &Metadata::new(),
                    Some(20),
                )
                .unwrap();
            collection
                .put(7, &[4.0, 5.0], b"gap without named vector")
                .unwrap();
        }
        // The named-vector store is independent; keep a vector cell while its primary expires.
        db.state_mut(collection_id).vectors.records.insert(
            3,
            crate::vectors::RecordVectors {
                dense: std::collections::BTreeMap::from([(
                    "n".into(),
                    vec![crate::vectors::VersionedVector {
                        version: "v1".into(),
                        vector: vec![0.3, -0.0],
                    }],
                )]),
                sparse: std::collections::BTreeMap::new(),
            },
        );
    }

    assert_compact_resident_pair(&candidate, &oracle, name);
    let actual = candidate.collection(name).unwrap();
    let expected = oracle.collection(name).unwrap();
    for now_ms in [19, 20] {
        let hits = actual
            .search_named_at("n", None, &[0.3, -0.0], 10, now_ms)
            .unwrap();
        assert_eq!(
            hits,
            expected
                .search_named_at("n", None, &[0.3, -0.0], 10, now_ms)
                .unwrap()
        );
        assert_eq!(
            hits.iter().any(|hit| hit.id == 3),
            now_ms < 20,
            "named-vector visibility must use the supplied clock"
        );
    }
    assert_eq!(
        actual.record_vectors(10).unwrap(),
        expected.record_vectors(10).unwrap()
    );
    assert_eq!(
        actual.reembed_pending("n", 100).unwrap(),
        expected.reembed_pending("n", 100).unwrap()
    );
    let pending = actual.reembed_pending("n", 1).unwrap();
    assert_eq!(pending, expected.reembed_pending("n", 1).unwrap());
    assert_eq!(pending.len(), 1);
}

#[test]
fn compact_named_vector_search_propagates_primary_read_error() {
    let _gates = gates();
    let (mut candidate, _, _) = compact_dispatch_fixture(false);
    let collection_id = candidate.collection("full").unwrap().id();
    let id = candidate.state(collection_id).records.first_id().unwrap();
    candidate.state_mut(collection_id).vectors.records.insert(
        id,
        crate::vectors::RecordVectors {
            dense: std::collections::BTreeMap::from([(
                "n".into(),
                vec![crate::vectors::VersionedVector {
                    version: "v1".into(),
                    vector: vec![0.3, -0.0],
                }],
            )]),
            sparse: std::collections::BTreeMap::new(),
        },
    );
    let crate::records::CollectionRecords::Compact { view, .. } =
        &candidate.state(collection_id).records
    else {
        panic!("compact source required")
    };
    let source = view.source().unwrap();
    let (block, row) = source.row(id).unwrap();
    let cell = &source.cells[source.block_starts[block] + row];
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = cell.initializing.lock().unwrap();
        panic!("poison compact record cell lock");
    }));

    let result =
        candidate
            .collection("full")
            .unwrap()
            .search_named_at("n", None, &[0.3, -0.0], 10, 0);
    assert!(
        matches!(
            &result,
            Err(crate::Error::InvalidQuery(
                "compact record cell lock poisoned"
            ))
        ),
        "{result:?}"
    );
}

#[test]
fn compact_query_validates_before_materializing_rows() {
    let _gates = gates();
    let (candidate, _) = indexed_dispatch_fixture();
    let collection_id = candidate.collection("indexed").unwrap().id();
    let crate::records::CollectionRecords::Compact { view, .. } =
        &candidate.state(collection_id).records
    else {
        panic!("compact source required")
    };
    assert_eq!(
        view.generation
            .decode_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    let query = crate::query_language::parse("SEARCH default NEAR $q").unwrap();
    assert!(matches!(
        query.execute(
            candidate.collection("indexed").unwrap(),
            &std::collections::BTreeMap::new()
        ),
        Err(crate::Error::InvalidQuery("missing vector parameter"))
    ));
    assert_eq!(
        view.generation
            .decode_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "invalid query parameters must be rejected before compact row decoding"
    );
}

#[test]
fn compact_hybrid_text_leg_matches_resident_after_overlay_resolution() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    let expiry = crate::current_time_ms().saturating_add(86_400_000);
    mutate_indexed_rows(&mut candidate, expiry);
    mutate_indexed_rows(&mut oracle, expiry);
    let query = crate::hybrid::HybridQuery::new(Some(vec![1.0, 1.0]), "apple", 10);
    let candidate_collection = candidate.collection("indexed").unwrap();
    let oracle_collection = oracle.collection("indexed").unwrap();
    for now_ms in [expiry - 1, expiry] {
        let actual = candidate_collection
            .search_hybrid_at(&query, now_ms)
            .unwrap();
        let expected = oracle_collection.search_hybrid_at(&query, now_ms).unwrap();
        assert_eq!(actual, expected);
        let ids = actual.0.iter().map(|hit| hit.id).collect::<Vec<_>>();
        if now_ms < expiry {
            assert_eq!(ids, vec![1, 9, 7, 8]);
        } else {
            assert_eq!(ids, vec![1, 9, 8]);
        }
    }
    let limited = candidate_collection
        .search_hybrid_at(
            &crate::hybrid::HybridQuery::new(Some(vec![1.0, 1.0]), "apple", 2),
            expiry - 1,
        )
        .unwrap()
        .0;
    assert_eq!(limited.len(), 2);

    let mut filtered = query.clone();
    filtered.conditions = vec![crate::Condition::Eq {
        field: "tag".into(),
        value: MetadataValue::String("alpha".into()),
    }];
    let filtered_ids = candidate_collection
        .search_hybrid_at(&filtered, expiry - 1)
        .unwrap()
        .0
        .into_iter()
        .map(|hit| hit.id)
        .collect::<Vec<_>>();
    assert_eq!(filtered_ids, vec![7, 9, 8]);
}

#[test]
fn compact_hybrid_ties_order_by_id() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("indexed").unwrap();
        for id in [10, 11] {
            collection
                .put_with_metadata(
                    id,
                    &[1.0, 1.0],
                    b"apple same-score",
                    &Metadata::from([
                        ("tag".into(), MetadataValue::String("alpha".into())),
                        (
                            "title".into(),
                            MetadataValue::String("apple same-score".into()),
                        ),
                    ]),
                )
                .unwrap();
        }
    }

    let query = crate::hybrid::HybridQuery::new(None, "apple", 10);
    let actual = candidate
        .collection("indexed")
        .unwrap()
        .search_hybrid_at(&query, 0)
        .unwrap()
        .0;
    let expected = oracle
        .collection("indexed")
        .unwrap()
        .search_hybrid_at(&query, 0)
        .unwrap()
        .0;
    assert_eq!(actual, expected);
    let ids = actual.iter().map(|hit| hit.id).collect::<Vec<_>>();
    assert!(
        ids.iter().position(|&id| id == 10) < ids.iter().position(|&id| id == 11),
        "equal hybrid scores must use ascending ids: {ids:?}"
    );
}

#[test]
fn compact_hybrid_requires_declared_text_fields() {
    let _gates = gates();
    let (candidate, _, _) = compact_dispatch_fixture(false);
    let collection = candidate.collection("full").unwrap();
    assert!(candidate.state(collection.id()).records.is_compact());
    assert!(matches!(
        collection.search_hybrid_at(&crate::hybrid::HybridQuery::new(None, "apple", 10), 0),
        Err(Error::InvalidQuery(
            "text search requires declared text_fields"
        ))
    ));
}

#[test]
fn compact_hybrid_rejects_poisoned_text_only_search() {
    let _gates = gates();
    let (mut candidate, _) = indexed_dispatch_fixture();
    let collection_id = candidate.collection("indexed").unwrap().id();
    candidate.poisoned = true;
    assert!(matches!(
        crate::CollectionRef {
            db: &candidate,
            id: collection_id
        }
        .search_hybrid_at(&crate::hybrid::HybridQuery::new(None, "apple", 10), 0),
        Err(Error::Poisoned)
    ));
}

#[test]
fn compact_grouped_search_and_facets_match_resident() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    let expiry = crate::current_time_ms().saturating_add(86_400_000);
    mutate_indexed_rows(&mut candidate, expiry);
    mutate_indexed_rows(&mut oracle, expiry);
    let conditions = [];
    let actual_collection = candidate.collection("indexed").unwrap();
    let expected_collection = oracle.collection("indexed").unwrap();
    let actual = actual_collection
        .search_grouped(
            &[1.0, 1.0],
            10,
            10,
            "tag",
            &conditions,
            crate::DistanceMetric::SquaredL2,
        )
        .unwrap();
    let expected = expected_collection
        .search_grouped(
            &[1.0, 1.0],
            10,
            10,
            "tag",
            &conditions,
            crate::DistanceMetric::SquaredL2,
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 2);
    assert_eq!(
        actual.iter().map(|group| group.hits.len()).sum::<usize>(),
        4
    );
    let alpha = [crate::Condition::Eq {
        field: "tag".into(),
        value: MetadataValue::String("alpha".into()),
    }];
    let actual_facets = actual_collection.facets("tag", &alpha).unwrap();
    assert_eq!(
        actual_facets,
        expected_collection.facets("tag", &alpha).unwrap()
    );
    assert_eq!(
        actual_facets.iter().map(|facet| facet.count).sum::<u64>(),
        3
    );
}

#[test]
fn compact_query_language_matches_resident_after_overlay_resolution() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    let expiry = crate::current_time_ms().saturating_add(86_400_000);
    mutate_indexed_rows(&mut candidate, expiry);
    mutate_indexed_rows(&mut oracle, expiry);
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("indexed").unwrap();
        for (id, tag, title) in [
            (1, "gamma", "apple alpha"),
            (7, "alpha", "apple bravo"),
            (8, "alpha", "apple charlie"),
            (9, "alpha", "apple delta"),
        ] {
            collection
                .update_metadata(
                    id,
                    &Metadata::from([
                        ("tag".into(), MetadataValue::String(tag.into())),
                        ("title".into(), MetadataValue::String(title.into())),
                    ]),
                )
                .unwrap();
        }
    }
    let query = crate::query_language::parse(
        "SEARCH default NEAR $q TEXT \"apple\" WHERE tag EXISTS GROUP BY tag FACET BY tag ORDER BY title LIMIT 3",
    )
    .unwrap();
    let parameters = std::collections::BTreeMap::from([("q".into(), vec![1.0, 1.0])]);
    let actual = query
        .execute(candidate.collection("indexed").unwrap(), &parameters)
        .unwrap();
    let expected = query
        .execute(oracle.collection("indexed").unwrap(), &parameters)
        .unwrap();
    assert_eq!(actual.hits, expected.hits);
    assert_eq!(actual.groups, expected.groups);
    assert_eq!(actual.facets, expected.facets);
    assert_eq!(
        actual.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![1, 7, 8]
    );
    assert_eq!(
        actual
            .groups
            .iter()
            .map(|group| group.hits.len())
            .sum::<usize>(),
        4
    );
    assert_eq!(
        actual.facets.iter().map(|facet| facet.count).sum::<u64>(),
        4
    );
}

#[test]
fn compact_query_language_vector_leg_uses_full_candidate_depth() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    for db in [&mut candidate, &mut oracle] {
        let mut collection = db.collection_mut("indexed").unwrap();
        for id in [3, 4] {
            collection
                .put_with_metadata(
                    id,
                    &[id as f32, 1.0],
                    b"",
                    &Metadata::from([("tag".into(), MetadataValue::String("alpha".into()))]),
                )
                .unwrap();
        }
    }

    let query = crate::query_language::parse("SEARCH default NEAR $q LIMIT 4").unwrap();
    let parameters = std::collections::BTreeMap::from([("q".into(), vec![1.0, 1.0])]);
    let actual = query
        .execute(candidate.collection("indexed").unwrap(), &parameters)
        .unwrap();
    let expected = query
        .execute(oracle.collection("indexed").unwrap(), &parameters)
        .unwrap();
    assert_eq!(actual.hits, expected.hits);
    assert_eq!(actual.hits.len(), 4);
    assert_eq!(
        actual.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
}

#[test]
fn compact_exact_search_candidate_and_stats_match_declared_index() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    let expiry = crate::current_time_ms().saturating_add(86_400_000);
    mutate_indexed_rows(&mut candidate, expiry);
    mutate_indexed_rows(&mut oracle, expiry);
    let condition = [crate::Condition::Eq {
        field: "tag".into(),
        value: MetadataValue::String("alpha".into()),
    }];
    let actual = candidate
        .collection("indexed")
        .unwrap()
        .search_with_stats(
            &[1.0, 1.0],
            10,
            &condition,
            crate::DistanceMetric::SquaredL2,
        )
        .unwrap();
    let expected = oracle
        .collection("indexed")
        .unwrap()
        .search_with_stats(
            &[1.0, 1.0],
            10,
            &condition,
            crate::DistanceMetric::SquaredL2,
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.1.index_candidates, 0);
    assert_eq!(
        actual.1.records_scanned, 4,
        "postings above half the live rows must fall back to the full exact scan"
    );
}

#[test]
fn compact_exact_search_validation_and_health_match_resident() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    let condition = [crate::Condition::Eq {
        field: String::new(),
        value: MetadataValue::String("alpha".into()),
    }];
    let candidate_id = candidate.collection("indexed").unwrap().id();
    let oracle_id = oracle.collection("indexed").unwrap().id();
    for db in [&candidate, &oracle] {
        let collection = db.collection("indexed").unwrap();
        let wrong_dimension = collection.search_filtered_with_metric_at(
            &[1.0],
            10,
            &[],
            crate::DistanceMetric::SquaredL2,
            0,
        );
        let invalid_filter = collection.search_filtered_with_metric_at(
            &[1.0, 1.0],
            10,
            &condition,
            crate::DistanceMetric::SquaredL2,
            0,
        );
        let invalid_dimension_and_filter = collection.search_filtered_with_metric_at(
            &[1.0],
            10,
            &condition,
            crate::DistanceMetric::SquaredL2,
            0,
        );
        assert!(matches!(
            wrong_dimension,
            Err(Error::DimensionMismatch { .. })
        ));
        assert!(matches!(invalid_filter, Err(Error::InvalidFilter(_))));
        assert!(matches!(
            invalid_dimension_and_filter,
            Err(Error::DimensionMismatch { .. })
        ));
    }
    candidate.poisoned = true;
    oracle.poisoned = true;
    for (db, id) in [(&candidate, candidate_id), (&oracle, oracle_id)] {
        assert!(matches!(
            crate::CollectionRef { db, id }.search_filtered_with_metric_at(
                &[1.0, 1.0],
                10,
                &[],
                crate::DistanceMetric::SquaredL2,
                0
            ),
            Err(Error::Poisoned)
        ));
    }
}

#[test]
fn compact_snapshot_search_preserves_validation_and_pinned_health() {
    let _gates = gates();
    let (mut candidate, mut oracle) = indexed_dispatch_fixture();
    assert_compact_resident_pair(&candidate, &oracle, "indexed");
    let actual = candidate
        .collection("indexed")
        .unwrap()
        .read_snapshot()
        .unwrap();
    let expected = oracle
        .collection("indexed")
        .unwrap()
        .read_snapshot()
        .unwrap();
    let invalid_filter = [crate::Condition::Eq {
        field: String::new(),
        value: MetadataValue::String("alpha".into()),
    }];
    assert!(matches!(
        actual.search_filtered_with_metric_at(&[1.0], 10, &[], crate::DistanceMetric::SquaredL2, 0),
        Err(Error::DimensionMismatch { .. })
    ));
    assert!(matches!(
        actual.search_filtered_with_metric_at(
            &[1.0, 1.0],
            10,
            &invalid_filter,
            crate::DistanceMetric::SquaredL2,
            0
        ),
        Err(Error::InvalidFilter(_))
    ));
    candidate.poisoned = true;
    oracle.poisoned = true;
    assert_eq!(
        actual
            .search_filtered_with_metric_at(
                &[1.0, 1.0],
                10,
                &[],
                crate::DistanceMetric::SquaredL2,
                0
            )
            .unwrap(),
        expected
            .search_filtered_with_metric_at(
                &[1.0, 1.0],
                10,
                &[],
                crate::DistanceMetric::SquaredL2,
                0
            )
            .unwrap()
    );
}

#[test]
fn compact_graph_retrieve_projects_effective_row_at_one_clock() {
    let _gates = gates();
    let (mut candidate, mut oracle, _) = compact_dispatch_fixture(false);
    assert_compact_resident_pair(&candidate, &oracle, "full");
    let expires_at = crate::current_time_ms().saturating_sub(1);
    assert!(matches!(
        candidate.graph("g"),
        Err(crate::Error::UnsupportedFeature(
            crate::format::FLAG_COMPACT
        ))
    ));
    for db in [&mut candidate, &mut oracle] {
        assert!(matches!(
            db.collection_mut("full").unwrap().put_with_expiry(
                0,
                &[0.1, -0.0],
                b"replacement evidence",
                &Metadata::from([("marker".into(), MetadataValue::String("overlay".into()))]),
                Some(1),
            ),
            Err(crate::Error::GraphAttachedRecordExpiry)
        ));
        let mut collection = db.collection_mut("full").unwrap();
        collection
            .put_with_metadata(
                0,
                &[0.1, -0.0],
                b"replacement evidence",
                &Metadata::from([("marker".into(), MetadataValue::String("overlay".into()))]),
            )
            .unwrap();
        collection
            .put_with_expiry(
                7,
                &[0.1, -0.0],
                b"temporary pivot",
                &Metadata::from([("marker".into(), MetadataValue::String("temporary".into()))]),
                Some(expires_at),
            )
            .unwrap();
    }

    let mut pivot = crate::property_graph::Pivot::new("full", 10);
    pivot.vector = Some(vec![0.1, -0.0]);
    let request = crate::property_graph::Retrieve {
        pivot,
        expansion: crate::property_graph::Expansion::default(),
        evidence: crate::property_graph::EvidenceSpec {
            collections: vec!["full".into()],
            fields: crate::property_graph::PropertyProjection::All,
            ..Default::default()
        },
    };
    let now_ms = crate::current_time_ms();
    let actual = candidate
        .compact_graph_for_test("g", "full")
        .unwrap()
        .with_clock(now_ms)
        .retrieve(&request, &crate::property_graph::ReadOptions::default())
        .unwrap();
    let expected = oracle
        .graph("g")
        .unwrap()
        .with_clock(now_ms)
        .retrieve(&request, &crate::property_graph::ReadOptions::default())
        .unwrap();
    let mut actual = actual;
    let mut expected = expected;
    actual.stats.pivot_ms = 0;
    actual.stats.expand_ms = 0;
    actual.stats.expansion.elapsed_ms = 0;
    expected.stats.pivot_ms = 0;
    expected.stats.expand_ms = 0;
    expected.stats.expansion.elapsed_ms = 0;
    assert_eq!(actual, expected);
    assert_eq!(actual.evidence.len(), 1);
    assert_eq!(actual.evidence[0].payload_excerpt, b"replacement evidence");
    assert_eq!(
        actual.evidence[0].metadata.get("marker"),
        Some(&MetadataValue::String("overlay".into()))
    );

    let mut expiring_pivot = crate::property_graph::Pivot::new("full", 10);
    expiring_pivot.vector = Some(vec![0.1, -0.0]);
    expiring_pivot.filter = vec![crate::Condition::Eq {
        field: "marker".into(),
        value: MetadataValue::String("temporary".into()),
    }];
    let request = crate::property_graph::Retrieve {
        pivot: expiring_pivot,
        expansion: crate::property_graph::Expansion::default(),
        evidence: crate::property_graph::EvidenceSpec::default(),
    };
    for now_ms in [expires_at - 1, expires_at] {
        let actual = candidate
            .compact_graph_for_test("g", "full")
            .unwrap()
            .with_clock(now_ms)
            .retrieve(&request, &crate::property_graph::ReadOptions::default())
            .unwrap();
        let expected = oracle
            .graph("g")
            .unwrap()
            .with_clock(now_ms)
            .retrieve(&request, &crate::property_graph::ReadOptions::default())
            .unwrap();
        let mut actual = actual;
        let mut expected = expected;
        actual.stats.pivot_ms = 0;
        actual.stats.expand_ms = 0;
        actual.stats.expansion.elapsed_ms = 0;
        expected.stats.pivot_ms = 0;
        expected.stats.expand_ms = 0;
        expected.stats.expansion.elapsed_ms = 0;
        assert_eq!(actual, expected);
        assert_eq!(actual.unmapped_pivots.count, u64::from(now_ms < expires_at));
    }
}
