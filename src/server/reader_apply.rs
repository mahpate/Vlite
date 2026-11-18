//! feature-aca-ha ITEM-019: a reader that follows the writer's archived,
//! verified checkpoints. The reader serves one private follower file
//! read-only while it advances a second one; a verified, advanced file is
//! opened read-only and swapped in under the database write lock, so a read
//! sees either the old generation or the new one, never a partial apply.

use std::sync::{
    Mutex, PoisonError,
    atomic::{AtomicU64, Ordering},
};

use axum::http::StatusCode;
use serde_json::json;

use super::{ApiError, Service, log_stderr};
use crate::Database;

/// Apply progress shared by every clone and generation of one reader.
#[derive(Default)]
pub(super) struct Progress {
    /// Set once when applying stops for good; only a restart re-enrolls.
    failed: Mutex<Option<String>>,
    applied_sequence: AtomicU64,
    /// When the reader last confirmed that it serves the newest verified
    /// checkpoint of the archive it follows.
    confirmed_unix_ms: AtomicU64,
}

impl Progress {
    /// Stops applying; readiness fails from now on. The first reason wins.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn fail(&self, reason: String) {
        let mut failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        if failed.is_none() {
            log_stderr(
                "error",
                "reader_apply",
                json!({"event":"reader_apply_failed","reason":reason}).to_string(),
            );
            *failed = Some(reason);
        }
    }

    pub(super) fn failure(&self) -> Option<String> {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records that the served generation is at `sequence`, the newest
    /// verified checkpoint as of `at` (Unix milliseconds).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn confirm(&self, sequence: u64, at: u64) {
        self.applied_sequence.store(sequence, Ordering::SeqCst);
        self.confirmed_unix_ms.store(at, Ordering::SeqCst);
    }

    /// The served sequence and when it was last confirmed current.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn applied(&self) -> (u64, u64) {
        (
            self.applied_sequence.load(Ordering::SeqCst),
            self.confirmed_unix_ms.load(Ordering::SeqCst),
        )
    }
}

/// Readiness answer of a reader that stopped applying. The reason is kept
/// in the log; positions it may name are not part of the response.
pub(super) fn failed() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "reader_apply_failed",
        "this reader stopped following the writer's archive and needs a re-seed; restart it to enroll again",
    )
}

impl Service {
    /// The readiness refusal of a reader that stopped applying, if any.
    pub(super) fn reader_failure(&self) -> Option<ApiError> {
        self.reader
            .as_ref()
            .and_then(|progress| progress.failure())
            .map(|_| failed())
    }

    /// Replaces the served read-only database with `next`, a read-only
    /// handle on an advanced copy of the same file family. The swap holds
    /// the database write lock only for the exchange; the retired handle is
    /// closed afterwards, which releases its shared file lock so its file
    /// can be advanced again. A refused `next` is closed and the served
    /// generation is kept.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn swap_reader_generation(&self, next: Database) -> std::io::Result<()> {
        let refusal = if !next.is_read_only() {
            Some("the new generation is not read-only")
        } else if next.is_encrypted() {
            Some("the new generation is encrypted")
        } else if self.lineage.is_none() || next.lineage() != self.lineage {
            Some("the new generation has another lineage")
        } else if next.format_version() != self.format_version {
            Some("the new generation has another format version")
        } else if next.is_graph_enabled() != self.graph_enabled {
            Some("the new generation has another graph setting")
        } else {
            None
        };
        if let Some(reason) = refusal {
            return refuse(next, reason);
        }
        let previous = {
            let mut owner = self
                .database
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            match owner.as_mut() {
                Some(current) if current.is_read_only() => Ok(std::mem::replace(current, next)),
                Some(_) => Err((next, "the served database is writable")),
                None => Err((next, "the database is closed")),
            }
        };
        match previous {
            Ok(previous) => previous
                .close()
                .map_err(|error| std::io::Error::other(error.to_string())),
            Err((next, reason)) => refuse(next, reason),
        }
    }
}

/// Closes a refused generation outside the database lock.
fn refuse(next: Database, reason: &str) -> std::io::Result<()> {
    let _ = next.close();
    Err(std::io::Error::other(format!(
        "reader generation swap refused: {reason}"
    )))
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::Progress;
    use crate::{
        Database,
        server::{ServeMode, Service, router},
    };

    fn file(dir: &Path, name: &str, ids: std::ops::RangeInclusive<u64>) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut db = Database::create(&path, 1).unwrap();
        for id in ids {
            db.put(id, &[id as f32], b"").unwrap();
        }
        db.close().unwrap();
        path
    }

    fn advance(path: &Path, ids: std::ops::RangeInclusive<u64>) {
        let mut db = Database::open(path).unwrap();
        for id in ids {
            db.put(id, &[id as f32], b"").unwrap();
        }
        db.close().unwrap();
    }

    fn reader(path: &Path) -> Service {
        Service::new(Database::open_read_only(path).unwrap())
            .with_serve_mode(ServeMode::Reader)
            .unwrap()
    }

    async fn status(service: &Service, uri: &str) -> StatusCode {
        router(service.clone())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn a_swapped_generation_serves_the_advanced_copy_and_frees_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let served = file(dir.path(), "a.vlite", 1..=2);
        let advanced = dir.path().join("b.vlite");
        std::fs::copy(&served, &advanced).unwrap();
        advance(&advanced, 3..=4);
        let service = reader(&served);
        assert_eq!(
            status(&service, "/v1/vectors/4").await,
            StatusCode::NOT_FOUND
        );

        service
            .swap_reader_generation(Database::open_read_only(&advanced).unwrap())
            .unwrap();

        assert_eq!(status(&service, "/v1/vectors/4").await, StatusCode::OK);
        assert_eq!(status(&service, "/ready").await, StatusCode::OK);
        // The retired generation released its shared lock: the file can be
        // advanced again while the new one is served.
        advance(&served, 3..=5);
        assert!(matches!(
            Database::open(&advanced),
            Err(crate::Error::Locked)
        ));
    }

    #[tokio::test]
    async fn a_swap_is_refused_for_another_family_or_a_writable_side() {
        let dir = tempfile::tempdir().unwrap();
        let served = file(dir.path(), "a.vlite", 1..=2);
        let other = file(dir.path(), "other.vlite", 1..=4);
        let service = reader(&served);

        let error = service
            .swap_reader_generation(Database::open_read_only(&other).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("another lineage"), "{error}");
        // The refused handle was closed and the served one kept.
        Database::open(&other).unwrap().close().unwrap();
        assert_eq!(status(&service, "/v1/vectors/2").await, StatusCode::OK);
        assert_eq!(
            status(&service, "/v1/vectors/4").await,
            StatusCode::NOT_FOUND
        );

        let copy = dir.path().join("b.vlite");
        std::fs::copy(&served, &copy).unwrap();
        let error = service
            .swap_reader_generation(Database::open(&copy).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("not read-only"), "{error}");
        Database::open(&copy).unwrap().close().unwrap();

        let writable = file(dir.path(), "writer.vlite", 1..=2);
        let twin = dir.path().join("twin.vlite");
        std::fs::copy(&writable, &twin).unwrap();
        let service = Service::new(Database::open(&writable).unwrap());
        let error = service
            .swap_reader_generation(Database::open_read_only(&twin).unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("writable"), "{error}");
        Database::open(&twin).unwrap().close().unwrap();
    }

    #[tokio::test]
    async fn a_failed_reader_is_not_ready_and_keeps_its_first_reason() {
        let dir = tempfile::tempdir().unwrap();
        let served = file(dir.path(), "a.vlite", 1..=2);
        let progress = Arc::new(Progress::default());
        let mut service = reader(&served);
        service.reader = Some(progress.clone());
        assert_eq!(status(&service, "/ready").await, StatusCode::OK);

        progress.fail("a newer published set".into());
        progress.fail("later".into());

        assert_eq!(
            status(&service, "/ready").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(progress.failure().as_deref(), Some("a newer published set"));
        // Reads keep serving the last verified generation.
        assert_eq!(status(&service, "/v1/vectors/2").await, StatusCode::OK);
    }

    #[test]
    fn progress_reports_the_confirmed_position() {
        let progress = Progress::default();
        progress.confirm(7, 1_000);
        assert_eq!(progress.applied(), (7, 1_000));
    }
}
