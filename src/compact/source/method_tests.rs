//! A-D matrix through real open, with slice-2 committed mutations enabled.
use super::dispatch_tests::{gates, point_parity, unsupported};
use crate::*;

fn compact_dispatch_fixture(encrypted: bool) -> (Database, Database, Vec<u8>) {
    let key = encrypted.then(super::super::catalog::tests::source_key);
    let image = super::super::catalog::tests::source_default_fixture(key.as_ref());
    let oracle = {
        let _source_off = super::Activation::set(false);
        Database::open_from_bytes(image.clone(), key.as_ref()).unwrap()
    };
    let candidate = Database::open_from_bytes(image.clone(), key.as_ref()).unwrap();
    (candidate, oracle, image)
}

// Keep every CollectionMut receiver in the closed method matrix. Only the
// selected committed-mutation methods are enabled by slice 2.
const COMPACT_REFUSALS: &[&str] = &[
    "CollectionMut::build_hnsw_with_metric",
    "CollectionMut::set_background_hnsw_rebuild",
    "CollectionMut::search_approximate_filtered_with_metric",
    "CollectionMut::start_hnsw_rebuild",
    "CollectionMut::poll_hnsw_rebuild",
    "CollectionMut::hnsw_maintenance_status",
    "CollectionMut::maintain_hnsw",
    "CollectionMut::import_chunk",
    "CollectionMut::finish_import",
    "CollectionMut::abort_import",
    "CollectionMut::search_auto",
    "CollectionMut::search_auto_with_settings",
    "CollectionMut::build_hnsw_with_options",
    "CollectionMut::start_hnsw_rebuild_with_options",
    "CollectionMut::hnsw_maintenance_status_with_options",
    "CollectionMut::maintain_hnsw_with_options",
    "CollectionMut::ann_memory",
    "CollectionMut::hnsw_health",
    "CollectionMut::ann_quality",
    "CollectionMut::search_approximate_ready_with_options",
    "CollectionMut::recommend",
    "CollectionMut::numeric_stats",
    "CollectionMut::search_geo",
    "CollectionMut::configure_vector",
    "CollectionMut::put_vectors",
    "CollectionMut::put_vector_version",
    "CollectionMut::prune_vector_history",
    "CollectionMut::rebuild_named_hnsw",
    "CollectionMut::begin_reembed",
    "CollectionMut::apply_reembed_batch_checked",
    "CollectionMut::apply_reembed_batch",
    "CollectionMut::finish_reembed",
];

const COMPACT_MUTATIONS: &[&str] = &[
    "Database::collection_mut",
    "Database::put",
    "Database::put_with_metadata",
    "Database::put_with_expiry",
    "Database::put_with_metadata_if",
    "Database::update_metadata",
    "Database::delete",
    "Database::delete_if",
    "Database::write_batch",
    "CollectionMut::view",
    "CollectionMut::put",
    "CollectionMut::put_with_metadata",
    "CollectionMut::put_with_expiry",
    "CollectionMut::update_metadata",
    "CollectionMut::put_with_metadata_if",
    "CollectionMut::delete",
    "CollectionMut::delete_if",
    "CollectionMut::write_batch",
];

#[test]
fn compact_installed_database_and_snapshot_method_matrix() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (mut db, mut oracle, image) = compact_dispatch_fixture(encrypted);
        let sequence = db.sequence;
        let end = db.end;
        let config = HnswConfig::default();
        let options = AnnOptions::default();
        let metric = DistanceMetric::SquaredL2;
        let limits = HnswRebuildLimits::default();
        let policy = HnswMaintenancePolicy::default();
        let cursor = changes::FeedCursor::beginning();
        let query = &[1.0, 2.0];
        let path_dir = tempfile::tempdir().unwrap();
        let path = path_dir.path().join("must-not-exist");
        macro_rules! d {
            ($method:literal, $result:expr) => {{
                unsupported($result, $method);
                assert_eq!(db.sequence, sequence, "{} sequence", $method);
                assert_eq!(db.end, end, "{} end", $method);
                assert_eq!(db.to_bytes().unwrap(), image, "{} bytes", $method);
                assert!(!path.exists(), "{} side effect", $method);
                point_parity(&db, &oracle);
            }};
        }
        macro_rules! a {
            ($method:literal, $actual:expr, $expected:expr) => {
                assert_eq!($actual, $expected, "{}", $method);
            };
        }
        a!("Database::dimensions", db.dimensions(), oracle.dimensions());
        a!(
            "Database::format_version",
            db.format_version(),
            oracle.format_version()
        );
        a!("Database::lineage", db.lineage(), oracle.lineage());
        a!(
            "Database::is_read_only",
            db.is_read_only(),
            oracle.is_read_only()
        );
        a!(
            "Database::is_poisoned",
            db.is_poisoned(),
            oracle.is_poisoned()
        );
        a!(
            "Database::is_directory",
            db.is_directory(),
            oracle.is_directory()
        );
        a!(
            "Database::is_encrypted",
            db.is_encrypted(),
            oracle.is_encrypted()
        );
        a!(
            "Database::is_graph_enabled",
            db.is_graph_enabled(),
            oracle.is_graph_enabled()
        );
        a!(
            "Database::encryption_key_id",
            db.encryption_key_id(),
            oracle.encryption_key_id()
        );
        a!(
            "Database::has_commit_timestamps",
            db.has_commit_timestamps(),
            oracle.has_commit_timestamps()
        );
        a!(
            "Database::unsynced_commits",
            db.unsynced_commits(),
            oracle.unsynced_commits()
        );
        a!("Database::durability", db.durability(), oracle.durability());
        a!("Database::len", db.len(), oracle.len());
        a!("Database::is_empty", db.is_empty(), oracle.is_empty());
        a!("Database::get", db.get(0).unwrap(), oracle.get(0).unwrap());
        a!(
            "Database::get_at",
            db.get_at(0, 0).unwrap(),
            oracle.get_at(0, 0).unwrap()
        );
        a!(
            "Database::revision",
            db.revision(0).unwrap(),
            oracle.revision(0).unwrap()
        );
        a!(
            "Database::expires_at",
            db.expires_at(0).unwrap(),
            oracle.expires_at(0).unwrap()
        );
        a!(
            "Database::collections",
            db.collections().unwrap(),
            oracle.collections().unwrap()
        );
        a!(
            "Database::collection",
            db.collection("full").unwrap().id(),
            oracle.collection("full").unwrap().id()
        );
        a!(
            "Database::text_index_status",
            db.text_index_status(),
            oracle.text_index_status()
        );
        a!(
            "Database::snapshot_retention_stats",
            db.snapshot_retention_stats(),
            oracle.snapshot_retention_stats()
        );
        // B: no compact index job can start, so cancellation is truthfully false.
        a!(
            "Database::cancel_hnsw_rebuild",
            db.cancel_hnsw_rebuild(),
            oracle.cancel_hnsw_rebuild()
        );
        assert!(!db.cancel_hnsw_rebuild());
        // C: raw bytes and configuration do not scan/mutate records.
        a!("Database::to_bytes", db.to_bytes(), oracle.to_bytes());
        db.set_commit_clock(Box::new(|| 0));
        db.set_checkpoint_threshold(Some(1));
        a!(
            "Database::set_commit_clock",
            db.get_at(0, 0).unwrap(),
            oracle.get_at(0, 0).unwrap()
        );
        a!(
            "Database::set_checkpoint_threshold",
            db.to_bytes(),
            oracle.to_bytes()
        );
        point_parity(&db, &oracle);
        let snapshot = db.read_snapshot().unwrap();
        let reference = oracle.read_snapshot().unwrap();
        a!(
            "Database::read_snapshot",
            snapshot.sequence(),
            reference.sequence()
        );
        a!(
            "ReadSnapshot::sequence",
            snapshot.sequence(),
            reference.sequence()
        );
        a!(
            "ReadSnapshot::lineage",
            snapshot.lineage(),
            reference.lineage()
        );
        a!(
            "ReadSnapshot::dimensions",
            snapshot.dimensions(),
            reference.dimensions()
        );
        a!("ReadSnapshot::len", snapshot.len(), reference.len());
        a!(
            "ReadSnapshot::is_empty",
            snapshot.is_empty(),
            reference.is_empty()
        );
        a!(
            "ReadSnapshot::get",
            snapshot.get(0).unwrap(),
            reference.get(0).unwrap()
        );
        a!(
            "ReadSnapshot::get_at",
            snapshot.get_at(0, 0).unwrap(),
            reference.get_at(0, 0).unwrap()
        );
        a!(
            "ReadSnapshot::browse",
            snapshot.browse(None, 1).unwrap(),
            reference.browse(None, 1).unwrap()
        );
        a!(
            "ReadSnapshot::browse_at",
            snapshot.browse_at(None, 1, 0).unwrap(),
            reference.browse_at(None, 1, 0).unwrap()
        );
        a!(
            "ReadSnapshot::search",
            snapshot.search(query, 2).unwrap(),
            reference.search(query, 2).unwrap()
        );
        a!(
            "ReadSnapshot::search_filtered_with_metric",
            snapshot
                .search_filtered_with_metric(query, 2, &[], metric)
                .unwrap(),
            reference
                .search_filtered_with_metric(query, 2, &[], metric)
                .unwrap()
        );
        a!(
            "ReadSnapshot::search_filtered_with_metric_at",
            snapshot
                .search_filtered_with_metric_at(query, 2, &[], metric, 0)
                .unwrap(),
            reference
                .search_filtered_with_metric_at(query, 2, &[], metric, 0)
                .unwrap()
        );
        drop(snapshot);
        drop(reference);
        assert!(
            COMPACT_REFUSALS
                .iter()
                .all(|method| !COMPACT_MUTATIONS.contains(method))
        );
        {
            let mut mutable = db.collection_mut("full").unwrap();
            a!(
                "Database::collection_mut",
                mutable.view().len(),
                oracle.collection("full").unwrap().len()
            );
            a!(
                "CollectionMut::text_index_status",
                mutable.text_index_status(),
                oracle.collection("full").unwrap().text_index_status()
            );
            a!(
                "CollectionMut::cancel_hnsw_rebuild",
                mutable.cancel_hnsw_rebuild(),
                false
            );
            a!(
                "CollectionMut::browse",
                mutable.browse(None, 1).unwrap(),
                oracle.collection("full").unwrap().browse(None, 1).unwrap()
            );
        }
        d!(
            "CollectionMut::configure_vector",
            db.collection_mut("full").unwrap().configure_vector(
                "new",
                vectors::VectorSpec {
                    dimensions: 2,
                    metric,
                    active_version: "v1".into(),
                    history: 1,
                }
            )
        );
        d!(
            "CollectionMut::import_chunk",
            db.collection_mut("full").unwrap().import_chunk(
                1,
                1,
                &[],
                import::InvalidRowPolicy::RejectChunk
            )
        );
        d!(
            "CollectionMut::build_hnsw_with_metric",
            db.collection_mut("full")
                .unwrap()
                .build_hnsw_with_metric(config, metric)
        );
        d!("Database::sync", db.sync());
        d!("Database::checkpoint", db.checkpoint());
        d!(
            "Database::set_durability",
            db.set_durability(Durability::None)
        );
        d!("Database::feed_cursor", db.feed_cursor());
        d!("Database::changes_after", db.changes_after(&cursor, 0));
        for events in [changes::GraphEventSet::Empty, changes::GraphEventSet::Ttl] {
            let graph_cursor = changes::GraphFeedCursor::beginning(events);
            d!(
                "Database::graph_changes_after",
                db.graph_changes_after(&graph_cursor, events, 0)
                    .map_err(|error| match error {
                        changes::GraphFeedError::Engine(error) => error,
                        other =>
                            panic!("graph feed {events:?} must refuse compact records: {other:?}"),
                    })
            );
        }
        d!(
            "Database::temporal_changes_after",
            db.temporal_changes_after(&cursor, 0)
        );
        d!("Database::export_log", db.export_log(0, 0));
        d!(
            "Database::create_collection",
            db.create_collection(
                "new",
                CollectionSpec {
                    dimensions: 2,
                    settings: CollectionSettings::default()
                }
            )
        );
        d!("Database::drop_collection", db.drop_collection("full"));
        {
            let (mut actual, mut expected, _) = compact_dispatch_fixture(encrypted);
            a!(
                "Database::set_collection_settings",
                actual
                    .set_collection_settings("full", CollectionSettings::default())
                    .unwrap(),
                expected
                    .set_collection_settings("full", CollectionSettings::default())
                    .unwrap()
            );
            assert_eq!(actual.sequence, expected.sequence);
            point_parity(&actual, &expected);
        }
        d!("Database::next_import_job_id", db.next_import_job_id());
        d!(
            "Database::import_chunk",
            db.import_chunk(1, 1, &[], import::InvalidRowPolicy::RejectChunk)
        );
        d!("Database::finish_import", db.finish_import(1));
        d!("Database::abort_import", db.abort_import(1));
        d!("Database::import_status", db.import_status(1));
        d!("Database::kv_store", db.kv_store("full"));
        assert!(matches!(db.kv_view("full"), Err(Error::NotKvCollection)));
        d!("Database::create_graph", db.create_graph("new"));
        d!("Database::drop_graph", db.drop_graph("g"));
        d!("Database::list_graphs", db.list_graphs());
        d!("Database::graph", db.graph("g"));
        d!("Database::graph_mut", db.graph_mut("g"));
        d!(
            "Database::set_background_hnsw_rebuild",
            db.set_background_hnsw_rebuild(None)
        );
        d!("Database::build_hnsw", db.build_hnsw(config));
        d!(
            "Database::build_hnsw_with_metric",
            db.build_hnsw_with_metric(config, metric)
        );
        d!(
            "Database::build_hnsw_with_options",
            db.build_hnsw_with_options(config, metric, options)
        );
        d!(
            "Database::start_hnsw_rebuild",
            db.start_hnsw_rebuild(config, metric, limits)
        );
        d!(
            "Database::start_hnsw_rebuild_with_options",
            db.start_hnsw_rebuild_with_options(config, metric, options, limits)
        );
        d!("Database::poll_hnsw_rebuild", db.poll_hnsw_rebuild());
        d!(
            "Database::hnsw_maintenance_status",
            db.hnsw_maintenance_status(config, metric, policy)
        );
        d!(
            "Database::hnsw_maintenance_status_with_options",
            db.hnsw_maintenance_status_with_options(config, metric, options, policy)
        );
        d!(
            "Database::maintain_hnsw",
            db.maintain_hnsw(config, metric, policy)
        );
        d!(
            "Database::maintain_hnsw_with_options",
            db.maintain_hnsw_with_options(config, metric, options, policy)
        );
        d!(
            "Database::ann_memory",
            db.ann_memory(config, metric, options)
        );
        d!(
            "Database::hnsw_health",
            db.hnsw_health(config, metric, options)
        );
        d!(
            "Database::ann_quality",
            db.ann_quality(
                config,
                metric,
                options,
                quality::QualitySampleOptions::default()
            )
        );
        d!("Database::save_ann_snapshot", db.save_ann_snapshot(&path));
        d!("Database::load_ann_snapshot", db.load_ann_snapshot(&path));
        d!(
            "Database::build_disk_ann",
            db.build_disk_ann("full", &path, config, metric)
        );
        let (_, plain_oracle, _) = compact_dispatch_fixture(false);
        let sidecar = path_dir.path().join("oracle.disk-ann");
        plain_oracle
            .build_disk_ann("full", &sidecar, config, metric)
            .unwrap();
        let disk =
            disk_ann::DiskAnnIndex::open(&sidecar, disk_ann::DiskAnnOptions::default()).unwrap();
        d!(
            "Database::search_disk_ann",
            db.search_disk_ann(&disk, query, 0, 1)
        );
        a!(
            "Database::search",
            db.search(query, 2).unwrap(),
            oracle.search(query, 2).unwrap()
        );
        a!(
            "Database::search_at",
            db.search_at(query, 2, 0).unwrap(),
            oracle.search_at(query, 2, 0).unwrap()
        );
        a!(
            "Database::search_filtered",
            db.search_filtered(query, 2, &[]).unwrap(),
            oracle.search_filtered(query, 2, &[]).unwrap()
        );
        a!(
            "Database::search_with_metric",
            db.search_with_metric(query, 2, metric).unwrap(),
            oracle.search_with_metric(query, 2, metric).unwrap()
        );
        a!(
            "Database::search_filtered_with_metric",
            db.search_filtered_with_metric(query, 2, &[], metric)
                .unwrap(),
            oracle
                .search_filtered_with_metric(query, 2, &[], metric)
                .unwrap()
        );
        a!(
            "Database::search_filtered_with_metric_at",
            db.search_filtered_with_metric_at(query, 2, &[], metric, 0)
                .unwrap(),
            oracle
                .search_filtered_with_metric_at(query, 2, &[], metric, 0)
                .unwrap()
        );
        d!(
            "Database::search_approximate",
            db.search_approximate(query, 0, 1, config)
        );
        d!(
            "Database::search_approximate_with_metric",
            db.search_approximate_with_metric(query, 0, 1, config, metric)
        );
        d!(
            "Database::search_approximate_filtered_with_metric",
            db.search_approximate_filtered_with_metric(query, 0, 1, config, &[], metric)
        );
        d!(
            "Database::search_approximate_ready",
            db.search_approximate_ready(query, 0, 1, config)
        );
        d!(
            "Database::search_approximate_ready_with_metric",
            db.search_approximate_ready_with_metric(query, 0, 1, config, metric)
        );
        d!(
            "Database::search_approximate_ready_filtered_with_metric",
            db.search_approximate_ready_filtered_with_metric(query, 0, 1, config, &[], metric)
        );
        d!(
            "Database::search_approximate_ready_with_options",
            db.search_approximate_ready_with_options(query, 0, 1, config, options, &[], metric)
        );
        d!(
            "Database::search_auto",
            db.search_auto(query, 0, &[], metric, config, options, 1)
        );
        a!(
            "Database::search_hybrid",
            db.search_hybrid(&hybrid::HybridQuery::new(Some(query.to_vec()), "", 0))
                .unwrap(),
            oracle
                .search_hybrid(&hybrid::HybridQuery::new(Some(query.to_vec()), "", 0))
                .unwrap()
        );
        a!(
            "Database::browse",
            db.browse(None, 1).unwrap(),
            oracle.browse(None, 1).unwrap()
        );
        a!(
            "Database::browse_at",
            db.browse_at(None, 1, 0).unwrap(),
            oracle.browse_at(None, 1, 0).unwrap()
        );
        a!(
            "Database::facets",
            db.facets("x", &[]).unwrap(),
            oracle.facets("x", &[]).unwrap()
        );
        d!("Database::numeric_stats", db.numeric_stats("x", &[]));
        let recommendation = RecommendationQuery {
            positive_ids: vec![0],
            negative_ids: vec![],
            positive_vectors: vec![],
            negative_vectors: vec![],
            k: 0,
            conditions: vec![],
            metric,
        };
        let geo = GeoSearchQuery {
            vector: query.to_vec(),
            k: 0,
            field: "place".into(),
            latitude: 0.0,
            longitude: 0.0,
            radius_meters: 1.0,
            conditions: vec![],
            metric,
            sort: GeoSort::GeoDistance,
        };
        d!("Database::recommend", db.recommend(&recommendation));
        d!("Database::search_geo", db.search_geo(&geo));
        a!(
            "Database::search_grouped",
            db.search_grouped(query, 0, 0, "x", &[], metric).unwrap(),
            oracle
                .search_grouped(query, 0, 0, "x", &[], metric)
                .unwrap()
        );
        point_parity(&db, &oracle);
        // Consuming D methods use fresh real-open handles.
        let key = encrypted.then(super::super::catalog::tests::source_key);
        unsupported(
            Database::open_from_bytes(image.clone(), key.as_ref())
                .unwrap()
                .with_durability(Durability::None),
            "Database::with_durability",
        );
        unsupported(
            Database::open_from_bytes(image, key.as_ref())
                .unwrap()
                .close(),
            "Database::close",
        );
    }
}

#[test]
fn compact_installed_collection_method_matrix() {
    let _gates = gates();
    for encrypted in [false, true] {
        let (mut db, mut oracle, bytes) = compact_dispatch_fixture(encrypted);
        let config = HnswConfig::default();
        let options = AnnOptions::default();
        let metric = DistanceMetric::SquaredL2;
        for info in oracle.collections().unwrap() {
            let expected_browse = oracle
                .collection(&info.name)
                .unwrap()
                .browse(None, 1)
                .unwrap();
            {
                let mutable = db.collection_mut(&info.name).unwrap();
                let expected_mutable = oracle.collection_mut(&info.name).unwrap();
                assert_eq!(
                    mutable.browse(None, 1).unwrap(),
                    expected_browse,
                    "CollectionMut::browse"
                );
                assert_eq!(
                    mutable
                        .search_hybrid(&hybrid::HybridQuery::new(Some(vec![1.0, 2.0]), "", 0))
                        .unwrap(),
                    expected_mutable
                        .search_hybrid(&hybrid::HybridQuery::new(Some(vec![1.0, 2.0]), "", 0))
                        .unwrap(),
                    "CollectionMut::search_hybrid"
                );
                assert_eq!(
                    mutable
                        .search_grouped(&[1.0, 2.0], 0, 0, "x", &[], DistanceMetric::SquaredL2)
                        .unwrap(),
                    expected_mutable
                        .search_grouped(&[1.0, 2.0], 0, 0, "x", &[], DistanceMetric::SquaredL2)
                        .unwrap(),
                    "CollectionMut::search_grouped"
                );
                assert_eq!(
                    mutable.facets("x", &[]).unwrap(),
                    expected_mutable.facets("x", &[]).unwrap(),
                    "CollectionMut::facets"
                );
            }
            let view = db.collection(&info.name).unwrap();
            let reference = oracle.collection(&info.name).unwrap();
            let query = &[1.0, 2.0];
            macro_rules! a {
                ($method:literal, $actual:expr, $expected:expr) => {
                    assert_eq!($actual, $expected, "{}", $method);
                };
            }
            macro_rules! d {
                ($method:literal, $result:expr) => {{
                    unsupported($result, $method);
                    assert_eq!(db.sequence, oracle.sequence, "{} sequence", $method);
                    assert_eq!(db.to_bytes().unwrap(), bytes, "{} bytes", $method);
                    point_parity(&db, &oracle);
                }};
            }
            a!("CollectionRef::id", view.id(), reference.id());
            a!("CollectionRef::name", view.name(), reference.name());
            a!(
                "CollectionRef::dimensions",
                view.dimensions(),
                reference.dimensions()
            );
            a!(
                "CollectionRef::settings",
                view.settings(),
                reference.settings()
            );
            a!("CollectionRef::len", view.len(), reference.len());
            a!(
                "CollectionRef::is_empty",
                view.is_empty(),
                reference.is_empty()
            );
            a!(
                "CollectionRef::get",
                view.get(0).unwrap(),
                reference.get(0).unwrap()
            );
            a!(
                "CollectionRef::get_at",
                view.get_at(0, 0).unwrap(),
                reference.get_at(0, 0).unwrap()
            );
            a!(
                "CollectionRef::revision",
                view.revision(0).unwrap(),
                reference.revision(0).unwrap()
            );
            a!(
                "CollectionRef::expires_at",
                view.expires_at(0).unwrap(),
                reference.expires_at(0).unwrap()
            );
            a!(
                "CollectionRef::text_index_status",
                view.text_index_status(),
                reference.text_index_status()
            );
            a!(
                "CollectionRef::read_snapshot",
                view.read_snapshot().unwrap().sequence(),
                reference.read_snapshot().unwrap().sequence()
            );
            a!(
                "CollectionRef::search",
                view.search(query, 2).unwrap(),
                reference.search(query, 2).unwrap()
            );
            a!(
                "CollectionRef::search_filtered",
                view.search_filtered(query, 2, &[]).unwrap(),
                reference.search_filtered(query, 2, &[]).unwrap()
            );
            a!(
                "CollectionRef::search_with_metric",
                view.search_with_metric(query, 2, metric).unwrap(),
                reference.search_with_metric(query, 2, metric).unwrap()
            );
            a!(
                "CollectionRef::search_filtered_with_metric",
                view.search_filtered_with_metric(query, 2, &[], metric)
                    .unwrap(),
                reference
                    .search_filtered_with_metric(query, 2, &[], metric)
                    .unwrap()
            );
            a!(
                "CollectionRef::search_filtered_with_metric_at",
                view.search_filtered_with_metric_at(query, 2, &[], metric, 0)
                    .unwrap(),
                reference
                    .search_filtered_with_metric_at(query, 2, &[], metric, 0)
                    .unwrap()
            );
            a!(
                "CollectionRef::search_with_stats",
                view.search_with_stats(query, 2, &[], metric).unwrap(),
                reference.search_with_stats(query, 2, &[], metric).unwrap()
            );
            d!(
                "CollectionRef::search_approximate_ready_filtered_with_metric",
                view.search_approximate_ready_filtered_with_metric(
                    query,
                    0,
                    1,
                    config,
                    &[],
                    metric
                )
            );
            d!(
                "CollectionRef::search_approximate_ready_with_options",
                view.search_approximate_ready_with_options(
                    query,
                    0,
                    1,
                    config,
                    options,
                    &[],
                    metric
                )
            );
            d!(
                "CollectionRef::search_auto",
                view.search_auto(query, 0, &[], metric, config, options, 1)
            );
            d!(
                "CollectionRef::search_auto_with_settings",
                view.search_auto_with_settings(query, 0, &[], 1)
            );
            d!(
                "CollectionRef::ann_memory",
                view.ann_memory(config, metric, options)
            );
            d!(
                "CollectionRef::hnsw_health",
                view.hnsw_health(config, metric, options)
            );
            d!(
                "CollectionRef::ann_quality",
                view.ann_quality(
                    config,
                    metric,
                    options,
                    quality::QualitySampleOptions::default()
                )
            );
            a!(
                "CollectionRef::browse",
                view.browse(None, 1).unwrap(),
                reference.browse(None, 1).unwrap()
            );
            a!(
                "CollectionRef::browse_at",
                view.browse_at(None, 1, 0).unwrap(),
                reference.browse_at(None, 1, 0).unwrap()
            );
            a!(
                "CollectionRef::facets",
                view.facets("x", &[]).unwrap(),
                reference.facets("x", &[]).unwrap()
            );
            d!("CollectionRef::numeric_stats", view.numeric_stats("x", &[]));
            d!("CollectionRef::vector_specs", view.vector_specs());
            a!(
                "CollectionRef::record_vectors",
                view.record_vectors(0).unwrap(),
                reference.record_vectors(0).unwrap()
            );
            assert_eq!(
                view.search_named("n", None, query, 0).is_err(),
                reference.search_named("n", None, query, 0).is_err(),
                "CollectionRef::search_named"
            );
            assert_eq!(
                view.search_named_at("n", None, query, 0, 0).is_err(),
                reference.search_named_at("n", None, query, 0, 0).is_err(),
                "CollectionRef::search_named_at"
            );
            d!(
                "CollectionRef::search_named_ann",
                view.search_named_ann("n", None, query, 0, 1)
            );
            d!(
                "CollectionRef::search_named_ann_at",
                view.search_named_ann_at("n", None, query, 0, 1, 0)
            );
            d!(
                "CollectionRef::search_sparse",
                view.search_sparse("s", &[], 0)
            );
            let dense_sparse = vectors::DenseSparseQuery {
                dense_name: "n".into(),
                version: None,
                dense: query.to_vec(),
                sparse_name: "s".into(),
                sparse: vec![],
                k: 1,
                fusion: hybrid::FusionOptions::default(),
            };
            if reference.vector_specs().unwrap().contains_key("n") {
                d!(
                    "CollectionRef::search_dense_sparse",
                    view.search_dense_sparse(&dense_sparse)
                );
            } else {
                assert!(matches!(
                    view.search_dense_sparse(&dense_sparse),
                    Err(Error::InvalidQuery(_))
                ));
            }
            d!("CollectionRef::reembed_status", view.reembed_status("n"));
            assert_eq!(
                view.reembed_pending("n", 0).is_err(),
                reference.reembed_pending("n", 0).is_err(),
                "CollectionRef::reembed_pending"
            );
            assert_eq!(
                view.reembed_pending_checked("n", 0).is_err(),
                reference.reembed_pending_checked("n", 0).is_err(),
                "CollectionRef::reembed_pending_checked"
            );
            a!(
                "CollectionRef::rerank",
                view.rerank(query, &[], 0, &rerank::ExactReranker { metric })
                    .unwrap(),
                reference
                    .rerank(query, &[], 0, &rerank::ExactReranker { metric })
                    .unwrap()
            );
            a!(
                "CollectionRef::search_hybrid",
                view.search_hybrid(&hybrid::HybridQuery::new(Some(query.to_vec()), "", 0))
                    .unwrap(),
                reference
                    .search_hybrid(&hybrid::HybridQuery::new(Some(query.to_vec()), "", 0))
                    .unwrap()
            );
            a!(
                "CollectionRef::search_grouped",
                view.search_grouped(query, 0, 0, "x", &[], metric).unwrap(),
                reference
                    .search_grouped(query, 0, 0, "x", &[], metric)
                    .unwrap()
            );
            d!(
                "CollectionRef::recommend",
                view.recommend(&RecommendationQuery {
                    positive_ids: vec![0],
                    negative_ids: vec![],
                    positive_vectors: vec![],
                    negative_vectors: vec![],
                    k: 0,
                    conditions: vec![],
                    metric
                })
            );
            d!(
                "CollectionRef::search_geo",
                view.search_geo(&GeoSearchQuery {
                    vector: query.to_vec(),
                    k: 0,
                    field: "place".into(),
                    latitude: 0.0,
                    longitude: 0.0,
                    radius_meters: 1.0,
                    conditions: vec![],
                    metric,
                    sort: GeoSort::GeoDistance
                })
            );
        }
        point_parity(&db, &oracle);
    }
}

#[test]
fn compact_installed_restricted_method_guards_refuse() {
    let _gates = gates();
    let (mut db, _, _) = compact_dispatch_fixture(false);
    let name = db
        .collections
        .iter()
        .find_map(|(_, state)| state.records.is_compact().then_some(state.name.clone()))
        .expect("fixture has a compact collection");
    let query = &[1.0, 2.0];
    let metric = DistanceMetric::SquaredL2;
    let invalid_config = HnswConfig {
        m: 1,
        ef_construction: 1,
        seed: 42,
    };
    let options = AnnOptions::default();
    unsupported(
        db.collection_mut(&name)
            .unwrap()
            .rebuild_named_hnsw("n", None, invalid_config),
        "P3B2-V8 rebuild_named_hnsw",
    );
    let view = db.collection(&name).unwrap();
    assert!(
        view.search_grouped(query, 1, 1, "marker", &[], metric)
            .is_ok()
    );
    unsupported(
        view.recommend(&RecommendationQuery {
            positive_ids: vec![],
            negative_ids: vec![],
            positive_vectors: vec![vec![1.0]],
            negative_vectors: vec![],
            k: 1,
            conditions: vec![],
            metric,
        }),
        "P3B2-R3 recommend",
    );
    assert!(view.facets("marker", &[]).is_ok());
    unsupported(view.numeric_stats("", &[]), "P3B2-R5 numeric_stats");
    assert!(matches!(
        view.search_hybrid(&hybrid::HybridQuery::new(Some(query.to_vec()), "", 1_001)),
        Err(Error::InvalidQuery("hybrid k must be at most 1000"))
    ));
    unsupported(
        view.search_auto(&[], 1, &[], metric, HnswConfig::default(), options, 1),
        "P3B2-P1 planner search",
    );
    assert!(matches!(
        crate::query_language::parse("SEARCH TEXT MATCH \"word\"")
            .unwrap()
            .execute(view, &std::collections::BTreeMap::new()),
        Err(Error::InvalidQuery("text fields must be declared"))
    ));
}
