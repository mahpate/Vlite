//! Lane B4 regressions for four survivors of lane B's mutation campaign.
//!
//! Every expectation is a literal, or the contract's error order for a damaged
//! block (`docs/projects/roadmap/p3b-seam-freeze.md`, ERR-1 to ERR-8), never a
//! value taken from the code under test. Source and records are test-gated, so
//! nothing here touches a release build.

use super::*;
use crate::records::CollectionRecords;
use crate::rerank::{ExactReranker, rerank_effective};
use crate::text_index::TextIndex;
use crate::{
    CollectionSettings, DistanceMetric, EncryptionKey, Metadata, MetadataValue, SearchHit,
};

const CHANGED: &str = "compact source frame changed";

fn corrupt_reason<T>(result: Result<T>) -> Option<&'static str> {
    match result {
        Err(Error::Corrupt { reason, .. }) => Some(reason),
        _ => None,
    }
}

/// Kills `x-rec-vector-error-swallowed`: the effective accessors, and the
/// rerank that sits on them, report a damaged block as `Corrupt` for every
/// read, never as an absent row.
#[test]
fn compact_effective_reads_of_a_damaged_block_are_errors_never_absence() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    for (how, encrypted, reason) in [
        (BlockTamper::Kind, false, CHANGED),
        (BlockTamper::Checksum, false, CHANGED),
        (BlockTamper::Kind, true, CHANGED),
        (BlockTamper::Checksum, true, CHANGED),
        (BlockTamper::Ciphertext, true, "frame authentication failed"),
    ] {
        let key = encrypted.then(source_key);
        let (_, mut loaded) = source_view_fixture(key.as_ref());
        let mut generation = loaded.compact_generation.take().unwrap();
        drop(loaded);
        tamper_first_block(&mut generation, how);
        let view = first_view(&generation);
        let id = view.id(0).unwrap();
        let rows = CollectionRecords::Compact {
            view,
            overlay: Default::default(),
        };
        let mut workspace = ReadWorkspace::new(4 << 20).unwrap();
        assert_eq!(
            corrupt_reason(rows.effective_vector(id, &mut workspace)),
            Some(reason),
            "vector (encrypted {encrypted})"
        );
        assert_eq!(
            corrupt_reason(rows.effective_metadata(id, &mut workspace)),
            Some(reason),
            "metadata (encrypted {encrypted})"
        );
        assert_eq!(
            corrupt_reason(rows.effective_payload(id, &mut workspace)),
            Some(reason),
            "payload (encrypted {encrypted})"
        );
        let hits = [SearchHit { id, distance: 0.0 }];
        let reranker = ExactReranker {
            metric: DistanceMetric::SquaredL2,
        };
        assert_eq!(
            corrupt_reason(rerank_effective(
                &rows,
                &Default::default(),
                0,
                &[0.0, 0.0],
                &hits,
                1,
                &reranker
            )),
            Some(reason),
            "rerank (encrypted {encrypted})"
        );
        let accounting = rows.source_accounting(&mut workspace).unwrap().unwrap();
        assert_eq!(
            (
                accounting.vectors_live,
                accounting.plaintext_live,
                accounting.cells_live
            ),
            (0, 0, 0),
            "a refused read leaves the workspace unchanged"
        );
    }
}

/// Kills `x-seam-frame-kind-check-removed` (both the bounded body read and the
/// record-cell decode): a frame whose recorded kind changed is refused on every
/// read path, and before the authentication that a damaged ciphertext would fail.
#[test]
fn compact_wrong_block_kind_is_refused_before_authentication_on_every_read_path() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (_, mut loaded) = source_view_fixture(key.as_ref());
        let mut generation = loaded.compact_generation.take().unwrap();
        drop(loaded);
        tamper_first_block(&mut generation, BlockTamper::Kind);
        if encrypted {
            // The ciphertext is damaged as well: the cheap kind check comes first.
            tamper_first_block(&mut generation, BlockTamper::Ciphertext);
        }
        let view = first_view(&generation);
        let id = view.id(0).unwrap();
        let mut workspace = ReadWorkspace::new(4 << 20).unwrap();
        assert_eq!(
            corrupt_reason(view.read_vector(0, &mut workspace)),
            Some(CHANGED),
            "vector (encrypted {encrypted})"
        );
        assert_eq!(
            corrupt_reason(view.read_metadata(0, &mut workspace)),
            Some(CHANGED),
            "metadata (encrypted {encrypted})"
        );
        assert_eq!(
            corrupt_reason(view.read_payload(0, &mut workspace)),
            Some(CHANGED),
            "payload (encrypted {encrypted})"
        );
        assert_eq!(
            corrupt_reason(view.get_physical(id)),
            Some(CHANGED),
            "record cell (encrypted {encrypted})"
        );
        let accounting = workspace.accounting(&view).unwrap();
        assert_eq!(
            (
                accounting.vectors_live,
                accounting.plaintext_live,
                accounting.cells_live
            ),
            (0, 0, 0),
            "a refused read leaves the workspace unchanged"
        );
    }
}

/// Pins the first gate of the compact settings path: a poisoned handle answers
/// `Poisoned` before validation, before the read-only gate and before any scan of
/// the base. The arm's own `ensure_healthy` repeats the check that `resolve` makes
/// first, so deleting that one call is not observable (`x-settings-health-removed`
/// is an equivalent mutant, see its record in `tests/compact_b4_mutants.json`);
/// making the health check inert (`x-health-gate-inert`) is, and this test kills it.
#[test]
fn compact_settings_check_the_handle_health_before_every_other_gate() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    let mut generation = generation();
    // A scan of this base fails, so a read that precedes the health check shows.
    Arc::get_mut(&mut generation)
        .unwrap()
        .collections
        .values_mut()
        .next()
        .unwrap()
        .blocks[0]
        .source
        .rows[0]
        .metadata
        .end = usize::MAX;
    let view = first_view(&generation);
    let mut db = crate::Database::create_in_memory(2, Default::default()).unwrap();
    db.state_mut(0).records = CollectionRecords::Compact {
        view,
        overlay: Default::default(),
    };
    let sequence = db.sequence;
    let valid = CollectionSettings {
        text_fields: vec!["tag".into()],
        ..Default::default()
    };
    let invalid = CollectionSettings {
        rerank_multiplier: 0,
        ..Default::default()
    };
    db.poisoned = true;
    for read_only in [false, true] {
        db.read_only = read_only;
        for settings in [&valid, &invalid] {
            assert!(
                matches!(
                    db.set_collection_settings("default", settings.clone()),
                    Err(Error::Poisoned)
                ),
                "read_only {read_only}"
            );
        }
    }
    assert_eq!(db.sequence, sequence);
    assert_eq!(db.default_state().settings, CollectionSettings::default());
    // Control: a healthy, writable handle does reach the damaged base.
    db.poisoned = false;
    db.read_only = false;
    assert!(matches!(
        db.set_collection_settings("default", valid),
        Err(Error::Corrupt { .. })
    ));
}

/// Collection 0 as a float32 compact image whose rows 1, 2 and 3 carry the
/// single-token `body` texts "a", "b" and "c" (one posting each).
fn body_fixture(key: Option<&EncryptionKey>) -> crate::format::Loaded {
    let mut db = crate::Database::create_in_memory(
        2,
        crate::CreateOptions {
            encryption: key.cloned(),
            ..crate::CreateOptions::default()
        },
    )
    .unwrap();
    for (id, body) in [(1, "a"), (2, "b"), (3, "c")] {
        let metadata =
            Metadata::from([("body".to_owned(), MetadataValue::String(body.to_owned()))]);
        db.put_with_metadata(id, &[id as f32, 0.5], b"p", &metadata)
            .unwrap();
    }
    let mut loaded =
        format::load_with(&mut std::io::Cursor::new(db.to_bytes().unwrap()), key).unwrap();
    loaded.flags |= format::FLAG_PRECISION;
    loaded.collections.get_mut(&0).unwrap().precision = Some(Precision::Float32);
    let mut image = Vec::new();
    crate::maintenance::write_v5_image(
        &mut image,
        &loaded,
        loaded.lineage.unwrap(),
        None,
        None,
        loaded.cipher.as_ref(),
        loaded.flags,
        loaded.sequence,
    )
    .unwrap();
    format::load_with(&mut std::io::Cursor::new(image), key).unwrap()
}

/// Kills `x-text-break-on-unavailable-removed`. The rebuilt text index and the
/// resident one agree on every answer, so only the work differs: once a posting
/// limit marks the index unavailable the scan must stop reading rows. The third
/// row's metadata extent is damaged, so a read of it is visible as an error.
#[test]
fn compact_text_rebuild_stops_reading_rows_once_the_index_is_unavailable() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let mut loaded = body_fixture(key.as_ref());
        let mut generation = loaded.compact_generation.take().unwrap();
        drop(loaded);
        Arc::get_mut(&mut generation)
            .unwrap()
            .collections
            .get_mut(&0)
            .unwrap()
            .blocks[0]
            .source
            .rows[2]
            .metadata
            .end = usize::MAX;
        let rows = CollectionRecords::Compact {
            view: generation.view(0),
            overlay: Default::default(),
        };
        let fields = ["body".to_owned()];
        let mut workspace = ReadWorkspace::new(4 << 20).unwrap();
        // One posting fits, so the second row trips the limit; row 3 is never read.
        let rebuilt = TextIndex::with_posting_limit(1)
            .rebuilt_effective(&fields, &rows, &mut workspace)
            .unwrap();
        let status = rebuilt.status();
        assert!(!status.available, "encrypted {encrypted}");
        assert_eq!((status.postings, status.documents), (0, 0));
        // Control: with room for every posting the scan reaches the damaged row.
        assert_eq!(
            corrupt_reason(TextIndex::with_posting_limit(3).rebuilt_effective(
                &fields,
                &rows,
                &mut workspace
            )),
            Some("compact metadata extent is out of bounds"),
            "encrypted {encrypted}"
        );
    }
}
