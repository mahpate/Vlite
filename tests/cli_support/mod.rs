#![allow(dead_code)]
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
pub struct Fixture(PathBuf);
impl Fixture {
    pub fn new() -> Self {
        let path = std::env::current_dir().unwrap().join(format!(
            ".vlite-cli-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
pub fn database(path: &Path) {
    let mut db = vlite::Database::create_with_format(path, 2, vlite::FormatVersion::V4).unwrap();
    for id in 0..20 {
        db.put(id, &[id as f32, 1.0], b"fixture").unwrap();
    }
    db.close().unwrap();
}
pub fn command(binary: &str) -> Command {
    Command::new(binary)
}
