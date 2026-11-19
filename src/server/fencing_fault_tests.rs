//! Library-level fault coverage for TEST-007/008/009/014 and AC-007.

use std::{
    io::Write,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::http::StatusCode;

use super::{
    ApiError,
    authority::{AuthorityError, AuthorityStore, Claim, HeadRef, Observed, Phase, Record},
    cloud_backup::{
        Blob, Cond, ObjectStore, StoreError,
        fake::{Fault, Memory, Op},
    },
    writer_fence::{State, WriterFence},
};

const KEY: &str = "test/authority.json";

#[derive(Default)]
struct CountedStore {
    memory: Arc<Memory>,
    gets: AtomicUsize,
    puts: Mutex<Vec<Vec<u8>>>,
}

impl CountedStore {
    fn reads(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }

    fn writes(&self) -> usize {
        self.puts.lock().unwrap().len()
    }

    fn attempted(&self, index: usize) -> Vec<u8> {
        self.puts.lock().unwrap()[index].clone()
    }
}

impl ObjectStore for CountedStore {
    fn get(&self, name: &str, max: u64) -> Result<Blob, StoreError> {
        if name == KEY {
            self.gets.fetch_add(1, Ordering::SeqCst);
        }
        self.memory.get(name, max)
    }

    fn download(&self, name: &str, out: &mut dyn Write, max: u64) -> Result<u64, StoreError> {
        self.memory.download(name, out, max)
    }

    fn put(&self, name: &str, bytes: &[u8], cond: Cond<'_>) -> Result<String, StoreError> {
        if name == KEY {
            self.puts.lock().unwrap().push(bytes.to_vec());
        }
        self.memory.put(name, bytes, cond)
    }

    fn put_file(&self, name: &str, path: &Path, cond: Cond<'_>) -> Result<String, StoreError> {
        self.put(name, &std::fs::read(path).unwrap(), cond)
    }

    fn delete(&self, name: &str) -> Result<(), StoreError> {
        self.memory.delete(name)
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.memory.list(prefix)
    }
}

fn setup() -> (Arc<CountedStore>, Arc<AuthorityStore>) {
    let counted = Arc::new(CountedStore::default());
    let store = Arc::new(AuthorityStore::new(counted.clone(), "test/".into()));
    (counted, store)
}

fn create(store: &AuthorityStore) -> Observed {
    store
        .create("installation", "scope", "0123456789abcdef")
        .unwrap()
}

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

fn serving(store: &AuthorityStore) -> (Claim, Observed) {
    let (claim, claimed) = store.claim(&create(store), 42).unwrap();
    let served = store.advance_head(&claim, &claimed, head(1, 10)).unwrap();
    (claim, served)
}

fn fence(counted: &Arc<CountedStore>, claim: Claim) -> WriterFence {
    let fence = WriterFence::new(AuthorityStore::new(counted.clone(), "test/".into()), claim);
    fence.transition(State::Claiming).unwrap();
    fence.transition(State::Serving).unwrap();
    fence
}

fn transfer(store: &AuthorityStore, current: &Observed) -> Observed {
    store
        .prepare_transfer(current, current.record.epoch, &current.record.revision)
        .unwrap()
}

fn change(memory: &Memory, update: impl FnOnce(&mut Record)) {
    let mut record: Record = serde_json::from_slice(&memory.raw(KEY).unwrap()).unwrap();
    update(&mut record);
    record.validate().unwrap();
    memory.set_raw(KEY, &serde_json::to_vec(&record).unwrap());
}

fn error(result: Result<(), ApiError>, status: StatusCode, code: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.status, status);
    assert_eq!(error.code, code);
}

fn assert_latched(fence: &WriterFence) {
    assert_eq!(fence.state(), State::Fenced);
    assert!(!fence.ready());
    assert_eq!(fence.exit_code(), Some(1));
    assert!(*fence.stop_signal().unwrap().borrow());
    assert!(fence.transition(State::Serving).is_err());
    error(
        fence.admit_mutation(),
        StatusCode::SERVICE_UNAVAILABLE,
        "writer_authority_unavailable",
    );
    error(
        fence.verify_before_commit(),
        StatusCode::SERVICE_UNAVAILABLE,
        "writer_authority_unavailable",
    );
    error(
        fence.verify_before_ack(),
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
    );
    assert_eq!(fence.state(), State::Fenced);
}

#[test]
fn fencing_fault_double_claim_has_one_publishing_winner() {
    let (counted, store) = setup();
    let prepared = create(&store);
    let rival_store = store.clone();
    let rival_prepared = prepared.clone();
    let winner = Arc::new(Mutex::new(None));
    let result = winner.clone();
    counted.memory.hook(Op::Put, KEY, move || {
        *result.lock().unwrap() = Some(rival_store.claim(&rival_prepared, 2).unwrap());
    });
    let first_attempt = counted.writes();
    assert!(matches!(
        store.claim(&prepared, 1),
        Err(AuthorityError::Lost)
    ));
    assert_eq!(counted.writes(), first_attempt + 2);
    let (winning_claim, claimed) = winner.lock().unwrap().take().unwrap();
    // The failed API returns no Claim; recover its proposed incarnation from its PUT.
    let rejected: Record = serde_json::from_slice(&counted.attempted(first_attempt)).unwrap();
    let losing_claim = Claim {
        writer_id: rejected.writer_id.unwrap(),
        epoch: rejected.epoch,
        revision: rejected.revision,
    };
    assert_ne!(losing_claim.writer_id, winning_claim.writer_id);
    let loser = fence(&counted, losing_claim.clone());
    let served = store
        .advance_head(&winning_claim, &claimed, head(1, 10))
        .unwrap();
    let writes = counted.writes();
    assert!(matches!(
        store.advance_head(&losing_claim, &served, head(1, 11)),
        Err(AuthorityError::Fenced { observed_epoch: 1 })
    ));
    assert_eq!(counted.writes(), writes);
    assert_eq!(store.read().unwrap(), served);
    error(
        loser.verify_before_commit(),
        StatusCode::SERVICE_UNAVAILABLE,
        "writer_authority_unavailable",
    );
    assert_latched(&loser);
    assert_eq!(counted.writes(), writes);
    let winner = fence(&counted, winning_claim);
    winner.verify_before_commit().unwrap();
    winner.verify_before_ack().unwrap();
    assert!(winner.ready());
}

#[test]
fn fencing_fault_former_owner_cannot_publish_or_clear_latch_after_transfer() {
    let (counted, store) = setup();
    let (claim_a, served_a) = serving(&store);
    let writer_a = fence(&counted, claim_a.clone());
    writer_a.verify_before_commit().unwrap();
    let prepared_b = transfer(&store, &served_a);
    let (claim_b, claimed_b) = store.claim(&prepared_b, 43).unwrap();
    let served_b = store
        .advance_head(&claim_b, &claimed_b, head(2, 10))
        .unwrap();
    let before = counted.memory.raw(KEY);
    assert!(matches!(
        store.advance_head(&claim_a, &served_a, head(1, 11)),
        Err(AuthorityError::Fenced { .. } | AuthorityError::HeadMoved(_))
    ));
    assert_eq!(counted.memory.raw(KEY), before);
    assert_eq!(store.read().unwrap(), served_b);
    assert!(writer_a.verify_before_commit().is_err());
    let reads = counted.reads();
    let writes = counted.writes();
    assert_latched(&writer_a);
    // Even a readable, valid record matching A cannot resurrect its local latch.
    counted
        .memory
        .set_raw(KEY, &serde_json::to_vec(&served_a.record).unwrap());
    store.verify_writer(&claim_a).unwrap();
    let reads_after_restore = counted.reads();
    assert_latched(&writer_a);
    assert_eq!(counted.reads(), reads_after_restore);
    assert_eq!(reads_after_restore, reads + 1);
    assert_eq!(counted.writes(), writes);
}

#[test]
fn fencing_fault_head_cas_412_never_retries_with_competing_etag() {
    let (counted, store) = setup();
    let (claim, observed) = serving(&store);
    let rival_store = store.clone();
    let rival_claim = claim.clone();
    let rival_observed = observed.clone();
    let competing = Arc::new(Mutex::new(None));
    let result = competing.clone();
    counted.memory.hook(Op::Put, KEY, move || {
        *result.lock().unwrap() = Some(
            rival_store
                .advance_head(&rival_claim, &rival_observed, head(1, 11))
                .unwrap(),
        );
    });
    let writes = counted.writes();
    let error = store
        .advance_head(&claim, &observed, head(1, 12))
        .unwrap_err();
    let competing = competing.lock().unwrap().take().unwrap();
    match error {
        AuthorityError::HeadMoved(current) => assert_eq!(current, competing),
        other => panic!("expected typed HeadMoved after CAS 412, got {other:?}"),
    }
    assert_ne!(observed.etag, competing.etag);
    assert_eq!(counted.writes(), writes + 2);
    assert_eq!(store.read().unwrap(), competing);
    assert_eq!(
        counted.memory.raw(KEY).unwrap(),
        counted.attempted(writes + 1)
    );
}

#[test]
fn fencing_fault_unknown_claim_requires_exact_readback() {
    for (fault, unreadable) in [
        (Fault::Before, false),
        (Fault::After, false),
        (Fault::After, true),
    ] {
        let (counted, store) = setup();
        let prepared = create(&store);
        counted.memory.fault(Op::Put, KEY, fault, 0, 1);
        if unreadable {
            counted.memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        }
        let writes = counted.writes();
        let reads = counted.reads();
        let result = store.claim(&prepared, 99);
        assert_eq!(counted.writes(), writes + 1);
        assert_eq!(counted.reads(), reads + 1);
        if unreadable {
            assert!(matches!(result, Err(AuthorityError::Unresolved)));
        } else if fault == Fault::Before {
            assert!(matches!(result, Err(AuthorityError::Lost)));
            assert_eq!(store.read().unwrap(), prepared);
        } else {
            let (claim, observed) = result.unwrap();
            assert_eq!(store.verify_writer(&claim).unwrap(), observed);
            assert_eq!(observed.record.claimed_unix_ms, Some(99));
        }
        if fault == Fault::After {
            assert_eq!(counted.memory.raw(KEY).unwrap(), counted.attempted(writes));
            let applied = store.read().unwrap();
            assert_ne!(applied.etag, prepared.etag);
            assert!(applied.record.writer_id.is_some());
        }
    }
}

#[test]
fn fencing_fault_unknown_head_requires_exact_readback() {
    for (fault, unreadable) in [
        (Fault::Before, false),
        (Fault::After, false),
        (Fault::After, true),
    ] {
        let (counted, store) = setup();
        let (claim, current) = serving(&store);
        let next = head(1, 11);
        counted.memory.fault(Op::Put, KEY, fault, 0, 1);
        if unreadable {
            counted.memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        }
        let writes = counted.writes();
        let reads = counted.reads();
        let result = store.advance_head(&claim, &current, next.clone());
        assert_eq!(counted.writes(), writes + 1);
        assert_eq!(counted.reads(), reads + 1);
        if unreadable {
            assert!(matches!(result, Err(AuthorityError::Unresolved)));
        } else if fault == Fault::Before {
            match result {
                Err(AuthorityError::HeadMoved(observed)) => assert_eq!(observed, current),
                other => panic!("expected unchanged head after unapplied PUT, got {other:?}"),
            }
            assert_eq!(store.read().unwrap(), current);
        } else {
            let observed = result.unwrap();
            assert_eq!(store.read().unwrap(), observed);
            assert_eq!(observed.record.head.as_ref(), Some(&next));
        }
        if fault == Fault::After {
            assert_eq!(counted.memory.raw(KEY).unwrap(), counted.attempted(writes));
            assert_eq!(store.read().unwrap().record.head, Some(next));
        }
    }
}

#[test]
fn fencing_fault_unknown_transfer_requires_exact_readback() {
    for (fault, unreadable) in [
        (Fault::Before, false),
        (Fault::After, false),
        (Fault::After, true),
    ] {
        let (counted, store) = setup();
        let (_, current) = serving(&store);
        counted.memory.fault(Op::Put, KEY, fault, 0, 1);
        if unreadable {
            counted.memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
        }
        let writes = counted.writes();
        let reads = counted.reads();
        let result = store.prepare_transfer(&current, 1, &current.record.revision);
        assert_eq!(counted.writes(), writes + 1);
        assert_eq!(counted.reads(), reads + 1);
        if unreadable {
            assert!(matches!(result, Err(AuthorityError::Unresolved)));
        } else if fault == Fault::Before {
            assert!(matches!(result, Err(AuthorityError::Lost)));
            assert_eq!(store.read().unwrap(), current);
        } else {
            assert_eq!(store.read().unwrap(), result.unwrap());
        }
        if fault == Fault::After {
            assert_eq!(counted.memory.raw(KEY).unwrap(), counted.attempted(writes));
            let applied = store.read().unwrap();
            assert_eq!(applied.record.epoch, 2);
            assert_eq!(applied.record.phase, Phase::Recovering);
            assert_eq!(applied.record.base, current.record.head);
            assert!(applied.record.writer_id.is_none());
            assert!(applied.record.head.is_none());
        }
    }
}

#[test]
fn fencing_fault_unknown_updates_reject_semantically_equal_nonexact_readback() {
    for operation in ["claim", "head", "transfer"] {
        let (counted, store) = setup();
        let (claim, current) = if operation == "claim" {
            (None, create(&store))
        } else {
            let (claim, current) = serving(&store);
            (Some(claim), current)
        };
        counted.memory.fault(Op::Put, KEY, Fault::After, 0, 1);
        let memory = counted.memory.clone();
        counted.memory.hook(Op::Get, KEY, move || {
            let bytes = memory.raw(KEY).unwrap();
            let record: Record = serde_json::from_slice(&bytes).unwrap();
            let reencoded = serde_json::to_vec_pretty(&record).unwrap();
            assert_ne!(bytes, reencoded);
            memory.set_raw(KEY, &reencoded);
        });
        let writes = counted.writes();
        let reads = counted.reads();
        match operation {
            "claim" => assert!(matches!(
                store.claim(&current, 99),
                Err(AuthorityError::Lost)
            )),
            "head" => assert!(matches!(
                store.advance_head(claim.as_ref().unwrap(), &current, head(1, 11)),
                Err(AuthorityError::HeadMoved(_))
            )),
            "transfer" => assert!(matches!(
                store.prepare_transfer(&current, 1, &current.record.revision),
                Err(AuthorityError::Lost)
            )),
            _ => unreachable!(),
        }
        assert_eq!(counted.writes(), writes + 1);
        assert_eq!(counted.reads(), reads + 1);
        let attempted: Record = serde_json::from_slice(&counted.attempted(writes)).unwrap();
        assert_eq!(store.read().unwrap().record, attempted);
        assert_ne!(counted.memory.raw(KEY).unwrap(), counted.attempted(writes));
    }
}

#[test]
fn fencing_fault_precommit_refusal_blocks_mutation_until_fresh_success() {
    let (counted, store) = setup();
    let (claim, _) = serving(&store);
    let writer = fence(&counted, claim);
    let before = counted.memory.raw(KEY);
    let writes = counted.writes();
    let reads = counted.reads();
    counted.memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
    let mut mutations = 0;
    error(
        writer.verify_before_commit().map(|()| mutations += 1),
        StatusCode::SERVICE_UNAVAILABLE,
        "writer_authority_unavailable",
    );
    assert_eq!(mutations, 0);
    assert_eq!(counted.reads(), reads + 1);
    assert_eq!(counted.memory.raw(KEY), before);
    assert_eq!(writer.state(), State::AuthorityUnavailable);
    assert!(!writer.ready());
    assert!(writer.admit_mutation().is_err());
    assert_eq!(writer.exit_code(), None);
    assert!(!*writer.stop_signal().unwrap().borrow());
    counted.memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
    assert!(writer.transition(State::Serving).is_err());
    assert_eq!(writer.state(), State::AuthorityUnavailable);
    assert_eq!(counted.reads(), reads + 2);
    writer.transition(State::Serving).unwrap();
    assert_eq!(counted.reads(), reads + 3);
    assert_eq!(writer.state(), State::Serving);
    writer.admit_mutation().unwrap();
    writer
        .verify_before_commit()
        .map(|()| mutations += 1)
        .unwrap();
    assert_eq!(mutations, 1);
    assert_eq!(counted.writes(), writes);
    assert_eq!(counted.memory.raw(KEY), before);
}

#[test]
fn fencing_fault_postcommit_refusal_is_ambiguous_and_never_resumes() {
    let (counted, store) = setup();
    let (claim, current) = serving(&store);
    let writer = fence(&counted, claim.clone());
    writer.verify_before_commit().unwrap();
    let committed = store.advance_head(&claim, &current, head(1, 11)).unwrap();
    let writes = counted.writes();
    counted.memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
    error(
        writer.verify_before_ack(),
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
    );
    assert_eq!(writer.state(), State::AuthorityUnavailable);
    assert_eq!(writer.exit_code(), Some(1));
    assert!(*writer.stop_signal().unwrap().borrow());
    assert_eq!(store.verify_writer(&claim).unwrap(), committed);
    let reads = counted.reads();
    for state in [State::AuthorityUnavailable, State::Draining, State::Stopped] {
        assert_eq!(writer.state(), state);
        assert!(!writer.ready());
        assert!(writer.admit_mutation().is_err());
        assert!(writer.transition(State::Serving).is_err());
        error(
            writer.verify_before_commit(),
            StatusCode::SERVICE_UNAVAILABLE,
            "writer_authority_unavailable",
        );
        error(
            writer.verify_before_ack(),
            StatusCode::BAD_GATEWAY,
            "write_outcome_unknown",
        );
        match state {
            State::AuthorityUnavailable => writer.begin_drain().unwrap(),
            State::Draining => writer.stopped().unwrap(),
            _ => {}
        }
    }
    assert_eq!(writer.exit_code(), Some(1));
    assert_eq!(counted.reads(), reads);
    assert_eq!(counted.writes(), writes);
    assert_eq!(store.read().unwrap(), committed);
}

#[test]
fn fencing_fault_paused_ack_observes_replacement_and_fences_zombie() {
    let (counted, store) = setup();
    let (claim_a, current) = serving(&store);
    let writer_a = fence(&counted, claim_a);
    let rival_store = store.clone();
    counted.memory.hook(Op::Get, KEY, move || {
        let prepared = transfer(&rival_store, &current);
        let (claim_b, claimed_b) = rival_store.claim(&prepared, 43).unwrap();
        rival_store
            .advance_head(&claim_b, &claimed_b, head(2, 10))
            .unwrap();
    });
    error(
        writer_a.verify_before_ack(),
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
    );
    assert_eq!(store.read().unwrap().record.epoch, 2);
    assert_latched(&writer_a);
    assert_eq!(writer_a.measurements().count, 1);
}

#[test]
fn fencing_fault_check_started_before_concurrent_fence_cannot_clear_latch() {
    let (counted, store) = setup();
    let (claim_a, current) = serving(&store);
    let writer_a = fence(&counted, claim_a.clone());
    let rival_store = store.clone();
    let concurrent_check = writer_a.clone();
    let memory = counted.memory.clone();
    counted.memory.hook(Op::Get, KEY, move || {
        let prepared = transfer(&rival_store, &current);
        rival_store.claim(&prepared, 43).unwrap();
        assert!(concurrent_check.verify_before_commit().is_err());
        assert_eq!(concurrent_check.state(), State::Fenced);
        memory.set_raw(KEY, &serde_json::to_vec(&current.record).unwrap());
    });
    error(
        writer_a.verify_before_ack(),
        StatusCode::BAD_GATEWAY,
        "write_outcome_unknown",
    );
    store.verify_writer(&claim_a).unwrap();
    assert_latched(&writer_a);
    assert_eq!(writer_a.measurements().count, 2);
}

#[test]
fn fencing_fault_clock_skew_never_changes_epoch_or_writer_identity_decisions() {
    for claimed_unix_ms in [0, 1, u64::MAX] {
        let (counted, store) = setup();
        let (claim, _) = serving(&store);
        let writer = fence(&counted, claim.clone());
        change(&counted.memory, |record| {
            record.claimed_unix_ms = Some(claimed_unix_ms);
        });
        let observed = store.verify_writer(&claim).unwrap();
        assert_eq!(observed.record.claimed_unix_ms, Some(claimed_unix_ms));
        writer.verify_before_commit().unwrap();
        writer.verify_before_ack().unwrap();
        assert!(writer.ready());
        assert_eq!(writer.exit_code(), None);
        let newer = Claim {
            epoch: claim.epoch + 1,
            ..claim.clone()
        };
        assert!(matches!(
            store.verify_writer(&newer),
            Err(AuthorityError::Fenced { observed_epoch: 1 })
        ));
        let newer_writer = fence(&counted, newer);
        assert!(newer_writer.verify_before_commit().is_err());
        assert_latched(&newer_writer);
        change(&counted.memory, |record| {
            record.writer_id = Some(if claim.writer_id == "0".repeat(32) {
                "1".repeat(32)
            } else {
                "0".repeat(32)
            });
        });
        assert!(matches!(
            store.verify_writer(&claim),
            Err(AuthorityError::Fenced { observed_epoch: 1 })
        ));
        assert!(writer.verify_before_commit().is_err());
        assert_latched(&writer);
    }
}

#[test]
fn fencing_fault_loss_bound_freezes_exact_last_successful_head() {
    let (counted, store) = setup();
    let (claim_a, first) = serving(&store);
    let last = store.advance_head(&claim_a, &first, head(1, 20)).unwrap();
    let rival_store = store.clone();
    let rival_last = last.clone();
    counted.memory.hook(Op::Put, KEY, move || {
        transfer(&rival_store, &rival_last);
    });
    let attempted = head(1, 30);
    assert!(matches!(
        store.advance_head(&claim_a, &last, attempted.clone()),
        Err(AuthorityError::Fenced { observed_epoch: 2 })
    ));
    let prepared = store.read().unwrap();
    assert_eq!(prepared.record.base, last.record.head);
    assert_eq!(prepared.record.base.as_ref(), Some(&head(1, 20)));
    assert_ne!(prepared.record.base.as_ref(), Some(&attempted));
    assert!(prepared.record.head.is_none());
    let (claim_b, claimed_b) = store.claim(&prepared, 43).unwrap();
    let served_b = store
        .advance_head(&claim_b, &claimed_b, head(2, 20))
        .unwrap();
    assert_eq!(served_b.record.base, last.record.head);
    let bytes = counted.memory.raw(KEY);
    let writes = counted.writes();
    assert!(matches!(
        store.advance_head(&claim_a, &served_b, attempted),
        Err(AuthorityError::Fenced { observed_epoch: 2 })
    ));
    assert_eq!(counted.writes(), writes);
    assert_eq!(counted.memory.raw(KEY), bytes);
    assert_eq!(store.read().unwrap(), served_b);
}

#[test]
fn fencing_fault_missing_or_garbage_authority_latches_without_store_writes() {
    for garbage in [false, true] {
        let (counted, store) = setup();
        let (claim, _) = serving(&store);
        let writer = fence(&counted, claim);
        let original = counted.memory.raw(KEY).unwrap();
        if garbage {
            counted.memory.set_raw(KEY, b"not valid authority JSON");
            assert!(matches!(store.read(), Err(AuthorityError::Invalid)));
        } else {
            counted.memory.remove(KEY);
            assert!(matches!(store.read(), Err(AuthorityError::Missing)));
        }
        let damaged = counted.memory.raw(KEY);
        let writes = counted.writes();
        let reads = counted.reads();
        error(
            writer.verify_before_commit(),
            StatusCode::SERVICE_UNAVAILABLE,
            "writer_authority_unavailable",
        );
        assert_eq!(counted.reads(), reads + 1);
        assert_latched(&writer);
        assert_eq!(counted.reads(), reads + 1);
        assert_eq!(counted.writes(), writes);
        assert_eq!(counted.memory.raw(KEY), damaged);
        counted.memory.set_raw(KEY, &original);
        assert_latched(&writer);
        assert_eq!(counted.reads(), reads + 1);
        assert_eq!(counted.writes(), writes);
    }
}
