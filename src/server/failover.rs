//! Operator-acknowledged failover preparation; no election or automatic retry.

use std::{ffi::OsString, io::Read, path::PathBuf};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    authority::{AuthorityError, AuthorityStore, HeadRef, Observed, Phase},
    backup_policy::ArchiveStatus,
    gateway_mode::{flags, utf8},
};

const FAILOVER_USAGE: &str = "vlite-server failover report|execute --target-file <json> [--shared-key-file <path>] [--archive-status-file <json>] [--observed-sequence <n> --observed-unix-ms <ms>] [execute only, all required: --ack-report-id <id> --expected-epoch <n> --expected-revision <rev> --old-replicas <n>]";

struct Request {
    status: Option<ArchiveStatus>,
    observation: Option<WriterObservation>,
    ack: Option<FailoverAck>,
}

fn number(name: &str, value: OsString) -> Result<u64, String> {
    utf8(name, value)?
        .parse()
        .map_err(|_| format!("invalid_arguments: {name} must be an unsigned 64-bit integer"))
}

fn identifier(name: &str, value: OsString) -> Result<String, String> {
    let value = utf8(name, value)?;
    if value.is_empty() || value.len() > 256 {
        return Err(format!(
            "invalid_arguments: {name} must contain 1 to 256 UTF-8 bytes"
        ));
    }
    Ok(value)
}

/// Blocking operator command; call off the async runtime.
pub fn failover_command(args: &[OsString]) -> Result<Value, String> {
    let verb = match args.first().and_then(|arg| arg.to_str()) {
        Some("report") => "report",
        Some("execute") => "execute",
        _ => "invalid",
    };
    let result = (|| {
        let usage = |detail: &str| format!("invalid_arguments: {detail}; usage: {FAILOVER_USAGE}");
        if verb == "invalid" {
            return Err(usage("the verb must be report or execute"));
        }
        let mut allowed = vec![
            "--target-file",
            "--shared-key-file",
            "--archive-status-file",
            "--observed-sequence",
            "--observed-unix-ms",
        ];
        if verb == "execute" {
            allowed.extend([
                "--ack-report-id",
                "--expected-epoch",
                "--expected-revision",
                "--old-replicas",
            ]);
        }
        let mut values = flags(&args[1..], &allowed, FAILOVER_USAGE)?;
        let mut required = |name: &str| {
            values
                .remove(name)
                .ok_or_else(|| usage(&format!("{name} is required")))
        };
        let target = PathBuf::from(required("--target-file")?);
        let ack = if verb == "execute" {
            Some(FailoverAck {
                report_id: identifier("--ack-report-id", required("--ack-report-id")?)?,
                expected_epoch: number("--expected-epoch", required("--expected-epoch")?)?,
                expected_revision: identifier(
                    "--expected-revision",
                    required("--expected-revision")?,
                )?,
                old_replicas: number("--old-replicas", required("--old-replicas")?)?,
            })
        } else {
            None
        };
        let observation = match (
            values.remove("--observed-sequence"),
            values.remove("--observed-unix-ms"),
        ) {
            (None, None) => None,
            (Some(sequence), Some(time)) => Some(WriterObservation {
                sequence: number("--observed-sequence", sequence)?,
                observed_unix_ms: number("--observed-unix-ms", time)?,
            }),
            _ => {
                return Err(
                    "invalid_arguments: --observed-sequence and --observed-unix-ms go together"
                        .into(),
                );
            }
        };
        let status = values
            .remove("--archive-status-file")
            .map(|path| -> Result<ArchiveStatus, String> {
                const LIMIT: u64 = 64 * 1024;
                let mut bytes = Vec::new();
                std::fs::File::open(PathBuf::from(path))
                    .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
                    .map_err(|error| {
                        format!(
                            "invalid_arguments: --archive-status-file cannot be read: {}",
                            error.kind()
                        )
                    })?;
                if bytes.len() as u64 > LIMIT {
                    return Err(
                        "invalid_arguments: --archive-status-file is larger than 64 KiB".into(),
                    );
                }
                serde_json::from_slice::<ArchiveStatus>(&bytes).map_err(|_| {
                    "invalid_arguments: --archive-status-file is not archive status JSON".into()
                })
            })
            .transpose()?;
        let key = values.remove("--shared-key-file").map(PathBuf::from);
        let sets = super::cloud_recovery::connect(&target, key.as_deref())
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        let (meta, prefix) = sets.coordination();
        run(
            &AuthorityStore::new(meta, prefix),
            Request {
                status,
                observation,
                ack,
            },
        )
        .map_err(|error| {
            let description = match error {
                FailoverError::FenceUnconfirmed => "old writer must have zero replicas",
                FailoverError::AckMismatch => "acknowledgement does not match the exact report",
                FailoverError::ReportStale => "authority changed since the report",
                FailoverError::Conflict => "authority mutation conflicted",
                FailoverError::Indeterminate => "authority mutation outcome is indeterminate",
                FailoverError::Unavailable => "authority is unavailable",
                FailoverError::Missing => "authority is missing",
                FailoverError::Invalid => "authority is invalid",
            };
            format!("{}: {description}", error.code())
        })
    })();
    let outcome = match &result {
        Ok(_) => "ok",
        Err(error) => error.split(':').next().unwrap_or("internal_error"),
    };
    super::log_stderr(
        if result.is_ok() { "info" } else { "warn" },
        "failover",
        json!({"event": "failover_command", "verb": verb, "outcome": outcome}).to_string(),
    );
    result
}

fn run(store: &AuthorityStore, request: Request) -> Result<Value, FailoverError> {
    let report = build_report(store, request.status.as_ref(), request.observation)?;
    let mut output = json!({"report": report, "text": report.render()});
    if let Some(ack) = request.ack {
        let result = execute(store, &report, &ack)?;
        output["result"] = json!({"record": result.record, "etag": result.etag});
    }
    Ok(output)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct WriterObservation {
    pub sequence: u64,
    pub observed_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct ArchiveCrossCheck {
    pub last_checkpoint_sequence: Option<u64>,
    pub last_checkpoint_unix_ms: Option<u64>,
    pub failures: u64,
    pub last_error_code: Option<String>,
    pub unpublished_commits: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Loss {
    Known { commits: u64 },
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LossReport {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub report_id: String,
    pub scope_id: String,
    pub lineage: String,
    pub epoch: u64,
    pub revision: String,
    pub phase: Phase,
    pub writer_id_prefix: Option<String>,
    pub archived: Option<HeadRef>,
    /// Authority references have no time; only an exactly matching status
    /// checkpoint can supply this diagnostic, never ownership evidence.
    pub archived_unix_ms: Option<u64>,
    pub archive_status: Option<ArchiveCrossCheck>,
    pub observation: Option<WriterObservation>,
    pub loss: Loss,
}

impl LossReport {
    fn digest(&self) -> Result<String, FailoverError> {
        let mut canonical = self.clone();
        canonical.report_id.clear();
        // Fixed struct field order, explicit nulls, integer positions, no maps.
        let bytes = serde_json::to_vec(&canonical).map_err(|_| FailoverError::Invalid)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    pub fn render(&self) -> String {
        let sequence = self
            .archived
            .as_ref()
            .map(|head| head.sequence.to_string())
            .unwrap_or_else(|| "unknown".into());
        let time = self
            .archived_unix_ms
            .map(|time| format!("{time} (archive-status cross-check)"))
            .unwrap_or_else(|| "unknown".into());
        let observed = match self.observation {
            Some(observed) => format!(
                "Observed writer sequence: {}; observed time (unix ms): {}",
                observed.sequence, observed.observed_unix_ms
            ),
            None => "Observed writer position: unknown".into(),
        };
        let loss = match self.loss {
            Loss::Known { commits } => {
                format!("{commits} commits at the observed position; later loss is unknown")
            }
            Loss::Unknown => "unknown".into(),
        };
        let warning = if self.archived.is_some() {
            format!("All commits after archived sequence {sequence} are lost.")
        } else {
            "Archived position is unknown; all commits after it are lost.".into()
        };
        let unpublished = self
            .archive_status
            .as_ref()
            .and_then(|status| status.unpublished_commits)
            .map(|commits| {
                format!("\n{commits} unpublished commits are lost (not authority-archived).")
            })
            .unwrap_or_default();
        format!(
            "Failover loss report: {}\nScope: {}; lineage: {}; epoch: {}; revision: {}; phase: {:?}\nArchived sequence: {sequence}\nArchived time (unix ms): {time}\n{observed}\nLoss: {loss}\n{warning}{unpublished}\nAcknowledge this report only after the old writer has zero replicas.\n",
            self.report_id, self.scope_id, self.lineage, self.epoch, self.revision, self.phase
        )
    }
}

#[derive(Clone, Debug)]
pub(super) struct FailoverAck {
    pub report_id: String,
    pub expected_epoch: u64,
    pub expected_revision: String,
    pub old_replicas: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FailoverError {
    FenceUnconfirmed,
    AckMismatch,
    ReportStale,
    Conflict,
    Indeterminate,
    Unavailable,
    Missing,
    Invalid,
}

impl FailoverError {
    pub fn code(self) -> &'static str {
        match self {
            Self::FenceUnconfirmed => "failover_fence_unconfirmed",
            Self::AckMismatch => "failover_ack_mismatch",
            Self::ReportStale => "failover_report_stale",
            Self::Conflict => "failover_conflict",
            Self::Indeterminate => "failover_indeterminate",
            Self::Unavailable => "failover_authority_unavailable",
            Self::Missing => "failover_authority_missing",
            Self::Invalid => "failover_authority_invalid",
        }
    }
}

impl std::fmt::Display for FailoverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for FailoverError {}

impl From<AuthorityError> for FailoverError {
    fn from(error: AuthorityError) -> Self {
        match error {
            AuthorityError::Lost
            | AuthorityError::HeadMoved(_)
            | AuthorityError::Fenced { .. }
            | AuthorityError::Exists => Self::Conflict,
            AuthorityError::Unresolved => Self::Indeterminate,
            AuthorityError::Unavailable => Self::Unavailable,
            AuthorityError::Missing => Self::Missing,
            AuthorityError::Invalid => Self::Invalid,
        }
    }
}

fn refused(error: &FailoverError) {
    super::log_stderr(
        "warn",
        "failover",
        json!({"event": "failover_refused", "code": error.code()}).to_string(),
    );
}

pub(super) fn build_report(
    store: &AuthorityStore,
    status: Option<&ArchiveStatus>,
    observation: Option<WriterObservation>,
) -> Result<LossReport, FailoverError> {
    (|| {
        let current = store.read()?;
        current.record.validate()?;
        let record = current.record;
        let archived = record.head.or(record.base);
        let sequence = archived.as_ref().map(|head| head.sequence);
        let archive_status = status.map(|status| ArchiveCrossCheck {
            last_checkpoint_sequence: status.last_checkpoint_sequence,
            last_checkpoint_unix_ms: status.last_checkpoint_unix_ms,
            failures: status.failures,
            last_error_code: status.last_error.as_ref().map(|error| error.code.clone()),
            unpublished_commits: status.last_checkpoint_sequence.zip(sequence).and_then(
                |(checkpoint, archived)| (checkpoint > archived).then(|| checkpoint - archived),
            ),
        });
        let loss = match (observation, sequence) {
            (Some(observed), Some(archived))
                if observed.sequence >= archived
                    && status
                        .and_then(|status| status.last_checkpoint_sequence)
                        .is_none_or(|checkpoint| checkpoint <= observed.sequence) =>
            {
                Loss::Known {
                    commits: observed.sequence - archived,
                }
            }
            _ => Loss::Unknown,
        };
        let mut report = LossReport {
            report_id: String::new(),
            scope_id: record.scope_id,
            lineage: record.lineage,
            epoch: record.epoch,
            revision: record.revision,
            phase: record.phase,
            writer_id_prefix: record
                .writer_id
                .map(|writer| writer.chars().take(8).collect()),
            archived,
            archived_unix_ms: status.and_then(|status| {
                sequence
                    .filter(|sequence| Some(*sequence) == status.last_checkpoint_sequence)
                    .and(status.last_checkpoint_unix_ms)
            }),
            archive_status,
            observation,
            loss,
        };
        report.report_id = report.digest()?;
        Ok(report)
    })()
    .inspect_err(refused)
}

pub(super) fn execute(
    store: &AuthorityStore,
    report: &LossReport,
    ack: &FailoverAck,
) -> Result<Observed, FailoverError> {
    (|| {
        if ack.old_replicas != 0 {
            return Err(FailoverError::FenceUnconfirmed);
        }
        if ack.report_id != report.report_id
            || ack.expected_epoch != report.epoch
            || ack.expected_revision != report.revision
            || report.digest()? != report.report_id
        {
            return Err(FailoverError::AckMismatch);
        }
        let current = store.read()?;
        if current.record.epoch != report.epoch
            || current.record.revision != report.revision
            || current.record.scope_id != report.scope_id
            || current.record.lineage != report.lineage
            || current.record.phase != report.phase
            || current
                .record
                .head
                .as_ref()
                .or(current.record.base.as_ref())
                != report.archived.as_ref()
        {
            return Err(FailoverError::ReportStale);
        }
        let prepared =
            store.prepare_transfer(&current, ack.expected_epoch, &ack.expected_revision)?;
        super::log_stderr(
            "info",
            "failover",
            json!({
                "event": "failover_prepared",
                "old_epoch": current.record.epoch,
                "new_epoch": prepared.record.epoch,
                "report_id": report.report_id,
            })
            .to_string(),
        );
        Ok(prepared)
    })()
    .inspect_err(refused)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::server::{
        authority::{Claim, HeadRef},
        backup_policy::LastError,
        cloud_backup::fake::{Fault, Memory, Op},
    };

    const KEY: &str = "test/authority.json";

    fn head(epoch: u64, sequence: u64) -> HeadRef {
        HeadRef {
            epoch,
            set_id: "20261118T000000Z-0123456789abcdef".into(),
            manifest_sha256: "a".repeat(64),
            index_sha256: Some("b".repeat(64)),
            sequence,
            chain_sha256: "c".repeat(64),
        }
    }

    fn setup() -> (Arc<Memory>, Arc<AuthorityStore>, Claim, Observed) {
        let memory = Arc::new(Memory::default());
        let store = Arc::new(AuthorityStore::new(memory.clone(), "test/".into()));
        let prepared = store
            .create("installation", "scope", "0123456789abcdef")
            .unwrap();
        let (claim, claimed) = store.claim(&prepared, 42).unwrap();
        let served = store.advance_head(&claim, &claimed, head(1, 10)).unwrap();
        (memory, store, claim, served)
    }

    fn observation(sequence: u64) -> WriterObservation {
        WriterObservation {
            sequence,
            observed_unix_ms: 100,
        }
    }

    fn ack(report: &LossReport) -> FailoverAck {
        FailoverAck {
            report_id: report.report_id.clone(),
            expected_epoch: report.epoch,
            expected_revision: report.revision.clone(),
            old_replicas: 0,
        }
    }

    fn assert_no_put<T: std::fmt::Debug>(
        memory: &Arc<Memory>,
        operation: impl FnOnce() -> Result<T, FailoverError>,
        expected: FailoverError,
    ) {
        let before = memory.raw(KEY);
        let puts = Arc::new(AtomicU64::new(0));
        let count = puts.clone();
        memory.hook(Op::Put, KEY, move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(operation().unwrap_err(), expected);
        assert_eq!(memory.raw(KEY), before);
        assert_eq!(puts.load(Ordering::SeqCst), 0);
    }

    fn command(args: &[&str]) -> Result<serde_json::Value, String> {
        failover_command(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    fn request(ack: Option<FailoverAck>) -> Request {
        Request {
            status: None,
            observation: Some(observation(15)),
            ack,
        }
    }

    #[test]
    fn failover_command_rejects_unknown_verb() {
        for args in [vec![], vec!["unknown"]] {
            assert_eq!(
                command(&args).unwrap_err(),
                format!(
                    "invalid_arguments: the verb must be report or execute; usage: {FAILOVER_USAGE}"
                )
            );
        }
    }

    #[test]
    fn failover_command_requires_target_file() {
        assert!(
            command(&["report"])
                .unwrap_err()
                .starts_with("invalid_arguments: --target-file is required")
        );
    }

    #[test]
    fn failover_command_observation_flags_go_together() {
        for flag in ["--observed-sequence", "--observed-unix-ms"] {
            assert_eq!(
                command(&["report", "--target-file", "missing", flag, "1"]).unwrap_err(),
                "invalid_arguments: --observed-sequence and --observed-unix-ms go together"
            );
        }
    }

    #[test]
    fn failover_command_execute_requires_ack_flags() {
        let mut args = vec!["execute", "--target-file", "missing"];
        for (flag, value) in [
            ("--ack-report-id", "id"),
            ("--expected-epoch", "1"),
            ("--expected-revision", "rev"),
            ("--old-replicas", "0"),
        ] {
            assert!(
                command(&args)
                    .unwrap_err()
                    .starts_with(&format!("invalid_arguments: {flag} is required"))
            );
            args.extend([flag, value]);
        }
    }

    #[test]
    fn failover_command_never_echoes_values() {
        let secret = "secret-expected-epoch-DO-NOT-PRINT";
        let error = command(&[
            "execute",
            "--target-file",
            "missing",
            "--ack-report-id",
            "id",
            "--expected-epoch",
            secret,
            "--expected-revision",
            "rev",
            "--old-replicas",
            "0",
        ])
        .unwrap_err();
        assert!(error.starts_with("invalid_arguments: --expected-epoch"));
        assert!(!error.contains(secret));
    }

    #[test]
    fn failover_command_archive_status_file_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("status.json");
        for (bytes, detail) in [
            (vec![b' '; 64 * 1024 + 1], "is larger than 64 KiB"),
            (b"secret-not-json".to_vec(), "is not archive status JSON"),
        ] {
            std::fs::write(&file, bytes).unwrap();
            assert_eq!(
                command(&[
                    "report",
                    "--target-file",
                    "missing",
                    "--archive-status-file",
                    file.to_str().unwrap(),
                ])
                .unwrap_err(),
                format!("invalid_arguments: --archive-status-file {detail}")
            );
        }
    }

    #[test]
    fn failover_run_report_matches_build_report() {
        let (_, store, _, _) = setup();
        let mut request = request(None);
        request.status = Some(ArchiveStatus {
            last_checkpoint_sequence: Some(10),
            last_checkpoint_unix_ms: Some(75),
            ..ArchiveStatus::default()
        });
        let report = build_report(&store, request.status.as_ref(), request.observation).unwrap();
        let output = run(&store, request).unwrap();
        assert_eq!(output["report"]["report_id"], report.report_id);
        assert_eq!(output["text"], report.render());
        assert!(output.get("result").is_none());
    }

    #[test]
    fn failover_run_execute_prepares_recovering_epoch() {
        let (_, store, _, served) = setup();
        let report = build_report(&store, None, Some(observation(15))).unwrap();
        let output = run(&store, request(Some(ack(&report)))).unwrap();
        assert_eq!(output["report"]["report_id"], report.report_id);
        assert_eq!(output["text"], report.render());
        assert_eq!(output["result"]["record"]["epoch"], served.record.epoch + 1);
        assert_eq!(output["result"]["record"]["phase"], "recovering");
        assert!(!output["result"]["etag"].as_str().unwrap().is_empty());
        assert_eq!(store.read().unwrap().record.phase, Phase::Recovering);
    }

    #[test]
    fn failover_run_execute_refuses_nonzero_replicas() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, Some(observation(15))).unwrap();
        let mut ack = ack(&report);
        ack.old_replicas = 1;
        assert_no_put(
            &memory,
            || run(&store, request(Some(ack))),
            FailoverError::FenceUnconfirmed,
        );
    }

    #[test]
    fn failover_run_execute_refuses_wrong_report_id() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, Some(observation(15))).unwrap();
        let mut ack = ack(&report);
        ack.report_id = "wrong".into();
        assert_no_put(
            &memory,
            || run(&store, request(Some(ack))),
            FailoverError::AckMismatch,
        );
    }

    #[test]
    fn failover_report_uses_authority_head_not_interval() {
        let (_, store, _, served) = setup();
        let status = ArchiveStatus {
            last_checkpoint_sequence: Some(8),
            last_checkpoint_unix_ms: Some(50),
            failures: 7,
            last_error: Some(LastError {
                code: "archive_upload_failed".into(),
                message: "secret diagnostic must not appear".into(),
            }),
            ..ArchiveStatus::default()
        };
        let report = build_report(&store, Some(&status), Some(observation(15))).unwrap();
        assert_eq!(report.archived, served.record.head);
        assert_eq!(report.archived_unix_ms, None);
        assert_eq!(report.loss, Loss::Known { commits: 5 });
        let cross_check = report.archive_status.as_ref().unwrap();
        assert_eq!(cross_check.failures, 7);
        assert_eq!(
            cross_check.last_error_code.as_deref(),
            Some("archive_upload_failed")
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret diagnostic"));
        assert!(!json.contains(served.record.writer_id.as_ref().unwrap()));
        assert_eq!(report.writer_id_prefix.as_ref().unwrap().len(), 8);
        let text = report.render();
        assert!(text.contains("Archived sequence: 10"));
        assert!(text.contains("Archived time (unix ms): unknown"));
        assert!(text.contains("Observed writer sequence: 15"));
        assert!(text.contains("All commits after archived sequence 10 are lost."));
    }

    #[test]
    fn failover_report_loss_known_only_with_observation() {
        let (_, store, _, _) = setup();
        for observed in [None, Some(observation(9))] {
            let report = build_report(&store, None, observed).unwrap();
            assert_eq!(report.loss, Loss::Unknown);
        }
        for (sequence, commits) in [(10, 0), (15, 5), (u64::MAX, u64::MAX - 10)] {
            let report = build_report(&store, None, Some(observation(sequence))).unwrap();
            assert_eq!(report.loss, Loss::Known { commits });
        }
        assert!(
            build_report(&store, None, None)
                .unwrap()
                .render()
                .contains("Observed writer position: unknown")
        );
    }

    #[test]
    fn failover_report_unpublished_checkpoint_is_lost() {
        let (_, store, _, _) = setup();
        let status = ArchiveStatus {
            last_checkpoint_sequence: Some(15),
            last_checkpoint_unix_ms: Some(75),
            ..ArchiveStatus::default()
        };
        let report = build_report(&store, Some(&status), Some(observation(20))).unwrap();
        assert_eq!(report.archived.as_ref().unwrap().sequence, 10);
        assert_eq!(report.archived_unix_ms, None);
        assert_eq!(report.loss, Loss::Known { commits: 10 });
        assert_eq!(
            report.archive_status.as_ref().unwrap().unpublished_commits,
            Some(5)
        );
        assert!(report.render().contains("5 unpublished commits are lost"));
        assert_eq!(
            build_report(&store, Some(&status), Some(observation(12)))
                .unwrap()
                .loss,
            Loss::Unknown
        );
        assert_eq!(
            build_report(&store, Some(&status), None).unwrap().loss,
            Loss::Unknown
        );
    }

    #[test]
    fn failover_report_id_is_deterministic() {
        let (_, store, _, _) = setup();
        let first = build_report(&store, None, Some(observation(15))).unwrap();
        let second = build_report(&store, None, Some(observation(15))).unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
        let mut canonical = first.clone();
        canonical.report_id.clear();
        let expected = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&canonical).unwrap())
        );
        assert_eq!(first.report_id, expected);
        assert_eq!(first.report_id.len(), 64);
        assert_ne!(
            first.report_id,
            build_report(&store, None, Some(observation(16)))
                .unwrap()
                .report_id
        );
        assert_eq!(first.render(), second.render());
    }

    #[test]
    fn failover_refuses_without_zero_replicas() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, None).unwrap();
        let mut ack = ack(&report);
        ack.old_replicas = 1;
        ack.report_id.clear();
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        assert_no_put(
            &memory,
            || execute(&store, &report, &ack),
            FailoverError::FenceUnconfirmed,
        );
        assert!(store.read().is_err(), "refusal must not read authority");
    }

    #[test]
    fn failover_refuses_ack_mismatch() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, None).unwrap();
        for field in 0..3 {
            let mut ack = ack(&report);
            match field {
                0 => ack.report_id.clear(),
                1 => ack.expected_epoch += 1,
                _ => ack.expected_revision.clear(),
            }
            memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
            assert_no_put(
                &memory,
                || execute(&store, &report, &ack),
                FailoverError::AckMismatch,
            );
            assert!(store.read().is_err(), "refusal must not read authority");
        }
    }

    #[test]
    fn failover_report_stale_after_head_advance() {
        let (memory, store, claim, served) = setup();
        let report = build_report(&store, None, None).unwrap();
        store.advance_head(&claim, &served, head(1, 11)).unwrap();
        assert_no_put(
            &memory,
            || execute(&store, &report, &ack(&report)),
            FailoverError::ReportStale,
        );
    }

    #[test]
    fn failover_conflict_on_concurrent_transfer() {
        let (memory, store, _, served) = setup();
        let report = build_report(&store, None, None).unwrap();
        let competitor = store.clone();
        let weak_memory = Arc::downgrade(&memory);
        let puts = Arc::new(AtomicU64::new(0));
        let count = puts.clone();
        memory.hook(Op::Put, KEY, move || {
            count.fetch_add(1, Ordering::SeqCst);
            competitor
                .prepare_transfer(&served, served.record.epoch, &served.record.revision)
                .unwrap();
            weak_memory.upgrade().unwrap().hook(Op::Put, KEY, move || {
                count.fetch_add(1, Ordering::SeqCst);
            });
        });
        assert_eq!(
            execute(&store, &report, &ack(&report)).unwrap_err(),
            FailoverError::Conflict
        );
        assert_eq!(puts.load(Ordering::SeqCst), 1);
        assert_eq!(store.read().unwrap().record.epoch, 2);
    }

    #[test]
    fn failover_unknown_applied_succeeds_by_readback() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, None).unwrap();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let prepared = execute(&store, &report, &ack(&report)).unwrap();
        assert_eq!(prepared.record.epoch, 2);
        assert_eq!(prepared, store.read().unwrap());
    }

    #[test]
    fn failover_unknown_unreadable_is_indeterminate() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, None).unwrap();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        memory.fault(Op::Get, KEY, Fault::Refuse, 1, 1);
        assert_eq!(
            execute(&store, &report, &ack(&report)).unwrap_err(),
            FailoverError::Indeterminate
        );
        assert_eq!(store.read().unwrap().record.epoch, 2);
    }

    #[test]
    fn failover_unknown_not_applied_is_conflict() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, None).unwrap();
        memory.fault(Op::Put, KEY, Fault::Before, 0, 1);
        assert_eq!(
            execute(&store, &report, &ack(&report)).unwrap_err(),
            FailoverError::Conflict
        );
        assert_eq!(store.read().unwrap().record.epoch, 1);
    }

    #[test]
    fn failover_prepares_recovering_epoch_plus_one() {
        let (_, store, claim, served) = setup();
        let report = build_report(&store, None, None).unwrap();
        let prepared = execute(&store, &report, &ack(&report)).unwrap();
        assert_eq!(prepared.record.epoch, served.record.epoch + 1);
        assert_eq!(prepared.record.base, served.record.head);
        assert_eq!(prepared.record.head, None);
        assert_eq!(prepared.record.writer_id, None);
        assert_eq!(prepared.record.phase, Phase::Recovering);
        assert_eq!(prepared.record.claimed_unix_ms, None);
        assert_ne!(prepared.record.revision, served.record.revision);
        let recovering_report = build_report(&store, None, None).unwrap();
        assert_eq!(recovering_report.archived, served.record.head);
        assert!(store.claim(&prepared, 200).is_ok());
        assert!(store.advance_head(&claim, &served, head(1, 11)).is_err());
    }

    #[test]
    fn failover_report_without_archive_is_unknown() {
        let memory = Arc::new(Memory::default());
        let store = AuthorityStore::new(memory, "test/".into());
        store
            .create("installation", "scope", "0123456789abcdef")
            .unwrap();
        let report = build_report(&store, None, Some(observation(20))).unwrap();
        assert_eq!(report.archived, None);
        assert_eq!(report.loss, Loss::Unknown);
        assert!(report.render().contains("Archived sequence: unknown"));
    }

    #[test]
    fn failover_matching_checkpoint_time_is_diagnostic() {
        let (_, store, _, _) = setup();
        let status = ArchiveStatus {
            last_checkpoint_sequence: Some(10),
            last_checkpoint_unix_ms: Some(75),
            ..ArchiveStatus::default()
        };
        let report = build_report(&store, Some(&status), None).unwrap();
        assert_eq!(report.archived_unix_ms, Some(75));
        assert!(report.render().contains("75 (archive-status cross-check)"));
    }

    #[test]
    fn failover_authority_errors_refuse_closed() {
        let (memory, store, _, _) = setup();
        let report = build_report(&store, None, None).unwrap();
        memory.fault(Op::Put, KEY, Fault::Refuse, 0, 1);
        assert_eq!(
            execute(&store, &report, &ack(&report)).unwrap_err(),
            FailoverError::Unavailable
        );
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        assert_no_put(
            &memory,
            || execute(&store, &report, &ack(&report)),
            FailoverError::Unavailable,
        );
        memory.set_raw(KEY, b"{}");
        assert_eq!(
            build_report(&store, None, None).unwrap_err(),
            FailoverError::Invalid
        );
        assert_no_put(
            &memory,
            || execute(&store, &report, &ack(&report)),
            FailoverError::Invalid,
        );
        memory.remove(KEY);
        assert_eq!(
            build_report(&store, None, None).unwrap_err(),
            FailoverError::Missing
        );
        assert_no_put(
            &memory,
            || execute(&store, &report, &ack(&report)),
            FailoverError::Missing,
        );
    }

    #[test]
    fn failover_error_codes_are_stable() {
        for (error, code) in [
            (
                FailoverError::FenceUnconfirmed,
                "failover_fence_unconfirmed",
            ),
            (FailoverError::AckMismatch, "failover_ack_mismatch"),
            (FailoverError::ReportStale, "failover_report_stale"),
            (FailoverError::Conflict, "failover_conflict"),
            (FailoverError::Indeterminate, "failover_indeterminate"),
            (FailoverError::Unavailable, "failover_authority_unavailable"),
            (FailoverError::Missing, "failover_authority_missing"),
            (FailoverError::Invalid, "failover_authority_invalid"),
        ] {
            assert_eq!(error.code(), code);
        }
    }
}
