use std::collections::HashSet;
use std::fs;
use tempfile::tempdir;
use vlite::{
    CreateOptions, Database, DiskAnnIndex, DiskAnnOptions, DistanceMetric, EncryptionKey, Error,
    HnswConfig,
};

const DIMS: usize = 8;

fn vector(seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF;
    (0..DIMS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn populated(path: &std::path::Path, count: u64) -> Database {
    let mut db = Database::create(path, DIMS).unwrap();
    for id in 0..count {
        db.put(id, &vector(id + 1), b"").unwrap();
    }
    db
}

fn recall(found: &[u64], truth: &[u64]) -> f64 {
    let truth: HashSet<_> = truth.iter().collect();
    found.iter().filter(|id| truth.contains(id)).count() as f64 / truth.len() as f64
}

#[test]
fn recall_matches_in_memory_hnsw_and_cache_stays_bounded() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let db = populated(&path, 1500);
    let sidecar = vlite::disk_ann::default_sidecar_path(&path);
    let config = HnswConfig::default();
    let source = db
        .build_disk_ann("default", &sidecar, config, DistanceMetric::SquaredL2)
        .unwrap();
    assert_eq!(source.live_records, 1500);

    // Room for ~40 of the ~1500 nodes.
    let probe = DiskAnnIndex::open(&sidecar, DiskAnnOptions { cache_bytes: 0 }).unwrap();
    let node_bytes = probe.memory().node_bytes;
    drop(probe);
    let cache_bytes = node_bytes * 40;
    let index = DiskAnnIndex::open(&sidecar, DiskAnnOptions { cache_bytes }).unwrap();
    index.verify().unwrap();
    assert_eq!(index.len(), 1500);

    let mut db = db;
    db.build_hnsw(config).unwrap();
    let memory_db = &db;
    let (mut disk_recall, mut memory_recall) = (0.0, 0.0);
    let queries = 40u64;
    for q in 0..queries {
        let query = vector(10_000 + q);
        let truth: Vec<u64> = db
            .search(&query, 10)
            .unwrap()
            .iter()
            .map(|h| h.id)
            .collect();
        let disk: Vec<u64> = db
            .search_disk_ann(&index, &query, 10, 64)
            .unwrap()
            .iter()
            .map(|h| h.id)
            .collect();
        let in_memory: Vec<u64> = memory_db
            .search_approximate_ready(&query, 10, 64, config)
            .unwrap()
            .iter()
            .map(|h| h.id)
            .collect();
        disk_recall += recall(&disk, &truth);
        memory_recall += recall(&in_memory, &truth);
        let memory = index.memory();
        assert!(memory.cache_used_bytes <= memory.cache_capacity_bytes);
    }
    disk_recall /= queries as f64;
    memory_recall /= queries as f64;
    assert!(disk_recall >= 0.9, "disk recall {disk_recall}");
    assert!(
        disk_recall >= memory_recall - 0.05,
        "disk {disk_recall} vs memory {memory_recall}"
    );

    let memory = index.memory();
    assert!(memory.disk_reads > 0 && memory.cache_hits > 0);
    assert!(memory.cached_nodes <= 41);
    assert!(
        (memory.resident_bytes + memory.cache_capacity_bytes) as u64 * 2 < memory.node_data_bytes,
        "resident {} + cache {} vs data {}",
        memory.resident_bytes,
        memory.cache_capacity_bytes,
        memory.node_data_bytes
    );

    // Exhaustive ef visits the whole connected graph and is exact.
    let query = vector(777);
    let truth: Vec<u64> = db.search(&query, 5).unwrap().iter().map(|h| h.id).collect();
    let exhaustive: Vec<u64> = index
        .search(&query, 5, 1500)
        .unwrap()
        .iter()
        .map(|h| h.id)
        .collect();
    assert_eq!(exhaustive, truth);
}

#[test]
fn cosine_graph_is_supported_and_zero_norm_queries_are_rejected() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = populated(&path, 300);
    db.put(9999, &[0.0; DIMS], b"zero").unwrap();
    let sidecar = directory.path().join("cos.dann");
    db.build_disk_ann(
        "default",
        &sidecar,
        HnswConfig::default(),
        DistanceMetric::Cosine,
    )
    .unwrap();
    let index = DiskAnnIndex::open(&sidecar, DiskAnnOptions::default()).unwrap();
    assert_eq!(index.len(), 300);
    assert_eq!(index.metric(), DistanceMetric::Cosine);
    let query = vector(5_000);
    let hits = index.search(&query, 5, 100).unwrap();
    let truth = db
        .search_with_metric(&query, 5, DistanceMetric::Cosine)
        .unwrap();
    assert_eq!(hits[0].id, truth[0].id);
    assert!((hits[0].distance - truth[0].distance).abs() < 1e-9);
    assert!(matches!(
        index.search(&[0.0; DIMS], 5, 10),
        Err(Error::ZeroNormQuery)
    ));
    assert!(matches!(
        index.search(&[1.0; 3], 5, 10),
        Err(Error::DimensionMismatch { .. })
    ));
}

#[test]
fn stale_corrupt_and_empty_sidecars_are_never_trusted() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("db.vlite");
    let mut db = populated(&path, 200);
    let sidecar = directory.path().join("x.dann");
    db.build_disk_ann(
        "default",
        &sidecar,
        HnswConfig::default(),
        DistanceMetric::SquaredL2,
    )
    .unwrap();
    let index = DiskAnnIndex::open(&sidecar, DiskAnnOptions::default()).unwrap();
    assert!(index.is_fresh_for(&db));
    db.put(5_000, &vector(5_000), b"new").unwrap();
    assert!(!index.is_fresh_for(&db));
    assert!(matches!(
        db.search_disk_ann(&index, &vector(1), 3, 20),
        Err(Error::StaleIndex)
    ));
    // Rebuilding atomically replaces the sidecar (close the old handle first on Windows).
    drop(index);
    db.build_disk_ann(
        "default",
        &sidecar,
        HnswConfig::default(),
        DistanceMetric::SquaredL2,
    )
    .unwrap();
    let rebuilt = DiskAnnIndex::open(&sidecar, DiskAnnOptions::default()).unwrap();
    assert_eq!(rebuilt.len(), 201);
    assert!(db.search_disk_ann(&rebuilt, &vector(1), 3, 20).is_ok());

    let original = fs::read(&sidecar).unwrap();
    // Header damage.
    let mut bytes = original.clone();
    bytes[30] ^= 1;
    fs::write(&sidecar, &bytes).unwrap();
    assert!(DiskAnnIndex::open(&sidecar, DiskAnnOptions::default()).is_err());
    // Node damage is caught by verify and by reads.
    let mut bytes = original.clone();
    bytes[200] ^= 0x10;
    fs::write(&sidecar, &bytes).unwrap();
    let damaged = DiskAnnIndex::open(&sidecar, DiskAnnOptions::default()).unwrap();
    assert!(damaged.verify().is_err());
    // Truncation.
    fs::write(&sidecar, &original[..original.len() - 40]).unwrap();
    assert!(DiskAnnIndex::open(&sidecar, DiskAnnOptions::default()).is_err());
    fs::write(&sidecar, b"junk").unwrap();
    assert!(DiskAnnIndex::open(&sidecar, DiskAnnOptions::default()).is_err());

    // Empty collection.
    let empty_path = directory.path().join("empty.vlite");
    let empty_db = Database::create(&empty_path, DIMS).unwrap();
    let empty_sidecar = directory.path().join("empty.dann");
    empty_db
        .build_disk_ann(
            "default",
            &empty_sidecar,
            HnswConfig::default(),
            DistanceMetric::SquaredL2,
        )
        .unwrap();
    let empty = DiskAnnIndex::open(&empty_sidecar, DiskAnnOptions::default()).unwrap();
    assert!(empty.is_empty());
    assert!(empty.search(&vector(1), 3, 10).unwrap().is_empty());
}

#[test]
fn encrypted_databases_do_not_get_plaintext_sidecars() {
    let directory = tempdir().unwrap();
    let mut db = Database::create_with_options(
        directory.path().join("enc.vlite"),
        DIMS,
        CreateOptions {
            encryption: Some(EncryptionKey::from_bytes([1; 32])),
            ..CreateOptions::default()
        },
    )
    .unwrap();
    db.put(1, &vector(1), b"").unwrap();
    let sidecar = directory.path().join("enc.dann");
    assert!(matches!(
        db.build_disk_ann(
            "default",
            &sidecar,
            HnswConfig::default(),
            DistanceMetric::SquaredL2
        ),
        Err(Error::SidecarUnavailable(_))
    ));
    assert!(!sidecar.exists());
}
