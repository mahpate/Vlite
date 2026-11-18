//! Local, latched authority gates. No request or signal-drain wiring lives here.
#![cfg_attr(not(test), allow(dead_code))]

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::sync::watch;

use super::{
    ApiError,
    authority::{AuthorityError, AuthorityStore, Claim},
    gateway, log_stderr,
};

const SAMPLE_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Disabled,
    Starting,
    Claiming,
    Recovering,
    Serving,
    AuthorityUnavailable,
    Fenced,
    Draining,
    Stopped,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct TransitionError {
    pub from: State,
    pub to: State,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Measurements {
    pub count: u64,
    pub p50: Duration,
    pub p95: Duration,
    pub max: Duration,
}

struct Gate {
    state: State,
    uncertain: bool,
    authority_stop: bool,
    observed_epoch: Option<u64>,
    reason: Option<&'static str>,
    // A check begun before a concurrent failure cannot clear that failure.
    generation: u64,
    count: u64,
    samples: VecDeque<Duration>,
    max: Duration,
}

struct Inner {
    store: AuthorityStore,
    claim: Claim,
    gate: Mutex<Gate>,
    stop: watch::Sender<bool>,
}

#[derive(Clone)]
pub(super) struct WriterFence {
    // Disabled gates do not allocate, lock, consult a clock, or access a store.
    inner: Option<Arc<Inner>>,
}

impl WriterFence {
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn new(store: AuthorityStore, claim: Claim) -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            inner: Some(Arc::new(Inner {
                store,
                claim,
                stop,
                gate: Mutex::new(Gate {
                    state: State::Starting,
                    uncertain: false,
                    authority_stop: false,
                    observed_epoch: None,
                    reason: None,
                    generation: 0,
                    count: 0,
                    samples: VecDeque::with_capacity(SAMPLE_LIMIT),
                    max: Duration::ZERO,
                }),
            })),
        }
    }

    pub fn state(&self) -> State {
        self.inner
            .as_ref()
            .map_or(State::Disabled, |inner| inner.gate.lock().unwrap().state)
    }

    pub fn ready(&self) -> bool {
        matches!(self.state(), State::Disabled | State::Serving)
    }

    pub fn admit_mutation(&self) -> Result<(), ApiError> {
        if self.ready() {
            Ok(())
        } else {
            Err(AuthorityError::Unavailable.into())
        }
    }

    pub fn stop_signal(&self) -> Option<watch::Receiver<bool>> {
        self.inner.as_ref().map(|inner| inner.stop.subscribe())
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.gate.lock().unwrap().authority_stop.then_some(1))
    }

    /// Startup transitions are explicit; outage recovery requires a fresh check.
    pub fn transition(&self, to: State) -> Result<(), TransitionError> {
        let Some(inner) = &self.inner else {
            return Err(TransitionError {
                from: State::Disabled,
                to,
            });
        };
        let mut gate = inner.gate.lock().unwrap();
        let from = gate.state;
        if from == State::AuthorityUnavailable && to == State::Serving && !gate.uncertain {
            drop(gate);
            return self
                .verify_before_commit()
                .map_err(|_| TransitionError { from, to });
        }
        let allowed =
            matches!(
                (from, to),
                (State::Starting, State::Claiming | State::Recovering)
                    | (State::Claiming, State::Recovering | State::Serving)
                    | (State::Recovering, State::Serving)
                    | (State::Serving, State::AuthorityUnavailable)
                    | (
                        State::Starting
                            | State::Recovering
                            | State::Serving
                            | State::AuthorityUnavailable,
                        State::Fenced
                    )
                    | (State::Fenced, State::Draining)
                    | (State::Draining, State::Stopped)
            ) || (from == State::AuthorityUnavailable && to == State::Draining && gate.uncertain);
        if !allowed {
            return Err(TransitionError { from, to });
        }
        if to == State::Fenced {
            inner.fence(&mut gate, None, "authority_lost");
        } else {
            gate.state = to;
            gate.generation = gate.generation.wrapping_add(1);
            if to == State::Stopped {
                inner.event("writer_stopped", &gate);
            }
        }
        Ok(())
    }

    pub fn begin_drain(&self) -> Result<(), TransitionError> {
        if self.inner.is_none() {
            return Ok(());
        }
        self.transition(State::Draining)
    }

    pub fn stopped(&self) -> Result<(), TransitionError> {
        if self.inner.is_none() {
            return Ok(());
        }
        self.transition(State::Stopped)
    }

    /// Called by the deadline owner; this library does not own timers.
    pub fn drain_timeout(&self) -> Result<(), TransitionError> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let mut gate = inner.gate.lock().unwrap();
        if gate.state != State::Draining {
            return Err(TransitionError {
                from: gate.state,
                to: State::Stopped,
            });
        }
        inner.event("writer_drain_timeout", &gate);
        gate.state = State::Stopped;
        gate.generation = gate.generation.wrapping_add(1);
        inner.event("writer_stopped", &gate);
        Ok(())
    }

    pub fn verify_before_commit(&self) -> Result<(), ApiError> {
        self.verify(false)
            .map_err(|()| AuthorityError::Unavailable.into())
    }

    pub fn verify_before_ack(&self) -> Result<(), ApiError> {
        self.verify(true).map_err(|()| gateway::ambiguous_write())
    }

    fn verify(&self, after_commit: bool) -> Result<(), ()> {
        let Some(inner) = &self.inner else {
            return Ok(());
        };
        let generation = {
            let mut gate = inner.gate.lock().unwrap();
            if !matches!(gate.state, State::Serving | State::AuthorityUnavailable) || gate.uncertain
            {
                return Err(());
            }
            gate.count += 1;
            gate.generation
        };
        let start = Instant::now();
        let result = inner.store.verify_writer(&inner.claim);
        let elapsed = start.elapsed();
        let mut gate = inner.gate.lock().unwrap();
        if gate.samples.len() == SAMPLE_LIMIT {
            gate.samples.pop_front();
        }
        gate.samples.push_back(elapsed);
        gate.max = gate.max.max(elapsed);
        match result {
            Ok(_) => {
                if gate.generation == generation
                    && !gate.uncertain
                    && matches!(gate.state, State::Serving | State::AuthorityUnavailable)
                {
                    gate.state = State::Serving;
                    return Ok(());
                }
                if after_commit {
                    inner.unavailable(&mut gate, true);
                }
            }
            Err(AuthorityError::Unavailable) => {
                inner.unavailable(&mut gate, after_commit);
            }
            Err(error) => {
                let (epoch, reason) = match error {
                    AuthorityError::Fenced { observed_epoch } => (
                        Some(observed_epoch),
                        if observed_epoch < inner.claim.epoch {
                            "authority_regressed"
                        } else {
                            "writer_changed"
                        },
                    ),
                    AuthorityError::Missing => (None, "authority_missing"),
                    AuthorityError::Invalid => (None, "authority_invalid"),
                    _ => (None, "authority_lost"),
                };
                inner.fence(&mut gate, epoch, reason);
            }
        }
        Err(())
    }

    /// Percentiles cover the latest bounded sample; max/count cover all checks.
    pub fn measurements(&self) -> Measurements {
        let Some(inner) = &self.inner else {
            return Measurements::default();
        };
        let gate = inner.gate.lock().unwrap();
        let mut samples: Vec<_> = gate.samples.iter().copied().collect();
        samples.sort_unstable();
        let percentile = |percent: usize| {
            samples
                .get((samples.len() * percent).div_ceil(100).saturating_sub(1))
                .copied()
                .unwrap_or_default()
        };
        Measurements {
            count: gate.count,
            p50: percentile(50),
            p95: percentile(95),
            max: gate.max,
        }
    }
}

impl Inner {
    fn unavailable(&self, gate: &mut Gate, after_commit: bool) {
        if matches!(gate.state, State::Serving | State::AuthorityUnavailable) {
            gate.state = State::AuthorityUnavailable;
            gate.generation = gate.generation.wrapping_add(1);
            if after_commit {
                gate.uncertain = true;
                gate.authority_stop = true;
                gate.reason = Some("uncertain_commit");
                self.stop.send_replace(true);
            }
        }
    }

    fn fence(&self, gate: &mut Gate, epoch: Option<u64>, reason: &'static str) {
        if matches!(gate.state, State::Fenced | State::Draining | State::Stopped) {
            return;
        }
        gate.state = State::Fenced;
        gate.authority_stop = true;
        gate.observed_epoch = epoch;
        gate.reason = Some(reason);
        gate.generation = gate.generation.wrapping_add(1);
        self.stop.send_replace(true);
        self.event("writer_fenced", gate);
    }

    fn event(&self, event: &'static str, gate: &Gate) {
        let writer_prefix: String = self
            .claim
            .writer_id
            .chars()
            .filter(char::is_ascii_hexdigit)
            .take(8)
            .collect();
        log_stderr(
            "error",
            "writer_fence",
            json!({
                "event": event,
                "local_epoch": self.claim.epoch,
                "observed_epoch": gate.observed_epoch,
                "writer_prefix": writer_prefix,
                "reason": gate.reason,
            })
            .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::authority::Record;
    use crate::server::cloud_backup::fake::{Fault, Memory, Op};
    use axum::http::StatusCode;

    const KEY: &str = "test/authority.json";

    fn setup() -> (Arc<Memory>, WriterFence) {
        let memory = Arc::new(Memory::default());
        let store = AuthorityStore::new(memory.clone(), "test/".into());
        let prepared = store
            .create("installation", "scope", "0123456789abcdef")
            .unwrap();
        let (claim, _) = store.claim(&prepared, 1).unwrap();
        let fence = WriterFence::new(store, claim);
        fence.transition(State::Recovering).unwrap();
        fence.transition(State::Serving).unwrap();
        (memory, fence)
    }

    fn change(memory: &Memory, update: impl FnOnce(&mut Record)) {
        let mut record: Record = serde_json::from_slice(&memory.raw(KEY).unwrap()).unwrap();
        update(&mut record);
        memory.set_raw(KEY, &serde_json::to_vec(&record).unwrap());
    }

    fn unavailable(memory: &Memory) {
        memory.fault(Op::Get, KEY, Fault::Refuse, 0, 1);
    }

    fn error(result: Result<(), ApiError>, status: StatusCode, code: &str) {
        let error = result.unwrap_err();
        assert_eq!(error.status, status);
        assert_eq!(error.code, code);
    }

    #[test]
    fn writer_fence_disabled_is_free() {
        let fence = WriterFence::disabled();
        for _ in 0..3 {
            fence.admit_mutation().unwrap();
            fence.verify_before_commit().unwrap();
            fence.verify_before_ack().unwrap();
            fence.begin_drain().unwrap();
            fence.stopped().unwrap();
            assert!(fence.ready());
        }
        assert_eq!(fence.state(), State::Disabled);
        assert_eq!(fence.measurements().count, 0);
        assert_eq!(fence.exit_code(), None);
        assert!(fence.stop_signal().is_none());
    }

    #[test]
    fn writer_fenced_after_local_commit_is_ambiguous() {
        let (memory, fence) = setup();
        fence.verify_before_commit().unwrap();
        let committed = true;
        change(&memory, |record| record.epoch += 1);
        error(
            fence.verify_before_ack(),
            StatusCode::BAD_GATEWAY,
            "write_outcome_unknown",
        );
        assert!(committed);
        assert_eq!(fence.state(), State::Fenced);
    }

    #[test]
    fn writer_fenced_latch_stops_queue_and_readiness() {
        let (memory, fence) = setup();
        let mut stop = fence.stop_signal().unwrap();
        change(&memory, |record| record.epoch += 1);
        assert!(fence.verify_before_ack().is_err());
        assert!(stop.has_changed().unwrap());
        assert!(*stop.borrow_and_update());
        for next in [State::Fenced, State::Draining, State::Stopped] {
            assert_eq!(fence.state(), next);
            error(
                fence.admit_mutation(),
                StatusCode::SERVICE_UNAVAILABLE,
                "writer_authority_unavailable",
            );
            assert!(!fence.ready());
            assert!(fence.transition(State::Serving).is_err());
            assert!(fence.verify_before_ack().is_err());
            match next {
                State::Fenced => fence.begin_drain().unwrap(),
                State::Draining => fence.stopped().unwrap(),
                _ => {}
            }
        }
    }

    #[test]
    fn writer_fenced_on_different_writer_id() {
        let (memory, fence) = setup();
        change(&memory, |record| record.writer_id = Some("f".repeat(32)));
        assert!(fence.verify_before_commit().is_err());
        assert_eq!(fence.state(), State::Fenced);
    }

    #[test]
    fn writer_fence_regression_or_missing_is_terminal() {
        for mode in 0..3 {
            let (memory, fence) = setup();
            match mode {
                0 => {
                    // A valid older authority, not merely an invalid epoch zero.
                    let mut claim = fence.inner.as_ref().unwrap().claim.clone();
                    claim.epoch = 2;
                    let store = AuthorityStore::new(memory.clone(), "test/".into());
                    let newer = WriterFence::new(store, claim);
                    newer.transition(State::Recovering).unwrap();
                    newer.transition(State::Serving).unwrap();
                    assert!(newer.verify_before_commit().is_err());
                    assert_eq!(newer.state(), State::Fenced);
                    continue;
                }
                1 => memory.remove(KEY),
                _ => memory.set_raw(KEY, b"not json"),
            }
            assert!(fence.verify_before_commit().is_err());
            assert_eq!(fence.state(), State::Fenced);
            assert!(*fence.stop_signal().unwrap().borrow());
        }
    }

    #[test]
    fn writer_fence_transient_unavailable_recovers_without_uncertain_commit() {
        let (memory, fence) = setup();
        unavailable(&memory);
        assert!(fence.verify_before_commit().is_err());
        assert_eq!(fence.state(), State::AuthorityUnavailable);
        assert!(!fence.ready());
        assert!(!*fence.stop_signal().unwrap().borrow());
        fence.verify_before_commit().unwrap();
        assert!(fence.ready());
        assert_eq!(fence.exit_code(), None);
    }

    #[test]
    fn writer_fence_uncertain_commit_cannot_resume() {
        let (memory, fence) = setup();
        fence.verify_before_commit().unwrap();
        unavailable(&memory);
        error(
            fence.verify_before_ack(),
            StatusCode::BAD_GATEWAY,
            "write_outcome_unknown",
        );
        assert_eq!(fence.state(), State::AuthorityUnavailable);
        assert!(*fence.stop_signal().unwrap().borrow());
        assert!(fence.verify_before_commit().is_err());
        assert!(fence.verify_before_ack().is_err());
        assert!(fence.transition(State::Serving).is_err());
        fence.begin_drain().unwrap();
        fence.drain_timeout().unwrap();
        assert_eq!(fence.state(), State::Stopped);
        assert_eq!(fence.exit_code(), Some(1));
    }

    #[test]
    fn writer_fence_precommit_failure_is_503_without_mutation() {
        let (memory, fence) = setup();
        unavailable(&memory);
        let mut mutations = 0;
        let result = fence.verify_before_commit().map(|()| mutations += 1);
        error(
            result,
            StatusCode::SERVICE_UNAVAILABLE,
            "writer_authority_unavailable",
        );
        assert_eq!(mutations, 0);
        assert!(fence.admit_mutation().is_err());
    }

    #[test]
    fn writer_fence_exit_code_is_one_when_fenced() {
        let (memory, fence) = setup();
        assert_eq!(fence.exit_code(), None);
        change(&memory, |record| record.epoch += 1);
        assert!(fence.verify_before_ack().is_err());
        assert_eq!(fence.exit_code(), Some(1));
        fence.begin_drain().unwrap();
        fence.stopped().unwrap();
        assert_eq!(fence.exit_code(), Some(1));
    }

    #[test]
    fn writer_fence_counts_gets_and_latency() {
        let (memory, fence) = setup();
        for _ in 0..SAMPLE_LIMIT + 5 {
            fence.verify_before_commit().unwrap();
        }
        unavailable(&memory);
        assert!(fence.verify_before_commit().is_err());
        fence.verify_before_ack().unwrap();
        let measurements = fence.measurements();
        assert_eq!(measurements.count, (SAMPLE_LIMIT + 7) as u64);
        assert!(measurements.p50 <= measurements.p95);
        assert!(measurements.p95 <= measurements.max);
        let inner = fence.inner.as_ref().unwrap().gate.lock().unwrap();
        assert_eq!(inner.samples.len(), SAMPLE_LIMIT);
    }

    #[test]
    fn writer_fence_refuses_illegal_transitions() {
        let (_, fence) = setup();
        for state in [
            State::Disabled,
            State::Starting,
            State::Claiming,
            State::Recovering,
            State::Serving,
            State::Draining,
            State::Stopped,
        ] {
            assert!(fence.transition(state).is_err());
            assert_eq!(fence.state(), State::Serving);
        }
    }

    #[test]
    fn writer_fence_recovery_transition_requires_fresh_authority() {
        let (memory, fence) = setup();
        unavailable(&memory);
        assert!(fence.verify_before_commit().is_err());
        unavailable(&memory);
        assert!(fence.transition(State::Serving).is_err());
        assert_eq!(fence.state(), State::AuthorityUnavailable);
        fence.transition(State::Serving).unwrap();
        assert!(fence.ready());
        assert_eq!(fence.measurements().count, 3);
    }

    #[test]
    fn writer_fence_older_success_cannot_clear_concurrent_outage() {
        let (memory, fence) = setup();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let paused = barrier.clone();
        memory.hook(Op::Get, KEY, move || {
            paused.wait();
            paused.wait();
        });
        let worker = fence.clone();
        let thread = std::thread::spawn(move || worker.verify_before_commit());
        barrier.wait();
        unavailable(&memory);
        assert!(fence.verify_before_commit().is_err());
        barrier.wait();
        assert!(thread.join().unwrap().is_err());
        assert_eq!(fence.state(), State::AuthorityUnavailable);
        assert!(!fence.ready());
        fence.verify_before_commit().unwrap();
        assert!(fence.ready());
    }

    #[test]
    fn writer_fence_paused_verification_cannot_resurrect_latched_fence() {
        let (memory, fence) = setup();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let paused = barrier.clone();
        memory.hook(Op::Get, KEY, move || {
            paused.wait();
            paused.wait();
        });
        let worker = fence.clone();
        let thread = std::thread::spawn(move || worker.verify_before_ack());
        barrier.wait();
        change(&memory, |record| record.epoch += 1);
        assert!(fence.verify_before_commit().is_err());
        // Even a later restored matching record cannot undo the local latch.
        change(&memory, |record| record.epoch -= 1);
        barrier.wait();
        assert!(thread.join().unwrap().is_err());
        assert_eq!(fence.state(), State::Fenced);
        assert!(!fence.ready());
    }
}
