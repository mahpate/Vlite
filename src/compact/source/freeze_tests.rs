//! Seam-freeze regressions (0.7.1 R0).
//!
//! Each test pins one statement of `docs/projects/roadmap/p3b-seam-freeze.md`.
//! Expectations are literals, or come from the resident records of the same
//! image, never from the code under test. Source and records are test-gated, so
//! nothing here touches a release build.

use super::dispatch_tests::{compact_dispatch_fixture, gates};
use super::*;
use crate::compact::catalog::payload::{KIND, MAX_BODY};
use crate::compact::catalog::tests::{source_default_fixture, source_key, source_view_fixture};
use crate::records::{CollectionRecords, RecordMap};
use crate::{CollectionSpec, EncryptionKey, Metadata, MetadataValue};
use std::collections::BTreeMap;
use std::io::Cursor;

fn tagged(tag: &str) -> Metadata {
    Metadata::from([("tag".into(), MetadataValue::String(tag.into()))])
}

/// Collection 0 as a float32 compact image holding exactly `rows` (ID, vector
/// start, tag, deadline), loaded back with its resident maps as the oracle.
fn loaded_with(
    rows: &[(u64, f32, &str, Option<u64>)],
    key: Option<&EncryptionKey>,
) -> crate::format::Loaded {
    let mut db = crate::Database::create_in_memory(
        2,
        crate::CreateOptions {
            encryption: key.cloned(),
            ..crate::CreateOptions::default()
        },
    )
    .unwrap();
    for &(id, start, tag, expiry) in rows {
        db.put_with_expiry(
            id,
            &[start, start + 0.5],
            format!("payload {id}").as_bytes(),
            &tagged(tag),
            expiry,
        )
        .unwrap();
    }
    compact_image(db, key)
}

fn compact_image(db: crate::Database, key: Option<&EncryptionKey>) -> crate::format::Loaded {
    let mut loaded =
        crate::format::load_with(&mut Cursor::new(db.to_bytes().unwrap()), key).unwrap();
    loaded.flags |= crate::format::FLAG_PRECISION;
    for collection in loaded.collections.values_mut() {
        collection.precision = Some(Precision::Float32);
    }
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
    crate::format::load_with(&mut Cursor::new(image), key).unwrap()
}

fn replayed(loaded: &crate::format::Loaded) -> CollectionRecords {
    let expected = &loaded.collections[&0];
    CollectionRecords::compact_from_replay(
        loaded.compact_generation.as_ref().unwrap().view(0),
        &expected.records,
        &expected.revisions,
        &expected.expiries,
        loaded.sequence,
    )
    .unwrap()
}

fn bits(vector: &[f32]) -> Vec<u32> {
    vector.iter().map(|value| value.to_bits()).collect()
}

const ROWS: [(u64, f32, &str, Option<u64>); 4] = [
    (1, 1.0, "one", None),
    (2, 2.0, "two", Some(100)),
    (3, 3.0, "three", None),
    (4, 4.0, "four", Some(1_000)),
];

fn is_corrupt<T>(result: Result<T>, reason: &str) -> bool {
    matches!(result, Err(Error::Corrupt { reason: found, .. }) if found == reason)
}

#[test]
fn freeze_deadline_hides_exactly_at_the_clock_in_every_arm() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let loaded = loaded_with(&ROWS, key.as_ref());
        let view = loaded.compact_generation.as_ref().unwrap().view(0);
        for (id, now, visible) in [
            (2, 0, true),
            (2, 99, true),
            (2, 100, false),
            (2, 101, false),
            (2, u64::MAX, false),
            (1, u64::MAX, true),
            (404, 0, false),
        ] {
            assert_eq!(
                view.get_at(id, now).unwrap().is_some(),
                visible,
                "source get_at({id}, {now})"
            );
        }
        assert!(
            view.get_physical(2).unwrap().is_some(),
            "physical reads never apply a deadline"
        );

        let expected = &loaded.collections[&0];
        let mut rows = replayed(&loaded);
        let sequence = loaded.sequence;
        let mut replacement = expected.records[&1].clone();
        replacement.payload = b"overlay".to_vec();
        rows.apply_committed(1, Some(replacement.clone()), sequence + 1, Some(200))
            .unwrap();
        rows.apply_committed(9, Some(replacement), sequence + 2, Some(300))
            .unwrap();
        rows.apply_committed(3, None, sequence + 3, None).unwrap();
        // The records arm of both readers must not consult the resident map.
        let unused = BTreeMap::new();
        for (id, now, visible) in [
            (2, 99, true),
            (2, 100, false),
            (1, 0, true),
            (1, 199, true),
            (1, 200, false),
            (9, 299, true),
            (9, 300, false),
            (3, 0, false),
            (3, u64::MAX, false),
            (4, 999, true),
            (4, 1_000, false),
            (404, 0, false),
        ] {
            assert_eq!(
                rows.effective_visible(id, now, &unused),
                visible,
                "effective_visible({id}, {now})"
            );
            assert_eq!(
                rows.get_at(id, now, &unused).unwrap().is_some(),
                visible,
                "get_at({id}, {now})"
            );
        }

        let resident = CollectionRecords::Resident(expected.records.clone());
        for (id, now, visible) in [
            (2, 99, true),
            (2, 100, false),
            (4, 1_000, false),
            (1, u64::MAX, true),
            (404, 0, false),
        ] {
            assert_eq!(
                resident.effective_visible(id, now, &expected.expiries),
                visible,
                "resident effective_visible({id}, {now})"
            );
            assert_eq!(
                resident
                    .get_at(id, now, &expected.expiries)
                    .unwrap()
                    .is_some(),
                visible,
                "resident get_at({id}, {now})"
            );
        }
    }
}

#[test]
fn freeze_effective_rows_follow_overlay_precedence_in_ascending_order() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let loaded = loaded_with(&ROWS, key.as_ref());
        let expected = &loaded.collections[&0];
        let mut rows = replayed(&loaded);
        let sequence = loaded.sequence;
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();

        let mut replacement = expected.records[&1].clone();
        replacement.vector = vec![7.0, -9.0];
        replacement.payload = b"overlay body".to_vec();
        replacement.metadata = tagged("overlay");
        rows.apply_committed(1, Some(replacement.clone()), sequence + 1, None)
            .unwrap();
        rows.apply_committed(9, Some(replacement.clone()), sequence + 2, None)
            .unwrap();
        rows.apply_committed(2, None, sequence + 3, None).unwrap();

        assert_eq!(rows.effective_ids(), [1, 3, 4, 9]);
        for id in [1, 9] {
            assert_eq!(
                bits(rows.effective_vector(id, &mut workspace).unwrap().unwrap()),
                bits(&replacement.vector)
            );
            assert_eq!(
                rows.effective_payload(id, &mut workspace)
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                replacement.payload
            );
            assert_eq!(
                rows.effective_metadata(id, &mut workspace)
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                &replacement.metadata
            );
        }
        for id in [3, 4] {
            let base = &expected.records[&id];
            assert_eq!(
                bits(rows.effective_vector(id, &mut workspace).unwrap().unwrap()),
                bits(&base.vector)
            );
            assert_eq!(
                rows.effective_payload(id, &mut workspace)
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                base.payload
            );
            assert_eq!(
                rows.effective_metadata(id, &mut workspace)
                    .unwrap()
                    .unwrap()
                    .as_ref(),
                &base.metadata
            );
        }
        assert!(rows.effective_vector(2, &mut workspace).unwrap().is_none());
        assert!(rows.effective_payload(2, &mut workspace).unwrap().is_none());
        assert!(
            rows.effective_metadata(2, &mut workspace)
                .unwrap()
                .is_none()
        );
        assert!(
            rows.effective_vector(404, &mut workspace)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            rows.overlay_logical_bytes().unwrap(),
            2 * crate::format::record_body_len(&replacement),
            "a put counts its record body and a tombstone counts nothing"
        );
        assert_eq!(
            workspace
                .accounting(&loaded.compact_generation.as_ref().unwrap().view(0))
                .unwrap()
                .cells_live,
            0,
            "effective reads never materialize record cells"
        );
    }
}

#[test]
fn freeze_replay_overlay_covers_every_kind_of_change() {
    let _gates = gates();
    let rows: Vec<_> = (1..=6)
        .map(|id| (id, id as f32, "base", None::<u64>))
        .collect();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let loaded = loaded_with(&rows, key.as_ref());
        let expected = &loaded.collections[&0];
        let sequence = loaded.sequence;
        let mut records: RecordMap = expected.records.clone();
        let mut revisions = expected.revisions.clone();
        let mut expiries = expected.expiries.clone();

        // 1: metadata-only write (new revision, same deadline).
        records.get_mut(&1).unwrap().metadata = tagged("changed");
        revisions.insert(1, sequence + 1);
        // 2: deadline-only change, same revision.
        expiries.insert(2, 777);
        // 3: deleted after the image.
        records.remove(&3);
        revisions.remove(&3);
        // 4: untouched.
        // 5: vector and payload rewrite.
        let rewritten = records.get_mut(&5).unwrap();
        rewritten.vector = vec![50.0, 51.0];
        rewritten.payload = b"rewritten".to_vec();
        revisions.insert(5, sequence + 2);
        // 7: new ID.
        let mut created = expected.records[&6].clone();
        created.payload = b"created".to_vec();
        records.insert(7, created.clone());
        revisions.insert(7, sequence + 3);
        // 6: deleted and written again with a new revision.
        revisions.insert(6, sequence + 4);

        let overlay = CollectionRecords::compact_from_replay(
            loaded.compact_generation.as_ref().unwrap().view(0),
            &records,
            &revisions,
            &expiries,
            sequence,
        )
        .unwrap();
        for (id, entry) in [
            (1, Some((sequence + 1, false))),
            (2, Some((expected.revisions[&2], false))),
            (3, Some((sequence, true))),
            (4, None),
            (5, Some((sequence + 2, false))),
            (6, Some((sequence + 4, false))),
            (7, Some((sequence + 3, false))),
        ] {
            assert_eq!(overlay.compact_overlay_revision(id), entry, "ID {id}");
        }
        assert_eq!(overlay.effective_ids(), [1, 2, 4, 5, 6, 7]);
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        assert_eq!(
            overlay
                .effective_metadata(1, &mut workspace)
                .unwrap()
                .unwrap()
                .as_ref(),
            &tagged("changed")
        );
        assert_eq!(
            overlay
                .effective_payload(7, &mut workspace)
                .unwrap()
                .unwrap()
                .as_ref(),
            created.payload
        );
        assert_eq!(
            bits(
                overlay
                    .effective_vector(5, &mut workspace)
                    .unwrap()
                    .unwrap()
            ),
            bits(&[50.0, 51.0])
        );
        assert!(
            overlay
                .effective_vector(3, &mut workspace)
                .unwrap()
                .is_none()
        );
        // The untouched base row is served from the pinned source, not the overlay.
        assert_eq!(
            bits(
                overlay
                    .effective_vector(4, &mut workspace)
                    .unwrap()
                    .unwrap()
            ),
            bits(&expected.records[&4].vector)
        );
    }
}

enum Tamper {
    Checksum,
    Kind,
    Ciphertext,
}

fn tamper_in_place(inner: &mut CompactGeneration, collection: u16, how: &Tamper) {
    let block = &mut inner.collections.get_mut(&collection).unwrap().blocks[0];
    match how {
        Tamper::Checksum => block.checksum ^= 1,
        Tamper::Kind => block.kind ^= 1,
        Tamper::Ciphertext => {
            let mut bytes = inner.backing.as_slice().to_vec();
            bytes[block.frame.end - FOOTER_SIZE - 1] ^= 0x80;
            let checksum = format::crc32(&bytes[block.frame.start..block.frame.end - FOOTER_SIZE]);
            bytes[block.frame.end - FOOTER_SIZE + 4..block.frame.end - FOOTER_SIZE + 8]
                .copy_from_slice(&checksum.to_le_bytes());
            block.checksum = checksum;
            inner.backing = Backing::Owned(Arc::from(bytes));
        }
    }
}

fn tampered(loaded: &mut crate::format::Loaded, how: &Tamper) -> Arc<CompactGeneration> {
    let mut generation = loaded.compact_generation.take().unwrap();
    let inner = Arc::get_mut(&mut generation).expect("the fixture owns its only handle");
    tamper_in_place(inner, 0, how);
    generation
}

#[test]
fn freeze_frame_tampering_is_an_error_on_every_read_path() {
    let _gates = gates();
    for (how, encrypted, reason) in [
        (Tamper::Checksum, false, "compact source frame changed"),
        (Tamper::Checksum, true, "compact source frame changed"),
        (Tamper::Kind, false, "compact source frame changed"),
        (Tamper::Kind, true, "compact source frame changed"),
        (Tamper::Ciphertext, true, "frame authentication failed"),
    ] {
        let key = encrypted.then(source_key);
        let mut loaded = loaded_with(&ROWS, key.as_ref());
        let generation = tampered(&mut loaded, &how);
        let view = generation.view(0);
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        assert!(
            is_corrupt(view.read_vector(0, &mut workspace), reason),
            "read_vector: {reason} (encrypted {encrypted})"
        );
        assert!(
            is_corrupt(view.read_metadata(0, &mut workspace), reason),
            "read_metadata: {reason}"
        );
        assert!(
            is_corrupt(view.read_payload(0, &mut workspace), reason),
            "read_payload: {reason}"
        );
        assert!(
            is_corrupt(view.get_physical(1), reason),
            "record cell: {reason}"
        );
        assert_eq!(workspace.accounting(&view).unwrap().plaintext_live, 0);
        assert_eq!(workspace.accounting(&view).unwrap().vectors_live, 0);
        assert_eq!(workspace.accounting(&view).unwrap().cells_live, 0);

        // The same failure reaches the effective accessors of a base row; it is
        // never reported as an absent row.
        let expected = &loaded.collections[&0];
        let rows = CollectionRecords::compact_from_replay(
            view,
            &expected.records,
            &expected.revisions,
            &expected.expiries,
            loaded.sequence,
        )
        .unwrap();
        assert!(is_corrupt(rows.effective_vector(1, &mut workspace), reason));
        assert!(is_corrupt(
            rows.effective_metadata(1, &mut workspace),
            reason
        ));
        assert!(is_corrupt(
            rows.effective_payload(1, &mut workspace),
            reason
        ));
    }
}

#[test]
fn freeze_extents_outside_the_body_are_corrupt_on_every_read_path() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let extent = |select: fn(&mut crate::compact::catalog::payload::SourceRow) -> &mut _| {
            let mut loaded = loaded_with(&ROWS, key.as_ref());
            let mut generation = loaded.compact_generation.take().unwrap();
            let inner = Arc::get_mut(&mut generation).unwrap();
            let row = &mut inner.collections.get_mut(&0).unwrap().blocks[0].source.rows[0];
            let range: &mut std::ops::Range<usize> = select(row);
            range.end = usize::MAX;
            generation
        };
        let generation = extent(|row| &mut row.payload);
        let view = generation.view(0);
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        assert!(is_corrupt(
            view.read_payload(0, &mut workspace),
            "compact payload extent is out of bounds"
        ));
        assert!(is_corrupt(
            view.get_physical(view.id(0).unwrap()),
            "compact payload extent is out of bounds"
        ));
        let generation = extent(|row| &mut row.metadata);
        let view = generation.view(0);
        assert!(is_corrupt(
            view.read_metadata(0, &mut workspace),
            "compact metadata extent is out of bounds"
        ));
        assert!(is_corrupt(
            view.get_physical(view.id(0).unwrap()),
            "compact metadata extent is out of bounds"
        ));
        let generation = extent(|row| &mut row.vector);
        let view = generation.view(0);
        assert!(is_corrupt(
            view.read_vector(0, &mut workspace),
            "invalid compact vector extent"
        ));
        assert!(is_corrupt(
            view.get_physical(view.id(0).unwrap()),
            "compact vector extent is out of bounds"
        ));
        assert_eq!(
            workspace.accounting(&view).unwrap().vectors_live,
            0,
            "a refused block is never accounted"
        );
    }
}

#[test]
fn freeze_cached_blocks_are_authenticated_again_and_shape_checked() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let mut loaded = loaded_with(&ROWS, key.as_ref());
        let mut generation = loaded.compact_generation.take().unwrap();
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        let view = generation.view(0);
        let first = view
            .read_vector(0, &mut workspace)
            .unwrap()
            .unwrap()
            .to_vec();
        assert_eq!(workspace.accounting(&view).unwrap().misses, 1);
        assert_eq!(
            view.read_vector(1, &mut workspace).unwrap().unwrap(),
            [2.0, 2.5],
            "a hit serves the cached block"
        );
        assert_eq!(workspace.accounting(&view).unwrap().hits, 1);

        // Injected cache entries with a wrong shape are refused, never served.
        let key_of = *workspace.vectors.keys().next().unwrap();
        for corrupt in [
            |block: &mut VectorBlock| {
                block.values.pop();
            },
            |block: &mut VectorBlock| block.dimensions += 1,
            |block: &mut VectorBlock| block.precision = Precision::Float16,
        ] {
            let mut poisoned = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
            view.read_vector(0, &mut poisoned).unwrap();
            corrupt(poisoned.vectors.get_mut(&key_of).unwrap());
            assert!(matches!(
                view.read_vector(0, &mut poisoned),
                Err(Error::InvalidQuery("compact cached block shape changed"))
            ));
        }
        assert_eq!(bits(&first), bits(&[1.0, 1.5]));

        // A cache hit still touches the current frame: tampering after the
        // first read is detected on the next one.
        drop(view);
        tamper_in_place(Arc::get_mut(&mut generation).unwrap(), 0, &Tamper::Checksum);
        let view = generation.view(0);
        assert!(is_corrupt(
            view.read_vector(0, &mut workspace),
            "compact source frame changed"
        ));

        // An encrypted block is authenticated again on a hit: a flipped
        // ciphertext bit under a consistent footer is caught by the AEAD.
        if encrypted {
            let mut again = loaded_with(&ROWS, key.as_ref());
            let mut generation = again.compact_generation.take().unwrap();
            let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
            let view = generation.view(0);
            view.read_vector(0, &mut workspace).unwrap();
            drop(view);
            tamper_in_place(
                Arc::get_mut(&mut generation).unwrap(),
                0,
                &Tamper::Ciphertext,
            );
            let view = generation.view(0);
            assert!(is_corrupt(
                view.read_vector(0, &mut workspace),
                "frame authentication failed"
            ));
            assert_eq!(workspace.accounting(&view).unwrap().plaintext_live, 0);
        }
    }
}

#[test]
fn freeze_a_refused_read_leaves_the_rest_of_the_workspace_as_it_was() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let mut db = crate::Database::create_in_memory(
            2,
            crate::CreateOptions {
                encryption: key.clone(),
                ..crate::CreateOptions::default()
            },
        )
        .unwrap();
        for (name, base) in [("a", 1.0), ("b", 2.0)] {
            db.create_collection(
                name,
                CollectionSpec {
                    dimensions: 2,
                    settings: crate::CollectionSettings::default(),
                },
            )
            .unwrap();
            db.collection_mut(name)
                .unwrap()
                .put(1, &[base, base], name.as_bytes())
                .unwrap();
        }
        let mut loaded = compact_image(db, key.as_ref());
        let mut generation = loaded.compact_generation.take().unwrap();
        let collection = |name: &str| {
            *loaded
                .collections
                .iter()
                .find(|(_, state)| state.name == name)
                .unwrap()
                .0
        };
        let (a, b) = (collection("a"), collection("b"));
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        let view = generation.view(a);
        assert_eq!(
            view.read_vector(0, &mut workspace).unwrap().unwrap(),
            [1.0, 1.0]
        );
        let cached = workspace.accounting(&view).unwrap();
        assert!(cached.vectors_live > 0);
        drop(view);

        // The block of `b` fails while its frame is read.
        let (how, reason) = if encrypted {
            (Tamper::Ciphertext, "frame authentication failed")
        } else {
            (Tamper::Checksum, "compact source frame changed")
        };
        tamper_in_place(Arc::get_mut(&mut generation).unwrap(), b, &how);
        let view = generation.view(b);
        assert!(is_corrupt(view.read_vector(0, &mut workspace), reason));
        let refused = workspace.accounting(&view).unwrap();
        assert_eq!(
            workspace.vectors.len(),
            1,
            "the refused block is never cached and the cached one stays"
        );
        assert_eq!(refused.vectors_live, cached.vectors_live);
        assert_eq!(refused.evictions, 0);
        assert_eq!(refused.cells_live, 0, "a refused read publishes no cell");
        assert_eq!(
            refused.plaintext_live, 0,
            "a read that fails while reading its frame leaves no plaintext"
        );
        assert!(is_corrupt(view.get_physical(1), reason));
        assert_eq!(workspace.accounting(&view).unwrap().cells_live, 0);
        let again = generation.view(a);
        assert_eq!(
            again.read_vector(0, &mut workspace).unwrap().unwrap(),
            [1.0, 1.0]
        );
        assert_eq!(workspace.accounting(&again).unwrap().hits, 1);

        // The scratch budget is checked before the frame is read at all.
        let mut tiny = ReadWorkspace::new(0).unwrap();
        assert!(matches!(
            view.read_vector(0, &mut tiny),
            Err(Error::InvalidQuery(
                "compact scratch budget is smaller than one block"
            ))
        ));
        assert_eq!(tiny.accounting(&view).unwrap().misses, 0);
    }
}

#[test]
fn freeze_budget_and_exact_fit_boundaries() {
    let _gates = gates();
    assert!(ReadWorkspace::new(4 * 1024 * 1024).is_ok());
    assert!(matches!(
        ReadWorkspace::new(4 * 1024 * 1024 + 1),
        Err(Error::InvalidQuery("compact workspace exceeds 4 MiB"))
    ));
    let generations: Vec<_> = (0..3)
        .map(|_| {
            let mut loaded = loaded_with(&ROWS, None);
            loaded.compact_generation.take().unwrap()
        })
        .collect();
    let views: Vec<_> = generations
        .iter()
        .map(|generation| generation.view(0))
        .collect();
    let bytes = ROWS.len() * 2 * 4;
    let mut exact = ReadWorkspace::new(bytes).unwrap();
    assert!(views[0].read_vector(0, &mut exact).unwrap().is_some());
    let mut short = ReadWorkspace::new(bytes - 1).unwrap();
    assert!(matches!(
        views[0].read_vector(0, &mut short),
        Err(Error::InvalidQuery(
            "compact scratch budget is smaller than one block"
        ))
    ));
    // Exactly two blocks fit: no eviction until a third needs room.
    let mut pair = ReadWorkspace::new(2 * bytes).unwrap();
    views[0].read_vector(0, &mut pair).unwrap();
    views[1].read_vector(0, &mut pair).unwrap();
    let accounting = pair.accounting(&views[0]).unwrap();
    assert_eq!(accounting.evictions, 0);
    assert_eq!(accounting.vectors_live, 2 * bytes);
    assert_eq!(accounting.vectors_peak, 2 * bytes);
    views[2].read_vector(0, &mut pair).unwrap();
    let accounting = pair.accounting(&views[0]).unwrap();
    assert_eq!((accounting.evictions, accounting.misses), (1, 3));
    assert_eq!(accounting.vectors_live, 2 * bytes);
    // The least recently used block (generation 0) was the one evicted.
    assert!(
        !pair
            .vectors
            .keys()
            .any(|key| key.generation == views[0].generation_id())
    );
}

#[test]
fn freeze_frame_size_bound_is_exact() {
    let _gates = gates();
    let seal = crate::crypto::SEAL_OVERHEAD;
    for (extra, outcome) in [
        (0, None),
        (1, Some("compact source body exceeds limit")),
        (seal, Some("compact source body exceeds limit")),
        (seal + 1, Some("compact source frame changed")),
    ] {
        let mut loaded = loaded_with(&ROWS, None);
        let mut generation = loaded.compact_generation.take().unwrap();
        let inner = Arc::get_mut(&mut generation).unwrap();
        let length = PREFIX_SIZE + MAX_BODY + extra + FOOTER_SIZE;
        inner.backing = Backing::Owned(Arc::from(vec![0u8; length]));
        let block = &mut inner.collections.get_mut(&0).unwrap().blocks[0];
        block.frame = 0..length;
        block.checksum = 0;
        let view = generation.view(0);
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        let result = view.read_payload(0, &mut workspace);
        match outcome {
            None => assert!(result.is_ok(), "a body of exactly MAX_BODY is read"),
            Some(reason) => assert!(
                is_corrupt(result, reason),
                "body {extra} bytes over MAX_BODY must fail with {reason}"
            ),
        }
    }
}

#[test]
fn freeze_accounting_reports_exact_values_and_keeps_peaks() {
    let _gates = gates();
    let (_, mut loaded) = source_view_fixture(None);
    let generation = loaded.compact_generation.take().unwrap();
    let populated: Vec<u16> = loaded
        .collections
        .iter()
        .filter(|&(&id, state)| generation.has_collection(id) && !state.records.is_empty())
        .map(|(&id, _)| id)
        .collect();
    assert!(
        populated.len() >= 2,
        "the fixture has two compact collections"
    );
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    let first = generation.view(populated[0]);
    let idle = workspace.accounting(&first).unwrap();
    assert_eq!(idle.source_live, generation.backing.as_slice().len());
    assert_eq!(idle.source_peak, idle.source_live);
    assert_eq!((idle.mapped_live, idle.mapped_peak), (0, 0));
    assert_eq!((idle.cells_live, idle.cells_peak), (0, 0));

    // Cells are generation-wide and counted by their decoded record body.
    let mut expected_cells = 0u64;
    for &collection in &populated[..2] {
        let view = generation.view(collection);
        let id = view.id(0).unwrap();
        view.get_physical(id).unwrap();
        expected_cells +=
            crate::format::record_body_len(&loaded.collections[&collection].records[&id]);
    }
    let busy = workspace.accounting(&first).unwrap();
    assert_eq!(busy.cells_live as u64, expected_cells);
    assert_eq!(busy.cells_peak, busy.cells_live);

    // Peaks survive a smaller generation.
    let mut smaller = loaded_with(&ROWS, None);
    let other = smaller.compact_generation.take().unwrap();
    assert!(
        other.backing.as_slice().len() < generation.backing.as_slice().len(),
        "the second generation is the smaller one"
    );
    let after = workspace.accounting(&other.view(populated[0])).unwrap();
    assert_eq!(after.cells_live, 0);
    assert_eq!(after.cells_peak, busy.cells_live);
    assert_eq!(after.source_live, other.backing.as_slice().len());
    assert_eq!(after.source_peak, generation.backing.as_slice().len());

    // Physical row observations come straight from the source columns.
    for &collection in &populated {
        let view = generation.view(collection);
        let state = &loaded.collections[&collection];
        assert_eq!(view.len(), state.records.len());
        for row in 0..view.len() {
            let id = view.id(row).unwrap();
            assert_eq!(view.revision(row), Some(state.revisions[&id]));
            assert_eq!(view.expiry(row), Some(state.expiries.get(&id).copied()));
            assert_eq!(view.row_of(id), Some(row));
        }
        assert_eq!(view.revision(view.len()), None);
        assert_eq!(view.expiry(view.len()), None);
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn freeze_mapped_accounting_keeps_its_own_peak() {
    let _gates = gates();
    let (_, mut loaded) = source_view_fixture(None);
    let owned = loaded.compact_generation.take().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mapped = owned.mapped_copy(&dir.path().join("mapped")).unwrap();
    let collection = *owned.collections.keys().next().unwrap();
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    let first = workspace.accounting(&mapped.view(collection)).unwrap();
    assert_eq!(first.mapped_live, mapped.backing.as_slice().len());
    assert_eq!(first.mapped_peak, first.mapped_live);
    let second = workspace.accounting(&owned.view(collection)).unwrap();
    assert_eq!(second.mapped_live, 0);
    assert_eq!(second.mapped_peak, first.mapped_live);
}

#[test]
fn freeze_vector_peak_survives_eviction_by_a_smaller_block() {
    let _gates = gates();
    let mut loaded = loaded_with(&[(0, 0.0, "template", None)], None);
    let template = loaded.collections[&0].records[&0].clone();
    let state = loaded.collections.get_mut(&0).unwrap();
    state.records = RecordMap::default();
    state.revisions.clear();
    state.expiries.clear();
    for id in 0..8_200u64 {
        let mut record = template.clone();
        record.vector = vec![id as f32, 0.0];
        state.records.insert(id, record);
        state.revisions.insert(id, loaded.sequence);
    }
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
    let checked = crate::format::load_with(&mut Cursor::new(image), None).unwrap();
    let generation = checked.compact_generation.unwrap();
    let view = generation.view(0);
    let blocks = &view.source().unwrap().blocks;
    let (big, small) = (blocks.first().unwrap(), blocks.last().unwrap());
    let big_bytes = big.source.rows.len() * 2 * 4;
    let small_bytes = small.source.rows.len() * 2 * 4;
    assert!(small_bytes < big_bytes, "the last block is the smaller one");
    let mut workspace = ReadWorkspace::new(big_bytes).unwrap();
    view.read_vector(0, &mut workspace).unwrap();
    view.read_vector(view.len() - 1, &mut workspace).unwrap();
    let accounting = workspace.accounting(&view).unwrap();
    assert_eq!(accounting.evictions, 1);
    assert_eq!(accounting.vectors_live, small_bytes);
    assert_eq!(accounting.vectors_peak, big_bytes);
}

#[test]
fn freeze_cache_key_separates_block_kinds() {
    let _gates = gates();
    let mut loaded = loaded_with(&ROWS, None);
    let mut generation = loaded.compact_generation.take().unwrap();
    let inner = Arc::get_mut(&mut generation).unwrap();
    let first = inner.collections.remove(&0).unwrap().blocks.remove(0);
    let rows = first.source.rows.len();
    // A second block over the same frame bytes, of another kind in the same
    // partition, whose rows read the first block's vectors rotated by one.
    let mut second = first.clone();
    second.kind = KIND + 1;
    second.source.frame_id = (first.source.frame_id & !0xFFFF) | u64::from(KIND + 1);
    for (index, row) in second.source.rows.iter_mut().enumerate() {
        row.id += 1_000;
        row.vector = first.source.rows[(index + 1) % rows].vector.clone();
    }
    inner.collections.insert(
        0,
        CollectionSource::new(vec![first.clone(), second]).unwrap(),
    );
    let view = generation.view(0);
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    assert_eq!(
        view.read_vector(0, &mut workspace).unwrap().unwrap(),
        [1.0, 1.5]
    );
    assert_eq!(
        view.read_vector(rows, &mut workspace).unwrap().unwrap(),
        [2.0, 2.5],
        "a block of another kind never reads another kind's cache entry"
    );
    assert_eq!(workspace.accounting(&view).unwrap().misses, 2);
    assert_eq!(workspace.vectors.len(), 2);
}

#[test]
fn freeze_cache_entries_belong_to_one_collection_of_one_generation() {
    let _gates = gates();
    let mut db = crate::Database::create_in_memory(2, crate::CreateOptions::default()).unwrap();
    for (name, base) in [("a", 1.0), ("b", 2.0)] {
        db.create_collection(
            name,
            CollectionSpec {
                dimensions: 2,
                settings: crate::CollectionSettings::default(),
            },
        )
        .unwrap();
        db.collection_mut(name)
            .unwrap()
            .put(1, &[base, base], name.as_bytes())
            .unwrap();
    }
    let mut loaded = compact_image(db, None);
    let generation = loaded.compact_generation.take().unwrap();
    let (a, b) = (
        loaded.collections.iter().find(|(_, c)| c.name == "a"),
        loaded.collections.iter().find(|(_, c)| c.name == "b"),
    );
    let (a, b) = (*a.unwrap().0, *b.unwrap().0);
    assert_ne!(a, b);
    let (view_a, view_b) = (generation.view(a), generation.view(b));
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    assert_eq!(
        view_a.read_vector(0, &mut workspace).unwrap().unwrap(),
        [1.0, 1.0]
    );
    assert_eq!(
        view_b.read_vector(0, &mut workspace).unwrap().unwrap(),
        [2.0, 2.0]
    );
    assert_eq!(
        view_a.read_vector(0, &mut workspace).unwrap().unwrap(),
        [1.0, 1.0]
    );
    let accounting = workspace.accounting(&view_a).unwrap();
    assert_eq!((accounting.misses, accounting.hits), (2, 1));
    assert_eq!(workspace.vectors.len(), 2);
    assert_eq!(view_a.generation_id(), view_b.generation_id());
    // A view reports the collection it was made for, never another.
    assert_eq!(view_b.id(0), Some(1));
    assert_eq!(
        view_b.read_payload(0, &mut workspace).unwrap().unwrap(),
        b"b"
    );
    assert_eq!(
        view_a.read_payload(0, &mut workspace).unwrap().unwrap(),
        b"a"
    );

    // Every vector-bearing block of every collection has the one record kind,
    // so the block kind is not needed to tell cache entries of one collection
    // apart. A second kind must revisit `CacheKey` before it is admitted.
    for source in generation.collections.values() {
        assert!(source.blocks.iter().all(|block| block.kind == KIND));
    }
    // Fresh generations never share an identity, even over identical bytes.
    let again = loaded_with(&ROWS, None).compact_generation.unwrap();
    let once_more = loaded_with(&ROWS, None).compact_generation.unwrap();
    assert_ne!(again.identity, once_more.identity);
    assert_ne!(again.identity, generation.identity);
}

#[test]
fn freeze_resident_and_compact_expiry_sources_agree_after_mutations() {
    let _gates = gates();
    let clocks = [
        0,
        99,
        100,
        499,
        500,
        899,
        900,
        1_233,
        1_234,
        1_235,
        u64::MAX,
    ];
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let image = source_default_fixture(key.as_ref());
        let mut db = crate::Database::open_from_bytes(image, key.as_ref()).unwrap();
        assert!(db.state(0).records.is_compact());
        // The default collection holds ID 0 (deadline 1234) and ID 1 (none).
        db.put_with_expiry(1, &[1.0, 1.0], b"one", &Metadata::new(), Some(500))
            .unwrap();
        db.put_with_expiry(50, &[5.0, 5.0], b"new", &Metadata::new(), Some(900))
            .unwrap();
        db.put(0, &[2.0, 2.0], b"cleared").unwrap();
        db.put_with_expiry(60, &[6.0, 6.0], b"gone", &Metadata::new(), Some(100))
            .unwrap();
        db.delete(60).unwrap();
        db.put_with_expiry(60, &[6.5, 6.5], b"back", &Metadata::new(), Some(1_235))
            .unwrap();
        let state = db.state(0);
        let ids = state.records.effective_ids();
        assert_eq!(ids, [0, 1, 50, 60]);
        // The deadline of each ID after the writes above, as literals: the
        // expectation never comes from either representation.
        let deadlines = BTreeMap::from([
            (0, None),
            (1, Some(500)),
            (50, Some(900)),
            (60, Some(1_235)),
        ]);
        assert_eq!(
            state
                .expiries
                .iter()
                .map(|(&id, &at)| (id, at))
                .collect::<Vec<_>>(),
            [(1, 500), (50, 900), (60, 1_235)],
            "the resident map holds exactly the deadlines that were written"
        );
        for id in ids.into_iter().chain([404]) {
            for now in clocks {
                let expected = deadlines
                    .get(&id)
                    .is_some_and(|deadline| deadline.is_none_or(|deadline| deadline > now));
                assert_eq!(
                    state.records.effective_visible(id, now, &state.expiries),
                    expected,
                    "ID {id} at {now}: columns"
                );
                let by_map = state.records.contains_key(&id)
                    && state
                        .expiries
                        .get(&id)
                        .is_none_or(|deadline| *deadline > now);
                assert_eq!(by_map, expected, "ID {id} at {now}: resident map");
                assert_eq!(
                    db.collection("default")
                        .unwrap()
                        .get_at(id, now)
                        .unwrap()
                        .is_some(),
                    expected,
                    "ID {id} at {now}: get_at"
                );
            }
        }
    }
}

#[test]
fn freeze_named_collections_keep_one_overlay_each() {
    let _gates = gates();
    let (mut candidate, oracle, _) = compact_dispatch_fixture(false);
    let names: Vec<String> = candidate
        .collections
        .values()
        .filter(|state| matches!(&state.records, CollectionRecords::Compact { view, .. } if view.len() > 0))
        .map(|state| state.name.clone())
        .collect();
    assert!(names.len() >= 2);
    let ids_of = |db: &crate::Database, name: &str| -> Vec<u64> {
        db.collections
            .values()
            .find(|state| state.name == name)
            .unwrap()
            .records
            .effective_ids()
    };
    // The resident oracle opened from the same bytes gives the expectation.
    let before: Vec<_> = names
        .iter()
        .map(|name| {
            (
                oracle.collection(name).unwrap().len(),
                ids_of(&oracle, name),
            )
        })
        .collect();
    candidate
        .collection_mut(&names[0])
        .unwrap()
        .put(u64::MAX - 1, &[1.0, 1.0], b"only here")
        .unwrap();
    for (name, (len, ids)) in names.iter().zip(&before) {
        let mut expected_ids = ids.clone();
        if name == &names[0] {
            expected_ids.push(u64::MAX - 1);
            expected_ids.sort_unstable();
        }
        assert_eq!(
            candidate.collection(name).unwrap().len(),
            len + usize::from(name == &names[0]),
            "{name}"
        );
        assert_eq!(ids_of(&candidate, name), expected_ids, "{name}");
    }
}

#[test]
fn freeze_frame_range_outside_the_backing_is_corrupt() {
    let _gates = gates();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let mut loaded = loaded_with(&ROWS, key.as_ref());
        let mut generation = loaded.compact_generation.take().unwrap();
        let inner = Arc::get_mut(&mut generation).unwrap();
        let end = inner.backing.as_slice().len();
        inner.collections.get_mut(&0).unwrap().blocks[0].frame = end..end + 8;
        let view = generation.view(0);
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        let reason = "compact source frame is out of bounds";
        assert!(is_corrupt(view.read_vector(0, &mut workspace), reason));
        assert!(is_corrupt(view.read_metadata(0, &mut workspace), reason));
        assert!(is_corrupt(view.read_payload(0, &mut workspace), reason));
        assert!(is_corrupt(view.get_physical(view.id(0).unwrap()), reason));
    }
}

#[test]
fn freeze_source_never_reads_the_wall_clock() {
    // CLK-1: every deadline decision in the pinned source takes the caller's clock.
    let source = include_str!("../source.rs");
    for forbidden in ["current_time_ms", "SystemTime", "Instant::now"] {
        assert!(
            !source.contains(forbidden),
            "source.rs must not read a clock: {forbidden}"
        );
    }
}
