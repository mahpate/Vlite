use super::*;
use crate::compact::catalog::tests::{source_key, source_view_fixture};
use crate::compact::{
    Activation as PrecisionActivation, catalog::payload::Activation as RecordsActivation,
};

#[test]
fn compact_seam_vector_metadata_leave_cells_empty() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    for encrypted in [false, true] {
        let key = encrypted.then(source_key);
        let (_, loaded) = source_view_fixture(key.as_ref());
        let generation = loaded.compact_generation.as_ref().unwrap();
        let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
        for (&collection, expected) in &loaded.collections {
            let view = generation.view(collection);
            for row in 0..view.len() {
                let id = view.id(row).unwrap();
                let actual = view.read_vector(row, &mut workspace).unwrap().unwrap();
                assert_eq!(
                    actual
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>(),
                    expected.records[&id]
                        .vector
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    view.read_metadata(row, &mut workspace).unwrap().unwrap(),
                    expected.records[&id].metadata
                );
            }
            assert!(
                view.read_vector(view.len(), &mut workspace)
                    .unwrap()
                    .is_none()
            );
            assert!(
                view.read_metadata(view.len(), &mut workspace)
                    .unwrap()
                    .is_none()
            );
            let accounting = workspace.accounting(&view).unwrap();
            assert_eq!(accounting.cells_live, 0);
            if view.len() > 0 {
                assert!(accounting.vectors_live > 0);
                assert_eq!(accounting.plaintext_live > 0, encrypted);
            }
            assert!(accounting.vectors_peak <= 4 * 1024 * 1024);
            assert_eq!(accounting.mapped_live, 0);
            assert_eq!(accounting.source_live, generation.backing.as_slice().len());
            assert_eq!(accounting.source_peak, accounting.source_live);
        }
        assert_eq!(
            generation
                .decode_count
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "vector/metadata-only access must not populate permanent cells"
        );
    }
}

fn generation() -> Arc<CompactGeneration> {
    let (_, mut loaded) = source_view_fixture(None);
    loaded.compact_generation.take().unwrap()
}

fn first_view(generation: &Arc<CompactGeneration>) -> CompactView {
    generation.view(*generation.collections.keys().next().unwrap())
}

#[test]
fn compact_seam_generation_pin_and_cache_identity() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    let first = generation();
    let second = generation();
    let first_id = first.identity;
    assert_ne!(
        first.identity, second.identity,
        "reopen must mint a fresh identity"
    );
    let view = first_view(&first);
    let weak = Arc::downgrade(&first);
    drop(first);
    assert!(weak.upgrade().is_some(), "view must pin its generation");
    assert_eq!(view.generation_id(), first_id);
    let other = first_view(&second);
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    let expected = view
        .read_vector(0, &mut workspace)
        .unwrap()
        .unwrap()
        .to_vec();
    assert_eq!(
        other.read_vector(0, &mut workspace).unwrap().unwrap(),
        expected
    );
    assert_eq!(
        workspace.accounting(&view).unwrap().misses,
        2,
        "cache key must include generation"
    );
    assert_eq!(workspace.accounting(&view).unwrap().hits, 0);
    assert_eq!(
        view.read_vector(0, &mut workspace).unwrap().unwrap(),
        expected
    );
    assert_eq!(workspace.accounting(&view).unwrap().hits, 1);
    drop(view);
    assert!(
        weak.upgrade().is_none(),
        "workspace cache must not retain the source"
    );
}

#[test]
fn compact_seam_lru_budget_and_accounting() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    let generations: Vec<_> = (0..3).map(|_| generation()).collect();
    let views: Vec<_> = generations.iter().map(first_view).collect();
    let block = &views[0].source().unwrap().blocks[0];
    let bytes = block.source.rows.len() * block.source.layout.dimensions * 4;
    let mut small = ReadWorkspace::new(bytes - 1).unwrap();
    assert!(
        views[0].read_vector(0, &mut small).is_err(),
        "budget must cover a whole decoded block"
    );
    assert!(ReadWorkspace::new(4 * 1024 * 1024 + 1).is_err());
    let mut workspace = ReadWorkspace::new(bytes * 2).unwrap();
    views[0].read_vector(0, &mut workspace).unwrap();
    views[1].read_vector(0, &mut workspace).unwrap();
    views[0].read_vector(0, &mut workspace).unwrap();
    views[2].read_vector(0, &mut workspace).unwrap();
    assert!(
        workspace
            .vectors
            .keys()
            .any(|key| key.generation == views[0].generation_id()),
        "LRU must retain the recently used block"
    );
    assert!(
        !workspace
            .vectors
            .keys()
            .any(|key| key.generation == views[1].generation_id()),
        "LRU must evict the oldest block"
    );
    let accounting = workspace.accounting(&views[0]).unwrap();
    assert_eq!(
        accounting.vectors_live,
        bytes * 2,
        "decoded vector bytes must be accounted"
    );
    assert_eq!(accounting.vectors_peak, bytes * 2);
    assert_eq!(accounting.misses, 3);
    assert_eq!(accounting.hits, 1);
    assert_eq!(accounting.evictions, 1);
    views[0].get_physical(views[0].id(0).unwrap()).unwrap();
    assert!(workspace.accounting(&views[0]).unwrap().cells_live > 0);
    workspace.clock = u64::MAX;
    assert!(
        views[0].read_vector(0, &mut workspace).is_err(),
        "clock overflow must be explicit"
    );
}

#[test]
fn compact_seam_authenticates_cached_vectors() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    let key = source_key();
    let (_, mut loaded) = source_view_fixture(Some(&key));
    let mut generation = loaded.compact_generation.take().unwrap();
    drop(loaded);
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    let view = first_view(&generation);
    view.read_vector(0, &mut workspace).unwrap();
    drop(view);
    let inner = Arc::get_mut(&mut generation).unwrap();
    let block = &mut inner.collections.values_mut().next().unwrap().blocks[0];
    let mut bytes = inner.backing.as_slice().to_vec();
    bytes[block.frame.end - FOOTER_SIZE - 1] ^= 0x80;
    let checksum = format::crc32(&bytes[block.frame.start..block.frame.end - FOOTER_SIZE]);
    bytes[block.frame.end - FOOTER_SIZE + 4..block.frame.end - FOOTER_SIZE + 8]
        .copy_from_slice(&checksum.to_le_bytes());
    block.checksum = checksum;
    inner.backing = Backing::Owned(Arc::from(bytes));
    let view = first_view(&generation);
    assert!(
        matches!(
            view.read_vector(0, &mut workspace),
            Err(Error::Corrupt {
                reason: "frame authentication failed",
                ..
            })
        ),
        "cached vectors must authenticate the current encrypted frame"
    );
    assert_eq!(workspace.accounting(&view).unwrap().plaintext_live, 0);
    assert_eq!(
        generation
            .decode_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn compact_seam_rejects_bad_extents_and_accounting_overflow() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    let mut generation = generation();
    let inner = Arc::get_mut(&mut generation).unwrap();
    let row = &mut inner.collections.values_mut().next().unwrap().blocks[0]
        .source
        .rows[0];
    row.vector.end += 1;
    row.metadata.end = usize::MAX;
    let view = first_view(&generation);
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    assert!(matches!(
        view.read_vector(0, &mut workspace),
        Err(Error::Corrupt { .. })
    ));
    assert!(matches!(
        view.read_metadata(0, &mut workspace),
        Err(Error::Corrupt { .. })
    ));
    assert_eq!(workspace.accounting(&view).unwrap().cells_live, 0);
    assert_eq!(workspace.accounting(&view).unwrap().vectors_live, 0);
    let good = self::generation();
    let view = first_view(&good);
    workspace.accounting.misses = u64::MAX;
    assert!(view.read_vector(0, &mut workspace).is_err());
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn compact_seam_mapped_source_accounting() {
    let _precision = PrecisionActivation::on();
    let _records = RecordsActivation::on();
    let _source = Activation::on();
    let generation = generation();
    let dir = tempfile::tempdir().unwrap();
    let mapped = generation.mapped_copy(&dir.path().join("mapped")).unwrap();
    let view = first_view(&mapped);
    let mut workspace = ReadWorkspace::new(4 * 1024 * 1024).unwrap();
    let expected = first_view(&generation)
        .read_vector(0, &mut workspace)
        .unwrap()
        .unwrap()
        .to_vec();
    assert_eq!(
        view.read_vector(0, &mut workspace).unwrap().unwrap(),
        expected
    );
    let accounting = workspace.accounting(&view).unwrap();
    assert_eq!(accounting.mapped_live, mapped.backing.as_slice().len());
    assert_eq!(accounting.source_live, accounting.mapped_live);
    assert_eq!(accounting.mapped_peak, accounting.mapped_live);
    assert_eq!(accounting.plaintext_live, 0);
    assert_eq!(accounting.cells_live, 0);
}
