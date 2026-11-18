//! EPIC-002 (FR-006): the serve mode fixed at startup. `standalone` (the
//! default) is today's single server. `writer` and `reader` are the roles of
//! a split deployment: a reader serves a read-only database and refuses every
//! mutation and writer-only administration route at the route layer, after
//! the same authentication and authorization every mode applies (NFR-004).
//! Neither role adds write capacity; replication, epochs and failover belong
//! to later epics.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::State as Extract,
    http::{Method, StatusCode},
    routing::get,
};
use serde_json::{Value, json};

use super::{ApiError, Service, backup_policy::ArchiveStatus};

/// How this process serves its database; fixed for the process lifetime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServeMode {
    #[default]
    Standalone,
    Writer,
    Reader,
}

impl ServeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Writer => "writer",
            Self::Reader => "reader",
        }
    }
}

impl std::str::FromStr for ServeMode {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "standalone" => Ok(Self::Standalone),
            "writer" => Ok(Self::Writer),
            "reader" => Ok(Self::Reader),
            _ => Err("must be standalone, writer or reader"),
        }
    }
}

/// Shared by every clone and compaction generation of one service.
#[derive(Default)]
pub(super) struct State {
    pub(super) mode: ServeMode,
    draining: AtomicBool,
}

impl Service {
    /// Fixes the serve mode. Call it before building any router from this
    /// service. A reader needs a database from `Database::open_read_only`; a
    /// writer needs a writable one.
    pub fn with_serve_mode(mut self, mode: ServeMode) -> std::io::Result<Self> {
        let read_only = self
            .database
            .read()
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .as_ref()
            .ok_or_else(|| std::io::Error::other("database is closed"))?
            .is_read_only();
        match mode {
            ServeMode::Reader if !read_only => {
                return Err(std::io::Error::other(
                    "reader mode needs a read-only database (Database::open_read_only)",
                ));
            }
            ServeMode::Writer if read_only => {
                return Err(std::io::Error::other(
                    "writer mode needs a writable database",
                ));
            }
            _ => {}
        }
        self.serve = std::sync::Arc::new(State {
            mode,
            draining: AtomicBool::new(false),
        });
        Ok(self)
    }

    /// Called when a stop signal arrives: a writer or reader then answers
    /// `/ready` with 503 `draining` so a balancer stops routing to it. A
    /// standalone server keeps its existing readiness behavior.
    pub fn begin_drain(&self) {
        if self.serve.mode != ServeMode::Standalone {
            self.serve.draining.store(true, Ordering::SeqCst);
        }
    }

    pub(super) fn draining(&self) -> bool {
        self.serve.draining.load(Ordering::SeqCst)
    }
}

/// Route-layer admission, applied after authorization: a reader serves only
/// the requests that authorization already classifies as reads.
pub(super) fn admit(
    service: &Service,
    method: &Method,
    route: &str,
    path: &str,
) -> Result<(), ApiError> {
    if service.serve.mode != ServeMode::Reader || super::read_request(method, route, path) {
        // `/ready` applies its own failure precedence; status stays diagnostic.
        if !matches!(path, "/ready" | "/v1/admin/serve-status") && service.reader_stale(unix_ms()) {
            return Err(super::reader_staleness::stale());
        }
        return Ok(());
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "reader_mode",
        "this server runs in reader mode and refuses mutations and writer-only administration; send them to the writer",
    ))
}

pub(super) fn routes() -> Router<Service> {
    Router::new().route("/v1/admin/serve-status", get(status))
}

pub(super) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Administrator-only role and progress report, independent of `/metrics`.
/// While memory bindings make `/metrics` refuse, positions are withheld too:
/// they move with memory writes.
async fn status(Extract(service): Extract<Service>) -> Result<Json<Value>, ApiError> {
    let hide = service.metrics_blocked_by_memory();
    let sequence = if hide {
        None
    } else {
        service
            .run_read(|db| Ok(db.feed_cursor().ok().map(|cursor| cursor.sequence)))
            .await?
    };
    let archive = match service.admin.cloud.get() {
        Some(runtime) => runtime.archive_progress()?,
        None => None,
    };
    let now = unix_ms();
    let archive = archive.map(|archive| archive_report(archive, hide, now));
    Ok(Json(json!({
        "mode": service.serve.mode.as_str(),
        "draining": service.draining(),
        "read_only": service.serve.mode == ServeMode::Reader,
        "lineage": service.lineage.map(|lineage| format!("{lineage:016x}")),
        "binding_generation": service.binding_generation(),
        "format_version": service.format_version,
        "sequence": sequence,
        "epoch": null,
        "archive": archive,
        "reader": super::reader_staleness::report(&service, hide, now),
        "uptime_ms": u64::try_from(service.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "note": "writer/reader roles add no write capacity: one writer accepts every write; readers serve the state they opened",
    })))
}

/// `last_error` is the latest failed archive attempt, cleared by the next
/// verified checkpoint. Its message can name log positions, so it is
/// withheld with them; the code stays.
fn archive_report(archive: ArchiveStatus, hide: bool, now: u64) -> Value {
    json!({
        "baseline_set": archive.baseline_set,
        "baseline_checkpoints": archive.baseline_checkpoints,
        "checkpoints": archive.checkpoints,
        "failures": archive.failures,
        "last_checkpoint_sequence": if hide { None } else { archive.last_checkpoint_sequence },
        "last_checkpoint_unix_ms": archive.last_checkpoint_unix_ms,
        "last_checkpoint_age_ms": archive
            .last_checkpoint_unix_ms
            .map(|at| now.saturating_sub(at)),
        "last_error": archive.last_error.map(|error| json!({
            "code": error.code,
            "message": if hide { None } else { Some(error.message) },
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::super::backup_policy::{ArchiveStatus, LastError};
    use super::archive_report;

    fn failed() -> ArchiveStatus {
        ArchiveStatus {
            runs: 3,
            checkpoints: 1,
            failures: 2,
            baseline_set: Some("set-1".into()),
            baseline_checkpoints: 1,
            last_checkpoint_sequence: Some(41),
            last_checkpoint_unix_ms: Some(1_000),
            last_error: Some(LastError {
                code: "archive_log_floor".into(),
                message: "the live log no longer holds the commits after 41".into(),
            }),
            ..ArchiveStatus::default()
        }
    }

    #[test]
    fn archive_report_carries_the_last_error() {
        let report = archive_report(failed(), false, 1_500);
        assert_eq!(report["last_error"]["code"], "archive_log_floor");
        assert_eq!(
            report["last_error"]["message"],
            "the live log no longer holds the commits after 41"
        );
        assert_eq!(report["last_checkpoint_sequence"], 41);
        assert_eq!(report["last_checkpoint_age_ms"], 500);
        assert_eq!(report["failures"], 2);
    }

    #[test]
    fn hidden_positions_also_withhold_the_error_message() {
        let report = archive_report(failed(), true, 1_500);
        assert_eq!(report["last_error"]["code"], "archive_log_floor");
        assert!(report["last_error"]["message"].is_null(), "{report}");
        assert!(report["last_checkpoint_sequence"].is_null(), "{report}");
    }

    #[test]
    fn a_healthy_archive_reports_a_null_last_error() {
        let mut status = failed();
        status.last_error = None;
        assert!(archive_report(status, false, 1_500)["last_error"].is_null());
    }
}
