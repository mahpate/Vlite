#![allow(dead_code)]
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use vlite::{Database, DistanceMetric, FormatVersion, vectors::VectorSpec};
static NEXT: AtomicU64 = AtomicU64::new(0);
pub struct Fixture {
    pub path: PathBuf,
}
impl Fixture {
    pub fn new() -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("vector-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "{}-{}-{}.vlite",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Self { path }
    }
    pub fn db(&self) -> Database {
        Database::create_with_format(&self.path, 2, FormatVersion::V5).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
pub fn spec(dimensions: usize, metric: DistanceMetric) -> VectorSpec {
    VectorSpec {
        dimensions,
        metric,
        active_version: "v1".into(),
        history: 2,
    }
}
pub fn seed(db: &mut Database) {
    for id in 1..=4 {
        db.put(id, &[id as f32, 1.0], format!("document {id}").as_bytes())
            .unwrap();
    }
    let mut c = db.collection_mut("default").unwrap();
    c.configure_vector("text", spec(2, DistanceMetric::SquaredL2))
        .unwrap();
    for id in 1..=4 {
        c.put_vector_version(id, "text", "v1", vec![id as f32, 1.0])
            .unwrap();
    }
}
