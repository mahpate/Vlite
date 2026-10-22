//! Persisted ANN snapshot sidecar: exact restore, staleness, corruption,
//! all-or-nothing install, destination guards and datafile isolation.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use vlite::{
    AnnOptions, AnnSnapshotLoad, AnnSnapshotRejection, CollectionSettings, CollectionSpec,
    Database, DistanceMetric, Error, FormatVersion, HnswConfig, HnswRebuildLimits,
    HnswRebuildStatus, Quantization, SearchHit, SearchStats,
};

const DIMS: usize = 8;
const K: usize = 10;
const EF: usize = 64;
const NAME: &str = "alpha";
const HEADER: usize = 64;
const TRAILER: usize = 32;

// ------------------------------------------------------------------ helpers

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn vector(&mut self) -> Vec<f32> {
        (0..DIMS)
            .map(|_| (self.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    V3Default,
    V4Default,
    V4Named,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    layout: Layout,
    metric: DistanceMetric,
    options: AnnOptions,
}

fn options(quantization: Quantization) -> AnnOptions {
    AnnOptions {
        quantization,
        ..AnnOptions::default()
    }
}

fn config() -> HnswConfig {
    HnswConfig::default()
}

fn named(layout: Layout) -> Option<&'static str> {
    (layout == Layout::V4Named).then_some(NAME)
}

fn spec() -> CollectionSpec {
    CollectionSpec {
        dimensions: DIMS,
        settings: CollectionSettings::default(),
    }
}

fn create(path: &Path, layout: Layout) -> Database {
    match layout {
        Layout::V3Default => Database::create(path, DIMS).unwrap(),
        Layout::V4Default => Database::create_with_format(path, DIMS, FormatVersion::V4).unwrap(),
        Layout::V4Named => {
            let mut db = Database::create_with_format(path, DIMS, FormatVersion::V4).unwrap();
            db.create_collection(NAME, spec()).unwrap();
            db
        }
    }
}

fn put(db: &mut Database, collection: Option<&str>, id: u64, vector: &[f32]) {
    match collection {
        Some(name) => {
            db.collection_mut(name)
                .unwrap()
                .put(id, vector, b"")
                .unwrap();
        }
        None => db.put(id, vector, b"").unwrap(),
    }
}

fn delete(db: &mut Database, collection: Option<&str>, id: u64) {
    match collection {
        Some(name) => db.collection_mut(name).unwrap().delete(id).unwrap(),
        None => db.delete(id).unwrap(),
    };
}

fn build(db: &mut Database, collection: Option<&str>, metric: DistanceMetric, options: AnnOptions) {
    match collection {
        Some(name) => {
            db.collection_mut(name)
                .unwrap()
                .build_hnsw_with_options(config(), metric, options)
        }
        None => db.build_hnsw_with_options(config(), metric, options),
    }
    .unwrap();
}

fn search(
    db: &Database,
    collection: Option<&str>,
    query: &[f32],
    metric: DistanceMetric,
    options: AnnOptions,
) -> vlite::Result<(Vec<SearchHit>, SearchStats)> {
    match collection {
        Some(name) => db.collection(name)?.search_approximate_ready_with_options(
            query,
            K,
            EF,
            config(),
            options,
            &[],
            metric,
        ),
        None => {
            db.search_approximate_ready_with_options(query, K, EF, config(), options, &[], metric)
        }
    }
}

type Answers = Vec<(Vec<(u64, u64)>, SearchStats)>;

fn answers(
    db: &Database,
    collection: Option<&str>,
    queries: &[Vec<f32>],
    metric: DistanceMetric,
    options: AnnOptions,
) -> Answers {
    queries
        .iter()
        .map(|query| {
            let (hits, stats) = search(db, collection, query, metric, options).unwrap();
            (
                hits.iter()
                    .map(|hit| (hit.id, hit.distance.to_bits()))
                    .collect(),
                stats,
            )
        })
        .collect()
}

fn queries(seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    (0..64).map(|_| rng.vector()).collect()
}

/// Inserts `count` records and returns the live vectors.
fn fill(
    db: &mut Database,
    collection: Option<&str>,
    count: u64,
    seed: u64,
) -> BTreeMap<u64, Vec<f32>> {
    let mut rng = Rng(seed);
    let mut live = BTreeMap::new();
    for id in 0..count {
        let vector = rng.vector();
        put(db, collection, id, &vector);
        live.insert(id, vector);
    }
    live
}

/// Buffered put / replace / delete, including zero vectors.
fn mutate(
    db: &mut Database,
    collection: Option<&str>,
    live: &mut BTreeMap<u64, Vec<f32>>,
    seed: u64,
) {
    let mut rng = Rng(seed);
    for id in 1_000..1_020 {
        let vector = rng.vector();
        put(db, collection, id, &vector);
        live.insert(id, vector);
    }
    for id in 0..10 {
        let vector = rng.vector();
        put(db, collection, id, &vector);
        live.insert(id, vector);
    }
    for id in 20..30 {
        delete(db, collection, id);
        live.remove(&id);
    }
    let zero = vec![0.0; DIMS];
    put(db, collection, 2_000, &zero);
    live.insert(2_000, zero.clone());
    put(db, collection, 40, &zero);
    live.insert(40, zero);
}

fn sidecar(dir: &Path) -> PathBuf {
    Database::ann_snapshot_path(dir.join("db.vlite"))
}

fn assert_not_ready(db: &Database, case: Case, query: &[f32]) {
    assert!(matches!(
        search(db, named(case.layout), query, case.metric, case.options),
        Err(Error::IndexNotReady)
    ));
}

fn resign(bytes: &mut [u8]) {
    let body = bytes.len() - TRAILER;
    let digest = Sha256::digest(&bytes[..body]);
    bytes[body..].copy_from_slice(&digest);
}

fn u16_at(bytes: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()))
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn u64_at(bytes: &[u8], at: usize) -> usize {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize
}

/// Byte offsets of one section's fields (test-side mirror of format v1).
#[derive(Debug)]
struct Section {
    name: usize,
    node_count: usize,
    entry: usize,
    node_ids: Vec<usize>,
    levels: Vec<usize>,
    links: Vec<usize>,
    buffered_count: usize,
    end: usize,
}

fn section(bytes: &[u8], start: usize) -> Section {
    let name_len = u16_at(bytes, start + 2);
    let name = start + 4;
    let dims = u32_at(bytes, name + name_len);
    let body = name + name_len + 4 + 8 + 32;
    let node_count = body + 20;
    let entry = node_count + 8;
    let nodes = u64_at(bytes, node_count);
    let mut at = entry + 8;
    let (mut node_ids, mut levels, mut links) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..nodes {
        node_ids.push(at);
        levels.push(at + 16);
        let layers = usize::from(bytes[at + 16]);
        at += 17;
        for _ in 0..layers {
            let count = u16_at(bytes, at);
            at += 2;
            for _ in 0..count {
                links.push(at);
                at += 4;
            }
        }
    }
    let buffered_count = at;
    let buffered = u32_at(bytes, at);
    at += 4;
    for _ in 0..buffered {
        let original = bytes[at + 16];
        at += 17;
        if original == 1 {
            at += 4 * dims;
        }
    }
    let quantized = bytes[at];
    at += 1;
    if quantized == 1 {
        let qdims = u32_at(bytes, at);
        at += 4 + 16 * qdims + nodes * qdims;
    }
    Section {
        name,
        node_count,
        entry,
        node_ids,
        levels,
        links,
        buffered_count,
        end: at,
    }
}

fn load(db: &mut Database, path: &Path) -> AnnSnapshotLoad {
    db.load_ann_snapshot(path).unwrap()
}

fn rejected(outcome: AnnSnapshotLoad) -> AnnSnapshotRejection {
    match outcome {
        AnnSnapshotLoad::Rejected(rejection) => rejection,
        other => panic!("expected rejection, got {other:?}"),
    }
}

fn assert_corrupt(outcome: AnnSnapshotLoad) {
    assert!(
        matches!(rejected(outcome), AnnSnapshotRejection::Corrupt(_)),
        "expected corrupt"
    );
}

fn temp_leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".vlite-ann-"))
        .collect()
}

/// Standard fixture: v3 default L2 index with a buffered tail.
fn fixture(dir: &Path) -> (PathBuf, PathBuf, BTreeMap<u64, Vec<f32>>) {
    let path = dir.join("db.vlite");
    let mut db = create(&path, Layout::V3Default);
    let mut live = fill(&mut db, None, 200, 1);
    build(
        &mut db,
        None,
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );
    mutate(&mut db, None, &mut live, 2);
    let snapshot = Database::ann_snapshot_path(&path);
    db.save_ann_snapshot(&snapshot).unwrap();
    db.close().unwrap();
    (path, snapshot, live)
}

// ------------------------------------------------------------------ TEST-001

#[test]
fn restores_exact_results_across_metrics_quantization_layouts_and_buffers() {
    let queries = queries(99);
    for layout in [Layout::V3Default, Layout::V4Default, Layout::V4Named] {
        for metric in [DistanceMetric::SquaredL2, DistanceMetric::Cosine] {
            for quantization in [Quantization::None, Quantization::ScalarU8] {
                for buffered in [false, true] {
                    let case = Case {
                        layout,
                        metric,
                        options: options(quantization),
                    };
                    let dir = tempfile::tempdir().unwrap();
                    let path = dir.path().join("db.vlite");
                    let collection = named(layout);
                    let mut db = create(&path, layout);
                    let mut live = fill(&mut db, collection, 300, 7);
                    build(&mut db, collection, metric, case.options);
                    if buffered {
                        mutate(&mut db, collection, &mut live, 8);
                    }
                    let before = answers(&db, collection, &queries, metric, case.options);
                    let snapshot = sidecar(dir.path());
                    let report = db.save_ann_snapshot(&snapshot).unwrap();
                    assert_eq!(report.indexes, 1, "{case:?}");
                    assert_eq!(report.bytes, fs::metadata(&snapshot).unwrap().len());
                    db.close().unwrap();

                    let mut db = Database::open(&path).unwrap();
                    assert_not_ready(&db, case, &queries[0]);
                    assert_eq!(
                        load(&mut db, &snapshot),
                        AnnSnapshotLoad::Loaded { indexes: 1 },
                        "{case:?}"
                    );
                    let after = answers(&db, collection, &queries, metric, case.options);
                    assert_eq!(before, after, "{case:?} buffered={buffered}");

                    // The installed index keeps tracking later commits.
                    put(&mut db, collection, 5_000, &queries[0]);
                    let (hits, _) =
                        search(&db, collection, &queries[0], metric, case.options).unwrap();
                    assert_eq!(hits[0].id, 5_000, "{case:?}");
                    assert!(hits[0].distance.abs() < 1e-9);
                    db.close().unwrap();
                }
            }
        }
    }
}

#[test]
fn two_indexed_collections_round_trip_and_install_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.vlite");
    let queries = queries(5);
    let mut db = create(&path, Layout::V4Named);
    db.create_collection("beta", spec()).unwrap();
    fill(&mut db, Some(NAME), 120, 1);
    fill(&mut db, Some("beta"), 120, 2);
    build(
        &mut db,
        Some(NAME),
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );
    let u8 = options(Quantization::ScalarU8);
    build(&mut db, Some("beta"), DistanceMetric::Cosine, u8);
    let alpha = answers(
        &db,
        Some(NAME),
        &queries,
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );
    let beta = answers(&db, Some("beta"), &queries, DistanceMetric::Cosine, u8);
    let snapshot = sidecar(dir.path());
    assert_eq!(db.save_ann_snapshot(&snapshot).unwrap().indexes, 2);
    db.close().unwrap();
    let good = fs::read(&snapshot).unwrap();

    // Second section corrupted (re-signed): nothing is installed.
    let first = section(&good, HEADER);
    let second = section(&good, first.end);
    let mut bad = good.clone();
    bad[second.links[0]..second.links[0] + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    resign(&mut bad);
    let bad_path = dir.path().join("bad.ann");
    fs::write(&bad_path, &bad).unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_corrupt(load(&mut db, &bad_path));
    let alpha_case = Case {
        layout: Layout::V4Named,
        metric: DistanceMetric::SquaredL2,
        options: AnnOptions::default(),
    };
    assert_not_ready(&db, alpha_case, &queries[0]);
    assert_eq!(
        load(&mut db, &snapshot),
        AnnSnapshotLoad::Loaded { indexes: 2 }
    );
    assert_eq!(
        alpha,
        answers(
            &db,
            Some(NAME),
            &queries,
            DistanceMetric::SquaredL2,
            AnnOptions::default()
        )
    );
    assert_eq!(
        beta,
        answers(&db, Some("beta"), &queries, DistanceMetric::Cosine, u8)
    );
}

#[test]
fn published_background_rebuild_with_catch_up_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.vlite");
    let queries = queries(11);
    for quantization in [Quantization::None, Quantization::ScalarU8] {
        let options = options(quantization);
        let metric = DistanceMetric::Cosine;
        let mut db = create(&path, Layout::V3Default);
        let mut live = fill(&mut db, None, 300, 3);
        build(&mut db, None, metric, options);
        db.start_hnsw_rebuild_with_options(config(), metric, options, HnswRebuildLimits::default())
            .unwrap();
        mutate(&mut db, None, &mut live, 4);
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            match db.poll_hnsw_rebuild().unwrap() {
                HnswRebuildStatus::Published => break,
                HnswRebuildStatus::Running => {
                    assert!(Instant::now() < deadline, "rebuild did not finish");
                    thread::sleep(Duration::from_millis(5));
                }
                other => panic!("unexpected rebuild status {other:?}"),
            }
        }
        let before = answers(&db, None, &queries, metric, options);
        let snapshot = sidecar(dir.path());
        db.save_ann_snapshot(&snapshot).unwrap();
        db.close().unwrap();
        let mut db = Database::open(&path).unwrap();
        assert_eq!(
            load(&mut db, &snapshot),
            AnnSnapshotLoad::Loaded { indexes: 1 }
        );
        assert_eq!(before, answers(&db, None, &queries, metric, options));
        db.close().unwrap();
        fs::remove_file(&path).unwrap();
    }
}

#[test]
fn pending_rebuild_saves_active_index_and_blocks_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.vlite");
    let queries = queries(12);
    let metric = DistanceMetric::SquaredL2;
    let options = AnnOptions::default();
    let mut db = create(&path, Layout::V3Default);
    let mut live = fill(&mut db, None, 300, 5);
    build(&mut db, None, metric, options);
    db.start_hnsw_rebuild_with_options(config(), metric, options, HnswRebuildLimits::default())
        .unwrap();
    mutate(&mut db, None, &mut live, 6);
    let before = answers(&db, None, &queries, metric, options);
    let snapshot = sidecar(dir.path());
    db.save_ann_snapshot(&snapshot).unwrap();
    // Publication requires an explicit poll, so the job is still pending.
    assert!(matches!(
        db.load_ann_snapshot(&snapshot),
        Err(Error::HnswRebuildBusy)
    ));
    assert_eq!(before, answers(&db, None, &queries, metric, options));
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        load(&mut db, &snapshot),
        AnnSnapshotLoad::Loaded { indexes: 1 }
    );
    assert_eq!(before, answers(&db, None, &queries, metric, options));
}

#[test]
fn database_without_indexes_saves_an_empty_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.vlite");
    let mut db = create(&path, Layout::V3Default);
    fill(&mut db, None, 10, 1);
    let snapshot = sidecar(dir.path());
    let report = db.save_ann_snapshot(&snapshot).unwrap();
    assert_eq!(report.indexes, 0);
    assert_eq!(report.bytes as usize, HEADER + TRAILER);
    assert_eq!(
        load(&mut db, &snapshot),
        AnnSnapshotLoad::Loaded { indexes: 0 }
    );
}

#[test]
fn empty_collection_index_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.vlite");
    let options = options(Quantization::ScalarU8);
    let mut db = create(&path, Layout::V3Default);
    build(&mut db, None, DistanceMetric::Cosine, options);
    put(&mut db, None, 1, &[0.5; DIMS]);
    let snapshot = sidecar(dir.path());
    db.save_ann_snapshot(&snapshot).unwrap();
    db.close().unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        load(&mut db, &snapshot),
        AnnSnapshotLoad::Loaded { indexes: 1 }
    );
    let (hits, _) = search(&db, None, &[0.5; DIMS], DistanceMetric::Cosine, options).unwrap();
    assert_eq!(hits[0].id, 1);
}

// ------------------------------------------------------------------ TEST-002

#[test]
fn stale_after_commit_compact_restore_or_different_content() {
    let dir = tempfile::tempdir().unwrap();
    let (path, snapshot, _) = fixture(dir.path());

    // A later commit changes the identity.
    let mut db = Database::open(&path).unwrap();
    put(&mut db, None, 9_999, &[0.25; DIMS]);
    assert_eq!(
        rejected(load(&mut db, &snapshot)),
        AnnSnapshotRejection::Stale("identity")
    );
    db.close().unwrap();

    // Compaction of a file with overwrites/deletes changes sequence and end.
    let compacted = dir.path().join("compacted.vlite");
    vlite::maintenance::compact(&path, &compacted).unwrap();
    let mut db = Database::open(&compacted).unwrap();
    assert_eq!(
        rejected(load(&mut db, &snapshot)),
        AnnSnapshotRejection::Stale("identity")
    );
    db.close().unwrap();

    // v4 restore assigns a new lineage.
    let v4 = dir.path().join("v4.vlite");
    let mut db = create(&v4, Layout::V4Default);
    fill(&mut db, None, 50, 1);
    build(
        &mut db,
        None,
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );
    let v4_snapshot = dir.path().join("v4.ann");
    db.save_ann_snapshot(&v4_snapshot).unwrap();
    db.close().unwrap();
    let backup = dir.path().join("v4.bak");
    let restored = dir.path().join("restored.vlite");
    vlite::maintenance::backup(&v4, &backup).unwrap();
    vlite::maintenance::restore(&backup, &restored).unwrap();
    let mut db = Database::open(&restored).unwrap();
    assert_eq!(
        rejected(load(&mut db, &v4_snapshot)),
        AnnSnapshotRejection::Stale("identity")
    );

    // Same operation shapes, different vectors: identity matches, content not.
    let other = dir.path().join("other.vlite");
    let mut db = create(&other, Layout::V3Default);
    let mut live = fill(&mut db, None, 200, 77);
    mutate(&mut db, None, &mut live, 78);
    assert_eq!(
        rejected(load(&mut db, &snapshot)),
        AnnSnapshotRejection::Stale("content")
    );
    db.close().unwrap();
}

#[test]
fn renamed_collection_section_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.vlite");
    let mut db = create(&path, Layout::V4Named);
    fill(&mut db, Some(NAME), 40, 1);
    build(
        &mut db,
        Some(NAME),
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );
    let snapshot = sidecar(dir.path());
    db.save_ann_snapshot(&snapshot).unwrap();
    let mut bytes = fs::read(&snapshot).unwrap();
    let name = section(&bytes, HEADER).name;
    bytes[name + NAME.len() - 1] ^= 0x01;
    resign(&mut bytes);
    let forged = dir.path().join("forged.ann");
    fs::write(&forged, &bytes).unwrap();
    assert_eq!(
        rejected(load(&mut db, &forged)),
        AnnSnapshotRejection::Stale("collection")
    );
}

// ------------------------------------------------------------------ TEST-003

#[test]
fn malformed_files_are_rejected_without_state_change() {
    let dir = tempfile::tempdir().unwrap();
    let (path, snapshot, _) = fixture(dir.path());
    let good = fs::read(&snapshot).unwrap();
    let case = Case {
        layout: Layout::V3Default,
        metric: DistanceMetric::SquaredL2,
        options: AnnOptions::default(),
    };
    let query = vec![0.1; DIMS];
    let mut db = Database::open(&path).unwrap();
    let probe = dir.path().join("probe.ann");
    let mut check = |bytes: &[u8], expected: Option<AnnSnapshotRejection>| {
        fs::write(&probe, bytes).unwrap();
        let outcome = rejected(load(&mut db, &probe));
        if let Some(expected) = expected {
            assert_eq!(outcome, expected);
        } else {
            assert!(
                matches!(outcome, AnnSnapshotRejection::Corrupt(_)),
                "{outcome:?}"
            );
        }
        assert_not_ready(&db, case, &query);
        assert_eq!(fs::read(&probe).unwrap(), bytes, "load never writes");
    };

    check(&[], Some(AnnSnapshotRejection::Corrupt("truncated")));
    check(
        &good[..HEADER + TRAILER - 1],
        Some(AnnSnapshotRejection::Corrupt("truncated")),
    );
    check(
        &good[..good.len() - 1],
        Some(AnnSnapshotRejection::Corrupt("checksum")),
    );
    let mut extra = good.clone();
    extra.push(0);
    check(&extra, Some(AnnSnapshotRejection::Corrupt("checksum")));
    let mut flipped = good.clone();
    flipped[HEADER + 40] ^= 0x10;
    check(&flipped, Some(AnnSnapshotRejection::Corrupt("checksum")));
    let mut magic = good.clone();
    magic[0] ^= 1;
    check(&magic, Some(AnnSnapshotRejection::Corrupt("magic")));
    let mut version = good.clone();
    version[8..12].copy_from_slice(&2u32.to_le_bytes());
    check(&version, Some(AnnSnapshotRejection::UnsupportedVersion(2)));
    let mut reserved = good.clone();
    reserved[12] = 1;
    resign(&mut reserved);
    check(&reserved, Some(AnnSnapshotRejection::Corrupt("reserved")));
    let mut sections = good.clone();
    sections[52..56].copy_from_slice(&u32::MAX.to_le_bytes());
    resign(&mut sections);
    check(
        &sections,
        Some(AnnSnapshotRejection::Corrupt("section count")),
    );

    // Checksum-valid structural corruption.
    let layout = section(&good, HEADER);
    let nodes = u64_at(&good, layout.node_count);
    let mut forge = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = good.clone();
        edit(&mut bytes);
        resign(&mut bytes);
        check(&bytes, None);
    };
    forge(&|b| {
        b[layout.node_count..layout.node_count + 8].copy_from_slice(&u64::MAX.to_le_bytes())
    });
    forge(&|b| {
        b[layout.node_count..layout.node_count + 8]
            .copy_from_slice(&(nodes as u64 + 1).to_le_bytes())
    });
    forge(&|b| b[layout.entry..layout.entry + 8].copy_from_slice(&(nodes as u64).to_le_bytes()));
    forge(&|b| b[layout.levels[0]] = 255);
    forge(&|b| b[layout.levels[0]] = 0);
    forge(&|b| {
        b[layout.links[0]..layout.links[0] + 4].copy_from_slice(&(nodes as u32).to_le_bytes())
    });
    forge(&|b| {
        let (a, c) = (layout.node_ids[0], layout.node_ids[1]);
        let second = b[c..c + 8].to_vec();
        b[a..a + 8].copy_from_slice(&second);
    });
    forge(&|b| {
        let at = layout.buffered_count;
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    });
    forge(&|b| {
        let at = layout.buffered_count;
        b[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    });
    // Norm of the first node.
    forge(&|b| b[layout.node_ids[0] + 8] ^= 0x01);
    // Metric byte flipped to an unknown code.
    forge(&|b| b[layout.node_count - 4] = 9);
    // Unquantized index claiming ScalarU8 without a quantizer section.
    forge(&|b| b[layout.node_count - 3] = 1);
    // Unknown quantization code.
    forge(&|b| b[layout.node_count - 3] = 7);
    // Nonzero reserved config byte.
    forge(&|b| b[layout.node_count - 1] = 1);

    // Missing, directory and datafile-as-sidecar.
    assert_eq!(
        load(&mut db, &dir.path().join("absent.ann")),
        AnnSnapshotLoad::Missing
    );
    assert_eq!(
        rejected(load(&mut db, dir.path())),
        AnnSnapshotRejection::Unreadable(std::io::ErrorKind::InvalidInput)
    );
    db.close().unwrap();
    let datafile = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    let outcome = rejected(load(&mut db, &path));
    // A locked datafile may be unreadable on some platforms; never loaded.
    assert!(
        matches!(
            outcome,
            AnnSnapshotRejection::Corrupt("magic") | AnnSnapshotRejection::Unreadable(_)
        ),
        "{outcome:?}"
    );
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap(), datafile);
}

#[cfg(unix)]
#[test]
fn oversized_file_is_rejected_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _, _) = fixture(dir.path());
    let huge = dir.path().join("huge.ann");
    let file = fs::File::create(&huge).unwrap();
    if file.set_len(vlite::MAX_ANN_SNAPSHOT_BYTES + 1).is_err() {
        eprintln!("skipping: sparse files unsupported");
        return;
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        rejected(load(&mut db, &huge)),
        AnnSnapshotRejection::TooLarge(vlite::MAX_ANN_SNAPSHOT_BYTES + 1)
    );
}

/// Seeded fuzz: unsigned damage is always rejected; checksum-valid damage is
/// either rejected or yields only live records with exact distances.
#[test]
fn seeded_fuzz_never_installs_inconsistent_state() {
    for (metric, quantization) in [
        (DistanceMetric::SquaredL2, Quantization::None),
        (DistanceMetric::Cosine, Quantization::ScalarU8),
    ] {
        let options = options(quantization);
        let case = Case {
            layout: Layout::V3Default,
            metric,
            options,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.vlite");
        let mut db = create(&path, Layout::V3Default);
        let mut live = fill(&mut db, None, 40, 21);
        build(&mut db, None, metric, options);
        mutate(&mut db, None, &mut live, 22);
        let snapshot = sidecar(dir.path());
        db.save_ann_snapshot(&snapshot).unwrap();
        db.close().unwrap();
        let good = fs::read(&snapshot).unwrap();
        let layout = section(&good, HEADER);
        let body = good.len() - TRAILER;
        let probe = dir.path().join("probe.ann");
        let mut rng = Rng(0xF00D ^ metric as u64);
        let mut db = Database::open(&path).unwrap();
        let query = vec![0.3; DIMS];
        let mut loaded = 0;
        for round in 0..1_000 {
            let mut bytes = good.clone();
            let must_reject = match round % 5 {
                0 => {
                    let at = rng.below(bytes.len());
                    bytes[at] ^= 1 << rng.below(8);
                    true
                }
                1 => {
                    bytes.truncate(rng.below(bytes.len()));
                    true
                }
                2 | 3 => {
                    for _ in 0..1 + rng.below(3) {
                        let at = rng.below(body);
                        bytes[at] ^= 1 << rng.below(8);
                    }
                    resign(&mut bytes);
                    false
                }
                _ => {
                    match rng.below(5) {
                        0 => bytes[layout.node_count + 7] = 0xFF,
                        1 => bytes[layout.levels[rng.below(layout.levels.len())]] = 255,
                        2 => {
                            let at = layout.links[rng.below(layout.links.len())];
                            bytes[at + 3] = 0x80;
                        }
                        3 => bytes[layout.buffered_count + 3] = 0xFF,
                        _ => bytes[layout.entry + 7] = 0x7F,
                    }
                    resign(&mut bytes);
                    true
                }
            };
            fs::write(&probe, &bytes).unwrap();
            match db.load_ann_snapshot(&probe).unwrap() {
                AnnSnapshotLoad::Rejected(_) => assert_not_ready(&db, case, &query),
                AnnSnapshotLoad::Loaded { .. } => {
                    assert!(!must_reject, "round {round} must be rejected");
                    loaded += 1;
                    // A re-signed ef_construction or rerank change is a
                    // different, structurally valid index identity; the
                    // configured ready search then simply is not ready.
                    let hits = match search(&db, None, &query, metric, options) {
                        Ok((hits, _)) => hits,
                        Err(Error::IndexNotReady) => Vec::new(),
                        Err(error) => panic!("round {round}: {error}"),
                    };
                    assert!(hits.len() <= K);
                    for pair in hits.windows(2) {
                        assert!(pair[0].distance <= pair[1].distance);
                    }
                    for hit in &hits {
                        let vector = live.get(&hit.id).expect("hit is a live record");
                        let exact = exact_distance(metric, &query, vector);
                        assert!((hit.distance - exact).abs() < 1e-6, "round {round}");
                    }
                    db.close().unwrap();
                    db = Database::open(&path).unwrap();
                }
                AnnSnapshotLoad::Missing => panic!("probe exists"),
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        // Flips of padding-free fields almost always break invariants; the
        // loop must still exercise both outcomes over time.
        eprintln!("{metric:?}: {loaded} checksum-valid mutations loaded");
    }
}

fn exact_distance(metric: DistanceMetric, query: &[f32], vector: &[f32]) -> f64 {
    match metric {
        DistanceMetric::Cosine => {
            let dot: f64 = query
                .iter()
                .zip(vector)
                .map(|(&a, &b)| f64::from(a) * f64::from(b))
                .sum();
            let norm = |v: &[f32]| v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
            1.0 - dot / (norm(query) * norm(vector))
        }
        _ => query
            .iter()
            .zip(vector)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum(),
    }
}

// ------------------------------------------------------------------ TEST-004

#[test]
fn save_refuses_datafiles_unrelated_files_and_directories() {
    let dir = tempfile::tempdir().unwrap();
    let (path, snapshot, _) = fixture(dir.path());
    let datafile = fs::read(&path).unwrap();
    let mut db = Database::open(&path).unwrap();
    build(
        &mut db,
        None,
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );

    // The open datafile itself.
    assert!(matches!(db.save_ann_snapshot(&path), Err(Error::Io(_))));
    // Another closed datafile.
    let other = dir.path().join("other.vlite");
    create(&other, Layout::V4Default).close().unwrap();
    let other_bytes = fs::read(&other).unwrap();
    let invalid = |result: vlite::Result<_>| matches!(result, Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput);
    assert!(invalid(db.save_ann_snapshot(&other)));
    // Unrelated and short files.
    let unrelated = dir.path().join("notes.txt");
    fs::write(&unrelated, b"keep me").unwrap();
    assert!(invalid(db.save_ann_snapshot(&unrelated)));
    let empty = dir.path().join("empty.ann");
    fs::write(&empty, b"").unwrap();
    assert!(invalid(db.save_ann_snapshot(&empty)));
    // Directory.
    assert!(invalid(db.save_ann_snapshot(dir.path())));
    assert!(temp_leftovers(dir.path()).is_empty());
    // An existing sidecar is replaced.
    db.save_ann_snapshot(&snapshot).unwrap();
    db.close().unwrap();

    assert_eq!(fs::read(&path).unwrap(), datafile);
    assert_eq!(fs::read(&other).unwrap(), other_bytes);
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep me");
    assert_eq!(fs::read(&empty).unwrap(), b"");
}

#[test]
fn fixed_tmp_name_is_never_used_and_no_staging_files_remain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.vlite");
    let snapshot = sidecar(dir.path());
    let mut fixed = snapshot.clone().into_os_string();
    fixed.push(".tmp");
    fs::write(&fixed, b"victim").unwrap();
    let mut db = create(&path, Layout::V3Default);
    fill(&mut db, None, 30, 1);
    build(
        &mut db,
        None,
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );
    db.save_ann_snapshot(&snapshot).unwrap();
    db.save_ann_snapshot(&snapshot).unwrap();
    assert_eq!(fs::read(&fixed).unwrap(), b"victim");
    assert!(temp_leftovers(dir.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn symlink_destination_is_refused_and_target_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (path, snapshot, _) = fixture(dir.path());
    let victim = dir.path().join("victim");
    fs::write(&victim, b"precious").unwrap();
    let link = dir.path().join("link.ann");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    // A symlink to a valid sidecar is refused too.
    let sidecar_link = dir.path().join("sidecar-link.ann");
    std::os::unix::fs::symlink(&snapshot, &sidecar_link).unwrap();
    let mut db = Database::open(&path).unwrap();
    build(
        &mut db,
        None,
        DistanceMetric::SquaredL2,
        AnnOptions::default(),
    );
    assert!(matches!(db.save_ann_snapshot(&link), Err(Error::Io(_))));
    assert!(matches!(
        db.save_ann_snapshot(&sidecar_link),
        Err(Error::Io(_))
    ));
    assert_eq!(fs::read(&victim).unwrap(), b"precious");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn failed_publication_leaves_previous_sidecar_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let (path, snapshot, _) = fixture(dir.path());
    let previous = fs::read(&snapshot).unwrap();
    let mut db = Database::open(&path).unwrap();
    put(&mut db, None, 7_777, &[0.5; DIMS]);
    build(&mut db, None, DistanceMetric::Cosine, AnnOptions::default());

    #[cfg(windows)]
    let result = {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ only: the replacing rename cannot proceed.
        let _guard = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&snapshot)
            .unwrap();
        db.save_ann_snapshot(&snapshot)
    };
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        if fs::File::create(dir.path().join("root-probe")).is_ok() {
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping: running with permission override");
            return;
        }
        let result = db.save_ann_snapshot(&snapshot);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        result
    };
    assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
    assert_eq!(fs::read(&snapshot).unwrap(), previous);
    assert!(temp_leftovers(dir.path()).is_empty());
    // The database stays healthy and can retry.
    db.save_ann_snapshot(&snapshot).unwrap();
}

#[test]
fn save_and_load_never_modify_the_datafile() {
    let dir = tempfile::tempdir().unwrap();
    let (path, snapshot, _) = fixture(dir.path());
    let bytes = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        load(&mut db, &snapshot),
        AnnSnapshotLoad::Loaded { indexes: 1 }
    );
    db.save_ann_snapshot(&snapshot).unwrap();
    assert_eq!(
        load(&mut db, &snapshot),
        AnnSnapshotLoad::Loaded { indexes: 1 }
    );
    db.close().unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
}

#[test]
fn rejection_codes_and_display_are_stable() {
    assert_eq!(AnnSnapshotLoad::Missing.code(), "missing");
    assert_eq!(AnnSnapshotLoad::Loaded { indexes: 2 }.code(), "loaded");
    let corrupt = AnnSnapshotRejection::Corrupt("checksum");
    assert_eq!(AnnSnapshotLoad::Rejected(corrupt).code(), "corrupt");
    assert_eq!(corrupt.to_string(), "corrupt: checksum");
    assert_eq!(
        AnnSnapshotRejection::Stale("identity").to_string(),
        "stale: identity"
    );
    assert_eq!(
        AnnSnapshotRejection::UnsupportedVersion(2).to_string(),
        "unsupported_version: 2"
    );
    assert_eq!(AnnSnapshotRejection::TooLarge(5).code(), "too_large");
    assert_eq!(
        AnnSnapshotRejection::Unreadable(std::io::ErrorKind::PermissionDenied).code(),
        "unreadable"
    );
    assert_eq!(
        Database::ann_snapshot_path("dir/db.vlite"),
        PathBuf::from("dir/db.vlite.ann")
    );
}
