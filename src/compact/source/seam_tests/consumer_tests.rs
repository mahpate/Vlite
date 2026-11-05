//! Lane B4: the compact settings and rerank consumers against the resident path.
//!
//! The oracle is the same image opened with the source gate off, so an expectation
//! is what the resident code answers, or a literal from the contract: a settings
//! change on a v5 file is persisted by one synced frame and consumes one sequence
//! number, and the caller's limit reaches a reranking provider unchanged. Source
//! and records are test-gated, so nothing here touches a release build.

use crate::compact::source::dispatch_tests::{compact_dispatch_fixture, gates};
use crate::rerank::{ExactReranker, RerankCandidate, Reranker};
use crate::{
    CollectionSettings, Condition, DistanceMetric, Metadata, MetadataIndexKind, MetadataIndexSpec,
    MetadataValue, SearchHit, SettingsPersistence,
};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the compact settings arm reports and records for a named collection is
/// what the resident arm reports and records: the persistence it returns, the
/// one sequence number it consumes, the collection description (the settings and
/// the persisted flag included) and the text index it derives.
#[test]
fn compact_settings_report_and_record_what_the_resident_path_does() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (mut candidate, mut oracle, _) = compact_dispatch_fixture(encrypted);
        let settings = CollectionSettings {
            metadata_indexes: vec![MetadataIndexSpec {
                field: "id".into(),
                kind: MetadataIndexKind::Eq,
            }],
            text_fields: vec!["id".into()],
            ..Default::default()
        };
        for name in ["half", "full"] {
            let sequence = candidate.sequence;
            assert_eq!(sequence, oracle.sequence, "{name}: same start");
            let reported = candidate
                .set_collection_settings(name, settings.clone())
                .unwrap();
            let expected = oracle
                .set_collection_settings(name, settings.clone())
                .unwrap();
            assert_eq!(reported, expected, "{name}: persistence");
            assert_eq!(reported, SettingsPersistence::Persisted, "{name}");
            assert_eq!(
                candidate.sequence,
                sequence + 1,
                "{name}: one settings frame"
            );
            assert_eq!(oracle.sequence, sequence + 1, "{name}: one settings frame");
            let info = |db: &crate::Database| {
                db.collections()
                    .unwrap()
                    .into_iter()
                    .find(|info| info.name == name)
                    .unwrap()
            };
            assert_eq!(info(&candidate), info(&oracle), "{name}: description");
            assert!(info(&candidate).settings_persisted, "{name}");
            assert_eq!(info(&candidate).settings, settings, "{name}");
            let status = |db: &crate::Database| db.collection(name).unwrap().text_index_status();
            assert_eq!(status(&candidate), status(&oracle), "{name}: text index");
            assert!(status(&candidate).available && status(&candidate).documents > 0);
        }
    }
}

/// After a delete, a metadata update and a put on top of the base, a settings
/// change rebuilds both derived indexes from the effective rows. Every answer
/// they give equals the answer of the resident indexes of the oracle, for every
/// value the oracle holds and for one it does not. The values to look up come from
/// the oracle's records, never from a list in this file.
#[test]
fn compact_rebuilt_indexes_match_the_resident_oracle_after_overlay_changes() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (mut candidate, mut oracle, _) = compact_dispatch_fixture(encrypted);
        let settings = CollectionSettings {
            metadata_indexes: vec![MetadataIndexSpec {
                field: "id".into(),
                kind: MetadataIndexKind::Eq,
            }],
            text_fields: vec!["id".into()],
            ..Default::default()
        };
        let text = |value: &str| {
            Metadata::from([("id".to_owned(), MetadataValue::String(value.to_owned()))])
        };
        for db in [&mut candidate, &mut oracle] {
            {
                let mut full = db.collection_mut("full").unwrap();
                assert!(full.delete(3).unwrap());
                full.update_metadata(10, &text("ten renamed")).unwrap();
                full.put_with_metadata(30, &[0.5, 0.5], b"thirty", &text("thirty"))
                    .unwrap();
            }
            db.set_collection_settings("full", settings.clone())
                .unwrap();
        }
        let id_of = |db: &crate::Database| db.collection("full").unwrap().id();
        let (candidate_id, oracle_id) = (id_of(&candidate), id_of(&oracle));
        let mut values: BTreeSet<String> = oracle
            .state(oracle_id)
            .records
            .resident()
            .unwrap()
            .values()
            .filter_map(|record| match record.metadata.get("id") {
                Some(MetadataValue::String(value)) => Some(value.clone()),
                _ => None,
            })
            .collect();
        assert!(
            values.contains("ten renamed") && values.contains("thirty") && !values.contains("3"),
            "the overlay changes are in the oracle: {values:?}"
        );
        values.insert("nowhere".into());
        for value in &values {
            let filter = [Condition::Eq {
                field: "id".into(),
                value: MetadataValue::String(value.clone()),
            }];
            let lookup = |db: &crate::Database, id| {
                db.state(id)
                    .metadata_indexes
                    .lookup(&filter, 100)
                    .map(|found| (found.estimate, found.field, found.ids))
            };
            assert_eq!(
                lookup(&candidate, candidate_id),
                lookup(&oracle, oracle_id),
                "metadata index: {value:?}"
            );
            let terms: BTreeSet<String> = crate::text_index::tokenize(value).into_iter().collect();
            let search =
                |db: &crate::Database, id| db.state(id).text_index.search(&terms, 10, |_| true);
            assert_eq!(
                search(&candidate, candidate_id),
                search(&oracle, oracle_id),
                "text index: {value:?}"
            );
        }
        let status = |db: &crate::Database, id| db.state(id).text_index.status();
        assert_eq!(
            status(&candidate, candidate_id),
            status(&oracle, oracle_id),
            "text index status"
        );
    }
}

struct Probe {
    candidates: AtomicUsize,
    limit: AtomicUsize,
}

impl Reranker for Probe {
    fn rerank(
        &self,
        _query: &[f32],
        candidates: &[RerankCandidate],
        limit: usize,
    ) -> crate::Result<Vec<SearchHit>> {
        self.candidates.store(candidates.len(), Ordering::SeqCst);
        self.limit.store(limit, Ordering::SeqCst);
        Ok(Vec::new())
    }
}

/// A compact rerank hands the provider the candidates the resident rerank hands
/// it and the caller's limit unchanged, and a binding limit cuts the real
/// provider's answer exactly where it cuts the resident one.
#[test]
fn compact_rerank_hands_the_provider_what_the_resident_path_does() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (candidate, oracle, _) = compact_dispatch_fixture(encrypted);
        // Rows 0, 3, 10, 20 and u64::MAX exist in both collections; 7 does not.
        let hits: Vec<SearchHit> = [0, 3, 7, 10, 20, u64::MAX]
            .into_iter()
            .map(|id| SearchHit { id, distance: 0.5 })
            .collect();
        for name in ["half", "full"] {
            let probed = [&candidate, &oracle].map(|db| {
                let probe = Probe {
                    candidates: AtomicUsize::new(usize::MAX),
                    limit: AtomicUsize::new(usize::MAX),
                };
                db.collection(name)
                    .unwrap()
                    .rerank(&[0.0, 0.0], &hits, 2, &probe)
                    .unwrap();
                (
                    probe.candidates.load(Ordering::SeqCst),
                    probe.limit.load(Ordering::SeqCst),
                )
            });
            assert_eq!(probed[0], probed[1], "{name}: what the provider receives");
            assert_eq!(probed[0], (5, 2), "{name}: five rows exist; the limit is 2");
            let exact = ExactReranker {
                metric: DistanceMetric::SquaredL2,
            };
            let actual = candidate
                .collection(name)
                .unwrap()
                .rerank(&[0.0, 0.0], &hits, 2, &exact)
                .unwrap();
            let expected = oracle
                .collection(name)
                .unwrap()
                .rerank(&[0.0, 0.0], &hits, 2, &exact)
                .unwrap();
            assert_eq!(actual, expected, "{name}: the cut");
            assert_eq!(actual.len(), 2, "{name}");
        }
    }
}
