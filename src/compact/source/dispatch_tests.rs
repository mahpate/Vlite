use crate::compact::catalog::tests::{source_key, source_view_fixture};
use crate::records::CollectionRecords;
use crate::rerank::{ExactReranker, RerankCandidate, Reranker};
use crate::{CollectionSettings, DistanceMetric, Expected, HnswConfig, Metadata, Mutation};
use crate::{Database, Error, Record, Result, SearchHit};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct ProviderSentinel(AtomicBool);

impl Reranker for ProviderSentinel {
    fn rerank(&self, _: &[f32], _: &[RerankCandidate], _: usize) -> Result<Vec<SearchHit>> {
        self.0.store(true, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

fn gates() -> (
    crate::compact::Activation,
    crate::compact::catalog::payload::Activation,
    crate::compact::source::Activation,
) {
    (
        crate::compact::Activation::on(),
        crate::compact::catalog::payload::Activation::on(),
        crate::compact::source::Activation::on(),
    )
}

// Dispatch-only fixture: the loader still installs resident collections in 1a.
// Inject the verified standalone views only after the normal open has completed.
fn compact_dispatch_fixture(encrypted: bool) -> (Database, Database, Vec<u8>) {
    let key = encrypted.then(source_key);
    let (image, loaded) = source_view_fixture(key.as_ref());
    let oracle = Database::open_from_bytes(image.clone(), key.as_ref()).unwrap();
    let mut candidate = Database::open_from_bytes(image.clone(), key.as_ref()).unwrap();
    let generation = loaded.compact_generation.unwrap();
    for (&id, collection) in &mut candidate.collections {
        collection.records = CollectionRecords::Compact {
            view: generation.view(id),
        };
    }
    (candidate, oracle, image)
}

fn point_parity(candidate: &Database, oracle: &Database) {
    assert_eq!(
        candidate.collections().unwrap(),
        oracle.collections().unwrap()
    );
    for (&id, expected) in &oracle.collections {
        let actual = candidate.collection(&expected.name).unwrap();
        let reference = oracle.collection(&expected.name).unwrap();
        assert_eq!(actual.id(), reference.id());
        assert_eq!(actual.name(), reference.name());
        assert_eq!(actual.dimensions(), reference.dimensions());
        assert_eq!(actual.settings(), reference.settings());
        assert_eq!(actual.len(), reference.len());
        assert_eq!(actual.is_empty(), reference.is_empty());
        assert_eq!(actual.first_id(), reference.first_id());
        for (&record_id, record) in expected.records.resident().unwrap() {
            assert!(candidate.state(id).records.contains_key(&record_id));
            assert_eq!(
                actual.revision(record_id).unwrap(),
                reference.revision(record_id).unwrap(),
                "revision parity"
            );
            let expiry = expected.expiries.get(&record_id).copied();
            assert_eq!(
                actual.expires_at(record_id).unwrap(),
                expiry,
                "expiry state"
            );
            assert_eq!(
                actual.expires_at(record_id).unwrap(),
                reference.expires_at(record_id).unwrap(),
                "expires_at parity"
            );
            let mut clocks = vec![0, u64::MAX];
            if let Some(deadline) = expiry {
                clocks.push(deadline);
                if let Some(predecessor) = deadline.checked_sub(1) {
                    clocks.push(predecessor);
                }
            }
            for now in clocks {
                let observed = actual.get_at(record_id, now).unwrap();
                let want = reference.get_at(record_id, now).unwrap();
                assert_eq!(
                    want.is_some(),
                    expiry.is_none_or(|deadline| now < deadline),
                    "resident expiry boundary must hide at the deadline"
                );
                assert_eq!(observed, want);
                if let Some(observed) = observed {
                    assert_eq!(
                        observed
                            .vector
                            .iter()
                            .map(|value| value.to_bits())
                            .collect::<Vec<_>>(),
                        record
                            .vector
                            .iter()
                            .map(|value| value.to_bits())
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
        let missing = (0..).find(|id| !expected.records.contains_key(id)).unwrap();
        assert!(!candidate.state(id).records.contains_key(&missing));
        assert!(actual.get_at(missing, 0).unwrap().is_none());
        assert_eq!(actual.revision(missing).unwrap(), None);
        assert_eq!(actual.expires_at(missing).unwrap(), None);
    }
}

fn unsupported<T>(result: Result<T>, method: &str) {
    assert!(
        matches!(
            result,
            Err(Error::UnsupportedFeature(crate::format::FLAG_COMPACT))
        ),
        "{method} must refuse compact records"
    );
}

#[test]
fn compact_dispatch_point_observations_and_raw_image_match_resident() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (mut candidate, oracle, image) = compact_dispatch_fixture(encrypted);
        assert!(
            oracle.collections.values().any(|state| state
                .expiries
                .values()
                .any(|&deadline| deadline > 0 && deadline < u64::MAX)),
            "point parity must exercise a finite expiry and predecessor"
        );
        point_parity(&candidate, &oracle);
        assert_eq!(
            candidate.to_bytes(),
            Some(image.clone()),
            "raw image parity"
        );
        assert_eq!(candidate.to_bytes(), oracle.to_bytes());
        assert_eq!(candidate.is_directory(), oracle.is_directory());
        assert_eq!(candidate.is_encrypted(), oracle.is_encrypted());
        assert_eq!(
            candidate.has_commit_timestamps(),
            oracle.has_commit_timestamps()
        );
        assert_eq!(candidate.is_poisoned(), oracle.is_poisoned());
        assert_eq!(candidate.format_version(), oracle.format_version());
        assert_eq!(candidate.lineage(), oracle.lineage());
        assert_eq!(candidate.dimensions(), oracle.dimensions());
        assert_eq!(candidate.len(), oracle.len());
        assert_eq!(candidate.is_empty(), oracle.is_empty());
        assert_eq!(candidate.durability(), oracle.durability());
        assert_eq!(candidate.is_read_only(), oracle.is_read_only());
        assert_eq!(candidate.encryption_key_id(), oracle.encryption_key_id());
        assert_eq!(candidate.unsynced_commits(), oracle.unsynced_commits());
        assert!(
            !candidate.cancel_hnsw_rebuild(),
            "no compact rebuild can be pending"
        );
        candidate.set_commit_clock(Box::new(|| u64::MAX));
        candidate.set_checkpoint_threshold(Some(1));
        point_parity(&candidate, &oracle);
        let key = encrypted.then(source_key);
        let reopened =
            Database::open_from_bytes(candidate.to_bytes().unwrap(), key.as_ref()).unwrap();
        point_parity(&reopened, &oracle);
    }
}

#[test]
fn compact_dispatch_mutable_acquisition_and_writes_refuse_before_changes() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (mut candidate, oracle, bytes) = compact_dispatch_fixture(encrypted);
        let sequence = candidate.sequence;
        let end = candidate.end;
        macro_rules! refuse {
            ($name:literal, $operation:expr) => {{
                unsupported($operation, $name);
                assert_eq!(candidate.sequence, sequence, "{} sequence", $name);
                assert_eq!(candidate.end, end, "{} end", $name);
                assert_eq!(candidate.to_bytes().unwrap(), bytes, "{} bytes", $name);
                point_parity(&candidate, &oracle);
            }};
        }
        for info in oracle.collections().unwrap() {
            refuse!("collection_mut", candidate.collection_mut(&info.name));
            refuse!("collection_mut_in", candidate.collection_mut_in(info.id));
            refuse!("kv_store", candidate.kv_store(&info.name));
            refuse!("kv_view", candidate.kv_view(&info.name));
            refuse!(
                "set_collection_settings",
                candidate.set_collection_settings(&info.name, CollectionSettings::default())
            );
        }
        refuse!("put", candidate.put(99, &[1.0, 2.0], b"new"));
        refuse!(
            "put_with_metadata",
            candidate.put_with_metadata(99, &[1.0, 2.0], b"new", &Metadata::new())
        );
        refuse!(
            "put_with_expiry",
            candidate.put_with_expiry(99, &[1.0, 2.0], b"new", &Metadata::new(), Some(1))
        );
        refuse!(
            "put_with_metadata_if",
            candidate.put_with_metadata_if(
                99,
                &[1.0, 2.0],
                b"new",
                &Metadata::new(),
                Expected::Any
            )
        );
        refuse!("delete", candidate.delete(u64::MAX));
        refuse!("delete_if", candidate.delete_if(u64::MAX, Expected::Any));
        refuse!(
            "update_metadata",
            candidate.update_metadata(0, &Metadata::new())
        );
        refuse!("write_batch empty", candidate.write_batch(&[]));
        refuse!(
            "write_batch",
            candidate.write_batch(&[Mutation::Delete { id: 0 }])
        );
        refuse!("checkpoint", candidate.checkpoint());
        refuse!("sync", candidate.sync());
        refuse!("feed_cursor", candidate.feed_cursor());
        refuse!("export_log", candidate.export_log(0, 1024));
        refuse!(
            "set_durability",
            candidate.set_durability(crate::Durability::None)
        );
        refuse!("build_hnsw", candidate.build_hnsw(HnswConfig::default()));
        refuse!(
            "start_hnsw_rebuild",
            candidate.start_hnsw_rebuild(
                HnswConfig::default(),
                DistanceMetric::SquaredL2,
                crate::HnswRebuildLimits::default()
            )
        );
        refuse!("poll_hnsw_rebuild", candidate.poll_hnsw_rebuild());
        refuse!("graph", candidate.graph("missing"));
        refuse!("graph_mut", candidate.graph_mut("missing"));
        refuse!("create_graph", candidate.create_graph("new"));
        refuse!("list_graphs", candidate.list_graphs());
        assert!(!candidate.cancel_hnsw_rebuild());
    }
}

#[test]
fn compact_dispatch_scan_search_and_snapshot_scans_refuse() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (candidate, oracle, bytes) = compact_dispatch_fixture(encrypted);
        let sequence = candidate.sequence;
        for info in oracle.collections().unwrap() {
            let view = candidate.collection(&info.name).unwrap();
            let query = vec![0.5; view.dimensions()];
            let exact = ExactReranker {
                metric: DistanceMetric::SquaredL2,
            };
            let sentinel = ProviderSentinel(AtomicBool::new(false));
            let hits = [SearchHit {
                id: oracle.state(info.id).records.first_id().unwrap_or(0),
                distance: 0.0,
            }];
            let empty = view.rerank(&query, &[], 0, &exact);
            let populated = view.rerank(&query, &hits, 1, &exact);
            let custom = view.rerank(&query, &hits, 1, &sentinel);
            assert!(
                !sentinel.0.load(Ordering::SeqCst),
                "compact rerank must not invoke the provider"
            );
            unsupported(empty, "rerank empty");
            unsupported(populated, "rerank populated");
            unsupported(custom, "rerank custom provider");
            unsupported(view.rerank(&[], &[], 0, &exact), "rerank invalid query");
            unsupported(view.browse(None, 1), "browse");
            unsupported(view.browse_at(None, 0, 0), "browse invalid limit");
            unsupported(view.search(&query, 0), "search zero k");
            unsupported(view.search_filtered(&query, 1, &[]), "search_filtered");
            unsupported(
                view.search_with_metric(&query, 1, DistanceMetric::Cosine),
                "search_with_metric",
            );
            unsupported(view.facets("tag", &[]), "facets");
            unsupported(view.numeric_stats("number", &[]), "numeric_stats");
            unsupported(view.vector_specs(), "vector_specs");
            unsupported(view.record_vectors(0), "record_vectors");
            unsupported(view.reembed_status("missing"), "reembed_status");
            unsupported(view.search_sparse("missing", &[], 0), "search_sparse");
            unsupported(
                view.search_named("missing", None, &query, 0),
                "search_named",
            );
            let snapshot = view.read_snapshot().unwrap();
            let resident = oracle
                .collection(&info.name)
                .unwrap()
                .read_snapshot()
                .unwrap();
            assert_eq!(snapshot.sequence(), resident.sequence());
            assert_eq!(snapshot.lineage(), resident.lineage());
            assert_eq!(snapshot.dimensions(), resident.dimensions());
            assert_eq!(snapshot.len(), resident.len());
            assert_eq!(snapshot.is_empty(), resident.is_empty());
            unsupported(snapshot.browse(None, 1), "snapshot browse");
            unsupported(snapshot.search(&query, 0), "snapshot search");
        }
        assert_eq!(candidate.sequence, sequence);
        assert_eq!(candidate.to_bytes().unwrap(), bytes);
        point_parity(&candidate, &oracle);
    }
}

#[test]
fn compact_dispatch_enum_clone_pins_source_and_refuses_resident_access() {
    let _gates = gates();
    let (_, loaded) = source_view_fixture(None);
    let generation = loaded.compact_generation.as_ref().unwrap();
    let weak = Arc::downgrade(generation);
    let collection = loaded
        .collections
        .iter()
        .find(|(_, state)| !state.records.is_empty())
        .unwrap()
        .0;
    let mut records = CollectionRecords::Compact {
        view: generation.view(*collection),
    };
    let retained = records.clone();
    unsupported(records.resident(), "resident scan");
    unsupported(records.resident_mut(), "resident mutation");
    let id = records.first_id().unwrap();
    let first = records.get_at(id, 0, &BTreeMap::new()).unwrap().unwrap() as *const Record;
    drop(loaded);
    drop(records);
    assert!(
        weak.upgrade().is_some(),
        "enum clone must pin its generation"
    );
    assert_eq!(
        retained.get_at(id, 0, &BTreeMap::new()).unwrap().unwrap() as *const Record,
        first
    );
    drop(retained);
    assert!(
        weak.upgrade().is_none(),
        "last enum drop must release its generation"
    );
}

#[test]
fn compact_dispatch_snapshot_fixture_pins_generation_after_database_drop() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (candidate, oracle, _) = compact_dispatch_fixture(encrypted);
        let expected = oracle
            .collections
            .values()
            .find(|state| !state.records.is_empty())
            .unwrap();
        let id = expected.records.resident().unwrap().keys().next().unwrap();
        let view = candidate.collection(&expected.name).unwrap();
        let weak = match &candidate.state(view.id()).records {
            CollectionRecords::Resident(_) => panic!("fixture must inject compact records"),
            CollectionRecords::Compact { view } => Arc::downgrade(&view.generation),
        };
        let snapshot = view.read_snapshot().unwrap();
        let clone = snapshot.clone();
        drop(candidate);
        assert!(
            weak.upgrade().is_some(),
            "snapshot must pin its generation after Database drop"
        );
        let record = snapshot.get_at(*id, 0).unwrap().unwrap();
        assert_eq!(Some(record), expected.records.resident().unwrap().get(id));
        assert!(std::ptr::eq(record, clone.get_at(*id, 0).unwrap().unwrap()));
        assert!(
            snapshot.get_at(*id, u64::MAX).unwrap().is_none()
                == oracle
                    .collection(&expected.name)
                    .unwrap()
                    .get_at(*id, u64::MAX)
                    .unwrap()
                    .is_none()
        );
        drop(snapshot);
        assert!(
            weak.upgrade().is_some(),
            "snapshot clone must retain the pin"
        );
        drop(clone);
        assert!(
            weak.upgrade().is_none(),
            "last snapshot drop must release the source"
        );
    }
}

#[test]
fn compact_dispatch_resident_collection_remains_writable_in_mixed_fixture() {
    let _gates = gates();
    let (mut candidate, _, _) = compact_dispatch_fixture(false);
    candidate.state_mut(0).records = CollectionRecords::Resident(crate::records::RecordMap::new());
    candidate.put(99, &[1.0, 2.0], b"resident").unwrap();
    assert_eq!(
        candidate.get_at(99, 0).unwrap().unwrap().payload,
        b"resident"
    );
    candidate
        .collection_mut("default")
        .unwrap()
        .put(100, &[2.0, 3.0], b"mutable")
        .unwrap();
    assert_eq!(
        candidate
            .read_snapshot()
            .unwrap()
            .browse(None, 10)
            .unwrap()
            .hits
            .len(),
        2
    );
    let view = candidate.collection("default").unwrap();
    let hits = [SearchHit {
        id: 99,
        distance: 0.0,
    }];
    let exact = ExactReranker {
        metric: DistanceMetric::SquaredL2,
    };
    assert_eq!(
        view.rerank(&[1.0, 2.0], &hits, 1, &exact).unwrap(),
        hits,
        "resident rerank must remain available in a mixed handle"
    );
    let sentinel = ProviderSentinel(AtomicBool::new(false));
    assert!(
        view.rerank(&[1.0, 2.0], &hits, 1, &sentinel)
            .unwrap()
            .is_empty()
    );
    assert!(sentinel.0.load(Ordering::SeqCst));
    let sequence = candidate.sequence;
    let bytes = candidate.to_bytes().unwrap();
    unsupported(candidate.checkpoint(), "mixed checkpoint");
    assert_eq!(candidate.sequence, sequence, "mixed checkpoint sequence");
    assert_eq!(
        candidate.to_bytes().unwrap(),
        bytes,
        "mixed checkpoint bytes"
    );
    unsupported(candidate.sync(), "mixed sync");
    assert_eq!(candidate.sequence, sequence, "mixed sync sequence");
    assert_eq!(candidate.to_bytes().unwrap(), bytes, "mixed sync bytes");
    candidate.poisoned = true;
    let request = crate::GeoSearchQuery {
        vector: vec![1.0, 2.0],
        k: 1,
        field: String::new(),
        latitude: 0.0,
        longitude: 0.0,
        radius_meters: 1.0,
        conditions: Vec::new(),
        metric: DistanceMetric::SquaredL2,
        sort: crate::GeoSort::GeoDistance,
    };
    assert!(matches!(
        candidate.collection("default"),
        Err(Error::Poisoned)
    ));
    let view = crate::CollectionRef {
        db: &candidate,
        id: 0,
    };
    assert!(
        view.rerank(&[], &[], 0, &sentinel).unwrap().is_empty(),
        "resident empty rerank must not acquire an earlier health error"
    );
    assert!(
        matches!(
            view.search_geo(&request),
            Err(Error::InvalidQuery("geo field must be 1..=256 bytes"))
        ),
        "resident geo validation must retain its original error precedence"
    );
}

#[test]
fn compact_dispatch_gate_off_refuses_before_database_construction() {
    let image = {
        let _gates = gates();
        source_view_fixture(None).0
    };
    std::thread::spawn(move || {
        let _precision = crate::compact::Activation::on();
        unsupported(
            Database::open_from_bytes(image, None),
            "gate-off Database open",
        );
    })
    .join()
    .unwrap();
}
