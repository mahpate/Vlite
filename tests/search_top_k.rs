use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};
use vlite::{Condition, Database, Metadata, MetadataValue, Mutation, SearchHit};

struct CountingAllocator;

thread_local! {
    static ALLOCATED: Cell<Option<usize>> = const { Cell::new(None) };
}

fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|allocated| {
        if let Some(total) = allocated.get() {
            allocated.set(Some(total.saturating_add(bytes)));
        }
    });
}

// Track requested allocation bytes only on the query thread, excluding fixture setup.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn oracle(
    db: &Database,
    query: &[f32],
    k: usize,
    eligible: impl Fn(u64) -> bool,
) -> Vec<SearchHit> {
    let mut hits = (0..257)
        .chain([u64::MAX])
        .filter(|&id| eligible(id))
        .filter_map(|id| {
            db.get(id).unwrap().map(|record| SearchHit {
                id,
                distance: record
                    .vector
                    .iter()
                    .zip(query)
                    .map(|(&a, &b)| {
                        let difference = f64::from(a) - f64::from(b);
                        difference * difference
                    })
                    .sum(),
            })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then_with(|| a.id.cmp(&b.id))
    });
    hits.truncate(k);
    hits
}

#[test]
fn bounded_top_k_matches_full_sort_for_ties_filters_extremes_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("top-k.vlite");
    let mut db = Database::create(&path, 3).unwrap();
    let operations = (0..257)
        .chain([u64::MAX])
        .map(|id| {
            let vector = match id {
                0 => vec![f32::MAX, -f32::MAX, f32::MIN_POSITIVE],
                1 => vec![-f32::MAX, f32::MAX, -f32::MIN_POSITIVE],
                _ => vec![(id % 7) as f32, (id % 11) as f32, -0.0],
            };
            Mutation::PutWithMetadata {
                id,
                vector,
                payload: vec![],
                metadata: Metadata::from([(
                    "group".into(),
                    MetadataValue::Integer((id % 4) as i64),
                )]),
            }
        })
        .collect::<Vec<_>>();
    db.write_batch(&operations).unwrap();
    for reopen in [false, true] {
        if reopen {
            db.close().unwrap();
            db = Database::open(&path).unwrap();
        }
        for query in [
            vec![0.0, -0.0, 0.0],
            vec![3.0, 5.0, 0.0],
            vec![f32::MAX, -f32::MAX, 0.0],
            vec![0.125, -2.5, 4.0],
        ] {
            for k in [0, 1, 2, 7, 64, 257, 258, 259, usize::MAX] {
                assert_eq!(
                    db.search(&query, k).unwrap(),
                    oracle(&db, &query, k, |_| true)
                );
                for group in 0..5 {
                    let filter = [Condition::Eq {
                        field: "group".into(),
                        value: MetadataValue::Integer(group),
                    }];
                    assert_eq!(
                        db.search_filtered(&query, k, &filter).unwrap(),
                        oracle(&db, &query, k, |id| (id % 4) as i64 == group)
                    );
                }
            }
        }
    }
    db.put(7, &[0.0; 3], b"replacement").unwrap();
    db.delete(0).unwrap();
    assert_eq!(
        db.search(&[0.0; 3], 17).unwrap(),
        oracle(&db, &[0.0; 3], 17, |_| true)
    );
    let filter = [Condition::Eq {
        field: "group".into(),
        value: MetadataValue::Integer(3),
    }];
    assert_eq!(
        db.search_filtered(&[0.0; 3], 17, &filter).unwrap(),
        oracle(&db, &[0.0; 3], 17, |id| id != 7 && id % 4 == 3)
    );
}

#[test]
fn exact_cutoff_preserves_chunk_boundaries_late_components_and_ties() {
    for dimensions in [31, 32, 33, 63, 64, 65, 129] {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("cutoff.vlite"), dimensions).unwrap();
        let operations = (0..257)
            .chain([u64::MAX])
            .map(|id| {
                let mut vector = vec![0.0; dimensions];
                match id % 5 {
                    0 => vector[0] = 1.0,
                    1 => vector[dimensions - 1] = 1.0,
                    2 => vector[dimensions - 1] = f32::MAX,
                    3 => vector[0] = -f32::MAX,
                    _ => vector.fill(0.125),
                }
                Mutation::Put {
                    id,
                    vector,
                    payload: vec![],
                }
            })
            .collect::<Vec<_>>();
        db.write_batch(&operations).unwrap();
        for query in [vec![0.0; dimensions], vec![0.25; dimensions]] {
            for k in [1, 7, 32, 257, usize::MAX] {
                assert_eq!(
                    db.search(&query, k).unwrap(),
                    oracle(&db, &query, k, |_| true),
                    "dimensions={dimensions}, k={k}"
                );
            }
        }
    }
}

#[test]
fn query_allocations_scale_with_k_not_matching_record_count() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create(directory.path().join("memory.vlite"), 1).unwrap();
    for start in [0, 4096] {
        let operations = (start..start + 4096)
            .map(|id| Mutation::Put {
                id,
                vector: vec![(8192 - id) as f32],
                payload: vec![],
            })
            .collect::<Vec<_>>();
        db.write_batch(&operations).unwrap();
    }
    let filter = [Condition::Range {
        field: "missing".into(),
        min: Some(MetadataValue::Integer(0)),
        max: None,
    }];
    for k in [1, 10, 100] {
        for conditions in [&[][..], &filter[..]] {
            ALLOCATED.with(|allocated| allocated.set(Some(0)));
            let hits = db.search_filtered(&[0.0], k, conditions).unwrap();
            let bytes = ALLOCATED.with(|allocated| allocated.replace(None).unwrap());
            assert!(
                bytes <= 4 * k * std::mem::size_of::<SearchHit>() + 512,
                "k={k}: allocated {bytes} bytes for 8192 records"
            );
            assert_eq!(hits.len(), if conditions.is_empty() { k } else { 0 });
            if conditions.is_empty() {
                assert_eq!(hits[0].id, 8191);
            }
        }
    }
    for k in [0, usize::MAX] {
        let hits = db.search(&[0.0], k).unwrap();
        assert_eq!(hits.len(), k.min(8192));
    }
}
