use std::{thread, time::Duration};
use vlite::{Database, DistanceMetric, HnswConfig, HnswRebuildLimits, HnswRebuildStatus};

fn main() -> vlite::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut db = Database::create(directory.path().join("background.vlite"), 3)?;
    db.put(1, &[1.0, 0.0, 0.0], b"first")?;
    let config = HnswConfig::default();
    db.start_hnsw_rebuild(config, DistanceMetric::Cosine, HnswRebuildLimits::default())?;
    db.put(2, &[0.9, 0.1, 0.0], b"committed while building")?;
    loop {
        match db.poll_hnsw_rebuild()? {
            HnswRebuildStatus::Running => thread::sleep(Duration::from_millis(1)),
            HnswRebuildStatus::Published => break,
            status => {
                return Err(
                    std::io::Error::other(format!("rebuild did not publish: {status:?}")).into(),
                );
            }
        }
    }
    let hits = db.search_approximate_ready_with_metric(
        &[1.0, 0.0, 0.0],
        2,
        10,
        config,
        DistanceMetric::Cosine,
    )?;
    assert_eq!(hits.len(), 2);
    db.close()
}
