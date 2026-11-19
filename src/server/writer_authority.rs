//! Explicit writer enrollment and the serving process's periodic authority check.

use std::{ffi::OsString, path::Path, path::PathBuf, time::Duration};

use serde_json::{Value, json};
use tokio::sync::watch;

use super::{
    ApiError, Service,
    authority::{AuthorityError, AuthorityStore, Record},
    gateway_mode::{flags, utf8},
    writer_fence::{State, WriterFence},
};

const AUTHORITY_USAGE: &str = "vlite-server authority init|show --target-file <json> [--shared-key-file <path>] [init only, all required: --installation-id <id> --scope-id <id> --lineage <16 lowercase hex>]";

fn describe(error: AuthorityError) -> String {
    let error = ApiError::from(error);
    format!("{}: {}", error.code, error.message)
}

fn summary(record: &Record) -> Value {
    json!({
        "schema": record.schema,
        "installation_id": record.installation_id,
        "scope_id": record.scope_id,
        "lineage": record.lineage,
        "epoch": record.epoch,
        "revision": record.revision,
        "phase": record.phase,
        "writer_present": record.writer_id.is_some(),
        "base": record.base,
        "head": record.head,
    })
}

/// Blocking operator command; call off the async runtime.
pub fn authority_command(args: &[OsString]) -> Result<Value, String> {
    let verb = match args.first().and_then(|arg| arg.to_str()) {
        Some("init") => "init",
        Some("show") => "show",
        _ => "invalid",
    };
    let result = (|| {
        let usage = |detail: &str| format!("invalid_arguments: {detail}; usage: {AUTHORITY_USAGE}");
        if verb == "invalid" {
            return Err(usage("the verb must be init or show"));
        }
        let mut allowed = vec!["--target-file", "--shared-key-file"];
        if verb == "init" {
            allowed.extend(["--installation-id", "--scope-id", "--lineage"]);
        }
        let mut values = flags(&args[1..], &allowed, AUTHORITY_USAGE)?;
        let mut required = |name: &str| {
            values
                .remove(name)
                .ok_or_else(|| usage(&format!("{name} is required")))
        };
        let target = PathBuf::from(required("--target-file")?);
        let init = if verb == "init" {
            Some((
                utf8("--installation-id", required("--installation-id")?)?,
                utf8("--scope-id", required("--scope-id")?)?,
                utf8("--lineage", required("--lineage")?)?,
            ))
        } else {
            None
        };
        let key = values.remove("--shared-key-file").map(PathBuf::from);
        let authority = WriterAuthority::connect(&target, key.as_deref())?;
        let observed = match init {
            Some((installation, scope, lineage)) => {
                authority.store.create(&installation, &scope, &lineage)
            }
            None => authority.store.read(),
        }
        .map_err(describe)?;
        Ok(summary(&observed.record))
    })();
    let outcome = match &result {
        Ok(_) => "ok",
        Err(error) => error.split(':').next().unwrap_or("internal_error"),
    };
    super::log_stderr(
        if result.is_ok() { "info" } else { "warn" },
        "authority",
        json!({"event": "authority_command", "verb": verb, "outcome": outcome}).to_string(),
    );
    result
}

/// Unclaimed authority connection used only by the server binary.
#[doc(hidden)]
pub struct WriterAuthority {
    store: AuthorityStore,
}

impl WriterAuthority {
    #[doc(hidden)]
    pub fn connect(target: &Path, key: Option<&Path>) -> Result<Self, String> {
        let sets = super::cloud_recovery::connect_explicit(target, key)
            .map_err(|error| format!("{}: {}", error.code, error.message))?;
        let (store, prefix) = sets.coordination();
        Ok(Self {
            store: AuthorityStore::new(store, prefix),
        })
    }
}

fn claim(store: AuthorityStore, lineage: Option<u64>) -> Result<WriterFence, String> {
    let observed = store.read().map_err(describe)?;
    if lineage.map(|lineage| format!("{lineage:016x}")).as_deref()
        != Some(observed.record.lineage.as_str())
    {
        return Err(
            "writer_authority_lineage_mismatch: authority and database must have the same v4 lineage"
                .into(),
        );
    }
    if observed.record.writer_id.is_some() {
        return Err(
            "writer_authority_claimed: explicit failover is required before another writer starts"
                .into(),
        );
    }
    if observed.record.base.is_some() {
        return Err(
            "writer_authority_base_unverified: startup cannot verify the archived base against the local database"
                .into(),
        );
    }
    let (claim, _) = store
        .claim(&observed, super::admin::now_ms() as u64)
        .map_err(describe)?;
    let epoch = claim.epoch;
    let writer_prefix = claim.writer_id[..8].to_owned();
    let fence = WriterFence::new(store, claim);
    for state in [State::Claiming, State::Recovering, State::Serving] {
        fence
            .transition(state)
            .map_err(|_| "internal_error: invalid writer startup transition".to_owned())?;
    }
    super::log_stderr(
        "info",
        "authority",
        json!({"event":"writer_authority_claimed", "epoch":epoch, "writer_id_prefix":writer_prefix})
            .to_string(),
    );
    Ok(fence)
}

impl Service {
    /// Blocking startup enrollment, immediately after choosing writer serve mode.
    #[doc(hidden)]
    pub fn with_writer_authority(self, authority: WriterAuthority) -> Result<Self, String> {
        if self.serve.mode != super::ServeMode::Writer {
            return Err("invalid_arguments: writer authority needs --serve-mode writer".into());
        }
        let fence = claim(authority.store, self.lineage)?;
        self.with_writer_fence(fence)
            .map_err(|_| "internal_error: cannot install writer authority".into())
    }

    #[doc(hidden)]
    pub async fn recheck_writer_authority(
        &self,
        interval: Duration,
        stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        recheck(self.writer_fence.clone(), interval, stop).await
    }
}

async fn recheck(
    fence: WriterFence,
    interval: Duration,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    loop {
        if !matches!(fence.state(), State::Serving | State::AuthorityUnavailable) || *stop.borrow()
        {
            return Ok(());
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
                continue;
            }
        }
        let fence = fence.clone();
        tokio::task::spawn_blocking(move || {
            if matches!(fence.state(), State::Serving | State::AuthorityUnavailable) {
                let before = fence.state();
                if let Err(error) = fence.verify_before_commit()
                    && before != State::AuthorityUnavailable
                    && fence.state() == State::AuthorityUnavailable
                {
                    super::log_stderr(
                        "warn",
                        "authority",
                        json!({"event":"writer_authority_recheck", "code":error.code}).to_string(),
                    );
                }
            }
        })
        .await
        .map_err(|_| "internal_error: writer authority recheck stopped unexpectedly".to_owned())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        authority::{HeadRef, Record},
        cloud_backup::fake::{Fault, Memory, Op},
    };
    use std::sync::Arc;

    const KEY: &str = "test/authority.json";
    const LINEAGE: u64 = 0x0123456789abcdef;

    fn setup() -> (Arc<Memory>, AuthorityStore) {
        let memory = Arc::new(Memory::default());
        let store = AuthorityStore::new(memory.clone(), "test/".into());
        store
            .create("installation", "scope", "0123456789abcdef")
            .unwrap();
        (memory, store)
    }

    fn refusal(store: AuthorityStore, lineage: Option<u64>, code: &str) {
        let error = claim(store, lineage).err().expect("startup must refuse");
        assert!(error.starts_with(&format!("{code}: ")), "{error}");
    }

    #[test]
    fn fresh_authority_is_claimed_once_and_output_redacts_writer() {
        let (memory, store) = setup();
        let fence = claim(store, Some(LINEAGE)).unwrap();
        assert_eq!(fence.state(), State::Serving);
        assert_eq!(fence.epoch(), Some(1));
        let record: Record = serde_json::from_slice(&memory.raw(KEY).unwrap()).unwrap();
        let output = summary(&record);
        assert_eq!(output["writer_present"], true);
        assert!(
            !output
                .to_string()
                .contains(record.writer_id.as_ref().unwrap())
        );
        assert!(record.claimed_unix_ms.is_some());
        refusal(
            AuthorityStore::new(memory, "test/".into()),
            Some(LINEAGE),
            "writer_authority_claimed",
        );
    }

    #[test]
    fn startup_refuses_missing_unavailable_invalid_and_wrong_lineage() {
        let (memory, store) = setup();
        memory.remove(KEY);
        refusal(store, Some(LINEAGE), "writer_authority_missing");
        let (memory, store) = setup();
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        refusal(store, Some(LINEAGE), "writer_authority_unavailable");
        let (memory, store) = setup();
        memory.set_raw(KEY, b"not-json");
        refusal(store, Some(LINEAGE), "writer_authority_invalid");
        for lineage in [None, Some(LINEAGE + 1)] {
            let (_, store) = setup();
            refusal(store, lineage, "writer_authority_lineage_mismatch");
        }
    }

    #[test]
    fn startup_never_serves_an_unverified_archive_base() {
        let (memory, store) = setup();
        let mut observed = store.read().unwrap();
        observed.record.epoch = 2;
        observed.record.base = Some(HeadRef {
            epoch: 1,
            set_id: "20261118T000000Z-0123456789abcdef".into(),
            manifest_sha256: "a".repeat(64),
            index_sha256: None,
            sequence: 10,
            chain_sha256: "b".repeat(64),
        });
        memory.set_raw(KEY, &serde_json::to_vec(&observed.record).unwrap());
        refusal(store, Some(LINEAGE), "writer_authority_base_unverified");
    }

    #[test]
    fn claim_cas_conflict_does_not_retry() {
        let (memory, store) = setup();
        let rival = AuthorityStore::new(memory.clone(), "test/".into());
        memory.hook(Op::Put, KEY, move || {
            rival.claim(&rival.read().unwrap(), 1).unwrap();
        });
        refusal(store, Some(LINEAGE), "writer_authority_conflict");
    }

    #[test]
    fn unknown_claim_is_read_back_or_refused_as_unresolved() {
        let (memory, store) = setup();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        assert!(claim(store, Some(LINEAGE)).unwrap().ready());
        let (memory, store) = setup();
        memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        memory.fault(Op::Get, KEY, Fault::Refuse, 1, 1);
        refusal(store, Some(LINEAGE), "writer_authority_unresolved");
    }

    #[tokio::test]
    async fn periodic_recheck_recovers_then_fences_an_idle_writer() {
        let (memory, store) = setup();
        let fence = claim(store, Some(LINEAGE)).unwrap();
        let mut authority_stop = fence.stop_signal().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        assert!(fence.verify_before_commit().is_err());
        assert_eq!(fence.state(), State::AuthorityUnavailable);
        let task = tokio::spawn(recheck(fence.clone(), Duration::from_millis(5), stop_rx));
        tokio::time::timeout(Duration::from_secs(2), async {
            while fence.state() != State::Serving {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let store = AuthorityStore::new(memory, "test/".into());
        let observed = store.read().unwrap();
        store
            .prepare_transfer(&observed, 1, &observed.record.revision)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), authority_stop.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(*authority_stop.borrow());
        assert_eq!(fence.state(), State::Fenced);
        assert_eq!(fence.exit_code(), Some(1));
        task.await.unwrap().unwrap();
        drop(stop_tx);
    }

    #[tokio::test]
    async fn disabled_recheck_does_no_store_operation() {
        let (memory, _) = setup();
        memory.hook(Op::Get, KEY, || panic!("disabled authority must not read"));
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        recheck(WriterFence::disabled(), Duration::from_millis(1), stop_rx)
            .await
            .unwrap();
    }
}
