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
// Thirty characters, as a float written out at length.
pub const LONG_FLOAT: &str = "0.1234567890123456789012345678";
// The limits on one metadata field, which the crate does not export: a field name of at most
// 256 bytes and a string value of at most 4,096.
pub const FIELD_NAME_BYTES: usize = 256;
pub const STRING_VALUE_BYTES: usize = 4096;
// The most fields of that size that fit the encoded metadata limit: after a 4-byte header, a
// string field takes 2 + name + 1 + 4 + value bytes.
pub const METADATA_FIELDS: usize =
    (vlite::MAX_METADATA_BYTES - 4) / (2 + FIELD_NAME_BYTES + 1 + 4 + STRING_VALUE_BYTES);
// The largest record the engine accepts, written as one JSON object: a payload of U+0001 (six
// characters each as a JSON escape), the most dimensions as floats of thirty characters and the
// most metadata fields of the largest size, 65,389 of the 65,536 encoded bytes allowed. It is
// about 8.7 MB.
pub fn largest_record_json(id: u64) -> String {
    let vector = vec![LONG_FLOAT; vlite::MAX_DIMENSIONS].join(",");
    let payload = "\\u0001".repeat(vlite::MAX_PAYLOAD_BYTES);
    let metadata = (0..METADATA_FIELDS)
        .map(|index| {
            format!(
                r#""{}{index:x}":{{"type":"string","value":"{}"}}"#,
                "k".repeat(FIELD_NAME_BYTES - 1),
                "\\u0001".repeat(STRING_VALUE_BYTES)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"id":{id},"vector":[{vector}],"payload":"{payload}","metadata":{{{metadata}}}}}"#)
}
pub fn command(binary: &str) -> Command {
    Command::new(binary)
}
